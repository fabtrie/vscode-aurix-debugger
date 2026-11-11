//! The hub's state: sessions (one worker each), MCP clients, events; and the hub's own
//! operations ([`HubApi`]).
//!
//! Hub operations block (they wait for workers), so they run on blocking threads; the client
//! they run for is a thread-local, as in the worker.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use api_spec::types::*;
use api_spec::{ApiError, ErrorCode, HubApi, API_VERSION, CLIENT_HEADER};

use crate::config::{random_hex, HubConfig};
use crate::{hlog, http1, paths, spawn};

thread_local! {
    static CLIENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Runs `f` as client `client`.
pub fn with_client<R>(client: Option<String>, f: impl FnOnce() -> R) -> R {
    let old = CLIENT.with(|c| c.replace(client));
    let r = f();
    CLIENT.with(|c| *c.borrow_mut() = old);
    r
}

pub fn current_client() -> Option<String> {
    CLIENT.with(|c| c.borrow().clone())
}

/// How long an exited session stays listed (for `session_log` and to see why it ended).
const KEEP_EXITED: Duration = Duration::from_secs(600);

/// One board connection.
pub struct Session {
    pub id: String,
    pub name: String,
    pub profile: Option<String>,
    pub opened_by: Option<String>,
    pub adapter: Option<String>,
    pub probe: Option<String>,
    pub created: Instant,
    pub state: SessionState,
    /// The worker process, when this hub started it (an adopted one has none).
    pub child: Option<Child>,
    pub pid: Option<u32>,
    pub api_port: Option<u16>,
    pub tcf_port: Option<u16>,
    pub telnet_port: Option<u16>,
    pub targets: Vec<String>,
    pub log_file: PathBuf,
    pub error: Option<String>,
    /// Connecting and the launch of `open_session`.
    pub startup: LaunchStatus,
    pub ended: Option<Instant>,
}

impl Session {
    pub fn live(&self) -> bool {
        self.state != SessionState::Exited
    }

    pub fn base(&self) -> Option<String> {
        self.api_port.filter(|_| self.state == SessionState::Running).map(|p| format!("http://127.0.0.1:{p}"))
    }
}

pub struct McpClient {
    pub last_seen: Instant,
}

#[derive(Default)]
pub struct State {
    pub sessions: Vec<Session>,
    pub mcp: HashMap<String, McpClient>,
    next_n: u32,
}

pub struct Hub {
    pub cfg: HubConfig,
    /// The port the hub listens on.
    pub port: u16,
    pub state: Mutex<State>,
    /// Signalled on every session change (registration, startup progress, exit).
    pub changed: Condvar,
    pub events: broadcast::Sender<HubEvent>,
    pub started: Instant,
    pub update_pending: AtomicBool,
    pub stopping: AtomicBool,
    pub shutdown: tokio::sync::Notify,
    /// The executable to start again when an update is applied.
    pub restart_exe: Mutex<Option<PathBuf>>,
    /// ID of this run (workers register again when it changes).
    pub instance: String,
    me: OnceLock<Weak<Hub>>,
}

/// What a session operation goes to.
pub enum Target {
    Worker { id: String, base: String },
}

/// Why no single session could be chosen: guidance for an MCP client.
pub enum Choose {
    Error(ApiError),
    Guidance(Value),
}

fn worker_err(status: u16, v: &Value) -> ApiError {
    match serde_json::from_value::<ApiError>(v.clone()) {
        Ok(e) => e,
        Err(_) => ApiError::failed(format!("the worker answered HTTP {status}: {v}")),
    }
}

/// The last `n` lines of a file.
pub fn tail(path: &Path, n: usize) -> String {
    let text = std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// A profile file: `[connect]` and an optional `[launch]`.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    #[serde(default)]
    connect: ConnectSpec,
    #[serde(default)]
    launch: Option<LaunchRequest>,
}

fn profile_path(name: &str) -> Result<PathBuf, ApiError> {
    if name.is_empty() || name.contains(['/', '\\', ':']) || name.starts_with('.') {
        return Err(ApiError::bad_request(format!("invalid profile name '{name}'")));
    }
    Ok(paths::profiles_dir().join(format!("{name}.toml")))
}

fn load_profile(name: &str) -> Result<(ProfileFile, PathBuf), ApiError> {
    let path = profile_path(name)?;
    let text = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ApiError::not_found(format!("no profile '{name}' (see `list_profiles`; profiles are {}\\<name>.toml)", paths::profiles_dir().display()))
        } else {
            ApiError::failed(format!("{}: {e}", path.display()))
        }
    })?;
    let p: ProfileFile = toml::from_str(&text).map_err(|e| ApiError::bad_request(format!("{}: {e}", path.display())))?;
    Ok((p, path))
}

/// The launch of a profile with the fields of `over` that are set.
fn merge_launch(base: Option<LaunchRequest>, over: Option<LaunchRequest>) -> Option<LaunchRequest> {
    match (base, over) {
        (None, o) => o,
        (b, None) => b,
        (Some(b), Some(o)) => Some(LaunchRequest {
            symbols: o.symbols.or(b.symbols),
            orti: o.orti.or(b.orti),
            flash: o.flash.or(b.flash),
            reset: o.reset.or(b.reset),
            breakpoints: o.breakpoints.or(b.breakpoints),
            run_to: o.run_to.or(b.run_to),
            stop_on_entry: o.stop_on_entry || b.stop_on_entry,
            timeout_s: o.timeout_s.or(b.timeout_s),
            wait_s: o.wait_s.or(b.wait_s),
        }),
    }
}

fn merge_connect(base: ConnectSpec, over: Option<ConnectSpec>) -> ConnectSpec {
    let Some(o) = over else { return base };
    ConnectSpec {
        config_files: if o.config_files.is_empty() { base.config_files } else { o.config_files },
        commands: if o.commands.is_empty() { base.commands } else { o.commands },
        search_dirs: if o.search_dirs.is_empty() { base.search_dirs } else { o.search_dirs },
        adapter: o.adapter.or(base.adapter),
        probe: o.probe.or(base.probe),
    }
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

impl Hub {
    pub fn new(cfg: HubConfig, port: u16) -> Arc<Hub> {
        let (events, _) = broadcast::channel(256);
        let hub = Arc::new(Hub {
            cfg,
            port,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            events,
            started: Instant::now(),
            update_pending: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            shutdown: tokio::sync::Notify::new(),
            restart_exe: Mutex::new(None),
            instance: random_hex(8),
            me: OnceLock::new(),
        });
        let _ = hub.me.set(Arc::downgrade(&hub));
        hub
    }

    fn arc(&self) -> Arc<Hub> {
        self.me.get().and_then(Weak::upgrade).expect("the hub is alive")
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn emit(&self, ev: HubEvent) {
        let _ = self.events.send(ev);
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn find<'a>(st: &'a State, key: &str) -> Option<&'a Session> {
        st.sessions.iter().find(|s| s.id == key).or_else(|| st.sessions.iter().find(|s| s.name == key && s.live())).or_else(|| st.sessions.iter().find(|s| s.name == key))
    }

    fn find_mut<'a>(st: &'a mut State, key: &str) -> Option<&'a mut Session> {
        let i = st.sessions.iter().position(|s| s.id == key).or_else(|| st.sessions.iter().position(|s| s.name == key && s.live())).or_else(|| st.sessions.iter().position(|s| s.name == key))?;
        st.sessions.get_mut(i)
    }

    pub fn live_sessions(&self) -> usize {
        self.lock().sessions.iter().filter(|s| s.live()).count()
    }

    /// The session a session operation goes to: `key`, or the only open one.
    pub fn choose(&self, key: Option<&str>) -> Result<Target, Choose> {
        let st = self.lock();
        let s = match key.filter(|k| !k.is_empty()) {
            Some(k) => Self::find(&st, k).ok_or_else(|| Choose::Error(ApiError::not_found(format!("no session '{k}' (see `list_sessions`)"))))?,
            None => {
                let live: Vec<&Session> = st.sessions.iter().filter(|s| s.live()).collect();
                match live.len() {
                    1 => live[0],
                    0 => {
                        drop(st);
                        let profiles: Vec<String> = list_profiles().profiles.into_iter().map(|p| p.name).collect();
                        return Err(Choose::Guidance(json!({
                            "status": "no_session",
                            "message": "No debug session is open. Open one with `open_session` (a `profile`, or `connect` with the board's configuration scripts) and retry. Opening a session connects to the hardware and may reset the target, so ask the user which board to use if unsure.",
                            "profiles": profiles,
                        })));
                    }
                    _ => {
                        let list: Vec<Value> = live.iter().map(|s| json!({ "session": s.id, "name": s.name, "targets": s.targets, "opened_by": s.opened_by })).collect();
                        return Err(Choose::Guidance(json!({
                            "status": "multiple_sessions",
                            "message": "Several debug sessions are open, so there is no default. Retry with a `session` argument set to one of these IDs or names.",
                            "sessions": list,
                        })));
                    }
                }
            }
        };
        match (s.state, s.base()) {
            (SessionState::Running, Some(base)) => Ok(Target::Worker { id: s.id.clone(), base }),
            (SessionState::Starting, _) => Err(Choose::Error(ApiError::new(ErrorCode::Busy, format!("session '{}' is still starting (see `wait_session`)", s.name)))),
            _ => Err(Choose::Error(ApiError::new(
                ErrorCode::NotFound,
                format!("session '{}' has ended{}", s.name, s.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default()),
            ))),
        }
    }

    /// Runs session operation `op` on a worker, as the current client.
    pub fn forward(&self, base: &str, op: &str, args: &Value, timeout: Duration) -> Result<Value, ApiError> {
        let client = current_client();
        let mut headers = Vec::new();
        if let Some(c) = &client {
            headers.push((CLIENT_HEADER, c.as_str()));
        }
        let (status, v) = http1::json(base, "POST", &format!("/api/v1/ops/{op}"), &headers, Some(args), timeout)
            .map_err(|e| ApiError::new(ErrorCode::Failed, format!("the session's worker does not answer: {e}")))?;
        if status == 200 {
            Ok(v)
        } else {
            Err(worker_err(status, &v))
        }
    }

    /// `wait_session`: the hub's own startup status first, then the worker's.
    pub fn wait_session(&self, key: Option<&str>, timeout_s: Option<u32>) -> Result<Value, Choose> {
        let timeout = Duration::from_secs(timeout_s.unwrap_or(60).min(3600) as u64);
        let deadline = Instant::now() + timeout;
        let id = {
            let st = self.lock();
            match key.filter(|k| !k.is_empty()) {
                Some(k) => Self::find(&st, k).map(|s| s.id.clone()).ok_or_else(|| Choose::Error(ApiError::not_found(format!("no session '{k}'"))))?,
                None => {
                    drop(st);
                    match self.choose(None) {
                        Ok(Target::Worker { id, .. }) => id,
                        Err(Choose::Error(e)) if e.code == ErrorCode::Busy => {
                            let st = self.lock();
                            st.sessions.iter().find(|s| s.state == SessionState::Starting).map(|s| s.id.clone()).ok_or(Choose::Error(e))?
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        };
        let mut st = self.lock();
        loop {
            let Some(s) = st.sessions.iter().find(|s| s.id == id) else {
                return Err(Choose::Error(ApiError::not_found("the session was closed")));
            };
            if s.startup.phase != LaunchPhase::Starting {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(serde_json::to_value(&s.startup).unwrap());
            }
            st = self.changed.wait_timeout(st, deadline - now).unwrap().0;
        }
        let s = st.sessions.iter().find(|s| s.id == id).unwrap();
        let (base, startup) = (s.base(), s.startup.clone());
        drop(st);
        let Some(base) = base else { return Ok(serde_json::to_value(&startup).unwrap()) };
        let left = deadline.saturating_duration_since(Instant::now()).as_secs() as u32;
        let worker = self
            .forward(&base, "wait_session", &json!({ "timeout_s": left }), Duration::from_secs(left as u64 + 30))
            .map_err(Choose::Error)?;
        // Before any launch on the worker, the startup (connect) is the whole story.
        Ok(if worker["phase"] == "idle" { serde_json::to_value(&startup).unwrap() } else { worker })
    }

    /// The `SessionInfo` of a session, with the worker's live launch state and lease holder.
    pub fn info(&self, s: &Session) -> SessionInfo {
        let mut launch = s.startup.clone();
        let mut control = None;
        if let (Some(base), LaunchPhase::Ready | LaunchPhase::Failed) = (s.base(), s.startup.phase) {
            let t = Duration::from_secs(2);
            if let Ok((200, w)) = http1::json(&base, "GET", "/api/v1/launch?timeout_s=0", &[], None, t) {
                if let Ok(l) = serde_json::from_value::<LaunchStatus>(w) {
                    // The worker's launch replaces the startup's once it is a later one (a
                    // `launch` or `relaunch` after `open_session`); the startup's own launch is
                    // shown with its `connect` step.
                    let same = |a: &LaunchStep, b: &LaunchStep| a.step == b.step && a.ms == b.ms && a.ok == b.ok;
                    let startup_launch = s.startup.steps.get(1..).unwrap_or_default();
                    let is_startup = l.steps.len() == startup_launch.len() && l.steps.iter().zip(startup_launch).all(|(a, b)| same(a, b));
                    if l.phase != LaunchPhase::Idle && !is_startup {
                        launch = l;
                    }
                }
            }
            if let Ok((200, i)) = http1::json(&base, "GET", "/api/v1/info", &[], None, t) {
                control = i["control"].as_str().map(str::to_string);
            }
        }
        SessionInfo {
            session: s.id.clone(),
            name: s.name.clone(),
            profile: s.profile.clone(),
            state: s.state,
            pid: s.pid,
            opened_by: s.opened_by.clone(),
            adapter: s.adapter.clone(),
            probe: s.probe.clone(),
            targets: s.targets.clone(),
            tcf_port: s.tcf_port,
            telnet_port: s.telnet_port,
            base_url: format!("{}{}", api_spec::SESSION_PREFIX, s.id),
            control,
            launch,
            log_file: s.log_file.to_string_lossy().into_owned(),
            age_s: s.created.elapsed().as_secs(),
            error: s.error.clone(),
        }
    }

    fn info_of(&self, id: &str) -> Result<SessionInfo, ApiError> {
        // Snapshot the fields, then ask the worker without holding the lock.
        let snap = {
            let st = self.lock();
            let s = st.sessions.iter().find(|s| s.id == id).ok_or_else(|| ApiError::not_found("the session was closed"))?;
            Session {
                id: s.id.clone(),
                name: s.name.clone(),
                profile: s.profile.clone(),
                opened_by: s.opened_by.clone(),
                adapter: s.adapter.clone(),
                probe: s.probe.clone(),
                created: s.created,
                state: s.state,
                child: None,
                pid: s.pid,
                api_port: s.api_port,
                tcf_port: s.tcf_port,
                telnet_port: s.telnet_port,
                targets: s.targets.clone(),
                log_file: s.log_file.clone(),
                error: s.error.clone(),
                startup: s.startup.clone(),
                ended: s.ended,
            }
        };
        Ok(self.info(&snap))
    }

    // --- Workers ----------------------------------------------------------------------------

    /// A worker registers (after `init`) or registers again (after a hub restart).
    pub fn register(&self, reg: WorkerRegistration) -> RegistrationReply {
        let ours = canonical(&self.cfg.worker_exe());
        if canonical(Path::new(&reg.exe)) != ours || reg.version != env!("CARGO_PKG_VERSION") {
            hlog!("refused worker {} (pid {}): {} {} is not this hub's {} {}", reg.session, reg.pid, reg.exe, reg.version, ours.display(), env!("CARGO_PKG_VERSION"));
            return RegistrationReply {
                ok: false,
                hub_pid: std::process::id(),
                instance: self.instance.clone(),
                error: Some(format!("the hub runs another installation ({} {}); close this session and open it again", ours.display(), env!("CARGO_PKG_VERSION"))),
            };
        }
        let mut st = self.lock();
        let ev = match Self::find_mut(&mut st, &reg.session).filter(|s| s.id == reg.session) {
            Some(s) => {
                let first = s.state == SessionState::Starting;
                s.state = SessionState::Running;
                s.pid = Some(reg.pid);
                s.api_port = Some(reg.api_port);
                s.tcf_port = reg.tcf_port;
                s.telnet_port = reg.telnet_port;
                s.targets = reg.targets.clone();
                if s.adapter.is_none() {
                    s.adapter = reg.adapter.clone();
                }
                s.error = None;
                s.ended = None;
                first.then(|| HubEvent::SessionRunning { session: s.id.clone(), name: s.name.clone() })
            }
            None => {
                // A worker of a hub that ended: adopt it.
                hlog!("adopted worker {} (pid {}, port {})", reg.session, reg.pid, reg.api_port);
                st.sessions.push(Session {
                    id: reg.session.clone(),
                    name: reg.session.clone(),
                    profile: None,
                    opened_by: None,
                    adapter: reg.adapter.clone(),
                    probe: None,
                    created: Instant::now(),
                    state: SessionState::Running,
                    child: None,
                    pid: Some(reg.pid),
                    api_port: Some(reg.api_port),
                    tcf_port: reg.tcf_port,
                    telnet_port: reg.telnet_port,
                    targets: reg.targets.clone(),
                    log_file: reg.log_file.clone().map(PathBuf::from).unwrap_or_default(),
                    error: None,
                    startup: LaunchStatus { phase: LaunchPhase::Ready, steps: Vec::new(), targets: Vec::new(), error: None },
                    ended: None,
                });
                Some(HubEvent::SessionRunning { session: reg.session.clone(), name: reg.session.clone() })
            }
        };
        drop(st);
        self.changed.notify_all();
        if let Some(e) = ev {
            self.emit(e);
        }
        RegistrationReply { ok: true, hub_pid: std::process::id(), instance: self.instance.clone(), error: None }
    }

    pub fn deregister(&self, session: &str, pid: u32) {
        let ev = {
            let mut st = self.lock();
            match Self::find_mut(&mut st, session).filter(|s| s.id == session && s.pid == Some(pid) && s.live()) {
                Some(s) => {
                    s.state = SessionState::Exited;
                    s.ended = Some(Instant::now());
                    s.error.get_or_insert_with(|| "the worker ended".into());
                    Some(HubEvent::SessionClosed { session: s.id.clone(), name: s.name.clone(), error: s.error.clone() })
                }
                None => None,
            }
        };
        self.changed.notify_all();
        if let Some(e) = ev {
            self.emit(e);
        }
    }

    /// Marks session `id` exited (its worker is gone).
    fn exited(&self, id: &str, why: String) {
        let ev = {
            let mut st = self.lock();
            let Some(s) = st.sessions.iter_mut().find(|s| s.id == id && s.live()) else { return };
            s.state = SessionState::Exited;
            s.ended = Some(Instant::now());
            if s.startup.phase == LaunchPhase::Starting {
                s.startup.phase = LaunchPhase::Failed;
                s.startup.error = Some(why.clone());
            }
            s.error = Some(why);
            HubEvent::SessionClosed { session: s.id.clone(), name: s.name.clone(), error: s.error.clone() }
        };
        self.changed.notify_all();
        self.emit(ev);
    }

    /// Checks the workers every second; ends idle MCP clients; applies a pending update.
    pub fn start_monitor(&self) {
        let hub = self.arc();
        thread::Builder::new()
            .name("hub-monitor".into())
            .spawn(move || {
                let mut tick: u64 = 0;
                let mut idle_since = Instant::now();
                while !hub.stopping.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(1));
                    tick += 1;
                    hub.check_workers(tick % 5 == 0);
                    if tick % 30 == 0 {
                        hub.reap_mcp();
                    }
                    let live = hub.live_sessions();
                    let busy = live > 0 || !hub.lock().mcp.is_empty();
                    if busy {
                        idle_since = Instant::now();
                    }
                    if live == 0 && hub.update_pending.load(Ordering::Relaxed) {
                        hlog!("no session open: applying the update");
                        hub.restart();
                        return;
                    }
                    let idle_exit = hub.cfg.idle_exit_min;
                    if idle_exit > 0 && idle_since.elapsed() >= Duration::from_secs(idle_exit * 60) {
                        hlog!("idle for {idle_exit} min: exiting");
                        hub.stop();
                        return;
                    }
                }
            })
            .expect("spawn hub-monitor");
    }

    fn check_workers(&self, ping: bool) {
        let mut gone = Vec::new();
        let mut ping_list = Vec::new();
        {
            let mut st = self.lock();
            let now = Instant::now();
            st.sessions.retain(|s| s.ended.is_none_or(|t| now - t < KEEP_EXITED));
            for s in st.sessions.iter_mut().filter(|s| s.live()) {
                match s.child.as_mut().map(|c| c.try_wait()) {
                    Some(Ok(Some(code))) => {
                        let tail = tail(&s.log_file, 30);
                        gone.push((s.id.clone(), format!("the worker exited ({code}){}", if tail.is_empty() { String::new() } else { format!(":\n{tail}") })));
                    }
                    Some(_) => {}
                    None => {
                        if let Some(pid) = s.pid {
                            if !spawn::alive(pid) {
                                let tail = tail(&s.log_file, 30);
                                gone.push((s.id.clone(), format!("the worker ended{}", if tail.is_empty() { String::new() } else { format!(":
{tail}") })));
                            } else if ping {
                                if let Some(b) = s.base() {
                                    ping_list.push((s.id.clone(), b));
                                }
                            }
                        }
                    }
                }
            }
        }
        for (id, why) in gone {
            hlog!("session {id}: {}", why.lines().next().unwrap_or_default());
            self.exited(&id, why);
        }
        for (id, base) in ping_list {
            if http1::json(&base, "GET", "/api/v1/info", &[], None, Duration::from_secs(3)).is_err() {
                self.exited(&id, "the worker stopped answering".into());
            }
        }
    }

    /// Ends the MCP sessions that sent nothing for `mcp_idle_min`.
    fn reap_mcp(&self) {
        let limit = Duration::from_secs(self.cfg.mcp_idle_min.max(1) * 60);
        let idle: Vec<String> = self.lock().mcp.iter().filter(|(_, c)| c.last_seen.elapsed() > limit).map(|(id, _)| id.clone()).collect();
        for id in idle {
            hlog!("MCP session {id} idle: ending it");
            self.end_mcp(&id);
        }
    }

    /// Ends an MCP session: its breakpoints and lease go on every worker.
    pub fn end_mcp(&self, id: &str) -> bool {
        if self.lock().mcp.remove(id).is_none() {
            return false;
        }
        self.end_client(&format!("mcp:{id}"));
        true
    }

    /// Ends API client `client` on every running worker.
    pub fn end_client(&self, client: &str) {
        let bases: Vec<String> = self.lock().sessions.iter().filter_map(|s| s.base()).collect();
        for b in bases {
            let _ = with_client(None, || self.forward(&b, "end_client", &json!({ "client": client }), Duration::from_secs(5)));
        }
    }

    /// Stops serving (the HTTP server drains and the process exits).
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.shutdown.notify_waiters();
        self.shutdown.notify_one();
    }

    /// Starts the installed (or the same) hub again and stops this one: the new one waits for
    /// the port.
    pub fn restart(&self) {
        let exe = self.restart_exe.lock().unwrap().clone().unwrap_or_else(crate::install::installed_hub_exe);
        hlog!("restarting from {}", exe.display());
        if let Err(e) = spawn::spawn_detached(&exe, &["serve", "--wait-port"]) {
            hlog!("cannot start {}: {e}", exe.display());
        }
        self.stop();
    }

    /// Closes every session (`stop --now`).
    pub fn close_all(&self) {
        let ids: Vec<String> = self.lock().sessions.iter().filter(|s| s.live()).map(|s| s.id.clone()).collect();
        for id in ids {
            let _ = self.close(&id);
        }
    }

    /// Ends a session's worker: `shutdown` through its API, then the hard way.
    fn close(&self, key: &str) -> Result<(), ApiError> {
        let (id, base, pid, child) = {
            let mut st = self.lock();
            let s = Self::find_mut(&mut st, key).ok_or_else(|| ApiError::not_found(format!("no session '{key}' (see `list_sessions`)")))?;
            (s.id.clone(), s.base(), s.pid, s.child.take())
        };
        if let Some(b) = &base {
            // As the hub itself: a lease does not keep a session open.
            let _ = http1::json(b, "POST", "/api/v1/ops/tcl", &[(CLIENT_HEADER, "aurix-hub")], Some(&json!({ "command": "shutdown" })), Duration::from_secs(3));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut child = child;
        loop {
            let done = match child.as_mut() {
                Some(c) => matches!(c.try_wait(), Ok(Some(_))),
                None => pid.is_none_or(|p| !spawn::alive(p)),
            };
            if done {
                break;
            }
            if Instant::now() >= deadline {
                match child.as_mut() {
                    Some(c) => {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                    None => {
                        if let Some(p) = pid {
                            spawn::kill(p);
                        }
                    }
                }
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let ev = {
            let mut st = self.lock();
            let pos = st.sessions.iter().position(|s| s.id == id);
            // A worker that deregistered on its way out was announced already.
            pos.and_then(|i| {
                let s = st.sessions.remove(i);
                s.live().then_some(HubEvent::SessionClosed { session: s.id, name: s.name, error: None })
            })
        };
        self.changed.notify_all();
        if let Some(e) = ev {
            self.emit(e);
        }
        Ok(())
    }

    /// Waits for the worker to register, takes the lease, runs the launch: on its own thread,
    /// so `open_session` can return early.
    fn startup(&self, id: String, launch: Option<LaunchRequest>, acquire: bool, close_on_error: bool, client: Option<String>, t0: Instant) {
        // Connecting: until the worker registered or exited.
        let (ok, base) = {
            let mut st = self.lock();
            loop {
                let Some(s) = st.sessions.iter().find(|s| s.id == id) else { return };
                match s.state {
                    SessionState::Running => break (true, s.base()),
                    SessionState::Exited => break (false, None),
                    SessionState::Starting => {}
                }
                st = self.changed.wait_timeout(st, Duration::from_millis(200)).unwrap().0;
                // The monitor notices an exited worker; a start that takes very long is given up.
                if t0.elapsed() > Duration::from_secs(600) {
                    drop(st);
                    self.exited(&id, "the worker did not finish `init` within 10 minutes".into());
                    return;
                }
            }
        };
        let connect_ms = t0.elapsed().as_millis() as u64;
        let update = |f: &dyn Fn(&mut LaunchStatus)| {
            let mut st = self.lock();
            if let Some(s) = st.sessions.iter_mut().find(|s| s.id == id) {
                f(&mut s.startup);
            }
            drop(st);
            self.changed.notify_all();
        };
        if !ok {
            let why = self.lock().sessions.iter().find(|s| s.id == id).and_then(|s| s.error.clone()).unwrap_or_default();
            update(&|l| {
                l.steps = vec![LaunchStep { step: "connect".into(), ok: false, ms: connect_ms, detail: why.clone() }];
                l.phase = LaunchPhase::Failed;
                l.error = Some(format!("connect: {why}"));
            });
            return;
        }
        let base = base.unwrap_or_default();
        let targets = self.lock().sessions.iter().find(|s| s.id == id).map(|s| s.targets.join(", ")).unwrap_or_default();
        update(&|l| l.steps = vec![LaunchStep { step: "connect".into(), ok: true, ms: connect_ms, detail: targets.clone() }]);

        let result: Result<Option<LaunchStatus>, ApiError> = with_client(client, || {
            if acquire {
                self.forward(&base, "acquire_control", &json!({}), Duration::from_secs(10))?;
            }
            match &launch {
                Some(l) => {
                    let mut l = l.clone();
                    l.wait_s = Some(3600);
                    let v = self.forward(&base, "launch", &serde_json::to_value(&l).unwrap(), Duration::from_secs(3700))?;
                    serde_json::from_value::<LaunchStatus>(v).map(Some).map_err(|e| ApiError::failed(e.to_string()))
                }
                None => Ok(None),
            }
        });
        let (phase, error) = match &result {
            Ok(Some(l)) => (l.phase, l.error.clone()),
            Ok(None) => (LaunchPhase::Ready, None),
            Err(e) => (LaunchPhase::Failed, Some(e.message.clone())),
        };
        update(&|s| {
            match &result {
                Ok(Some(l)) => {
                    s.steps.extend(l.steps.iter().cloned());
                    s.targets = l.targets.clone();
                }
                Ok(None) => {}
                Err(e) => s.steps.push(LaunchStep { step: if acquire { "acquire_control".into() } else { "launch".into() }, ok: false, ms: 0, detail: e.message.clone() }),
            }
            s.error = error.clone();
            s.phase = phase;
        });
        let name = self.lock().sessions.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_default();
        self.emit(HubEvent::SessionLaunched { session: id.clone(), name, phase, error });
        if phase == LaunchPhase::Failed && close_on_error {
            let _ = self.close(&id);
        }
    }
}

/// The profiles of this machine.
pub fn list_profiles() -> ProfileList {
    let dir = paths::profiles_dir();
    let mut profiles = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut files: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect();
        files.sort();
        for f in files {
            let name = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let (connect, launch, error) = match load_profile(&name) {
                Ok((p, _)) => (p.connect, p.launch, None),
                Err(e) => (ConnectSpec::default(), None, Some(e.message)),
            };
            profiles.push(ProfileInfo { name, file: f.to_string_lossy().into_owned(), connect, launch, error });
        }
    }
    ProfileList { dir: dir.to_string_lossy().into_owned(), profiles }
}

impl HubApi for Hub {
    fn hub_info(&self, _: NoArgs) -> Result<HubInfo, ApiError> {
        let (sessions, mcp_clients) = {
            let st = self.lock();
            (st.sessions.iter().filter(|s| s.live()).count() as u32, st.mcp.len() as u32)
        };
        Ok(HubInfo {
            server: "aurix-hub".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            api_version: API_VERSION,
            pid: std::process::id(),
            exe: std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
            port: self.port,
            bind: self.cfg.bind.clone(),
            token_required: self.cfg.token().is_some(),
            sessions,
            mcp_clients,
            update_pending: self.update_pending.load(Ordering::Relaxed),
            autostart: crate::install::autostart_registered(),
            log_file: paths::hub_log().to_string_lossy().into_owned(),
        })
    }

    fn list_sessions(&self, _: NoArgs) -> Result<SessionList, ApiError> {
        let ids: Vec<String> = self.lock().sessions.iter().map(|s| s.id.clone()).collect();
        Ok(SessionList { sessions: ids.iter().filter_map(|id| self.info_of(id).ok()).collect() })
    }

    fn open_session(&self, req: OpenSessionRequest) -> Result<SessionInfo, ApiError> {
        if self.stopping.load(Ordering::Relaxed) {
            return Err(ApiError::new(ErrorCode::Busy, "the hub is stopping"));
        }
        let t0 = Instant::now();
        let (base_connect, base_launch, profile_dir) = match &req.profile {
            Some(p) => {
                let (pf, path) = load_profile(p)?;
                (pf.connect, pf.launch, path.parent().map(Path::to_path_buf))
            }
            None => (ConnectSpec::default(), None, None),
        };
        let connect = merge_connect(base_connect, req.connect.clone());
        let launch = merge_launch(base_launch, req.launch.clone());
        if connect.config_files.is_empty() && connect.commands.is_empty() {
            return Err(ApiError::bad_request("give a `profile` or `connect` with the board's `config_files` (see `list_profiles`)"));
        }
        let exe = self.cfg.worker_exe();
        if !exe.is_file() {
            return Err(ApiError::failed(format!("the worker executable is missing: {}", exe.display())));
        }
        let client = current_client();
        let (id, name, log_file) = {
            let mut st = self.lock();
            if let Some(probe) = &connect.probe {
                if let Some(s) = st.sessions.iter().find(|s| s.live() && s.probe.as_deref() == Some(probe.as_str()) && s.adapter == connect.adapter) {
                    return Err(ApiError::new(ErrorCode::Busy, format!("probe {probe} is in use by session '{}' ({})", s.name, s.id)));
                }
            }
            st.next_n += 1;
            let id = random_hex(4);
            let mut name = req.name.clone().or_else(|| req.profile.clone()).unwrap_or_else(|| format!("session-{}", st.next_n));
            if st.sessions.iter().any(|s| s.live() && s.name == name) {
                let mut n = 2;
                while st.sessions.iter().any(|s| s.live() && s.name == format!("{name}-{n}")) {
                    n += 1;
                }
                name = format!("{name}-{n}");
            }
            let log_file = paths::logs_dir().join(format!("session-{id}.log"));
            (id, name, log_file)
        };
        let _ = std::fs::create_dir_all(paths::logs_dir());
        std::fs::write(&log_file, b"").map_err(|e| ApiError::failed(format!("{}: {e}", log_file.display())))?;

        // `-l` first, so that every later error lands in the log.
        let mut args: Vec<String> = vec!["-l".into(), log_file.to_string_lossy().into_owned()];
        if let Some(t) = self.cfg.tcl_dir() {
            args.extend(["-s".into(), t.to_string_lossy().into_owned()]);
        }
        for d in &connect.search_dirs {
            args.extend(["-s".into(), d.clone()]);
        }
        if let Some(a) = &connect.adapter {
            args.extend(["--adapter".into(), a.clone()]);
        }
        for f in &connect.config_files {
            args.extend(["-f".into(), f.clone()]);
        }
        for c in &connect.commands {
            args.extend(["-c".into(), c.clone()]);
        }
        args.extend(["--hub".into(), self.url(), "--session".into(), id.clone()]);
        let cwd = profile_dir.unwrap_or_else(paths::config_dir_or_exe);
        let pid = spawn::spawn_worker(&exe, &args, &cwd).map_err(|e| ApiError::failed(format!("cannot start {}: {e}", exe.display())))?;
        hlog!("session {id} ({name}): worker pid {pid}, log {}", log_file.display());
        {
            let mut st = self.lock();
            st.sessions.push(Session {
                id: id.clone(),
                name: name.clone(),
                profile: req.profile.clone(),
                opened_by: client.clone(),
                adapter: connect.adapter.clone(),
                probe: connect.probe.clone(),
                created: Instant::now(),
                state: SessionState::Starting,
                child: None,
                pid: Some(pid),
                api_port: None,
                tcf_port: None,
                telnet_port: None,
                targets: Vec::new(),
                log_file,
                error: None,
                startup: LaunchStatus { phase: LaunchPhase::Starting, steps: Vec::new(), targets: Vec::new(), error: None },
                ended: None,
            });
        }
        self.emit(HubEvent::SessionOpened { session: id.clone(), name, opened_by: client.clone() });

        let hub = self.arc();
        let sid = id.clone();
        let (acquire, close_on_error) = (req.acquire_control, req.close_on_error);
        thread::Builder::new()
            .name(format!("hub-startup-{id}"))
            .spawn(move || hub.startup(sid, launch, acquire, close_on_error, client, t0))
            .map_err(|e| ApiError::failed(e.to_string()))?;

        // Wait for the startup, at most `wait_s`.
        let deadline = t0 + Duration::from_secs(req.wait_s.unwrap_or(60).min(3600) as u64);
        let mut st = self.lock();
        loop {
            match st.sessions.iter().find(|s| s.id == id) {
                Some(s) if s.startup.phase == LaunchPhase::Starting => {}
                _ => break,
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            st = self.changed.wait_timeout(st, deadline - now).unwrap().0;
        }
        let present = st.sessions.iter().any(|s| s.id == id);
        drop(st);
        if !present {
            // Closed on error: say how the launch went.
            return Err(ApiError::failed(format!("the launch failed and the session was closed (close_on_error); see the hub log {}", paths::hub_log().display())));
        }
        let info = self.info_of(&id)?;
        if info.state == SessionState::Exited {
            return Err(ApiError::failed(format!(
                "the worker of session '{}' ended before it was connected: {}",
                info.name,
                info.error.as_deref().unwrap_or("no reason given")
            )));
        }
        Ok(info)
    }

    fn close_session(&self, req: SessionRef) -> Result<SessionList, ApiError> {
        self.close(&req.session)?;
        self.list_sessions(NoArgs {})
    }

    fn session_log(&self, req: SessionLogRequest) -> Result<SessionLog, ApiError> {
        let (id, file) = {
            let st = self.lock();
            let s = Self::find(&st, &req.session).ok_or_else(|| ApiError::not_found(format!("no session '{}'", req.session)))?;
            (s.id.clone(), s.log_file.clone())
        };
        let n = req.lines.unwrap_or(200).clamp(1, 100_000) as usize;
        Ok(SessionLog { session: id, file: file.to_string_lossy().into_owned(), text: tail(&file, n) })
    }

    fn list_profiles(&self, _: NoArgs) -> Result<ProfileList, ApiError> {
        Ok(list_profiles())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_merge() {
        let base = LaunchRequest { orti: Some("a.ort".into()), run_to: Some("main".into()), ..Default::default() };
        let over = LaunchRequest { run_to: Some("init".into()), stop_on_entry: true, ..Default::default() };
        let m = merge_launch(Some(base), Some(over)).unwrap();
        assert_eq!((m.orti.as_deref(), m.run_to.as_deref(), m.stop_on_entry), (Some("a.ort"), Some("init"), true));
        let c = merge_connect(ConnectSpec { config_files: vec!["a.cfg".into()], adapter: Some("tas_client".into()), ..Default::default() }, Some(ConnectSpec { commands: vec!["x".into()], ..Default::default() }));
        assert_eq!((c.config_files.len(), c.commands.len(), c.adapter.as_deref()), (1, 1, Some("tas_client")));
        let p: ProfileFile = toml::from_str("[connect]\nconfig_files = ['b.cfg']\n[launch]\nrun_to = 'main'\nreset = 'none'\n").unwrap();
        assert_eq!(p.launch.unwrap().run_to.as_deref(), Some("main"));
        assert!(profile_path("../x").is_err());
    }
}
