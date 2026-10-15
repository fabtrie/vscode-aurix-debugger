//! Connecting to a winIDEA instance.
//!
//! [`ConnectionMgr`] is the entry point for everything else: every controller
//! (DAQ, debug, execution, ...) is constructed from a live connection and
//! borrows it, so the borrow checker prevents using a controller after its
//! connection is gone.
//!
//! ```no_run
//! use winidea_sdk::{WinIdea, ConnectionMgr};
//!
//! # fn main() -> Result<(), winidea_sdk::Error> {
//! let winidea = WinIdea::load(&winidea_sdk::default_search_paths())?;
//! let mut conn = ConnectionMgr::new(&winidea)?;
//!
//! // Attach to the most recently used winIDEA, opening a workspace.
//! conn.connect_mru("C:/ws/MyProject.xjrf")?;
//! println!("connected to winIDEA {}", conn.version()?.to_string_lossy());
//! # Ok(())
//! # }
//! ```

use crate::WinIdea;
use crate::runtime::{self, Error, Owned};

/// Whether disconnecting should also close the winIDEA instance.
///
/// Mirrors the low nibble of `IConnect::EDetachFlags` (`dfCloseServer*`); values
/// are taken from the C# SDK's own constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloseServer {
    /// Leave winIDEA running.
    #[default]
    Never,
    /// Close it only if we were the last client attached.
    IfLastClient,
    /// Close it regardless of other clients.
    Unconditional,
}

impl CloseServer {
    const MASK: i32 = 0x0F;

    fn to_raw(self) -> i32 {
        match self {
            Self::Never => 0x00,
            Self::IfLastClient => 0x01,
            Self::Unconditional => 0x02,
        }
    }
}

/// What winIDEA should save when it closes.
///
/// Mirrors the `dfCloseAutoSave*` bits of `IConnect::EDetachFlags`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoSave {
    /// Whatever winIDEA is configured to do.
    #[default]
    Default,
    /// Save everything without prompting.
    All,
    /// Discard unsaved changes.
    None,
}

impl AutoSave {
    const MASK: i32 = 0x30;

    fn to_raw(self) -> i32 {
        match self {
            Self::Default => 0x00,
            Self::All => 0x10,
            Self::None => 0x20,
        }
    }
}

/// How to detach from winIDEA.
///
/// `IConnect::EDetachFlags` packs two independent choices into one integer --
/// a close-server policy in bits 0-3 and an autosave policy in bits 4-5 -- so
/// this is a pair of enums rather than a single one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DetachFlags {
    pub close_server: CloseServer,
    pub auto_save: AutoSave,
}

impl DetachFlags {
    /// Disconnect but leave winIDEA running (the SDK's default).
    pub fn keep() -> Self {
        Self::default()
    }

    /// Close winIDEA if we are its last client.
    pub fn close() -> Self {
        Self {
            close_server: CloseServer::IfLastClient,
            auto_save: AutoSave::Default,
        }
    }

    fn to_raw(self) -> i32 {
        (self.close_server.to_raw() & CloseServer::MASK)
            | (self.auto_save.to_raw() & AutoSave::MASK)
    }
}

/// One winIDEA instance found by [`ConnectionMgr::enumerate_instances`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceInfo {
    /// The instance id, as [`ConnectionMgr::connect_mru_id`] takes it. Empty
    /// for an instance started without an explicit id.
    pub id: String,
    /// The workspace that instance actually has open. Empty when it has none.
    pub workspace: String,
    /// The port it listens on, for [`ConnectionMgr::connect_tcp`].
    pub tcp_port: i32,
}

impl InstanceInfo {
    /// How to name this instance to a user who has to choose between several.
    ///
    /// The workspace is what an engineer recognises — a bare id means nothing
    /// — so it leads, and the id is only a fallback for an instance with no
    /// workspace open.
    pub fn label(&self) -> String {
        match (self.workspace.trim(), self.id.trim()) {
            ("", "") => format!("an unnamed winIDEA on port {}", self.tcp_port),
            ("", id) => format!("winIDEA id {id} (no workspace open)"),
            (ws, "") => ws.to_string(),
            (ws, id) => format!("{ws} (id {id})"),
        }
    }
}

/// A connection to one winIDEA instance.
pub struct ConnectionMgr<'a> {
    inner: Owned<'a>,
}

impl<'a> ConnectionMgr<'a> {
    /// Create a connection manager that is not yet connected.
    pub fn new(winidea: &'a WinIdea) -> Result<Self, Error> {
        let sdk = winidea.raw();
        // SAFETY: the default constructor takes no arguments and returns an
        // owned ConnectionMgr.
        let ptr = unsafe { (sym!(sdk, new_ConnectionMgr__SWIG_0))() };
        // SAFETY: `ptr` is a fresh ConnectionMgr; delete_ConnectionMgr is its
        // destructor.
        let inner = unsafe { Owned::adopt(winidea, ptr, sdk.delete_ConnectionMgr)? };
        Ok(Self { inner })
    }

    /// The raw `ConnectionMgr` handle.
    ///
    /// Needed to construct controllers that do not have a safe wrapper yet.
    /// The pointer is valid only while `self` lives.
    pub fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.inner.ptr
    }

    /// Connect to the most recently used winIDEA, opening `workspace`.
    ///
    /// Starts winIDEA if it is not already running. Pass an empty string to
    /// reuse whatever workspace winIDEA opens by itself.
    pub fn connect_mru(&mut self, workspace: &str) -> Result<(), Error> {
        let ws = runtime::to_cstring(workspace)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        unsafe {
            (sym!(self.inner.sdk(), ConnectionMgr_connectMRU__SWIG_3))(self.inner.ptr, ws.as_ptr())
        };
        runtime::check()
    }

    /// Connect to a winIDEA instance identified by `id`, opening `workspace`.
    ///
    /// Use this when several instances run concurrently; the id distinguishes
    /// them.
    ///
    /// # This is NOT an attach
    ///
    /// Despite the id, this is an overload of `connectMRU` and inherits its
    /// behaviour: when the search does not find that instance it **starts
    /// winIDEA**. The id narrows the search; it does not make the call
    /// non-launching.
    ///
    /// To bind to something already running without ever launching, use
    /// [`ConnectionMgr::connect_tcp`] with the `tcp_port` from
    /// [`ConnectionMgr::enumerate_instances`].
    pub fn connect_mru_id(&mut self, workspace: &str, id: &str) -> Result<(), Error> {
        let ws = runtime::to_cstring(workspace)?;
        let id = runtime::to_cstring(id)?;
        // SAFETY: live handle; both strings are copied during the call.
        unsafe {
            (sym!(self.inner.sdk(), ConnectionMgr_connectMRU__SWIG_2))(
                self.inner.ptr,
                ws.as_ptr(),
                id.as_ptr(),
            )
        };
        runtime::check()
    }

    /// Connect to a winIDEA already listening on `host:port`.
    ///
    /// **Never starts winIDEA** — unlike every `connectMRU` variant, including
    /// [`ConnectionMgr::connect_mru_id`]. It reaches a process that is already
    /// listening, or it fails. That makes this the only safe way to attach to
    /// an instance found by [`ConnectionMgr::enumerate_instances`], whose
    /// `tcp_port` is exactly what this takes.
    ///
    /// Pass an empty `host` for the local machine, matching the address
    /// convention `enumerate_instances` uses.
    pub fn connect_tcp(&mut self, host: &str, port: i32) -> Result<(), Error> {
        let h = runtime::to_cstring(host)?;
        // SAFETY: live handle; the host string is copied during the call.
        unsafe {
            (sym!(self.inner.sdk(), ConnectionMgr_connect__SWIG_2))(
                self.inner.ptr,
                h.as_ptr(),
                port,
            )
        };
        runtime::check()
    }

    /// Connect to the winIDEA process with the given OS process id.
    pub fn connect_pid(&mut self, pid: u32) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.inner.sdk(), ConnectionMgr_connect__SWIG_3))(self.inner.ptr, pid) };
        runtime::check()
    }

    /// Every winIDEA currently running on this machine.
    ///
    /// **Never starts one** — unlike [`ConnectionMgr::connect_mru`], which
    /// launches winIDEA when none is running. That makes this the way to ask
    /// "is there anything to attach to?" without side effects.
    ///
    /// An empty vector is a normal answer, not an error.
    pub fn enumerate_instances(&mut self) -> Result<Vec<InstanceInfo>, Error> {
        let sdk = self.inner.sdk();

        // The SDK requires a connection config: it selects the search domain
        // (shared memory vs UDP). A default-constructed one means "the usual
        // local domain", which is what we want — narrowing it by workspace/id
        // here would defeat the point of enumerating.
        // SAFETY: the default constructor takes no arguments and returns an
        // owned CConnectionConfig.
        let cfg_ptr = unsafe { (sym!(sdk, new_CConnectionConfig))() };
        // SAFETY: `cfg_ptr` is a fresh CConnectionConfig.
        let cfg =
            unsafe { Owned::adopt(self.inner.winidea, cfg_ptr, sdk.delete_CConnectionConfig)? };

        // SAFETY: creates an empty VectorWinIDEAInstanceInfo we own.
        let v_ptr = unsafe { (sym!(sdk, new_VectorWinIDEAInstanceInfo__SWIG_0))() };
        // SAFETY: `v_ptr` is a fresh VectorWinIDEAInstanceInfo.
        let vec = unsafe {
            Owned::adopt(
                self.inner.winidea,
                v_ptr,
                sdk.delete_VectorWinIDEAInstanceInfo,
            )?
        };

        // Signature (iSYSTEM docs):
        //   enumerateWinIDEAInstances(address, spConnectionConfig, instances)
        // `address` empty = the local host. The output vector is the LAST
        // argument — passing it in the config's slot would corrupt memory.
        let address = runtime::to_cstring("")?;
        // SAFETY: live handles; the SDK copies the address and fills `vec`.
        unsafe {
            (sym!(sdk, ConnectionMgr_enumerateWinIDEAInstances))(
                self.inner.ptr,
                address.as_ptr(),
                cfg.ptr,
                vec.ptr,
            )
        };
        runtime::check()?;

        // SAFETY: live vector handle.
        let n = unsafe { (sym!(sdk, VectorWinIDEAInstanceInfo_size))(vec.ptr) };
        runtime::check()?;

        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            // SAFETY: 0 <= i < size. The element is BORROWED from the vector,
            // so it must not be adopted or freed — the vector owns it.
            let e = unsafe { (sym!(sdk, VectorWinIDEAInstanceInfo_getitem))(vec.ptr, i as i32) };
            runtime::check()?;
            if e.is_null() {
                continue;
            }
            // SAFETY: `e` is a live WinIDEAInstanceInfo owned by `vec`; the
            // strings are copied out before the vector is dropped.
            let info = unsafe {
                InstanceInfo {
                    id: runtime::from_cstr((sym!(sdk, WinIDEAInstanceInfo_getInstanceId))(e))
                        .unwrap_or_default(),
                    workspace: runtime::from_cstr((sym!(sdk, WinIDEAInstanceInfo_getWorkspace))(e))
                        .unwrap_or_default(),
                    tcp_port: (sym!(sdk, WinIDEAInstanceInfo_getTcpPort))(e),
                }
            };
            runtime::check()?;
            out.push(info);
        }
        Ok(out)
    }

    /// Whether this manager currently holds a connection.
    ///
    /// With `verify`, the SDK probes the link rather than reporting its cached
    /// belief — slower, but detects a winIDEA that died underneath us.
    pub fn is_connected(&self, verify: bool) -> Result<bool, Error> {
        // SAFETY: live handle.
        let c = unsafe {
            (sym!(self.inner.sdk(), ConnectionMgr_isConnected__SWIG_0))(self.inner.ptr, verify)
        };
        runtime::check()?;
        Ok(c)
    }

    /// Disconnect, leaving winIDEA running.
    pub fn disconnect_keep(&mut self) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.inner.sdk(), ConnectionMgr_disconnect_keep))(self.inner.ptr) };
        runtime::check()
    }

    /// Disconnect with explicit detach behaviour.
    pub fn disconnect(&mut self, flags: DetachFlags) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe {
            (sym!(self.inner.sdk(), ConnectionMgr_disconnect__SWIG_0))(
                self.inner.ptr,
                flags.to_raw(),
            )
        };
        runtime::check()
    }

    /// This instance's id, as used by [`ConnectionMgr::connect_mru_id`].
    pub fn id(&self) -> Result<String, Error> {
        // SAFETY: live handle.
        let p = unsafe { (sym!(self.inner.sdk(), ConnectionMgr_getId))(self.inner.ptr) };
        runtime::check()?;
        // SAFETY: `p` is null or a NUL-terminated string from the SDK.
        Ok(unsafe { runtime::from_cstr(p) }.unwrap_or_default())
    }

    /// The most recent error message the SDK recorded.
    ///
    /// Independent of the exception channel: some calls report a failure this
    /// way instead of raising.
    pub fn last_error_msg(&self) -> Result<String, Error> {
        // SAFETY: live handle.
        let p = unsafe { (sym!(self.inner.sdk(), ConnectionMgr_getLastErrorMsg))(self.inner.ptr) };
        runtime::check()?;
        // SAFETY: `p` is null or a NUL-terminated string from the SDK.
        Ok(unsafe { runtime::from_cstr(p) }.unwrap_or_default())
    }

    /// Version of the connected winIDEA.
    pub fn version(&self) -> Result<Version, Error> {
        let sdk = self.inner.sdk();
        // SAFETY: live handle; the returned CWinIDEAVersion is owned by us.
        let h = unsafe { (sym!(sdk, ConnectionMgr_getWinIDEAVersion))(self.inner.ptr) };
        // SAFETY: `h` is a fresh CWinIDEAVersion.
        let owned = unsafe { Owned::adopt(self.inner.winidea, h, sdk.delete_CWinIDEAVersion)? };
        // SAFETY: live CWinIDEAVersion handle for the whole block.
        let v = unsafe {
            Version {
                major: (sym!(sdk, CWinIDEAVersion_getMajor))(owned.ptr),
                minor: (sym!(sdk, CWinIDEAVersion_getMinor))(owned.ptr),
                build: (sym!(sdk, CWinIDEAVersion_getBuild))(owned.ptr),
                subbuild: (sym!(sdk, CWinIDEAVersion_getSubbuild))(owned.ptr),
                text: runtime::from_cstr((sym!(sdk, CWinIDEAVersion_toString))(owned.ptr)),
            }
        };
        runtime::check()?;
        Ok(v)
    }

    /// Terminate the connected winIDEA instance.
    pub fn terminate(&mut self) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.inner.sdk(), ConnectionMgr_terminate))(self.inner.ptr) };
        runtime::check()
    }

    /// Whether the current process was itself started by winIDEA.
    pub fn is_started_from_winidea(winidea: &WinIdea) -> Result<bool, Error> {
        // SAFETY: a static call taking no handle.
        let b = unsafe { (sym!(winidea.raw(), ConnectionMgr_isStartedFromWinIDEA))() };
        runtime::check()?;
        Ok(b)
    }
}

/// A winIDEA version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
    pub build: u16,
    pub subbuild: u16,
    /// The SDK's own rendering, when it provides one.
    pub text: Option<String>,
}

impl Version {
    /// The SDK's rendering if available, else `major.minor.build`.
    pub fn to_string_lossy(&self) -> String {
        self.text
            .clone()
            .unwrap_or_else(|| format!("{}.{}.{}", self.major, self.minor, self.build))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These encodings are load-bearing: a wrong bit silently discards unsaved
    /// work or leaves winIDEA running. Values are from the C# SDK's own
    /// `IConnect/EDetachFlags` constants.
    #[test]
    fn detach_flag_encoding() {
        assert_eq!(DetachFlags::keep().to_raw(), 0x00);
        assert_eq!(DetachFlags::close().to_raw(), 0x01);
        assert_eq!(DetachFlags::default(), DetachFlags::keep());

        // The two fields occupy disjoint bits and must combine, not clobber.
        let both = DetachFlags {
            close_server: CloseServer::Unconditional,
            auto_save: AutoSave::None,
        };
        assert_eq!(both.to_raw(), 0x22);

        assert_eq!(
            DetachFlags {
                close_server: CloseServer::Never,
                auto_save: AutoSave::All
            }
            .to_raw(),
            0x10
        );
    }

    /// The label is what a user reads when several instances are running and
    /// they have to pick one, so it must lead with the workspace — an id alone
    /// is meaningless to an engineer choosing between two ECU projects.
    #[test]
    fn instance_labels_lead_with_the_workspace() {
        let inst = |ws: &str, id: &str| InstanceInfo {
            id: id.into(),
            workspace: ws.into(),
            tcp_port: 5000,
        };

        assert_eq!(inst("C:/ws/Brake.xjrf", "").label(), "C:/ws/Brake.xjrf");
        assert_eq!(
            inst("C:/ws/Brake.xjrf", "A").label(),
            "C:/ws/Brake.xjrf (id A)"
        );
        // No workspace open: the id is all there is to go on.
        assert_eq!(inst("", "A").label(), "winIDEA id A (no workspace open)");
        // Neither: fall back to something that still distinguishes two rows.
        assert_eq!(inst("", "").label(), "an unnamed winIDEA on port 5000");
    }

    #[test]
    fn version_display_prefers_sdk_text() {
        let v = Version {
            major: 9,
            minor: 21,
            build: 417,
            subbuild: 2,
            text: Some("9.21.417.2".into()),
        };
        assert_eq!(v.to_string(), "9.21.417.2");

        let bare = Version { text: None, ..v };
        assert_eq!(bare.to_string(), "9.21.417");
    }
}

impl<'a> ConnectionMgr<'a> {
    /// Wraps a `CConnectionMgr` handle that this crate did not construct.
    ///
    /// # Safety
    ///
    /// `ptr` must be a live `CConnectionMgr`. Pass `free: None` when the handle
    /// belongs to something else -- as it does for the one
    /// the SDK keeps (see `tools/csil.txt` for which results are caller-owned).
    pub unsafe fn from_raw(
        winidea: &'a WinIdea,
        ptr: *mut std::ffi::c_void,
        free: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    ) -> Result<Self, Error> {
        // SAFETY: guaranteed by the caller.
        let inner = unsafe { Owned::adopt(winidea, ptr, free)? };
        Ok(Self { inner })
    }
}

impl<'a> ConnectionMgr<'a> {
    /// Connects as a [`crate::config::ConnectionConfig`] describes.
    ///
    /// This is the only connect that honours the config's **start mode**, so it
    /// is what "attach to a running winIDEA, and launch one only if none is
    /// there" actually goes through. The `connect_mru` family ignores the
    /// config entirely and always launches.
    pub fn connect_with(
        &mut self,
        config: &crate::config::ConnectionConfig<'_>,
    ) -> Result<(), Error> {
        // SAFETY: both handles are live; the SDK reads the config during the call.
        unsafe {
            (sym!(self.inner.sdk(), ConnectionMgr_connect__SWIG_0))(self.inner.ptr, config.as_ptr())
        };
        runtime::check()
    }
}

/// Multi-core, the way winIDEA does it on AURIX: every core beyond the primary
/// is debugged in a winIDEA instance of its own, which winIDEA opens -- and
/// creates the workspace for -- when the core is launched. That new window is
/// normal.
///
/// Core indices are the SoC topology's (`CSessionCtrl::get_topology`); on a
/// TC377: CPU0 = 0, CPU1 = 1, CPU2 = 2, SCR = 3, HSM = 4. Parameter names and
/// ownership below are from the managed wrappers in `tools/csil.txt`.
impl<'a> ConnectionMgr<'a> {
    /// Whether the winIDEA instance for core `index` is running. Read-only.
    /// (`isCoreLaunched(int32 coreIndex)`)
    pub fn is_core_launched(&self, index: i32) -> Result<bool, Error> {
        // SAFETY: live handle.
        let v = unsafe { (sym!(self.inner.sdk(), ConnectionMgr_isCoreLaunched))(self.inner.ptr, index) };
        runtime::check()?;
        Ok(v)
    }

    /// Opens (or finds) the instance for a core and returns its workspace
    /// path. (`launchCore(int32 coreIndex, bool isPrimaryCoreIndexed)`)
    ///
    /// With `primary_indexed` false -- the SDK's one-argument default -- the
    /// index does **not** count the primary core, so on a TC377 `1` is CPU2,
    /// not CPU1. Pass `true` to use topology indices.
    pub fn launch_core(&mut self, index: i32, primary_indexed: bool) -> Result<String, Error> {
        // SAFETY: live handle; the returned string is copied immediately.
        let p = unsafe {
            (sym!(self.inner.sdk(), ConnectionMgr_launchCore__SWIG_0))(self.inner.ptr, index, primary_indexed)
        };
        runtime::check()?;
        // SAFETY: `p` is null or a NUL-terminated string from the SDK.
        Ok(unsafe { runtime::from_cstr(p) }.unwrap_or_default())
    }

    /// Opens (or finds) the instance for core `index` and returns a connection
    /// to it. (`ConnectionMgr launchCore1(uint32 coreIndex)`; the returned
    /// connection is owned by the caller.)
    pub fn launch_core_connection(&mut self, index: u32) -> Result<ConnectionMgr<'a>, Error> {
        let sdk = self.inner.sdk();
        // SAFETY: live handle.
        let ptr = unsafe { (sym!(sdk, ConnectionMgr_launchCore1))(self.inner.ptr, index) };
        // SAFETY: the managed wrapper constructs its result with cMemoryOwn =
        // true, so it is ours to delete.
        let inner = unsafe { Owned::adopt(self.inner.winidea, ptr, sdk.delete_ConnectionMgr)? };
        Ok(ConnectionMgr { inner })
    }
}

impl<'a> ConnectionMgr<'a> {
    /// Wraps a handle this crate already took ownership of.
    pub(crate) fn from_owned(inner: Owned<'a>) -> Self {
        Self { inner }
    }
}
