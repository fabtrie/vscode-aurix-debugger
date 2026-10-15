//! Breakpoints, memory/register access and expression evaluation.
//!
//! `CDebugFacade` is the SDK's convenience front-end. It duplicates the run
//! control already in [`crate::execution`]; this wrapper deliberately exposes
//! only what the facade *adds* — breakpoints, memory, registers and symbol
//! evaluation — so there is one obvious way to do each thing.
//!
//! ```no_run
//! use winidea_sdk::{WinIdea, ConnectionMgr, debug::DebugFacade};
//!
//! # fn main() -> Result<(), winidea_sdk::Error> {
//! let winidea = WinIdea::load(&winidea_sdk::default_search_paths())?;
//! let mut conn = ConnectionMgr::new(&winidea)?;
//! conn.connect_mru("C:/ws/MyProject.xjrf")?;
//!
//! let mut dbg = DebugFacade::new(&winidea, &conn)?;
//! dbg.set_breakpoint_at_function("main")?;
//! println!("counter = {}", dbg.evaluate("gCounter")?);
//! # Ok(())
//! # }
//! ```

use crate::runtime::{self, Error, Owned};
use crate::value::{self, Value};
use crate::{ConnectionMgr, WinIdea};

/// What kind of memory access the SDK may perform to satisfy a request.
///
/// These are `IConnectDebug::EAccessFlags`, threaded through every memory,
/// register and expression call. They control *how* the value is fetched --
/// whether the CPU may be stopped, whether cached values are acceptable --
/// not which value is fetched.
///
/// Values are pinned against the SDK's constants by `tests/enum_values.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccessFlags(u32);

impl AccessFlags {
    /// The SDK's defaults: stop the CPU if required, use the default cache
    /// policy. This is what winIDEA's own watch window uses.
    pub const DEFAULT: Self = Self(0);

    /// Read without stopping the CPU. Fails if the target cannot do it.
    pub const REAL_TIME: Self = Self(0x0000_0001);

    /// Do not refresh a cached value before reading.
    pub const NO_REFRESH: Self = Self(0x0000_0040);

    /// Bypass winIDEA's caching and go straight to the target.
    pub const DIRECT: Self = Self(0x0000_0080);

    /// Combine two flag sets.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The raw bitmask, for flags this wrapper does not name.
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl std::ops::BitOr for AccessFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// Which access a hardware breakpoint triggers on.
///
/// These are the `bHW_acc*` members of `IConnectDebug::EBreakpointFlags`. Note
/// they live in **bits 16-18**, not the low bits — the values are pinned
/// against the SDK's own constants by `tests/enum_values.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HwAccess {
    /// Trigger on any access.
    #[default]
    Any,
    /// Trigger on data reads.
    Read,
    /// Trigger on data writes.
    Write,
    /// Trigger on data reads or writes.
    ReadWrite,
    /// Trigger on instruction fetch.
    Fetch,
}

impl HwAccess {
    pub(crate) fn to_raw(self) -> u32 {
        match self {
            Self::Any => 0x0000_0000,
            Self::Read => 0x0001_0000,
            Self::Write => 0x0002_0000,
            Self::ReadWrite => 0x0003_0000,
            Self::Fetch => 0x0004_0000,
        }
    }
}

/// A target address plus the memory area it lives in.
///
/// The SDK's `CMemAddress`. Targets with more than one address space (code vs
/// data, or several SoCs) need both halves to identify a location, which is why
/// this is not just a `u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemAddress {
    pub address: u64,
    pub mem_area: u8,
}

impl std::fmt::Display for MemAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.mem_area == 0 {
            write!(f, "{:#x}", self.address)
        } else {
            write!(f, "{:#x}@{}", self.address, self.mem_area)
        }
    }
}

/// Breakpoints, memory and symbol access on a connected winIDEA instance.
pub struct DebugFacade<'a> {
    inner: Owned<'a>,
}

impl<'a> DebugFacade<'a> {
    /// Create a debug facade bound to an existing connection.
    pub fn new(winidea: &'a WinIdea, conn: &'a ConnectionMgr<'a>) -> Result<Self, Error> {
        let sdk = winidea.raw();
        // SAFETY: `conn` outlives this facade, so its handle stays valid.
        let ptr = unsafe { (sym!(sdk, new_CDebugFacade))(conn.as_ptr()) };
        // SAFETY: `ptr` is a fresh CDebugFacade.
        let inner = unsafe { Owned::adopt(winidea, ptr, sdk.delete_CDebugFacade)? };
        Ok(Self { inner })
    }

    fn sdk(&self) -> &crate::generated::ffi::Sdk {
        self.inner.sdk()
    }

    // --- breakpoints ------------------------------------------------------

    /// Set a breakpoint at a function, by symbol name.
    pub fn set_breakpoint_at_function(&mut self, symbol: &str) -> Result<i32, Error> {
        let s = runtime::to_cstring(symbol)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let rc =
            unsafe { (sym!(self.sdk(), CDebugFacade_setBP__SWIG_2))(self.inner.ptr, s.as_ptr()) };
        runtime::check()?;
        Ok(rc)
    }

    /// Set a breakpoint at a source line.
    pub fn set_breakpoint_at_line(&mut self, file: &str, line: i32) -> Result<i32, Error> {
        let f = runtime::to_cstring(file)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let rc = unsafe {
            (sym!(self.sdk(), CDebugFacade_setBP__SWIG_5))(self.inner.ptr, line, f.as_ptr())
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Set a breakpoint at an address.
    pub fn set_breakpoint_at_address(&mut self, mem_area: i32, address: u64) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe {
            (sym!(self.sdk(), CDebugFacade_setBP__SWIG_8))(self.inner.ptr, mem_area, address)
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Remove the breakpoint on a function.
    pub fn delete_breakpoint_at_function(&mut self, symbol: &str) -> Result<i32, Error> {
        let s = runtime::to_cstring(symbol)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let rc = unsafe {
            (sym!(self.sdk(), CDebugFacade_deleteBP__SWIG_0))(self.inner.ptr, s.as_ptr())
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Remove the breakpoint at an address.
    pub fn delete_breakpoint_at_address(
        &mut self,
        mem_area: i32,
        address: u64,
    ) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe {
            (sym!(self.sdk(), CDebugFacade_deleteBP__SWIG_2))(self.inner.ptr, mem_area, address)
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Remove every breakpoint.
    pub fn delete_all_breakpoints(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe { (sym!(self.sdk(), CDebugFacade_deleteAll))(self.inner.ptr) };
        runtime::check()?;
        Ok(rc)
    }

    /// Enable or disable the breakpoint on a function.
    pub fn set_breakpoint_enabled(&mut self, symbol: &str, enabled: bool) -> Result<i32, Error> {
        let s = runtime::to_cstring(symbol)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let rc = unsafe {
            (sym!(self.sdk(), CDebugFacade_setEnabled__SWIG_0))(self.inner.ptr, enabled, s.as_ptr())
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Set a hardware breakpoint / watchpoint on a symbol.
    ///
    /// `access_size` and `used_data_bytes` are in bytes; `data` is the value to
    /// match on for a data watchpoint.
    pub fn set_hw_breakpoint(
        &mut self,
        symbol: &str,
        access: HwAccess,
        access_size: u32,
        used_data_bytes: u32,
        entire_object: bool,
        data: u32,
    ) -> Result<i32, Error> {
        let s = runtime::to_cstring(symbol)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let rc = unsafe {
            (sym!(self.sdk(), CDebugFacade_setHWBP__SWIG_0))(
                self.inner.ptr,
                access.to_raw(),
                access_size,
                used_data_bytes,
                entire_object,
                s.as_ptr(),
                data,
            )
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Remove the hardware breakpoint on a symbol.
    pub fn delete_hw_breakpoint(&mut self, symbol: &str) -> Result<i32, Error> {
        let s = runtime::to_cstring(symbol)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let rc = unsafe {
            (sym!(self.sdk(), CDebugFacade_deleteHWBP__SWIG_0))(self.inner.ptr, s.as_ptr())
        };
        runtime::check()?;
        Ok(rc)
    }

    /// Remove every hardware breakpoint.
    pub fn delete_all_hw_breakpoints(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe { (sym!(self.sdk(), CDebugFacade_deleteAllHWBP))(self.inner.ptr) };
        runtime::check()?;
        Ok(rc)
    }

    /// Re-apply every breakpoint to the target, e.g. after a download.
    pub fn reapply_all_breakpoints(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe { (sym!(self.sdk(), CDebugFacade_reapplyAll))(self.inner.ptr) };
        runtime::check()?;
        Ok(rc)
    }

    // --- symbols & expressions -------------------------------------------

    /// Evaluate an expression in the target's symbol context.
    pub fn evaluate(&mut self, expression: &str) -> Result<Value, Error> {
        self.evaluate_in(AccessFlags::DEFAULT, expression)
    }

    /// Evaluate an expression in a specific core / execution context.
    pub fn evaluate_in(&mut self, flags: AccessFlags, expression: &str) -> Result<Value, Error> {
        let e = runtime::to_cstring(expression)?;
        // SAFETY: live handle; the SDK copies the string during the call. The
        // returned CValueType is owned by us.
        let p = unsafe {
            (sym!(self.sdk(), CDebugFacade_evaluate__SWIG_0))(
                self.inner.ptr,
                flags.bits() as i32,
                e.as_ptr(),
            )
        };
        runtime::check()?;
        // SAFETY: `p` is null or a CValueType we own.
        unsafe { value::decode_owned(self.inner.winidea, p) }
    }

    /// Assign to a target variable, given the new value as text.
    ///
    /// Text is the SDK's own input format, so enum labels and expressions work
    /// exactly as they do in winIDEA's watch window.
    pub fn modify(&mut self, expression: &str, new_value: &str) -> Result<String, Error> {
        self.modify_in(AccessFlags::DEFAULT, expression, new_value)
    }

    /// Assign to a target variable with explicit access flags.
    pub fn modify_in(
        &mut self,
        flags: AccessFlags,
        expression: &str,
        new_value: &str,
    ) -> Result<String, Error> {
        let e = runtime::to_cstring(expression)?;
        let v = runtime::to_cstring(new_value)?;
        // SAFETY: live handle; the SDK copies both strings during the call.
        let p = unsafe {
            (sym!(self.sdk(), CDebugFacade_modify__SWIG_3))(
                self.inner.ptr,
                flags.bits() as i32,
                e.as_ptr(),
                v.as_ptr(),
            )
        };
        runtime::check()?;
        // SAFETY: `p` is null or a NUL-terminated string from the SDK.
        Ok(unsafe { runtime::from_cstr(p) }.unwrap_or_default())
    }

    /// Address of a named variable.
    pub fn variable_address(&mut self, symbol: &str) -> Result<MemAddress, Error> {
        let s = runtime::to_cstring(symbol)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let p = unsafe {
            (sym!(self.sdk(), CDebugFacade_getVariableAddress))(self.inner.ptr, s.as_ptr())
        };
        // SAFETY: `p` is a fresh CMemAddress we own.
        self.decode_mem_address(p)
    }

    /// Address of a named function.
    ///
    /// For a function in a non-default download file, use the SDK's qualified
    /// form: `"module.c"#func,,file.elf`.
    pub fn function_address(&mut self, symbol: &str) -> Result<MemAddress, Error> {
        let s = runtime::to_cstring(symbol)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let p = unsafe {
            (sym!(self.sdk(), CDebugFacade_getFunctionAddress))(self.inner.ptr, s.as_ptr())
        };
        // SAFETY: `p` is a fresh CMemAddress we own.
        self.decode_mem_address(p)
    }

    /// Address of an arbitrary expression, e.g. `&myStruct.field`.
    pub fn expression_address(&mut self, expression: &str) -> Result<MemAddress, Error> {
        let s = runtime::to_cstring(expression)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let p = unsafe {
            (sym!(self.sdk(), CDebugFacade_getExpressionAddress))(self.inner.ptr, s.as_ptr())
        };
        // SAFETY: `p` is a fresh CMemAddress we own.
        self.decode_mem_address(p)
    }

    /// Consume a `CMemAddress` handle the SDK just returned.
    ///
    /// These getters return **`CMemAddress`**, not `CValueType` -- decoding one
    /// as the other reads an unrelated object's memory and yields garbage
    /// (caught in testing: `function_address("main")` printed `1VA<mojibake>`).
    fn decode_mem_address(&self, ptr: *mut std::ffi::c_void) -> Result<MemAddress, Error> {
        let sdk = self.sdk();
        // SAFETY: `ptr` is null or a CMemAddress we own; this checks the SDK
        // error channel and rejects null before use.
        let owned = unsafe { Owned::adopt(self.inner.winidea, ptr, sdk.delete_CMemAddress)? };
        // SAFETY: `owned.ptr` is a live CMemAddress for the whole block.
        let a = unsafe {
            MemAddress {
                address: (sym!(sdk, CMemAddress_getAddress))(owned.ptr),
                mem_area: (sym!(sdk, CMemAddress_getMemArea))(owned.ptr),
            }
        };
        runtime::check()?;
        Ok(a)
    }

    // --- registers --------------------------------------------------------

    /// Read a CPU register by name.
    pub fn read_register(&mut self, name: &str) -> Result<Value, Error> {
        self.read_register_in(AccessFlags::DEFAULT, name)
    }

    /// Read a CPU register in a specific context.
    pub fn read_register_in(&mut self, flags: AccessFlags, name: &str) -> Result<Value, Error> {
        let n = runtime::to_cstring(name)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let p = unsafe {
            (sym!(self.sdk(), CDebugFacade_readRegister))(
                self.inner.ptr,
                flags.bits() as i32,
                n.as_ptr(),
            )
        };
        runtime::check()?;
        // SAFETY: `p` is null or a CValueType we own.
        unsafe { value::decode_owned(self.inner.winidea, p) }
    }

    /// Write a CPU register by name.
    pub fn write_register(&mut self, name: &str, v: i64) -> Result<(), Error> {
        self.write_register_in(AccessFlags::DEFAULT, name, v)
    }

    /// Write a CPU register in a specific context.
    pub fn write_register_in(
        &mut self,
        flags: AccessFlags,
        name: &str,
        v: i64,
    ) -> Result<(), Error> {
        let n = runtime::to_cstring(name)?;
        let val = value::make_i64(self.inner.winidea, v)?;
        // SAFETY: live handle; `val` outlives the call and the SDK copies the
        // name string during it.
        unsafe {
            (sym!(self.sdk(), CDebugFacade_writeRegister))(
                self.inner.ptr,
                flags.bits() as i32,
                n.as_ptr(),
                val.ptr,
            )
        };
        runtime::check()
    }

    // --- memory -----------------------------------------------------------

    /// Read a typed value from an address.
    pub fn read_value(&mut self, mem_area: u8, address: u64, size: u64) -> Result<Value, Error> {
        let sdk = self.sdk();
        // The SDK takes a CValueType describing the size/type to read into.
        let hint = value::make_i64(self.inner.winidea, size as i64)?;
        // SAFETY: live handle; `hint` outlives the call. The returned
        // CValueType is owned by us.
        let p = unsafe {
            (sym!(sdk, CDebugFacade_readValue))(
                self.inner.ptr,
                AccessFlags::DEFAULT.bits() as i32,
                mem_area,
                address,
                hint.ptr,
            )
        };
        runtime::check()?;
        // SAFETY: `p` is null or a CValueType we own.
        unsafe { value::decode_owned(self.inner.winidea, p) }
    }

    /// Write a typed value to an address.
    pub fn write_value(&mut self, mem_area: u8, address: u64, v: i64) -> Result<(), Error> {
        let val = value::make_i64(self.inner.winidea, v)?;
        // SAFETY: live handle; `val` outlives the call.
        unsafe {
            (sym!(self.sdk(), CDebugFacade_writeValue__SWIG_0))(
                self.inner.ptr,
                AccessFlags::DEFAULT.bits() as i32,
                mem_area,
                address,
                val.ptr,
            )
        };
        runtime::check()
    }

    /// Write a floating-point value to an address.
    pub fn write_value_f64(&mut self, mem_area: u8, address: u64, v: f64) -> Result<(), Error> {
        let val = value::make_f64(self.inner.winidea, v)?;
        // SAFETY: live handle; `val` outlives the call.
        unsafe {
            (sym!(self.sdk(), CDebugFacade_writeValue__SWIG_0))(
                self.inner.ptr,
                AccessFlags::DEFAULT.bits() as i32,
                mem_area,
                address,
                val.ptr,
            )
        };
        runtime::check()
    }

    // --- misc -------------------------------------------------------------

    /// Download (flash) the configured program file to the target.
    pub fn download(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let rc = unsafe { (sym!(self.sdk(), CDebugFacade_download))(self.inner.ptr) };
        runtime::check()?;
        Ok(rc)
    }

    /// Bytes per minimum addressable unit for a memory area.
    ///
    /// Needed to interpret addresses on targets where a MAU is not one byte.
    ///
    /// Note the widened parameter: the SDK types `mem_area` as `int32` here but
    /// as `uint8` on the memory read/write calls. The signature is mirrored as
    /// declared rather than normalised, so the FFI boundary stays exact.
    pub fn bytes_per_mau(&self, mem_area: i32) -> Result<i32, Error> {
        // SAFETY: live handle.
        let n = unsafe {
            (sym!(self.sdk(), CDebugFacade_getMemoryAreaBytesPerMAU))(self.inner.ptr, mem_area)
        };
        runtime::check()?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `bHW_acc*` flags sit at bit 16, NOT in the low bits. Getting this
    /// wrong sets an unrelated breakpoint flag and silently watches the wrong
    /// access -- it does not fail.
    #[test]
    fn hw_access_encoding() {
        assert_eq!(HwAccess::Any.to_raw(), 0x0000_0000);
        assert_eq!(HwAccess::Read.to_raw(), 0x0001_0000);
        assert_eq!(HwAccess::Write.to_raw(), 0x0002_0000);
        assert_eq!(HwAccess::ReadWrite.to_raw(), 0x0003_0000);
        assert_eq!(HwAccess::Fetch.to_raw(), 0x0004_0000);

        // Read|Write must equal ReadWrite, or the flags are not really bits.
        assert_eq!(
            HwAccess::Read.to_raw() | HwAccess::Write.to_raw(),
            HwAccess::ReadWrite.to_raw()
        );
        assert_eq!(HwAccess::default(), HwAccess::Any);
    }
}

impl<'a> DebugFacade<'a> {
    /// Wraps a `DebugFacade` handle that did not come from a [`ConnectionMgr`].
    ///
    /// For a handle obtained from an SDK call this crate does not wrap (via
    /// [`crate::WinIdea::raw`]).
    ///
    /// # Safety
    ///
    /// `ptr` must be a live `DebugFacade`. `free` must be the matching destructor
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
