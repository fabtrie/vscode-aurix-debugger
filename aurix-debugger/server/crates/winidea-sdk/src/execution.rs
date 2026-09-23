//! Run control: start, stop, step and reset the target CPU.
//!
//! ```no_run
//! use winidea_sdk::{WinIdea, ConnectionMgr, execution::ExecutionController};
//!
//! # fn main() -> Result<(), winidea_sdk::Error> {
//! let winidea = WinIdea::load(&winidea_sdk::default_search_paths())?;
//! let mut conn = ConnectionMgr::new(&winidea)?;
//! conn.connect_mru("C:/ws/MyProject.xjrf")?;
//!
//! let mut exec = ExecutionController::new(&winidea, &conn)?;
//! exec.reset()?;
//! exec.run_until_function("main")?;
//! exec.wait_until_stopped(5_000)?;
//!
//! let cpu = exec.cpu_status()?;
//! println!("halted={} pc={:#x}", cpu.is_halted, cpu.execution_point);
//! # Ok(())
//! # }
//! ```

use crate::runtime::{self, Error, Owned};
use crate::{ConnectionMgr, WinIdea};

/// How long a run-control call waits for the CPU before giving up.
///
/// Mirrors `CExecutionController::ETimeoutMode`; values are pinned against the
/// SDK's own constants by `tests/enum_values.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeoutMode {
    /// Return immediately, without waiting.
    #[default]
    None,
    OneSecond,
    TenSeconds,
    HundredSeconds,
    /// Wait forever.
    Infinite,
}

impl TimeoutMode {
    fn to_raw(self) -> i32 {
        match self {
            Self::None => 0,
            Self::OneSecond => 1,
            Self::TenSeconds => 2,
            Self::HundredSeconds => 3,
            Self::Infinite => 4,
        }
    }
}

/// A snapshot of the CPU's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuStatus {
    pub is_running: bool,
    pub is_stopped: bool,
    pub is_halted: bool,
    pub is_idle: bool,
    pub is_waiting: bool,
    pub is_reset: bool,
    pub is_attached: bool,
    pub must_init: bool,
    /// Execution stopped at a software breakpoint.
    pub stopped_at_breakpoint: bool,
    /// Execution stopped at a hardware breakpoint / watchpoint.
    pub stopped_at_hw_breakpoint: bool,
    /// Execution stopped because a step completed.
    pub stopped_after_step: bool,
    /// Execution stopped because the host asked it to.
    pub stopped_explicitly: bool,
    /// Program counter at the time of the snapshot.
    pub execution_point: u64,
    /// Memory area the execution point lives in.
    pub execution_area: u8,
    /// The SDK's own rendering, when it provides one.
    pub text: Option<String>,
}

/// Controls execution on a connected winIDEA instance.
///
/// Borrows the [`ConnectionMgr`] it was built from, so the connection cannot be
/// dropped while the controller is still alive.
pub struct ExecutionController<'a> {
    inner: Owned<'a>,
}

impl<'a> ExecutionController<'a> {
    /// Create an execution controller bound to an existing connection.
    pub fn new(winidea: &'a WinIdea, conn: &'a ConnectionMgr<'a>) -> Result<Self, Error> {
        let sdk = winidea.raw();
        // SAFETY: `conn` outlives this controller, so its handle stays valid.
        let ptr = unsafe { (sym!(sdk, new_CExecutionController))(conn.as_ptr()) };
        // SAFETY: `ptr` is a fresh CExecutionController.
        let inner = unsafe { Owned::adopt(winidea, ptr, sdk.delete_CExecutionController)? };
        Ok(Self { inner })
    }

    fn sdk(&self) -> &crate::generated::ffi::Sdk {
        self.inner.sdk()
    }

    /// Start execution.
    pub fn run(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe { (sym!(self.sdk(), CExecutionController_run__SWIG_0))(self.inner.ptr) };
        runtime::check()?;
        Ok(rc)
    }

    /// Start execution, waiting for the CPU according to `timeout`.
    pub fn run_with_timeout(&mut self, timeout: TimeoutMode) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe {
            (sym!(self.sdk(), CExecutionController_run__SWIG_2))(self.inner.ptr, timeout.to_raw())
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Stop execution.
    pub fn stop(&mut self) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CExecutionController_stop))(self.inner.ptr) };
        runtime::check()
    }

    /// Reset the CPU, leaving it halted.
    pub fn reset(&mut self) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CExecutionController_reset__SWIG_1))(self.inner.ptr) };
        runtime::check()
    }

    /// Reset the CPU and immediately start execution.
    pub fn reset_and_run(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc =
            unsafe { (sym!(self.sdk(), CExecutionController_resetAndRun__SWIG_0))(self.inner.ptr) };
        runtime::check()?;
        Ok(rc)
    }

    /// Run until `function` is reached.
    pub fn run_until_function(&mut self, function: &str) -> Result<i32, Error> {
        let f = runtime::to_cstring(function)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let rc = unsafe {
            (sym!(self.sdk(), CExecutionController_runUntilFunction__SWIG_0))(
                self.inner.ptr,
                f.as_ptr(),
            )
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Run until the given address is reached.
    pub fn run_until_address(&mut self, mem_area: u8, address: u64) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe {
            (sym!(self.sdk(), CExecutionController_runUntilAddress__SWIG_0))(
                self.inner.ptr,
                mem_area,
                address,
            )
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Run until the current function returns.
    pub fn run_until_return(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe {
            (sym!(self.sdk(), CExecutionController_runUntilReturn__SWIG_0))(self.inner.ptr)
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Step one source line, entering called functions.
    pub fn step_into(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc =
            unsafe { (sym!(self.sdk(), CExecutionController_stepHigh__SWIG_2))(self.inner.ptr) };
        runtime::check()?;
        Ok(rc)
    }

    /// Step one source line, stepping over called functions.
    pub fn step_over(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe {
            (sym!(self.sdk(), CExecutionController_stepOverHigh__SWIG_2))(self.inner.ptr)
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Step one machine instruction, entering calls.
    pub fn step_instruction(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc =
            unsafe { (sym!(self.sdk(), CExecutionController_stepInst__SWIG_2))(self.inner.ptr) };
        runtime::check()?;
        Ok(rc)
    }

    /// Step one machine instruction, stepping over calls.
    pub fn step_over_instruction(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe {
            (sym!(self.sdk(), CExecutionController_stepOverInst__SWIG_2))(self.inner.ptr)
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Move the execution point to `function` without running to it.
    pub fn goto_function(&mut self, function: &str) -> Result<(), Error> {
        let f = runtime::to_cstring(function)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        unsafe {
            (sym!(self.sdk(), CExecutionController_gotoFunction))(self.inner.ptr, f.as_ptr())
        };
        runtime::check()
    }

    /// Move the execution point to an address without running to it.
    pub fn goto_address(&mut self, mem_area: u8, address: u64) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe {
            (sym!(self.sdk(), CExecutionController_gotoAddress))(self.inner.ptr, mem_area, address)
        };
        runtime::check()
    }

    /// Block until the CPU stops, or `timeout_ms` elapses.
    ///
    /// Returns `true` if the CPU is stopped. Pass `0` to wait indefinitely --
    /// that is the SDK's own convention for this parameter.
    pub fn wait_until_stopped(&mut self, timeout_ms: i32) -> Result<bool, Error> {
        // SAFETY: live handle.
        let stopped = unsafe {
            (sym!(self.sdk(), CExecutionController_waitUntilStopped__SWIG_2))(
                self.inner.ptr,
                timeout_ms,
            )
        };
        runtime::check()?;
        Ok(stopped)
    }

    /// Block while the CPU runs, up to `timeout_ms`.
    pub fn wait_while_running(&mut self, timeout_ms: i32) -> Result<bool, Error> {
        // SAFETY: live handle.
        let running = unsafe {
            (sym!(self.sdk(), CExecutionController_waitWhileRunning__SWIG_2))(
                self.inner.ptr,
                timeout_ms,
            )
        };
        runtime::check()?;
        Ok(running)
    }

    /// Attach to a running target without halting it.
    pub fn hot_attach(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe { (sym!(self.sdk(), CExecutionController_hotAttach))(self.inner.ptr) };
        runtime::check()?;
        Ok(rc)
    }

    /// Detach from a target that was hot-attached, leaving it running.
    pub fn hot_detach(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe { (sym!(self.sdk(), CExecutionController_hotDetach))(self.inner.ptr) };
        runtime::check()?;
        Ok(rc)
    }

    /// Enable or disable winIDEA's background polling of target state.
    pub fn set_polling_enabled(&mut self, enabled: bool) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe {
            (sym!(self.sdk(), CExecutionController_setPollingEnabled))(self.inner.ptr, enabled)
        };
        runtime::check()
    }

    /// Evaluate an expression on the target and return the result as text.
    pub fn call(&mut self, expression: &str) -> Result<String, Error> {
        let e = runtime::to_cstring(expression)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let p = unsafe {
            (sym!(self.sdk(), CExecutionController_call__SWIG_0))(self.inner.ptr, e.as_ptr())
        };
        runtime::check()?;
        // SAFETY: `p` is null or a NUL-terminated string from the SDK.
        Ok(unsafe { runtime::from_cstr(p) }.unwrap_or_default())
    }

    /// Current CPU state.
    pub fn cpu_status(&self) -> Result<CpuStatus, Error> {
        let sdk = self.sdk();
        // SAFETY: live handle; the returned CPUStatus is owned by us.
        let h = unsafe { (sym!(sdk, CExecutionController_getCPUStatus__SWIG_1))(self.inner.ptr) };
        // SAFETY: `h` is a fresh CPUStatus.
        let owned = unsafe { Owned::adopt(self.inner.winidea, h, sdk.delete_CPUStatus)? };
        // SAFETY: `owned.ptr` is a live CPUStatus for the whole block.
        let st = unsafe {
            CpuStatus {
                is_running: (sym!(sdk, CPUStatus_isRunning))(owned.ptr),
                is_stopped: (sym!(sdk, CPUStatus_isStopped))(owned.ptr),
                is_halted: (sym!(sdk, CPUStatus_isHalted))(owned.ptr),
                is_idle: (sym!(sdk, CPUStatus_isIdle))(owned.ptr),
                is_waiting: (sym!(sdk, CPUStatus_isWaiting))(owned.ptr),
                is_reset: (sym!(sdk, CPUStatus_isReset))(owned.ptr),
                is_attached: (sym!(sdk, CPUStatus_isAttach))(owned.ptr),
                must_init: (sym!(sdk, CPUStatus_isMustInit))(owned.ptr),
                stopped_at_breakpoint: (sym!(sdk, CPUStatus_isStopReasonBP))(owned.ptr),
                stopped_at_hw_breakpoint: (sym!(sdk, CPUStatus_isStopReasonHW))(owned.ptr),
                stopped_after_step: (sym!(sdk, CPUStatus_isStopReasonStep))(owned.ptr),
                stopped_explicitly: (sym!(sdk, CPUStatus_isStopReasonExplicit))(owned.ptr),
                execution_point: (sym!(sdk, CPUStatus_getExecutionPoint))(owned.ptr),
                execution_area: (sym!(sdk, CPUStatus_getExecutionArea))(owned.ptr),
                text: runtime::from_cstr((sym!(sdk, CPUStatus_toString))(owned.ptr)),
            }
        };
        runtime::check()?;
        Ok(st)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_mode_encoding() {
        assert_eq!(TimeoutMode::None.to_raw(), 0);
        assert_eq!(TimeoutMode::OneSecond.to_raw(), 1);
        assert_eq!(TimeoutMode::TenSeconds.to_raw(), 2);
        assert_eq!(TimeoutMode::HundredSeconds.to_raw(), 3);
        assert_eq!(TimeoutMode::Infinite.to_raw(), 4);
        assert_eq!(TimeoutMode::default(), TimeoutMode::None);
    }
}

impl<'a> ExecutionController<'a> {
    /// Wraps a `ExecutionController` handle that did not come from a [`ConnectionMgr`].
    ///
    /// For a handle obtained from an SDK call this crate does not wrap (via
    /// [`crate::WinIdea::raw`]).
    ///
    /// # Safety
    ///
    /// `ptr` must be a live `ExecutionController`. `free` must be the matching destructor
    /// **only if this wrapper owns the handle**; pass `None` when the handle
    /// belongs to something else. The managed wrappers in `tools/csil.txt` say
    /// which: a result constructed with `cMemoryOwn = true` is the caller's.
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

/// Run control with an explicit wait.
///
/// The argument-less forms above block until winIDEA considers the operation
/// complete (default timeout). These take the wait as a parameter and never
/// raise on timeout: when `timeout` expires the command is still in progress
/// on the target, and the caller polls [`ExecutionController::cpu_status`] to
/// see it finish. That bounds how long a caller's thread is held -- a source
/// step over a slow function would otherwise block for up to the default.
///
/// Measured on a BlueBox (winIDEA 9.21.356, TC377): an instruction step
/// ~250 ms and a source step over ~200-350 ms, either way; the cost is
/// winIDEA's own work per stop, not the wait. A high-level step at an address
/// without line information blocks regardless of `timeout`, so only use those
/// where winIDEA has a source line for the PC.
///
/// (Overloads `(ETimeoutMode timeoutMode, bool throwOnTimeout)` in
/// `tools/csil.txt`.)
impl<'a> ExecutionController<'a> {
    fn step_kind(
        &mut self,
        f: Option<unsafe extern "C" fn(*mut std::ffi::c_void, i32, bool) -> i32>,
        name: &'static str,
        timeout: TimeoutMode,
    ) -> Result<i32, Error> {
        let f = f.ok_or(Error::MissingSymbol(name))?;
        // SAFETY: live handle; throwOnTimeout = false.
        let rc = unsafe { f(self.inner.ptr, timeout.to_raw(), false) };
        runtime::check()?;
        Ok(rc)
    }

    pub fn step_instruction_with(&mut self, timeout: TimeoutMode) -> Result<i32, Error> {
        let f = self.sdk().CExecutionController_stepInst__SWIG_0;
        self.step_kind(f, "CExecutionController_stepInst__SWIG_0", timeout)
    }

    pub fn step_over_instruction_with(&mut self, timeout: TimeoutMode) -> Result<i32, Error> {
        let f = self.sdk().CExecutionController_stepOverInst__SWIG_0;
        self.step_kind(f, "CExecutionController_stepOverInst__SWIG_0", timeout)
    }

    /// Source-level step into, as winIDEA does it (`stepHigh`).
    pub fn step_into_with(&mut self, timeout: TimeoutMode) -> Result<i32, Error> {
        let f = self.sdk().CExecutionController_stepHigh__SWIG_0;
        self.step_kind(f, "CExecutionController_stepHigh__SWIG_0", timeout)
    }

    /// Source-level step over, as winIDEA does it (`stepOverHigh`).
    pub fn step_over_with(&mut self, timeout: TimeoutMode) -> Result<i32, Error> {
        let f = self.sdk().CExecutionController_stepOverHigh__SWIG_0;
        self.step_kind(f, "CExecutionController_stepOverHigh__SWIG_0", timeout)
    }

    /// Step out: run until the current function returns (`runUntilReturn`).
    pub fn run_until_return_with(&mut self, timeout: TimeoutMode) -> Result<i32, Error> {
        let f = self.sdk().CExecutionController_runUntilReturn__SWIG_1;
        self.step_kind(f, "CExecutionController_runUntilReturn__SWIG_1", timeout)
    }
}
