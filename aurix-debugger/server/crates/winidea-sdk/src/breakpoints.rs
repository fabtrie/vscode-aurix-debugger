//! Breakpoints and watchpoints **by address**.
//!
//! [`crate::DebugFacade`] can only set a hardware breakpoint on a *symbol*, and
//! delete it by symbol too. A debug server works in addresses: a source line
//! has already been resolved to one by the time a breakpoint is planted, and a
//! watchpoint is an address, a length and an access kind.
//!
//! `CBreakpointController` provides the address-keyed forms of both, which is
//! why this module exists rather than reusing the facade.

use crate::debug::HwAccess;
use crate::runtime::{self, Error, Owned};
use crate::{ConnectionMgr, WinIdea};

/// Address-keyed breakpoints and watchpoints on a connected winIDEA.
pub struct BreakpointController<'a> {
    inner: Owned<'a>,
}

impl<'a> BreakpointController<'a> {
    pub fn new(winidea: &'a WinIdea, conn: &'a ConnectionMgr<'a>) -> Result<Self, Error> {
        let sdk = winidea.raw();
        // SAFETY: `conn` outlives this controller, so its handle stays valid.
        let ptr = unsafe { (sym!(sdk, new_CBreakpointController))(conn.as_ptr()) };
        // SAFETY: `ptr` is a fresh CBreakpointController.
        let inner = unsafe { Owned::adopt(winidea, ptr, sdk.delete_CBreakpointController)? };
        Ok(Self { inner })
    }

    /// Wraps a handle obtained elsewhere.
    ///
    /// # Safety
    ///
    /// `ptr` must be a live `CBreakpointController`; pass `free: None` when the
    /// handle belongs to something else.
    pub unsafe fn from_raw(
        winidea: &'a WinIdea,
        ptr: *mut std::ffi::c_void,
        free: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    ) -> Result<Self, Error> {
        // SAFETY: guaranteed by the caller.
        let inner = unsafe { Owned::adopt(winidea, ptr, free)? };
        Ok(Self { inner })
    }

    /// Sets an execution breakpoint at `address`.
    pub fn set_at_address(&mut self, mem_area: i32, address: u64) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe {
            (sym!(self.inner.sdk(), CBreakpointController_setBP__SWIG_8))(
                self.inner.ptr,
                mem_area,
                address,
            )
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Removes the execution breakpoint at `address`.
    pub fn delete_at_address(&mut self, mem_area: i32, address: u64) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe {
            (sym!(self.inner.sdk(), CBreakpointController_deleteBP__SWIG_2))(
                self.inner.ptr,
                mem_area,
                address,
            )
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Enables or disables the breakpoint at `address`.
    pub fn set_enabled_at_address(
        &mut self,
        enabled: bool,
        mem_area: i32,
        address: u64,
    ) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe {
            (sym!(self.inner.sdk(), CBreakpointController_setEnabled__SWIG_2))(
                self.inner.ptr,
                enabled,
                mem_area,
                address,
            )
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Sets a hardware breakpoint or watchpoint at `address`.
    ///
    /// `access` selects fetch (an execution breakpoint) or a data access (a
    /// watchpoint). `access_size` is the width in bytes that the access must
    /// have to match, and `used_data_bytes` how many of `data` take part in a
    /// value comparison -- 0 for "any value", which is what a plain watchpoint
    /// wants.
    pub fn set_hw_at_address(
        &mut self,
        access: HwAccess,
        access_size: u32,
        used_data_bytes: u32,
        mem_area: i32,
        address: u64,
        data: u32,
    ) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe {
            (sym!(self.inner.sdk(), CBreakpointController_setHWBP__SWIG_1))(
                self.inner.ptr,
                access.to_raw(),
                access_size,
                used_data_bytes,
                mem_area,
                address,
                data,
            )
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Removes the hardware breakpoint or watchpoint at `address`.
    pub fn delete_hw_at_address(&mut self, mem_area: i32, address: u64) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe {
            (sym!(self.inner.sdk(), CBreakpointController_deleteHWBP__SWIG_1))(
                self.inner.ptr,
                mem_area,
                address,
            )
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Removes every breakpoint winIDEA holds.
    ///
    /// Worth doing when a session starts: a workspace can carry breakpoints
    /// saved from a previous run, and they would stop the target for reasons
    /// this server never asked for and cannot explain to a frontend.
    pub fn delete_all(&mut self) -> Result<(), Error> {
        let sdk = self.inner.sdk();
        // SAFETY: live handle.
        unsafe { (sym!(sdk, CBreakpointController_deleteAll))(self.inner.ptr) };
        runtime::check()?;
        // SAFETY: live handle.
        unsafe { (sym!(sdk, CBreakpointController_deleteAllHWBP))(self.inner.ptr) };
        runtime::check()
    }

    /// Re-applies every breakpoint to the target, after a download or reset.
    pub fn reapply_all(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe { (sym!(self.inner.sdk(), CBreakpointController_reapplyAll))(self.inner.ptr) };
        runtime::check()?;
        Ok(rc)
    }
}
