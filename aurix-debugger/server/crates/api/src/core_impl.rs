//! The API operations on the debug core and the session shared with the TCF frontend.
//!
//! Each operation runs as one job on the core thread ([`ServerHandle::call`]), with the same
//! `ops` functions the other frontends use, so their events reach all clients. Waiting happens
//! here, never on the core thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use api_spec::types::*;
use api_spec::{ApiError, DebuggerApi, ErrorCode, API_VERSION, MAX_TRANSFER};
use aurix_core::arch::RegGroup;
use aurix_core::events::emit;
use aurix_core::target::{DebugReason, TargetState};
use aurix_core::{ops, Cx, Event, ServerHandle};

use crate::events::Hub;
use crate::orti_state::CoreTarget;
use crate::variables;
use orti::Awareness;

pub struct CoreApi {
    pub handle: ServerHandle,
    pub session: Arc<tcf::Shared>,
    pub hub: Arc<Hub>,
    /// Sorted global lists for `list_globals`.
    pub globals: variables::GlobalsCache,
    /// The symbol files `info` last saw, for while a load holds the symbol store.
    pub symbol_files: Mutex<Vec<InfoSymbolFile>>,
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

/// Splits a `load_symbols` file into its path and the TriCore cores of a trailing `@N` or `@N,M`,
/// as VS Code's `symbolFiles` writes them. A file that exists as given is never split, so a path
/// that really ends in `@2` still loads.
fn split_run_cores(f: &str) -> (PathBuf, Vec<u32>) {
    let whole = || (PathBuf::from(f), Vec::new());
    let Some((path, list)) = f.trim().rsplit_once('@') else { return whole() };
    if Path::new(f).is_file() || path.trim().is_empty() {
        return whole();
    }
    let cores: Option<Vec<u32>> = list.split(',').map(|n| n.trim().parse().ok()).collect();
    match cores {
        Some(c) => (PathBuf::from(path.trim_end()), c),
        None => whole(),
    }
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

    /// The target and primary ELF an ORTI file is resolved against.
    ///
    /// ORTI names plain globals (`OsCfg_Task_…`), which every core's ELF sees the same way, so
    /// without an explicit target the first core that has symbols is as good as any.
    fn orti_symbols(&self, want: Option<&str>) -> Result<(String, Arc<symbols::Symbols>), ApiError> {
        let cores = tcf::cores(&self.handle);
        let chosen: Vec<&tcf::CoreInfo> = match want {
            Some(n) => vec![cores
                .iter()
                .find(|c| c.name == n || c.idx.to_string() == n)
                .ok_or_else(|| ApiError::not_found(format!("unknown target '{n}' (see `targets`)")))?],
            None => cores.iter().collect(),
        };
        for c in chosen {
            let ctx = tcf::owning_core(&c.id).unwrap_or_else(|| c.id.clone());
            if let Some(set) = self.session.symbols_for(&ctx) {
                if let Some((_, syms)) = set.elfs.first() {
                    return Ok((c.name.clone(), syms.clone()));
                }
            }
        }
        Err(ApiError::bad_request("no symbols loaded (use `load_symbols`, or start a VS Code debug session)"))
    }

    /// The symbol set of every core that has one, primary ELF first.
    fn symbols_info(&self) -> SymbolsInfo {
        let cores = tcf::cores(&self.handle);
        let store = self.session.symbols.lock().unwrap();
        let targets = cores
            .iter()
            .filter_map(|c| {
                let ctx = tcf::owning_core(&c.id).unwrap_or_else(|| c.id.clone());
                let set = store.set(&ctx)?;
                Some(SymbolsForTarget {
                    target: c.name.clone(),
                    files: set.elfs.iter().map(|(_, s)| s.path.to_string_lossy().into_owned()).collect(),
                    functions: set.elfs.iter().map(|(_, s)| s.functions.len() as u32).sum(),
                    globals: set.elfs.iter().map(|(_, s)| s.globals.len() as u32).sum(),
                    source_files: set.elfs.iter().map(|(_, s)| s.source_files() as u32).sum(),
                })
            })
            .collect();
        SymbolsInfo { targets, load_seconds: 0.0 }
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

/// The `os` / `load_orti` view of a loaded ORTI file.
fn os_info(aw: &Awareness, target: Option<String>) -> OsInfo {
    let types = aw
        .orti
        .decls
        .iter()
        .map(|d| OsTypeInfo {
            name: d.name.clone(),
            description: d.desc.clone(),
            count: aw.objects_of(&d.name).count() as u32,
            attributes: d
                .attrs
                .iter()
                .map(|a| OsAttributeInfo {
                    name: a.name.clone(),
                    description: a.desc.clone(),
                    kind: match a.kind {
                        orti::AttrKind::CType => "ctype",
                        orti::AttrKind::Enum => "enum",
                        orti::AttrKind::String => "string",
                    }
                    .into(),
                    per_core: a.indexed,
                    // Whether it is live is a property of the planned instances, not of the
                    // declaration: `STACK` is an address constant in every task.
                    live: aw
                        .objects_of(&d.name)
                        .flat_map(|o| o.attrs.iter())
                        .any(|p| p.name == a.name && p.plan.is_live()),
                })
                .collect(),
        })
        .collect();
    OsInfo {
        file: Some(aw.file.to_string_lossy().into_owned()),
        implementation: Some(aw.orti.implementation.clone()),
        version: aw.orti.version.semantics_version.clone(),
        cores: aw.orti.core_count(),
        target,
        types,
        errors: aw.errors.clone(),
    }
}

impl DebuggerApi for CoreApi {
    fn info(&self, _: NoArgs) -> Result<ServerInfo, ApiError> {
        // The liveness probe: nothing here may wait for the core thread.
        let (status, lag) = self.handle.status();
        let (symbol_files, symbols_loading) = match self.session.symbols.try_lock() {
            Ok(store) => {
                let mut files: Vec<(usize, InfoSymbolFile)> = Vec::new();
                for t in &status.targets {
                    let Some(set) = store.set(&tcf::core_id(t.coreid)) else { continue };
                    for (n, s) in &set.elfs {
                        match files.iter_mut().find(|(m, _)| m == n) {
                            Some((_, f)) if !f.targets.contains(&t.name) => f.targets.push(t.name.clone()),
                            Some(_) => {}
                            None => files.push((
                                *n,
                                InfoSymbolFile {
                                    path: s.path.to_string_lossy().into_owned(),
                                    targets: vec![t.name.clone()],
                                    local_cores: store.run_cores(*n).to_vec(),
                                },
                            )),
                        }
                    }
                }
                files.sort_by_key(|(n, _)| *n);
                let files: Vec<InfoSymbolFile> = files.into_iter().map(|(_, f)| f).collect();
                *self.symbol_files.lock().unwrap() = files.clone();
                (files, false)
            }
            // `load_symbols` holds the store while it parses an ELF, which takes seconds.
            Err(_) => (self.symbol_files.lock().unwrap().clone(), true),
        };
        Ok(ServerInfo {
            server: "aurix-dbg".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            api_version: API_VERSION,
            pid: std::process::id(),
            adapter: status.adapter,
            connected: status.connected,
            examined: status.targets.iter().any(|t| t.examined),
            targets: status.targets.into_iter().map(|t| InfoTarget { name: t.name, examined: t.examined, reachable: t.reachable }).collect(),
            symbol_files,
            symbols_loading,
            core_lag_ms: lag.as_millis() as u64,
        })
    }

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

    fn load_symbols(&self, req: LoadSymbolsRequest) -> Result<SymbolsInfo, ApiError> {
        if req.files.is_empty() {
            return Err(ApiError::bad_request("give at least one symbol file"));
        }
        let cores = tcf::cores(&self.handle);
        let tricores: Vec<u32> = cores.iter().filter(|c| c.tricore()).map(|c| c.coreid).collect();
        let mut paths: Vec<PathBuf> = Vec::new();
        // The TriCore cores each file is assigned to (`path@N,M`); none clears an assignment.
        let mut runs: Vec<(PathBuf, Vec<u32>)> = Vec::new();
        for f in &req.files {
            let (p, run) = split_run_cores(f);
            if !p.is_file() {
                return Err(ApiError::not_found(format!("no such file: {}", p.display())));
            }
            if let Some(&n) = run.iter().find(|n| !tricores.is_empty() && !tricores.contains(n)) {
                let have: Vec<String> = tricores.iter().map(u32::to_string).collect();
                return Err(ApiError::bad_request(format!("`{f}`: there is no TriCore core {n} (the cores are {})", have.join(", "))));
            }
            if !paths.contains(&p) {
                paths.push(p.clone());
                runs.push((p, run));
            }
        }
        let chosen: Vec<&tcf::CoreInfo> = match &req.targets {
            Some(names) => names
                .iter()
                .map(|n| {
                    cores
                        .iter()
                        .find(|c| &c.name == n || c.idx.to_string() == *n)
                        .ok_or_else(|| ApiError::not_found(format!("unknown target '{n}' (see `targets`)")))
                })
                .collect::<Result<_, _>>()?,
            None => cores.iter().collect(),
        };
        if chosen.is_empty() {
            return Err(ApiError::new(ErrorCode::NotExamined, "no targets (run `init` first)"));
        }
        // This runs on an API thread, never on the core thread: loading a large ELF takes
        // seconds and would otherwise freeze run control for everybody.
        let t0 = Instant::now();
        {
            let mut store = self.session.symbols.lock().unwrap();
            for c in &chosen {
                let ctx = tcf::owning_core(&c.id).unwrap_or_else(|| c.id.clone());
                store.assign(&ctx, &paths).map_err(ApiError::failed)?;
            }
            for (p, run) in runs {
                store.set_run_cores(&p, run);
            }
        }
        self.session.stacks.lock().unwrap().clear();
        self.session.reresolve_breakpoints();
        let mut info = self.symbols_info();
        info.load_seconds = t0.elapsed().as_secs_f64();
        Ok(info)
    }

    fn symbols(&self, _: NoArgs) -> Result<SymbolsInfo, ApiError> {
        Ok(self.symbols_info())
    }

    fn list_globals(&self, req: ListGlobalsRequest) -> Result<GlobalList, ApiError> {
        variables::list_globals(self, req)
    }

    fn symbol_children(&self, req: SymbolChildrenRequest) -> Result<SymbolChildren, ApiError> {
        variables::symbol_children(self, req)
    }

    fn evaluate(&self, req: EvaluateRequest) -> Result<VariableValue, ApiError> {
        variables::evaluate(self, req)
    }

    fn write_variable(&self, req: WriteVariableRequest) -> Result<VariableValue, ApiError> {
        variables::write_variable(self, req)
    }

    fn load_orti(&self, req: LoadOrtiRequest) -> Result<OsInfo, ApiError> {
        let path = PathBuf::from(&req.file);
        if !path.is_file() {
            return Err(ApiError::not_found(format!("no such file: {}", req.file)));
        }
        let (target_name, syms) = self.orti_symbols(req.target.as_deref())?;

        // On an API thread: parsing and planning walk DWARF for every attribute, which takes long
        // enough that doing it on the core thread would stall run control.
        let mut t = CoreTarget::new(&self.handle);
        let aw = Awareness::load(&path, syms, &mut t).map_err(ApiError::bad_request)?;
        let info = os_info(&aw, Some(target_name));
        *self.session.orti.lock().unwrap() = Some(Arc::new(aw));
        Ok(info)
    }

    fn os(&self, _: NoArgs) -> Result<OsInfo, ApiError> {
        let guard = self.session.orti.lock().unwrap();
        match guard.as_ref() {
            Some(aw) => Ok(os_info(aw, None)),
            None => Ok(OsInfo {
                file: None,
                implementation: None,
                version: None,
                cores: 0,
                target: None,
                types: Vec::new(),
                errors: Vec::new(),
            }),
        }
    }

    fn os_objects(&self, req: OsObjectsRequest) -> Result<OsObjectList, ApiError> {
        let aw = self.session.orti.lock().unwrap().clone().ok_or_else(|| ApiError::not_found("no ORTI file loaded (use `load_orti`)"))?;
        if aw.orti.decl(&req.kind).is_none() {
            let known: Vec<&str> = aw.orti.decls.iter().map(|d| d.name.as_str()).collect();
            return Err(ApiError::not_found(format!("unknown object type '{}' (have: {})", req.kind, known.join(", "))));
        }
        let live_only = req.live_only.unwrap_or(false);
        let mut t = CoreTarget::new(&self.handle);
        let mut objects = Vec::new();
        for o in aw.objects_of(&req.kind) {
            if req.name.as_ref().is_some_and(|n| *n != o.name) {
                continue;
            }
            let mut attributes = Vec::new();
            for a in &o.attrs {
                if live_only && !a.plan.is_live() {
                    continue;
                }
                let (value, number, link, error) = match aw.read(&req.kind, a, &mut t) {
                    Ok(bytes) => {
                        let s = a.show(&bytes);
                        (s.text, s.number, s.link, None)
                    }
                    Err(e) => (String::new(), None, None, Some(e)),
                };
                attributes.push(OsAttributeValue { name: a.name.clone(), core: a.index, value, number, link, error });
            }
            objects.push(OsObject { name: o.name.clone(), attributes });
        }
        if objects.is_empty() && req.name.is_some() {
            return Err(ApiError::not_found(format!("no {} object named '{}'", req.kind, req.name.unwrap_or_default())));
        }
        Ok(OsObjectList { kind: req.kind, objects, reads: t.reads })
    }

    fn os_stacks(&self, req: OsStacksRequest) -> Result<OsStackList, ApiError> {
        let aw = self.session.orti.lock().unwrap().clone().ok_or_else(|| ApiError::not_found("no ORTI file loaded (use `load_orti`)"))?;
        let mut t = CoreTarget::new(&self.handle);
        let measured = orti::stack::measure(&aw, req.names.as_deref(), &mut t);
        let stacks = measured
            .iter()
            .map(|s| OsStack {
                name: s.name.clone(),
                base: s.base,
                size: s.size,
                direction: if s.direction == orti::Direction::Up { "UP".into() } else { "DOWN".into() },
                pattern: s.pattern,
                used: s.used,
                free: s.used.map(|u| s.size.saturating_sub(u)),
                percent: s.percent(),
                error: s.error.clone(),
            })
            .collect();
        Ok(OsStackList { stacks, reads: t.reads })
    }
}


