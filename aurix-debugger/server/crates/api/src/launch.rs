//! The launch steps of a session (`launch`, `relaunch`, `wait_session`): what a VS Code launch
//! configuration does, for every client.
//!
//! The steps run on their own thread, as the calling client, so the call can return before a
//! long flash finishes; `wait_session` waits for the rest. Order:
//!
//! 1. symbols, 2. ORTI, 3. flash (after `reset init`: the flash can only be erased right after
//!    a halting reset), 4. breakpoints and the temporary `run_to` breakpoint, 5. reset (the
//!    breakpoints are re-planted after it, so they catch the startup code), 6. wait for `run_to`,
//!    7. `stop_on_entry`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use api_spec::types::*;
use api_spec::{ApiError, DebuggerApi, ErrorCode};
use aurix_core::{log_info, log_warn};

use crate::client;
use crate::core_impl::CoreApi;

#[derive(Default)]
struct State {
    status: Option<LaunchStatus>,
    /// The arguments of the last launch, for `relaunch`.
    last: Option<LaunchRequest>,
}

/// The launch state of a session.
#[derive(Default)]
pub struct Launcher {
    state: Mutex<State>,
    changed: Condvar,
}

impl Launcher {
    pub fn phase(&self) -> LaunchPhase {
        self.state.lock().unwrap().status.as_ref().map_or(LaunchPhase::Idle, |s| s.phase)
    }

    pub fn status(&self) -> LaunchStatus {
        self.state.lock().unwrap().status.clone().unwrap_or_else(LaunchStatus::idle)
    }

    pub fn last(&self) -> Option<LaunchRequest> {
        self.state.lock().unwrap().last.clone()
    }

    /// Waits until no launch is running (at most `timeout`) and returns the status.
    pub fn wait(&self, timeout: Duration) -> LaunchStatus {
        let deadline = Instant::now() + timeout;
        let mut st = self.state.lock().unwrap();
        while st.status.as_ref().is_some_and(|s| s.phase == LaunchPhase::Starting) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            st = self.changed.wait_timeout(st, deadline - now).unwrap().0;
        }
        st.status.clone().unwrap_or_else(LaunchStatus::idle)
    }

    fn update(&self, f: impl FnOnce(&mut LaunchStatus)) {
        let mut st = self.state.lock().unwrap();
        if let Some(s) = st.status.as_mut() {
            f(s);
        }
        drop(st);
        self.changed.notify_all();
    }
}

/// Starts the launch steps and waits up to `req.wait_s` for them.
pub fn start(api: &CoreApi, req: LaunchRequest) -> Result<LaunchStatus, ApiError> {
    let me = api.me.get().and_then(|w| w.upgrade()).ok_or_else(|| ApiError::failed("the API is shutting down"))?;
    {
        let mut st = api.launcher.state.lock().unwrap();
        if st.status.as_ref().is_some_and(|s| s.phase == LaunchPhase::Starting) {
            return Err(ApiError::new(ErrorCode::Busy, "a launch is running on this session (see `wait_session`)"));
        }
        st.status = Some(LaunchStatus { phase: LaunchPhase::Starting, steps: Vec::new(), targets: Vec::new(), error: None });
        st.last = Some(req.clone());
    }
    let wait = Duration::from_secs(req.wait_s.unwrap_or(60).min(3600) as u64);
    let who = client::current();
    thread::Builder::new()
        .name("api-launch".into())
        .spawn(move || client::with_client(who, || run(&me, req)))
        .map_err(|e| ApiError::failed(format!("cannot start the launch: {e}")))?;
    Ok(api.launcher.wait(wait))
}

/// Runs one step and records it. Returns false when it failed.
fn step(api: &CoreApi, name: &str, f: impl FnOnce() -> Result<String, ApiError>) -> bool {
    let t0 = Instant::now();
    let r = f();
    let ms = t0.elapsed().as_millis() as u64;
    let ok = r.is_ok();
    let detail = match &r {
        Ok(d) => d.clone(),
        Err(e) => e.message.clone(),
    };
    match &r {
        Ok(_) => log_info!("launch: {name} done in {ms} ms"),
        Err(e) => log_warn!("launch: {name} failed: {}", e.message),
    }
    api.launcher.update(|s| {
        s.steps.push(LaunchStep { step: name.into(), ok, ms, detail: clip(&detail, 8000) });
        if !ok {
            s.error = Some(format!("{name}: {detail}"));
        }
    });
    ok
}

/// The last `max` bytes of `s` (flash output can be long; its end says how it went).
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut i = s.len() - max;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    format!("...{}", &s[i..])
}

fn run(api: &Arc<CoreApi>, req: LaunchRequest) {
    let ok = steps(api, &req);
    let targets = api.targets(NoArgs {}).map(|t| t.targets).unwrap_or_default();
    api.launcher.update(|s| {
        s.phase = if ok { LaunchPhase::Ready } else { LaunchPhase::Failed };
        s.targets = targets;
    });
}

fn steps(api: &CoreApi, req: &LaunchRequest) -> bool {
    if let Some(files) = req.symbols.as_ref().filter(|f| !f.is_empty()) {
        if !step(api, "symbols", || load_symbols(api, files)) {
            return false;
        }
    }
    if let Some(orti) = &req.orti {
        if !step(api, "orti", || {
            let info = api.load_orti(LoadOrtiRequest { file: orti.clone(), target: None })?;
            let objects: u32 = info.types.iter().map(|t| t.count).sum();
            Ok(format!("{} object types, {objects} objects, {} unresolved attributes", info.types.len(), info.errors.len()))
        }) {
            return false;
        }
    }
    let flashed = match &req.flash {
        Some(f) => {
            if !step(api, "flash", || flash(api, f)) {
                return false;
            }
            true
        }
        None => false,
    };
    // Breakpoints first: the reset below re-plants them, so they catch the startup code.
    let mut temp: Option<String> = None;
    if let Some(bps) = req.breakpoints.as_ref().filter(|b| !b.is_empty()) {
        if !step(api, "breakpoints", || {
            let mut out = Vec::new();
            for b in bps {
                let info = api.add_breakpoint(breakpoint_request(&b.location, b.condition.clone()))?;
                out.push(match &info.error {
                    Some(e) => format!("{} {}: pending ({e})", info.id, b.location),
                    None => format!("{} {}", info.id, b.location),
                });
            }
            Ok(out.join("\n"))
        }) {
            return false;
        }
    }
    if let Some(loc) = &req.run_to {
        let mut id = None;
        let ok = step(api, "run_to", || {
            let info = api.add_breakpoint(breakpoint_request(loc, None))?;
            id = Some(info.id.clone());
            match &info.error {
                Some(e) if info.instances.is_empty() => Err(ApiError::bad_request(format!("cannot break at {loc}: {e}"))),
                _ => Ok(format!("temporary breakpoint {} at {loc}", info.id)),
            }
        });
        temp = id;
        if !ok {
            remove_temp(api, &temp);
            return false;
        }
    }
    // The reset: `run_to` needs the target to run; otherwise as asked, by default a running
    // reset after flashing and none without.
    let mode = match (&req.reset, req.run_to.is_some(), flashed) {
        (Some(LaunchReset::Keyword(LaunchResetKeyword::None)), true, _) => Some(None),
        (Some(LaunchReset::Keyword(LaunchResetKeyword::None)), false, _) => None,
        (_, true, _) => Some(Some(ResetMode::Run)),
        (Some(LaunchReset::Reset(r)), false, _) => Some(Some(if r.halt { ResetMode::Halt } else { ResetMode::Run })),
        (None, false, true) => Some(Some(ResetMode::Run)),
        (None, false, false) => None,
    };
    if let Some(LaunchReset::Reset(r)) = &req.reset {
        if let Some(k) = r.kind.as_deref().filter(|k| !k.is_empty() && *k != "reset") {
            log_warn!("launch: reset type '{k}' is not known, using 'reset'");
        }
    }
    let t0 = Instant::now();
    match mode {
        Some(Some(m)) => {
            if !step(api, "reset", || {
                let targets = api.reset(ResetRequest { mode: Some(m) })?;
                Ok(format!("{} ({} targets)", if m == ResetMode::Run { "reset run" } else { "reset halt" }, targets.targets.len()))
            }) {
                remove_temp(api, &temp);
                return false;
            }
        }
        // `run_to` without a reset: let the halted cores run into it.
        Some(None) if !step(api, "resume", || resume_halted(api)) => {
            remove_temp(api, &temp);
            return false;
        }
        Some(None) | None => {}
    }
    if let (Some(loc), Some(id)) = (&req.run_to, &temp) {
        let timeout = Duration::from_secs(req.timeout_s.unwrap_or(30).clamp(1, 3600) as u64);
        let ok = step(api, "wait_run_to", || wait_for_hit(api, id, loc, timeout, t0));
        remove_temp(api, &temp);
        if !ok {
            return false;
        }
    }
    if req.stop_on_entry && !step(api, "stop_on_entry", || halt_all(api)) {
        return false;
    }
    true
}

fn remove_temp(api: &CoreApi, temp: &Option<String>) {
    if let Some(id) = temp {
        let _ = api.remove_breakpoint(BreakpointRef { id: id.clone() });
    }
}

/// An `add_breakpoint` request for a location: an address, `file:line` or a function.
pub fn breakpoint_request(location: &str, condition: Option<String>) -> AddBreakpointRequest {
    let loc = location.trim();
    let mut req = AddBreakpointRequest { address: None, symbol: None, file: None, line: None, targets: None, condition };
    if let Some(a) = parse_u32(loc) {
        req.address = Some(Addr(a));
    } else if let Some((file, line)) = loc.rsplit_once(':').filter(|(f, l)| !f.is_empty() && !l.is_empty() && l.chars().all(|c| c.is_ascii_digit())) {
        req.file = Some(file.to_string());
        req.line = line.parse().ok();
    } else {
        req.symbol = Some(loc.to_string());
    }
    req
}

fn parse_u32(s: &str) -> Option<u32> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u32::from_str_radix(h, 16).ok(),
        None if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) => s.parse().ok(),
        None => None,
    }
}

fn flash(api: &CoreApi, f: &LaunchFlash) -> Result<String, ApiError> {
    if f.files.is_empty() {
        return Err(ApiError::bad_request("no image files to flash"));
    }
    let mut out = api.tcl_command("reset init")?.output;
    let w = api.flash_write(FlashWriteRequest {
        files: f.files.clone(),
        erase: true,
        incremental: f.incremental.unwrap_or(true),
        verify: f.verify.unwrap_or(true),
    })?;
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&w.output);
    Ok(out)
}

fn resume_halted(api: &CoreApi) -> Result<String, ApiError> {
    let mut resumed = Vec::new();
    for t in api.targets(NoArgs {})?.targets {
        if t.examined && t.state == RunState::Halted {
            api.resume(ResumeRequest { target: t.name.clone(), address: None })?;
            resumed.push(t.name);
        }
    }
    Ok(if resumed.is_empty() { "no halted target".into() } else { format!("resumed {}", resumed.join(", ")) })
}

fn halt_all(api: &CoreApi) -> Result<String, ApiError> {
    let mut halted = Vec::new();
    for t in api.targets(NoArgs {})?.targets {
        if t.examined && t.reachable {
            let st = api.halt(TargetRef { target: t.name.clone() })?;
            halted.push(format!("{} at {}", st.name, st.pc.map_or("?".into(), |pc| format!("{pc:#010x}"))));
        }
    }
    Ok(halted.join(", "))
}

/// Waits until a target halts on breakpoint `id` (after `since`).
fn wait_for_hit(api: &CoreApi, id: &str, loc: &str, timeout: Duration, since: Instant) -> Result<String, ApiError> {
    let wake = api.hub.waiter();
    let deadline = since + timeout;
    loop {
        let list = api.breakpoint_list()?;
        let ours = list.breakpoints.iter().find(|b| b.id == id);
        let addrs: Vec<u32> = ours.map(|b| b.instances.iter().filter(|i| i.error.is_none()).map(|i| i.address).collect()).unwrap_or_default();
        // The debugger's other breakpoints: a halt elsewhere (the HSM at its boot ROM breakpoint
        // after a reset) is not one that came first.
        let others: Vec<u32> = list.breakpoints.iter().filter(|b| b.id != id).flat_map(|b| b.instances.iter().map(|i| i.address)).collect();
        let targets = api.targets(NoArgs {})?.targets;
        for t in targets.iter().filter(|t| t.state == RunState::Halted && t.reason == "breakpoint") {
            let Some(pc) = t.pc else { continue };
            if addrs.contains(&pc) {
                let line = ours
                    .and_then(|b| b.instances.iter().find(|i| i.address == pc))
                    .and_then(|i| Some(format!(" ({}:{})", i.file.as_deref()?, i.line?)));
                return Ok(format!("{} halted at {loc}{}", t.name, line.unwrap_or_default()));
            }
            if !others.contains(&pc) {
                continue;
            }
            // Another breakpoint came first: the target stopped there, as it would in VS Code.
            return Err(ApiError::new(
                ErrorCode::Failed,
                format!("{} halted at another breakpoint ({pc:#010x}) before reaching {loc}", t.name),
            ));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(ApiError::new(ErrorCode::Timeout, format!("no target reached {loc} within {} s", timeout.as_secs())));
        }
        let _ = wake.recv_timeout((deadline - now).min(Duration::from_millis(200)));
    }
}

/// The architecture a symbol file is for: `None` when it cannot tell (all cores get it).
fn file_arch(path: &Path) -> Option<&'static str> {
    let ext = |x: &str| path.extension().is_some_and(|e| e.eq_ignore_ascii_case(x));
    if ext("asm") {
        return Some("gtm_mcs");
    }
    if ext("cdb") {
        return Some("xc800");
    }
    let mut head = [0u8; 20];
    let mut f = std::fs::File::open(path).ok()?;
    std::io::Read::read_exact(&mut f, &mut head).ok()?;
    if &head[..4] != b"\x7fELF" {
        return None;
    }
    // e_machine, little endian (both TriCore and ARM ELF files are).
    match u16::from_le_bytes([head[18], head[19]]) {
        44 => Some("tricore"),
        40 => Some("cortex_m"),
        _ => None,
    }
}

/// `*` and `?` glob, case-insensitive.
fn glob(pattern: &str, text: &str) -> bool {
    fn m(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => m(&p[1..], t) || (!t.is_empty() && m(p, &t[1..])),
            (Some(b'?'), Some(_)) => m(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => m(&p[1..], &t[1..]),
            _ => false,
        }
    }
    m(pattern.as_bytes(), text.as_bytes())
}

fn core_matches(c: &tcf::CoreInfo, pattern: &str) -> bool {
    let short = c.name.rsplit_once('.').map_or(c.name.as_str(), |(_, s)| s);
    glob(pattern, &c.name) || glob(pattern, short) || pattern == c.coreid.to_string() || pattern == c.idx.to_string()
}

/// Loads the symbol files: each core gets the files of its architecture, or those whose
/// `cores` name it.
fn load_symbols(api: &CoreApi, files: &[LaunchSymbolFile]) -> Result<String, ApiError> {
    let cores = tcf::cores(&api.handle);
    if cores.is_empty() {
        return Err(ApiError::new(ErrorCode::NotExamined, "no targets (the configuration defines none)"));
    }
    let tricores: Vec<u32> = cores.iter().filter(|c| c.tricore()).map(|c| c.coreid).collect();
    // Per core (by context): its files, in the order given.
    let mut per_core: Vec<(String, Vec<PathBuf>)> = Vec::new();
    let mut runs: Vec<(PathBuf, Vec<u32>)> = Vec::new();
    let mut lines = Vec::new();
    for f in files {
        let (path, run) = crate::core_impl::split_run_cores(f.path());
        if !path.is_file() {
            return Err(ApiError::not_found(format!("no such file: {}", path.display())));
        }
        if let Some(&n) = run.iter().find(|n| !tricores.is_empty() && !tricores.contains(n)) {
            return Err(ApiError::bad_request(format!("`{}`: there is no TriCore core {n}", f.path())));
        }
        let chosen: Vec<&tcf::CoreInfo> = match f.cores() {
            Some(patterns) => cores.iter().filter(|c| patterns.iter().any(|p| core_matches(c, p))).collect(),
            None => match file_arch(&path) {
                Some(arch) => cores.iter().filter(|c| c.arch.name == arch).collect(),
                None => cores.iter().collect(),
            },
        };
        if chosen.is_empty() {
            return Err(ApiError::bad_request(format!("`{}` applies to no core of this board", f.path())));
        }
        for c in &chosen {
            let ctx = tcf::owning_core(&c.id).unwrap_or_else(|| c.id.clone());
            match per_core.iter_mut().find(|(x, _)| *x == ctx) {
                Some((_, list)) if !list.contains(&path) => list.push(path.clone()),
                Some(_) => {}
                None => per_core.push((ctx, vec![path.clone()])),
            }
        }
        lines.push(format!("{} -> {}", path.display(), chosen.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")));
        if !runs.iter().any(|(p, _)| *p == path) {
            runs.push((path, run));
        }
    }
    {
        let mut store = api.session.symbols.lock().unwrap();
        for (ctx, paths) in &per_core {
            store.assign(ctx, paths).map_err(ApiError::failed)?;
        }
        for (p, run) in runs {
            store.set_run_cores(&p, run);
        }
    }
    api.session.stacks.lock().unwrap().clear();
    api.session.reresolve_breakpoints();
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations() {
        let r = breakpoint_request("0x80000100", None);
        assert_eq!(r.address, Some(Addr(0x8000_0100)));
        let r = breakpoint_request(r"C:\src\main.c:42", None);
        assert_eq!((r.file.as_deref(), r.line), (Some(r"C:\src\main.c"), Some(42)));
        let r = breakpoint_request("main", Some("x > 1".into()));
        assert_eq!((r.symbol.as_deref(), r.condition.as_deref()), (Some("main"), Some("x > 1")));
        let r = breakpoint_request("C:", None);
        assert_eq!(r.symbol.as_deref(), Some("C:"));
    }

    #[test]
    fn globs() {
        assert!(glob("mcs0.*", "mcs0.ch3"));
        assert!(glob("CPU?", "cpu1"));
        assert!(!glob("cpu?", "cpu10"));
    }
}
