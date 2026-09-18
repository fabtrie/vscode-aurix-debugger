//! The API operations on the debug core and the session shared with the TCF frontend.
//!
//! Each operation runs as one job on the core thread ([`ServerHandle::call`]), with the same
//! `ops` functions the other frontends use, so their events reach all clients. Waiting happens
//! here, never on the core thread.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use api_spec::types::*;
use api_spec::{ApiError, DebuggerApi, ErrorCode, MAX_TRANSFER};
use aurix_core::arch::RegGroup;
use aurix_core::events::emit;
use aurix_core::target::{DebugReason, TargetState};
use aurix_core::{ops, Cx, Event, ServerHandle};

use crate::events::Hub;

pub struct CoreApi {
    pub handle: ServerHandle,
    pub session: Arc<tcf::Shared>,
    pub hub: Arc<Hub>,
}

fn api_err(e: aurix_core::Error) -> ApiError {
    use aurix_core::Error as E;
    let code = match &e {
        E::Adapter(_) => ErrorCode::Adapter,
        E::DeviceLocked => ErrorCode::DeviceLocked,
        E::DeviceAccess => ErrorCode::DeviceAccess,
        E::Memory { .. } => ErrorCode::Memory,
        E::NotHalted(_) => ErrorCode::NotHalted,
        E::NotExamined(_) => ErrorCode::NotExamined,
        E::Resource(_) => ErrorCode::Resource,
        E::Timeout(_) => ErrorCode::Timeout,
        E::Usage(_) => ErrorCode::BadRequest,
        E::Other(_) => ErrorCode::Failed,
    };
    ApiError::new(code, e.to_string())
}

fn busy(target: &str) -> ApiError {
    ApiError::new(ErrorCode::Busy, format!("[{target}] a source-level step of a debugger client is in progress"))
}

fn resolve(cx: &Cx, name: &str) -> Result<usize, ApiError> {
    cx.with(|c| c.target_by_name(name)).ok_or_else(|| ApiError::not_found(format!("unknown target '{name}' (see `targets`)")))
}

fn run_state(s: TargetState) -> RunState {
    match s {
        TargetState::Unknown => RunState::Unknown,
        TargetState::Running => RunState::Running,
        TargetState::Halted => RunState::Halted,
        TargetState::Reset => RunState::Reset,
        TargetState::DebugRunning => RunState::DebugRunning,
    }
}

fn info(cx: &Cx, t: usize) -> TargetInfo {
    let (name, arch, state, reason, examined, reachable, pc_reg) = cx.with(|c| {
        let e = &c.targets[t];
        let arch = e.core.arch();
        (e.name.clone(), arch.name, e.core.state(), e.core.reason(), e.core.examined(), !e.unreachable, arch.pc)
    });
    let halted = examined && state == TargetState::Halted;
    let pc = if halted { ops::read_reg(cx, t, pc_reg).ok() } else { None };
    let reason = if halted { reason } else { DebugReason::NotHalted };
    TargetInfo { index: t as u32, name, arch: arch.into(), state: run_state(state), reason: reason.name().into(), examined, reachable, pc }
}

fn halted(cx: &Cx, t: usize) -> bool {
    cx.with(|c| c.targets[t].core.examined() && c.targets[t].core.state() == TargetState::Halted)
}

fn reg_group(g: RegGroup) -> RegisterGroup {
    if g == RegGroup::Debug {
        RegisterGroup::Debug
    } else {
        RegisterGroup::Core
    }
}

/// Register index of `name` on target `t`; core registers need a halted target.
fn register(cx: &Cx, t: usize, name: &str) -> Result<usize, ApiError> {
    let (arch, tname) = cx.with(|c| (c.targets[t].core.arch(), c.targets[t].name.clone()));
    let idx = arch.reg_index(name).ok_or_else(|| ApiError::not_found(format!("[{tname}] unknown register '{name}'")))?;
    if arch.regs[idx].group != RegGroup::Debug && !halted(cx, t) {
        return Err(ApiError::new(ErrorCode::NotHalted, format!("[{tname}] target not halted (core registers need a halted target)")));
    }
    Ok(idx)
}

fn register_value(cx: &Cx, t: usize, idx: usize) -> Result<RegisterValue, ApiError> {
    let r = &cx.with(|c| c.targets[t].core.arch()).regs[idx];
    let value = ops::read_reg(cx, t, idx).map_err(api_err)?;
    Ok(RegisterValue { name: r.name.into(), value, group: reg_group(r.group), description: r.description.into() })
}

fn check_range(address: u32, len: usize) -> Result<(), ApiError> {
    if len == 0 || len > MAX_TRANSFER as usize {
        return Err(ApiError::bad_request(format!("length must be 1..{MAX_TRANSFER} bytes, got {len}")));
    }
    if address as u64 + len as u64 > 1 << 32 {
        return Err(ApiError::bad_request("the range exceeds the 32-bit address space"));
    }
    Ok(())
}

fn watch_access(a: aurix_core::target::WatchAccess) -> WatchAccess {
    match a {
        aurix_core::target::WatchAccess::Read => WatchAccess::Read,
        aurix_core::target::WatchAccess::Write => WatchAccess::Write,
        aurix_core::target::WatchAccess::Access => WatchAccess::Access,
    }
}

fn watchpoints(cx: &Cx) -> WatchpointList {
    let watchpoints = cx.with(|c| {
        c.targets
            .iter()
            .flat_map(|e| {
                e.watchpoints.iter().map(|w| WatchpointInfo { id: w.id, target: e.name.clone(), address: w.addr, length: w.len, access: watch_access(w.access) })
            })
            .collect()
    });
    WatchpointList { watchpoints }
}

fn addr_prop(v: &Value) -> Option<u32> {
    match v {
        Value::Number(n) => n.as_u64().and_then(|x| u32::try_from(x).ok()),
        Value::String(s) => parse_u32(s),
        _ => None,
    }
}

fn breakpoint_info(id: &str, bp: &tcf::Bp, names: &HashMap<usize, String>, session: &tcf::Shared) -> BreakpointInfo {
    let p = &bp.props;
    let location = p.get("Location");
    let address = p.get("Address").and_then(addr_prop).or_else(|| location.and_then(addr_prop));
    let symbol = location.filter(|l| addr_prop(l).is_none()).and_then(Value::as_str).map(str::to_string);
    let instances = bp
        .instances
        .iter()
        .map(|i| {
            let (file, line) = session.source_line(&i.ctx, i.addr).unzip();
            BreakpointInstance { target: names.get(&i.target).cloned().unwrap_or_else(|| i.target.to_string()), address: i.addr, file, line, error: i.error.clone() }
        })
        .collect();
    BreakpointInfo {
        id: id.to_string(),
        owner: if bp.owner == tcf::API_OWNER { BreakpointOwner::Api } else { BreakpointOwner::Client },
        enabled: bp.enabled(),
        address,
        symbol,
        file: p.get("File").and_then(Value::as_str).map(str::to_string),
        line: bp.line.or_else(|| p.get("Line").and_then(Value::as_u64).map(|l| l as u32)),
        instances,
        hits: bp.hits,
        error: bp.error.clone(),
    }
}

impl CoreApi {
    /// Runs `f` on the core thread.
    fn call<R, F>(&self, f: F) -> Result<R, ApiError>
    where
        R: Send + 'static,
        F: for<'a, 'b> FnOnce(&'b Cx<'a>) -> Result<R, ApiError> + Send + 'static,
    {
        self.handle.call(f).unwrap_or_else(|| Err(ApiError::failed("the debug server stopped")))
    }

    fn target(&self, name: &str) -> Result<usize, ApiError> {
        let name = name.to_string();
        self.call(move |cx| resolve(cx, &name))
    }

    fn state_of(&self, t: usize) -> Result<TargetInfo, ApiError> {
        self.call(move |cx| Ok(info(cx, t)))
    }

    fn names(&self) -> Result<HashMap<usize, String>, ApiError> {
        self.call(|cx| Ok(cx.with(|c| c.targets.iter().enumerate().map(|(i, e)| (i, e.name.clone())).collect())))
    }

    /// Waits for a state change without blocking the core thread: woken by the core events,
    /// the state is checked again at least every 500 ms.
    fn wait_for(&self, t: usize, want: WaitFor, timeout: Duration) -> Result<TargetInfo, ApiError> {
        let wake = self.hub.waiter();
        let deadline = Instant::now() + timeout;
        loop {
            let st = self.state_of(t)?;
            let reached = match want {
                WaitFor::Halted => st.state == RunState::Halted,
                WaitFor::Running => matches!(st.state, RunState::Running | RunState::DebugRunning),
            };
            if reached {
                return Ok(st);
            }
            let now = Instant::now();
            if now >= deadline {
                let what = if want == WaitFor::Halted { "halted" } else { "running" };
                return Err(ApiError::new(ErrorCode::Timeout, format!("[{}] not {what} after {} ms", st.name, timeout.as_millis())));
            }
            let _ = wake.recv_timeout((deadline - now).min(Duration::from_millis(500)));
        }
    }

    fn breakpoint_list(&self) -> Result<BreakpointList, ApiError> {
        let names = self.names()?;
        Ok(BreakpointList { breakpoints: self.session.breakpoints().iter().map(|(id, bp)| breakpoint_info(id, bp, &names, &self.session)).collect() })
    }

    fn tcl_command(&self, command: &str) -> Result<CommandOutput, ApiError> {
        let (res, lines) = self.handle.tcl_with_output(command);
        let mut out = lines.join("\n");
        match res {
            Ok(r) => {
                if !r.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&r);
                }
                Ok(CommandOutput { output: out })
            }
            Err(e) => Err(ApiError::failed(if out.is_empty() { e } else { format!("{out}\n{e}") })),
        }
    }
}

impl DebuggerApi for CoreApi {
    fn targets(&self, _: NoArgs) -> Result<TargetList, ApiError> {
        self.call(|cx| Ok(TargetList { targets: (0..cx.with(|c| c.targets.len())).map(|t| info(cx, t)).collect() }))
    }

    fn state(&self, req: TargetRef) -> Result<TargetInfo, ApiError> {
        self.call(move |cx| Ok(info(cx, resolve(cx, &req.target)?)))
    }

    fn halt(&self, req: TargetRef) -> Result<TargetInfo, ApiError> {
        let t = self.target(&req.target)?;
        // Pause during a source-level step of a TCF client stops the step (it reports the halt).
        if !self.session.cancel_step(t) {
            self.call(move |cx| if halted(cx, t) { Ok(()) } else { ops::halt(cx, t).map_err(api_err) })?;
        }
        match self.wait_for(t, WaitFor::Halted, Duration::from_secs(1)) {
            Err(e) if e.code == ErrorCode::Timeout => self.state_of(t),
            r => r,
        }
    }

    fn resume(&self, req: ResumeRequest) -> Result<TargetInfo, ApiError> {
        let t = self.target(&req.target)?;
        if self.session.is_stepping(t) {
            return Err(busy(&req.target));
        }
        let addr = req.address.map(|a| a.0);
        self.call(move |cx| {
            if !halted(cx, t) {
                return Err(ApiError::new(ErrorCode::NotHalted, format!("[{}] target not halted (already running)", req.target)));
            }
            ops::resume(cx, t, addr).map_err(api_err)?;
            Ok(info(cx, t))
        })
    }

    fn step(&self, req: TargetRef) -> Result<TargetInfo, ApiError> {
        let t = self.target(&req.target)?;
        if self.session.is_stepping(t) {
            return Err(busy(&req.target));
        }
        self.call(move |cx| {
            if !halted(cx, t) {
                return Err(ApiError::new(ErrorCode::NotHalted, format!("[{}] target not halted", req.target)));
            }
            // Like a TCF instruction step: the clients see the core run, then halt again.
            emit(Event::Resumed { target: t, name: ops::target_name(cx, t) });
            ops::step(cx, t, None).map_err(api_err)?;
            Ok(info(cx, t))
        })
    }

    fn wait_state(&self, req: WaitRequest) -> Result<TargetInfo, ApiError> {
        let t = self.target(&req.target)?;
        let ms = req.timeout_ms.unwrap_or(5000).min(600_000);
        self.wait_for(t, req.state.unwrap_or(WaitFor::Halted), Duration::from_millis(ms as u64))
    }

    fn reset(&self, req: ResetRequest) -> Result<TargetList, ApiError> {
        if self.session.any_stepping() {
            return Err(ApiError::new(ErrorCode::Busy, "a source-level step of a debugger client is in progress"));
        }
        let mode = match req.mode.unwrap_or(ResetMode::Halt) {
            ResetMode::Run => "run",
            ResetMode::Halt => "halt",
            ResetMode::Init => "init",
        };
        self.call(move |cx| {
            ops::reset(cx, mode).map_err(api_err)?;
            Ok(TargetList { targets: (0..cx.with(|c| c.targets.len())).map(|t| info(cx, t)).collect() })
        })
    }

    fn read_registers(&self, req: ReadRegistersRequest) -> Result<RegisterList, ApiError> {
        let group = req.group.unwrap_or(RegisterGroup::Core);
        self.call(move |cx| {
            let t = resolve(cx, &req.target)?;
            let (arch, name) = cx.with(|c| (c.targets[t].core.arch(), c.targets[t].name.clone()));
            if group == RegisterGroup::Core && !halted(cx, t) {
                return Err(ApiError::new(ErrorCode::NotHalted, format!("[{name}] target not halted (core registers need a halted target)")));
            }
            let registers = (0..arch.regs.len())
                .filter(|&i| reg_group(arch.regs[i].group) == group)
                .map(|i| register_value(cx, t, i))
                .collect::<Result<_, _>>()?;
            Ok(RegisterList { target: name, registers })
        })
    }

    fn read_register(&self, req: RegisterRef) -> Result<RegisterValue, ApiError> {
        self.call(move |cx| {
            let t = resolve(cx, &req.target)?;
            let idx = register(cx, t, &req.name)?;
            register_value(cx, t, idx)
        })
    }

    fn write_register(&self, req: WriteRegisterRequest) -> Result<RegisterValue, ApiError> {
        self.call(move |cx| {
            let t = resolve(cx, &req.target)?;
            let idx = register(cx, t, &req.name)?;
            ops::write_reg(cx, t, idx, req.value.0).map_err(api_err)?;
            let v = register_value(cx, t, idx)?;
            emit(Event::RegisterChanged { target: t, name: v.name.clone() });
            Ok(v)
        })
    }

    fn read_memory(&self, req: ReadMemoryRequest) -> Result<MemoryData, ApiError> {
        let (address, len) = (req.address.0, req.length as usize);
        check_range(address, len)?;
        self.call(move |cx| {
            let data = match &req.target {
                Some(name) => ops::read_memory_as(cx, resolve(cx, name)?, address, len),
                None => ops::read_memory(cx, address, len),
            }
            .map_err(api_err)?;
            Ok(MemoryData { address, data: Hex(data) })
        })
    }

    fn write_memory(&self, req: WriteMemoryRequest) -> Result<MemoryWritten, ApiError> {
        let address = req.address.0;
        let len = req.data.0.len();
        check_range(address, len)?;
        self.call(move |cx| {
            let t = req.target.as_deref().map(|n| resolve(cx, n)).transpose()?;
            match t {
                Some(t) => ops::write_memory_as(cx, t, address, &req.data.0),
                None => ops::write_memory(cx, address, &req.data.0),
            }
            .map_err(api_err)?;
            emit(Event::MemoryChanged { target: t, addr: address, len: len as u32 });
            Ok(MemoryWritten { address, length: len as u32 })
        })
    }

    fn list_breakpoints(&self, _: NoArgs) -> Result<BreakpointList, ApiError> {
        self.breakpoint_list()
    }

    fn add_breakpoint(&self, req: AddBreakpointRequest) -> Result<BreakpointInfo, ApiError> {
        let id = self.session.next_api_breakpoint_id();
        let mut props: Map<String, Value> = Map::new();
        props.insert("ID".into(), json!(id));
        props.insert("Enabled".into(), json!(true));
        props.insert("BreakpointType".into(), json!("Hardware"));
        match (&req.address, &req.symbol, &req.file, req.line) {
            (Some(a), None, None, None) => {
                props.insert("Address".into(), json!(a.0));
            }
            (None, Some(s), None, None) => {
                props.insert("Location".into(), json!(s));
            }
            (None, None, Some(f), Some(l)) => {
                props.insert("File".into(), json!(f));
                props.insert("Line".into(), json!(l));
            }
            _ => return Err(ApiError::bad_request("give exactly one of `address`, `symbol` or `file` with `line`")),
        }
        if let Some(targets) = &req.targets {
            let cores = tcf::cores(&self.handle);
            let ids = targets
                .iter()
                .map(|n| {
                    cores.iter().find(|c| &c.name == n || c.idx.to_string() == *n).map(|c| c.id.clone()).ok_or_else(|| ApiError::not_found(format!("unknown target '{n}'")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            props.insert("ContextIds".into(), json!(ids));
        }
        self.session.apply_breakpoints(vec![props], false, tcf::API_OWNER).map_err(ApiError::bad_request)?;
        let (bp, _) = self.session.breakpoint(&id).ok_or_else(|| ApiError::failed("breakpoint vanished"))?;
        // An address breakpoint that cannot be planted will not be later: report and drop it.
        if req.address.is_some() && !bp.planted() {
            self.session.remove_breakpoints(vec![id]);
            let why = bp.error.clone().or_else(|| bp.instances.iter().find_map(|i| i.error.clone())).unwrap_or_else(|| "not planted".into());
            return Err(ApiError::new(ErrorCode::Resource, why));
        }
        Ok(breakpoint_info(&id, &bp, &self.names()?, &self.session))
    }

    fn remove_breakpoint(&self, req: BreakpointRef) -> Result<BreakpointList, ApiError> {
        if self.session.remove_breakpoints(vec![req.id.clone()]).is_empty() {
            return Err(ApiError::not_found(format!("unknown breakpoint '{}'", req.id)));
        }
        self.breakpoint_list()
    }

    fn list_watchpoints(&self, _: NoArgs) -> Result<WatchpointList, ApiError> {
        self.call(|cx| Ok(watchpoints(cx)))
    }

    fn add_watchpoint(&self, req: AddWatchpointRequest) -> Result<WatchpointInfo, ApiError> {
        let access = req.access.unwrap_or(WatchAccess::Write);
        let len = req.length.unwrap_or(4);
        self.call(move |cx| {
            let t = resolve(cx, &req.target)?;
            let a = match access {
                WatchAccess::Read => aurix_core::target::WatchAccess::Read,
                WatchAccess::Write => aurix_core::target::WatchAccess::Write,
                WatchAccess::Access => aurix_core::target::WatchAccess::Access,
            };
            let id = ops::add_watchpoint(cx, t, req.address.0, len, a).map_err(api_err)?;
            Ok(WatchpointInfo { id, target: ops::target_name(cx, t), address: req.address.0, length: len, access })
        })
    }

    fn remove_watchpoint(&self, req: WatchpointRef) -> Result<WatchpointList, ApiError> {
        self.call(move |cx| {
            let t = resolve(cx, &req.target)?;
            ops::remove_watchpoint(cx, t, req.address.0).map_err(api_err)?;
            Ok(watchpoints(cx))
        })
    }

    fn flash_write(&self, req: FlashWriteRequest) -> Result<CommandOutput, ApiError> {
        if req.files.is_empty() {
            return Err(ApiError::bad_request("no image files given"));
        }
        let mut cmd = String::from("flash write_images");
        for (flag, on) in [("erase", req.erase), ("incremental", req.incremental), ("verify", req.verify)] {
            if on {
                cmd += " ";
                cmd += flag;
            }
        }
        for f in &req.files {
            cmd += " ";
            cmd += &jim::quote(f);
        }
        self.tcl_command(&cmd)
    }

    fn flash_verify(&self, req: FlashVerifyRequest) -> Result<CommandOutput, ApiError> {
        self.tcl_command(&format!("verify_image {}", jim::quote(&req.file)))
    }

    fn tcl(&self, req: TclRequest) -> Result<CommandOutput, ApiError> {
        self.tcl_command(&req.command)
    }
}
