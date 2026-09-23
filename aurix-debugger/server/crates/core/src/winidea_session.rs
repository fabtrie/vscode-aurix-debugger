//! The live winIDEA connection behind [`crate::winidea_adapter::WinIdeaAdapter`].
//!
//! Kept apart from the adapter so the policy decisions there -- which writes to
//! refuse, which declarations to ignore -- stay readable next to the plain
//! plumbing here.
//!
//! # One instance per core
//!
//! On AURIX, winIDEA debugs the primary core (CPU0) in the instance the user
//! opened, and every further core in an instance of its own. A further core is
//! attached through the session (`CSessionCtrl::instance_attach`): winIDEA opens
//! the core's window, creating its workspace if it has none, and returns a
//! connection to it. That new window is normal.
//!
//! Which core is which comes from winIDEA's own session topology -- on a TC377
//! `CPU0`..`CPU2`, `SCR`, `HSM` -- so TriCore core `n` is the core winIDEA
//! names `CPU<n>`; nothing about it is configured or guessed.
//!
//! # Choosing the instance
//!
//! Several winIDEA instances run side by side as soon as a further core is
//! attached, and "attach to whichever one is running" then picks an arbitrary
//! one (seen on hardware). So the primary is chosen explicitly: by
//! `winidea workspace` when set; otherwise the running instance whose
//! workspace the core windows are named after (`Project.xjrf` for
//! `Project_CPU1.xjrf`). If that still leaves more than one -- two unrelated
//! projects open -- the connect fails and lists them.
//!
//! # Threading and lifetimes
//!
//! Every SDK controller is `!Send`, and the SDK reports failures through a
//! process-global slot rather than a return value, so all of this has to run on
//! one thread. That is already how the server works: the core thread owns `Ctx`
//! (an `Rc<RefCell<_>>`, itself `!Send`) and therefore the adapter, and
//! frontends reach it through `ServerHandle::call`.
//!
//! [`winidea_sdk::WinIdea`] is leaked to `'static`: it is one DLL handle
//! obtained once, and unloading a library that has installed C callbacks is
//! unsound anyway. That lets each connection be an ordinary owned field, so
//! disconnecting and reconnecting still work.
//!
//! Controllers are built per call from a connection. They are C++ object
//! allocations, not round trips to winIDEA (a status query measured 79 us, a
//! memory read ~3.6 ms, on a BlueBox), so this costs nothing that matters.

use std::sync::OnceLock;

use winidea_sdk::config::{ConnectionConfig, StartMode};
use winidea_sdk::memareas::MemAreas;
use winidea_sdk::{
    breakpoints::BreakpointController, connection::InstanceInfo, loader::LoaderController,
    session::SessionCtrl,
    AccessFlags, ConnectionMgr, DataController, DebugFacade, ExecutionController, WinIdea,
};

use crate::error::{other, Error, Result};
use crate::winidea_adapter::Start;

/// The DLL, loaded once per process.
static SDK: OnceLock<&'static WinIdea> = OnceLock::new();

fn sdk() -> Result<&'static WinIdea> {
    if let Some(w) = SDK.get() {
        return Ok(*w);
    }
    let loaded = WinIdea::load(&winidea_sdk::default_search_paths()).map_err(map_err)?;
    // Leaked deliberately: see the module docs.
    let leaked: &'static WinIdea = Box::leak(Box::new(loaded));
    Ok(SDK.get_or_init(|| leaked))
}

fn map_err(e: winidea_sdk::Error) -> Error {
    other(format!("winidea: {e}"))
}

/// Compares workspace paths the way Windows does: separators and case differ
/// between what the user typed and what winIDEA reports (`launchCore` hands
/// back backslashes, `enumerate_instances` forward slashes).
pub fn same_workspace(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().replace('\\', "/").to_ascii_lowercase();
    !a.trim().is_empty() && norm(a) == norm(b)
}

/// Picks the instance to attach to, or says why none can be picked.
///
/// `Ok(None)` means nothing is running, so the start mode decides whether to
/// launch one.
pub fn choose_instance<'i>(
    running: &'i [InstanceInfo],
    workspace: Option<&str>,
) -> std::result::Result<Option<&'i InstanceInfo>, String> {
    if let Some(ws) = workspace.filter(|w| !w.trim().is_empty()) {
        return Ok(running.iter().find(|i| same_workspace(&i.workspace, ws)));
    }
    // winIDEA names the workspace of a further core's window after the
    // primary's: `Project.xjrf` gets `Project_CPU1.xjrf`, `Project_SCR.xjrf`.
    // Those windows are ours to attach through the session, not candidates for
    // the primary.
    let stem = |ws: &str| {
        let w = ws.trim().replace('\\', "/").to_ascii_lowercase();
        w.strip_suffix(".xjrf").map(str::to_string).unwrap_or(w)
    };
    let primaries: Vec<&InstanceInfo> = running
        .iter()
        .filter(|i| {
            let s = stem(&i.workspace);
            !running.iter().any(|p| {
                let ps = stem(&p.workspace);
                !ps.is_empty() && s.len() > ps.len() + 1 && s.starts_with(&format!("{ps}_"))
            })
        })
        .collect();
    match (running, primaries.as_slice()) {
        ([], _) => Ok(None),
        (_, [only]) => Ok(Some(only)),
        (many, _) => Err(format!(
            "several winIDEA instances are running; choose one with 'winidea workspace <path>': {}",
            many.iter().map(|i| i.workspace.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// A connected winIDEA, and what the server learned from it at connect time.
pub struct Session {
    winidea: &'static WinIdea,
    conn: ConnectionMgr<'static>,
    /// The instances debugging further cores, by TriCore core id.
    secondaries: Vec<(u32, ConnectionMgr<'static>)>,
    /// Memory area ids, read from the target rather than assumed -- lazily,
    /// because they are only available once a target is actually up.
    areas: std::cell::Cell<Option<MemAreas>>,
    /// winIDEA's version, for the log line that makes version skew obvious.
    pub version: String,
    /// The primary instance's workspace, as winIDEA reports it.
    pub workspace: String,
}

impl Session {
    /// Attaches to the primary instance, launching one only as `start` allows.
    pub fn connect(workspace: Option<&str>, start: Start) -> Result<Session> {
        let winidea = sdk()?;
        let mut conn = ConnectionMgr::new(winidea).map_err(map_err)?;

        // `always` means a fresh instance no matter what is running; otherwise
        // prefer an instance that is already there, chosen explicitly.
        let running = if start == Start::Always {
            Vec::new()
        } else {
            ConnectionMgr::new(winidea).map_err(map_err)?.enumerate_instances().map_err(map_err)?
        };
        let chosen = choose_instance(&running, workspace).map_err(|e| other(format!("winidea: {e}")))?;
        let attached_to = match chosen {
            Some(inst) => {
                conn.connect_tcp("", inst.tcp_port).map_err(map_err)?;
                inst.workspace.clone()
            }
            None => {
                // Through the config, so the start mode applies: `existing`
                // must fail here rather than launch.
                let mut cfg = ConnectionConfig::new(winidea).map_err(map_err)?;
                cfg.workspace(workspace.unwrap_or("")).map_err(map_err)?;
                cfg.start(match start {
                    Start::IfRequired => StartMode::IfRequired,
                    Start::Existing => StartMode::Existing,
                    Start::Always => StartMode::Always,
                })
                .map_err(map_err)?;
                conn.connect_with(&cfg).map_err(map_err)?;
                workspace.unwrap_or("").to_string()
            }
        };

        let version = conn.version().map_err(map_err)?.to_string_lossy();
        // Version skew shows up as an obscure winIDEA-internal path error much
        // later, so say up front what we are talking to.
        crate::log_info!("[winidea] connected to winIDEA {version} ({attached_to})");
        let missing = winidea.missing().len();
        if missing > 0 {
            crate::log_warn!(
                "[winidea] this isystemConnect.dll is missing {missing} exports these bindings \
                 know about - it is a different winIDEA version"
            );
        }

        let session = Session {
            winidea,
            conn,
            secondaries: Vec::new(),
            areas: std::cell::Cell::new(None),
            version,
            workspace: attached_to,
        };

        // A workspace can carry breakpoints from a previous run. They would
        // stop the target for reasons this server never asked for and cannot
        // report to a frontend, so start from a clean slate.
        //
        // Not fatal: with no target up yet there is nothing to clear, and
        // failing the connection over it would be worse than saying so.
        if let Err(e) = session.breakpoints(0).and_then(|mut b| b.delete_all().map_err(map_err)) {
            crate::log_debug!("[winidea] could not clear breakpoints at connect: {e}");
        }

        Ok(session)
    }

    /// Makes sure TriCore core `coreid` is attached, attaching it if needed.
    ///
    /// Core 0 is the primary instance. Any other core is looked up by name in
    /// winIDEA's session topology (`CPU<n>`) and attached through the session,
    /// which opens its winIDEA window -- creating the workspace if the project
    /// has none -- and returns a connection to it.
    pub fn ensure_core(&mut self, coreid: u32) -> Result<()> {
        if coreid == 0 || self.secondaries.iter().any(|(c, _)| *c == coreid) {
            return Ok(());
        }
        let want = format!("CPU{coreid}");
        let conn = {
            let mut session = SessionCtrl::new(self.winidea, &self.conn).map_err(map_err)?;
            let topo = session.topology().map_err(map_err)?;
            let index = topo
                .iter()
                .flat_map(|s| s.cores.iter())
                .find(|c| c.name.eq_ignore_ascii_case(&want))
                .map(|c| c.index)
                .ok_or_else(|| {
                    let names: Vec<_> =
                        topo.iter().flat_map(|s| s.cores.iter().map(|c| c.name.clone())).collect();
                    other(format!("winidea: the session has no core {want} (it has {})", names.join(", ")))
                })?;
            session.instance_attach(index).map_err(map_err)?
        };
        crate::log_info!("[winidea] core {coreid} attached ({want})");
        self.secondaries.push((coreid, conn));
        Ok(())
    }

    /// The connection that drives TriCore core `coreid`.
    ///
    /// Core 0 is the primary instance unless mapped elsewhere. Any other core
    /// has to be mapped: silently driving the primary's core while a frontend
    /// believes it is stepping another would be the worst possible failure.
    fn conn_for(&self, coreid: u32) -> Result<&ConnectionMgr<'static>> {
        if let Some((_, c)) = self.secondaries.iter().find(|(id, _)| *id == coreid) {
            return Ok(c);
        }
        if coreid == 0 {
            return Ok(&self.conn);
        }
        Err(other(format!(
            "winidea: core {coreid} is not attached (it is attached when the core is examined)"
        )))
    }

    pub fn is_mapped(&self, coreid: u32) -> bool {
        self.conn_for(coreid).is_ok()
    }

    pub fn execution(&self, coreid: u32) -> Result<ExecutionController<'_>> {
        ExecutionController::new(self.winidea, self.conn_for(coreid)?).map_err(map_err)
    }

    pub fn debug(&self, coreid: u32) -> Result<DebugFacade<'_>> {
        DebugFacade::new(self.winidea, self.conn_for(coreid)?).map_err(map_err)
    }

    pub fn breakpoints(&self, coreid: u32) -> Result<BreakpointController<'_>> {
        BreakpointController::new(self.winidea, self.conn_for(coreid)?).map_err(map_err)
    }

    /// Memory is one SoC-wide space, so it goes through the primary instance.
    pub fn data(&self) -> Result<DataController<'_>> {
        DataController::new(self.winidea, &self.conn).map_err(map_err)
    }

    pub fn loader(&self) -> Result<LoaderController<'_>> {
        LoaderController::new(self.winidea, &self.conn).map_err(map_err)
    }

    pub fn ide(&self) -> Result<winidea_sdk::IdeController<'_>> {
        winidea_sdk::IdeController::new(self.winidea, &self.conn).map_err(map_err)
    }

    /// Reads `len` bytes, without stopping the target if it can be avoided.
    ///
    /// `REAL_TIME` is what makes a live watch and the DAQ sampler possible; it
    /// fails rather than halting when the target cannot serve a running read,
    /// which is the right trade for a debugger that must not perturb the
    /// application. `DIRECT` bypasses winIDEA's cache, so what comes back is
    /// what the target holds now, not what a view last displayed. (Measured on
    /// a BlueBox: neither flag changes the ~3.6 ms a read costs.)
    pub fn read(&self, addr: u32, len: usize) -> std::result::Result<Vec<u8>, ReadFailure> {
        let mut data = self.data().map_err(|_| ReadFailure::NoSession)?;
        let flags = AccessFlags::REAL_TIME | AccessFlags::DIRECT;
        match data.read_memory_maus(flags, self.areas().data_area(), addr as u64, len as u64, 1) {
            Ok(block) => {
                if block.is_fully_readable() && block.data.len() >= len {
                    Ok(block.data)
                } else {
                    let good = block.readable.iter().take_while(|r| **r).count();
                    Err(ReadFailure::Partial { data: block.data, bytes_ok: good })
                }
            }
            Err(e) => Err(ReadFailure::Failed(e.to_string())),
        }
    }

    /// Writes `data` through the primary instance.
    pub fn write(&self, addr: u32, data: &[u8]) -> std::result::Result<(), String> {
        let mut dc = self.data().map_err(|e| e.to_string())?;
        dc.write_memory_maus(AccessFlags::DIRECT, self.areas().data_area(), addr as u64, data, 1)
            .map_err(|e| e.to_string())
    }

    /// Detaches from every instance, leaving winIDEA running: the session never
    /// closes an instance it attached to.
    pub fn disconnect(&mut self) {
        for (_, c) in &mut self.secondaries {
            let _ = c.disconnect_keep();
        }
        if let Err(e) = self.conn.disconnect_keep() {
            crate::log_debug!("[winidea] disconnect: {e}");
        }
    }

    /// The target's memory area ids.
    ///
    /// Read on first use rather than at connect: `getSystemMemoryAreas` needs a
    /// target that is actually up, and it reports `ICONNECT_E_NOT_AVAILABLE`
    /// when winIDEA is connected but has no debug session yet. Connecting must
    /// still succeed then -- a server that refuses to start because the board
    /// is not powered would be useless.
    ///
    /// Falls back to area 0, which is what a target with a single flat address
    /// space uses (and what the TC377 project reports for every area), and
    /// retries on the next call so the real ids are picked up once the target
    /// appears.
    pub fn areas(&self) -> MemAreas {
        if let Some(a) = self.areas.get() {
            return a;
        }
        let read = self.data().and_then(|mut d| MemAreas::read(self.winidea, &mut d).map_err(map_err));
        match read {
            Ok(a) => {
                crate::log_debug!("[winidea] memory areas: {a:?}");
                self.areas.set(Some(a));
                a
            }
            Err(e) => {
                crate::log_debug!("[winidea] memory areas not available yet ({e}); assuming 0");
                MemAreas { code_physical: 0, code_virtual: 0, data_physical: 0, data_virtual: 0, sfr: 0 }
            }
        }
    }
}

/// What winIDEA's own symbol table says, for delegating source steps.
impl Session {
    /// Whether the instance debugging `coreid` has loaded `elf` itself (its
    /// project's download files -- the server never loads any).
    ///
    /// Only then do winIDEA's line boundaries match ours, which is the
    /// condition for letting winIDEA do a whole source step.
    pub fn has_elf(&self, coreid: u32, elf: &std::path::Path) -> Result<bool> {
        let mut d = DataController::new(self.winidea, self.conn_for(coreid)?).map_err(map_err)?;
        let parts = d.partitions().map_err(map_err)?;
        let ours = elf.to_string_lossy();
        Ok(parts.iter().any(|p| same_workspace(&p.path, &ours)))
    }

    /// Whether winIDEA has a source line at `pc`. A high-level step anywhere
    /// else blocks inside winIDEA regardless of its timeout (seen on hardware).
    pub fn has_line_at(&self, coreid: u32, pc: u32) -> bool {
        let Ok(conn) = self.conn_for(coreid) else { return false };
        winidea_sdk::AddressController::new(self.winidea, conn)
            .and_then(|mut a| a.source_line_at_address(pc as u64))
            .is_ok_and(|l| l.is_some())
    }
}

/// Why a read did not return everything asked for.
pub enum ReadFailure {
    NoSession,
    /// Some leading bytes were readable; the rest were not.
    Partial { data: Vec<u8>, bytes_ok: usize },
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(ws: &str, port: i32) -> InstanceInfo {
        InstanceInfo { id: String::new(), workspace: ws.into(), tcp_port: port }
    }

    #[test]
    fn workspaces_compare_like_windows_paths() {
        // launchCore returns backslashes, enumerate_instances forward slashes.
        assert!(same_workspace(
            r"C:\workspace\project\Tools\WinIdea\Project_CPU2.xjrf",
            "C:/workspace/project/Tools/WinIdea/Project_CPU2.xjrf"
        ));
        assert!(same_workspace("c:/WS/A.xjrf", "C:/ws/a.xjrf"));
        assert!(!same_workspace("C:/ws/a.xjrf", "C:/ws/a_CPU2.xjrf"));
        assert!(!same_workspace("", ""), "an empty workspace matches nothing");
    }

    #[test]
    fn one_running_instance_is_chosen_without_a_workspace() {
        let running = [inst("C:/ws/Project.xjrf", 5315)];
        assert_eq!(choose_instance(&running, None).unwrap().unwrap().tcp_port, 5315);
        assert!(choose_instance(&[], None).unwrap().is_none(), "nothing running: start mode decides");
    }

    /// Seen on hardware: with the CPU2 and SCR windows open, an unqualified
    /// attach picked one of those instead of the instance debugging cpu0. The
    /// core windows are named after the primary, which is how it is found.
    #[test]
    fn the_primary_is_found_among_its_core_windows() {
        let running = [
            inst("C:/ws/Project_SCR.xjrf", 5317),
            inst(r"C:\ws\Project_CPU1.xjrf", 5316),
            inst("C:/ws/Project.xjrf", 5315),
        ];
        assert_eq!(choose_instance(&running, None).unwrap().unwrap().tcp_port, 5315);
        // A named workspace still wins, and one that is not running is not
        // silently replaced by another.
        assert_eq!(choose_instance(&running, Some(r"C:\ws\Project_CPU1.xjrf")).unwrap().unwrap().tcp_port, 5316);
        assert!(choose_instance(&running, Some("C:/ws/other.xjrf")).unwrap().is_none());
    }

    /// Two unrelated projects open at once: no way to tell which one is meant.
    #[test]
    fn unrelated_instances_need_a_workspace() {
        let running = [inst("C:/ws/Project.xjrf", 5315), inst("C:/other/Demo.xjrf", 5320)];
        let err = choose_instance(&running, None).unwrap_err();
        assert!(err.contains("winidea workspace") && err.contains("Demo"), "{err}");
    }
}
