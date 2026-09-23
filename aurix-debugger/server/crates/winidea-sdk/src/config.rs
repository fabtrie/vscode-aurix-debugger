//! How a connection to winIDEA is made: which instance, and whether to start one.
//!
//! `CConnectionConfig` is a builder the SDK consumes when connecting. Its start
//! modes are the reason this is worth wrapping: "attach to a running winIDEA,
//! and only launch one if none is there" is a mode the SDK already has, so it
//! does not have to be rebuilt out of [`crate::ConnectionMgr::enumerate_instances`]
//! plus a `connect_tcp` fallback.

use crate::runtime::{self, Error, Owned};
use crate::WinIdea;

/// Whether connecting may start a winIDEA process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StartMode {
    /// Attach to a running instance, launching one only if none is found.
    #[default]
    IfRequired,
    /// Attach only. Fails when nothing is running, and never launches.
    Existing,
    /// Always launch a fresh instance.
    Always,
}

/// Builder for a connection.
///
/// The SDK's setters return the config itself for chaining; this wrapper
/// mutates in place and ignores that return, which is the same thing.
pub struct ConnectionConfig<'a> {
    inner: Owned<'a>,
}

impl<'a> ConnectionConfig<'a> {
    pub fn new(winidea: &'a WinIdea) -> Result<Self, Error> {
        let sdk = winidea.raw();
        // SAFETY: a plain constructor with no arguments.
        let ptr = unsafe { (sym!(sdk, new_CConnectionConfig))() };
        // SAFETY: `ptr` is a fresh CConnectionConfig.
        let inner = unsafe { Owned::adopt(winidea, ptr, sdk.delete_CConnectionConfig)? };
        Ok(Self { inner })
    }

    pub(crate) fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.inner.ptr
    }

    /// The workspace to open. Empty means whatever winIDEA opens by itself.
    pub fn workspace(&mut self, path: &str) -> Result<&mut Self, Error> {
        let p = runtime::to_cstring(path)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        unsafe {
            (sym!(self.inner.sdk(), CConnectionConfig_workspace))(self.inner.ptr, p.as_ptr())
        };
        runtime::check()?;
        Ok(self)
    }

    /// Bind to an instance already listening on `host:port`. An empty host
    /// means this machine.
    pub fn host(&mut self, host: &str, port: u16) -> Result<&mut Self, Error> {
        let h = runtime::to_cstring(host)?;
        // SAFETY: live handle; the host string is copied during the call.
        unsafe {
            (sym!(self.inner.sdk(), CConnectionConfig_host__SWIG_0))(
                self.inner.ptr,
                h.as_ptr(),
                port,
            )
        };
        runtime::check()?;
        Ok(self)
    }

    /// Run without a visible winIDEA window.
    pub fn headless(&mut self, on: bool) -> Result<&mut Self, Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.inner.sdk(), CConnectionConfig_headless))(self.inner.ptr, on) };
        runtime::check()?;
        Ok(self)
    }

    /// Whether connecting may launch winIDEA.
    ///
    /// Each mode has its own export rather than an integer, so there is no
    /// enum value to get wrong here.
    pub fn start(&mut self, mode: StartMode) -> Result<&mut Self, Error> {
        let sdk = self.inner.sdk();
        // SAFETY: live handle; all three take only the config.
        unsafe {
            match mode {
                StartMode::IfRequired => {
                    (sym!(sdk, CConnectionConfig_start_if_required))(self.inner.ptr)
                }
                StartMode::Existing => {
                    (sym!(sdk, CConnectionConfig_start_existing))(self.inner.ptr)
                }
                StartMode::Always => (sym!(sdk, CConnectionConfig_start_always))(self.inner.ptr),
            }
        };
        runtime::check()?;
        Ok(self)
    }

    /// How long to wait for a launched instance, in seconds.
    pub fn wait_time(&mut self, seconds: i32) -> Result<&mut Self, Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.inner.sdk(), CConnectionConfig_waitTime))(self.inner.ptr, seconds) };
        runtime::check()?;
        Ok(self)
    }

    /// The configured workspace, as the SDK sees it. Useful for logging what a
    /// connection actually asked for.
    pub fn configured_workspace(&self) -> Result<String, Error> {
        // SAFETY: live handle; the SDK owns the returned string.
        let p = unsafe { (sym!(self.inner.sdk(), CConnectionConfig_getWorkspace))(self.inner.ptr) };
        runtime::check()?;
        // SAFETY: `p` is null or a NUL-terminated string from the SDK.
        Ok(unsafe { runtime::from_cstr(p) }.unwrap_or_default())
    }
}

impl<'a> ConnectionConfig<'a> {
    /// Bind to the winIDEA instance with this id -- what
    /// [`crate::ConnectionMgr::launch_core`] returns for a secondary core.
    pub fn instance_id(&mut self, id: &str) -> Result<&mut Self, Error> {
        let s = runtime::to_cstring(id)?;
        // SAFETY: live handle; the id is copied during the call.
        unsafe { (sym!(self.inner.sdk(), CConnectionConfig_instanceId))(self.inner.ptr, s.as_ptr()) };
        runtime::check()?;
        Ok(self)
    }
}
