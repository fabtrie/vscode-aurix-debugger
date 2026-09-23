//! Symbol lookup: names ↔ addresses ↔ source lines.
//!
//! This is what turns a raw target address into something a human (or a log
//! viewer) can read, and vice versa.
//!
//! ```no_run
//! use winidea_sdk::{WinIdea, ConnectionMgr, address::AddressController};
//!
//! # fn main() -> Result<(), winidea_sdk::Error> {
//! let winidea = WinIdea::load(&winidea_sdk::default_search_paths())?;
//! let mut conn = ConnectionMgr::new(&winidea)?;
//! conn.connect_mru("C:/ws/MyProject.xjrf")?;
//!
//! let mut addr = AddressController::new(&winidea, &conn)?;
//!
//! // name -> address
//! let a = addr.function_address("main")?;
//! println!("main is at {a}");
//!
//! // address -> source line
//! if let Some(loc) = addr.source_line_at_address(a.address)? {
//!     println!("that is {}:{}", loc.file, loc.line);
//! }
//! # Ok(())
//! # }
//! ```

use crate::debug::MemAddress;
use crate::runtime::{self, Error, Owned};
use crate::{ConnectionMgr, WinIdea};

/// What kinds of symbol a lookup should consider.
///
/// These are `IConnectDebug::ESymbolFlags`; values pinned by
/// `tests/enum_values.rs`. Combine with `|`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SymbolFlags(u32);

impl SymbolFlags {
    pub const VARIABLES: Self = Self(0x0000_0001);
    pub const LABELS: Self = Self(0x0000_0002);
    pub const FUNCTIONS: Self = Self(0x0000_0004);
    pub const LINES: Self = Self(0x0000_0008);
    pub const CONSTANTS: Self = Self(0x0000_0010);
    pub const SOURCE_CODE: Self = Self(0x0000_0080);

    /// Anything with a name: variables, labels, functions, constants.
    pub const ANY_SYMBOL: Self = Self(0x0000_0001 | 0x0000_0002 | 0x0000_0004 | 0x0000_0010);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl std::ops::BitOr for SymbolFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// A position in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    pub file: String,
    pub line: u32,
}

impl std::fmt::Display for SourceLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// Where a symbol lives and how big it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolInfo {
    pub address: u64,
    pub mem_area: u8,
    /// Size in minimum addressable units, not necessarily bytes — see
    /// [`crate::debug::DebugFacade::bytes_per_mau`].
    pub size_maus: u64,
}

/// What kind of symbol was found at an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Label,
    /// No function or label was found; the SDK reports the constants bucket.
    Constant,
    Variable,
    /// A flag combination outside the named variants.
    Other(i32),
}

impl SymbolKind {
    fn from_raw(v: i32) -> Self {
        match v {
            0x01 => Self::Variable,
            0x02 => Self::Label,
            0x04 => Self::Function,
            0x10 => Self::Constant,
            other => Self::Other(other),
        }
    }
}

/// One entry of a batch address→name lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionName {
    pub address: u64,
    /// Empty (or a hex address) when nothing was found.
    pub name: String,
    pub kind: SymbolKind,
}

/// Symbol and source-line lookup on a connected winIDEA instance.
pub struct AddressController<'a> {
    inner: Owned<'a>,
}

impl<'a> AddressController<'a> {
    /// Create an address controller bound to an existing connection.
    pub fn new(winidea: &'a WinIdea, conn: &'a ConnectionMgr<'a>) -> Result<Self, Error> {
        let sdk = winidea.raw();
        // SAFETY: `conn` outlives this controller, so its handle stays valid.
        let ptr = unsafe { (sym!(sdk, new_CAddressController))(conn.as_ptr()) };
        // SAFETY: `ptr` is a fresh CAddressController.
        let inner = unsafe { Owned::adopt(winidea, ptr, sdk.delete_CAddressController)? };
        Ok(Self { inner })
    }

    fn sdk(&self) -> &crate::generated::ffi::Sdk {
        self.inner.sdk()
    }

    // --- name -> address --------------------------------------------------

    /// Address of a named function.
    pub fn function_address(&mut self, name: &str) -> Result<MemAddress, Error> {
        let s = runtime::to_cstring(name)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let p = unsafe {
            (sym!(self.sdk(), CAddressController_getFunctionAddress))(self.inner.ptr, s.as_ptr())
        };
        self.decode_mem_address(p)
    }

    /// Address of a named variable.
    pub fn variable_address(&mut self, name: &str) -> Result<MemAddress, Error> {
        let s = runtime::to_cstring(name)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let p = unsafe {
            (sym!(self.sdk(), CAddressController_getVariableAddress))(self.inner.ptr, s.as_ptr())
        };
        self.decode_mem_address(p)
    }

    /// Address of a named assembler label.
    pub fn label_address(&mut self, name: &str) -> Result<MemAddress, Error> {
        let s = runtime::to_cstring(name)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let p = unsafe {
            (sym!(self.sdk(), CAddressController_getLabelAddress))(self.inner.ptr, s.as_ptr())
        };
        self.decode_mem_address(p)
    }

    /// Address an arbitrary expression evaluates to, e.g. `&cfg.field`.
    pub fn expression_address(&mut self, expression: &str) -> Result<MemAddress, Error> {
        let s = runtime::to_cstring(expression)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let p = unsafe {
            (sym!(self.sdk(), CAddressController_getExpressionAddress))(self.inner.ptr, s.as_ptr())
        };
        self.decode_mem_address(p)
    }

    /// Address, memory area and size of a symbol.
    pub fn symbol_info(&mut self, name: &str, flags: SymbolFlags) -> Result<SymbolInfo, Error> {
        let s = runtime::to_cstring(name)?;
        let sdk = self.sdk();
        // SAFETY: live handle; the SDK copies the string during the call. The
        // returned CSymbolInfo is owned by us.
        let p = unsafe {
            (sym!(sdk, CAddressController_getSymbolInfo))(self.inner.ptr, flags.bits(), s.as_ptr())
        };
        // SAFETY: `p` is a fresh CSymbolInfo.
        let owned = unsafe { Owned::adopt(self.inner.winidea, p, sdk.delete_CSymbolInfo)? };
        // SAFETY: `owned.ptr` is a live CSymbolInfo for the whole block.
        let info = unsafe {
            SymbolInfo {
                address: (sym!(sdk, CSymbolInfo_getAddress))(owned.ptr),
                mem_area: (sym!(sdk, CSymbolInfo_getMemArea))(owned.ptr),
                size_maus: (sym!(sdk, CSymbolInfo_getSizeMAUs))(owned.ptr),
            }
        };
        runtime::check()?;
        Ok(info)
    }

    // --- address -> name --------------------------------------------------

    /// Name of the symbol at an address.
    ///
    /// Returns an empty string when nothing matches.
    pub fn symbol_at_address(
        &mut self,
        flags: SymbolFlags,
        mem_area: u8,
        address: u64,
    ) -> Result<String, Error> {
        // SAFETY: live handle.
        let s = unsafe {
            (sym!(self.sdk(), CAddressController_getSymbolAtAddress__SWIG_1))(
                self.inner.ptr,
                flags.bits() as i32,
                mem_area,
                address,
            )
        };
        runtime::check()?;
        // SAFETY: `s` is null or a NUL-terminated string from the SDK.
        Ok(unsafe { runtime::from_cstr(s) }.unwrap_or_default())
    }

    /// Batch address→name lookup.
    ///
    /// Far cheaper than calling [`AddressController::symbol_at_address`] in a
    /// loop: one SDK round trip instead of N, which matters when symbolizing a
    /// whole trace. Addresses may point anywhere inside a function.
    pub fn function_names(&mut self, addresses: &[u64]) -> Result<Vec<FunctionName>, Error> {
        let sdk = self.sdk();

        // Input vector.
        // SAFETY: creates an empty AddressVector we own.
        let in_ptr = unsafe { (sym!(sdk, new_AddressVector__SWIG_0))() };
        // SAFETY: `in_ptr` is a fresh AddressVector.
        let addrs = unsafe { Owned::adopt(self.inner.winidea, in_ptr, sdk.delete_AddressVector)? };
        for a in addresses {
            // SAFETY: live vector handle.
            unsafe { (sym!(sdk, AddressVector_Add))(addrs.ptr, *a) };
            runtime::check()?;
        }

        // Output vectors, filled by the SDK.
        // SAFETY: creates empty StrVector / IntVector we own.
        let names_ptr = unsafe { (sym!(sdk, new_StrVector__SWIG_0))() };
        // SAFETY: `names_ptr` is a fresh StrVector.
        let names = unsafe { Owned::adopt(self.inner.winidea, names_ptr, sdk.delete_StrVector)? };
        // SAFETY: creates an empty IntVector we own.
        let types_ptr = unsafe { (sym!(sdk, new_IntVector__SWIG_0))() };
        // SAFETY: `types_ptr` is a fresh IntVector.
        let types = unsafe { Owned::adopt(self.inner.winidea, types_ptr, sdk.delete_IntVector)? };

        // SAFETY: all three handles are live; the SDK fills the two outputs.
        unsafe {
            (sym!(sdk, CAddressController_getFunctionNames))(
                self.inner.ptr,
                addrs.ptr,
                names.ptr,
                types.ptr,
            )
        };
        runtime::check()?;

        // SAFETY: live vector handles.
        let n = unsafe { (sym!(sdk, StrVector_size))(names.ptr) };
        runtime::check()?;

        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            // SAFETY: 0 <= i < size.
            let name = unsafe { (sym!(sdk, StrVector_getitem))(names.ptr, i as i32) };
            runtime::check()?;
            // SAFETY: `name` is null or a NUL-terminated string from the SDK.
            let name = unsafe { runtime::from_cstr(name) }.unwrap_or_default();

            // The SDK documents `types` as having the same length as the input,
            // but read defensively rather than trusting it.
            // SAFETY: live vector handle.
            let kind = if i < unsafe { (sym!(sdk, IntVector_size))(types.ptr) } {
                // SAFETY: bounds just checked.
                SymbolKind::from_raw(unsafe { (sym!(sdk, IntVector_getitem))(types.ptr, i as i32) })
            } else {
                SymbolKind::Other(0)
            };
            runtime::check()?;

            out.push(FunctionName {
                address: addresses.get(i as usize).copied().unwrap_or_default(),
                name,
                kind,
            });
        }
        Ok(out)
    }

    // --- address <-> source line ------------------------------------------

    /// Source position of an address.
    ///
    /// `None` when the address has no line information — common for library or
    /// startup code compiled without debug info.
    pub fn source_line_at_address(
        &mut self,
        address: u64,
    ) -> Result<Option<SourceLocation>, Error> {
        let sdk = self.sdk();
        // SAFETY: live handle; returns a CLineLocation we own.
        let p = unsafe {
            (sym!(sdk, CAddressController_getSourceLineAtAddress__SWIG_0))(self.inner.ptr, address)
        };
        runtime::check()?;
        if p.is_null() {
            return Ok(None);
        }
        // SAFETY: `p` is a fresh CLineLocation.
        let owned = unsafe { Owned::adopt(self.inner.winidea, p, sdk.delete_CLineLocation)? };
        // SAFETY: `owned.ptr` is a live CLineLocation for the whole block.
        let (file, line) = unsafe {
            (
                runtime::from_cstr((sym!(sdk, CLineLocation_getFileName))(owned.ptr)),
                (sym!(sdk, CLineLocation_getLineNumber))(owned.ptr),
            )
        };
        runtime::check()?;

        // An empty file name means "no source here", not an error.
        Ok(file
            .filter(|f| !f.is_empty())
            .map(|file| SourceLocation { file, line }))
    }

    /// Address of a source line.
    pub fn address_of_source_line(&mut self, file: &str, line: i32) -> Result<MemAddress, Error> {
        let f = runtime::to_cstring(file)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let p = unsafe {
            (sym!(
                self.sdk(),
                CAddressController_getAddressOfSourceLine__SWIG_1
            ))(self.inner.ptr, f.as_ptr(), line)
        };
        self.decode_mem_address(p)
    }

    /// The code memory area, physical or logical.
    pub fn code_area(&mut self, physical: bool) -> Result<i32, Error> {
        // SAFETY: live handle.
        let a =
            unsafe { (sym!(self.sdk(), CAddressController_getCodeArea))(self.inner.ptr, physical) };
        runtime::check()?;
        Ok(a)
    }

    /// Consume a `CMemAddress` handle the SDK just returned.
    ///
    /// These getters return `CMemAddress`, not `CValueType` — see
    /// [`crate::debug::MemAddress`].
    fn decode_mem_address(&self, ptr: *mut std::ffi::c_void) -> Result<MemAddress, Error> {
        let sdk = self.sdk();
        // SAFETY: `ptr` is null or a CMemAddress we own; adopt checks the SDK
        // error channel and rejects null.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_flag_encoding() {
        assert_eq!(SymbolFlags::VARIABLES.bits(), 0x01);
        assert_eq!(SymbolFlags::LABELS.bits(), 0x02);
        assert_eq!(SymbolFlags::FUNCTIONS.bits(), 0x04);
        assert_eq!(SymbolFlags::LINES.bits(), 0x08);
        assert_eq!(SymbolFlags::CONSTANTS.bits(), 0x10);
        assert_eq!(SymbolFlags::SOURCE_CODE.bits(), 0x80);

        // ANY_SYMBOL must be exactly the union of the named symbol kinds.
        assert_eq!(
            SymbolFlags::ANY_SYMBOL.bits(),
            (SymbolFlags::VARIABLES
                | SymbolFlags::LABELS
                | SymbolFlags::FUNCTIONS
                | SymbolFlags::CONSTANTS)
                .bits()
        );
        // ...and must not accidentally include line/source bits.
        assert_eq!(
            SymbolFlags::ANY_SYMBOL.bits() & SymbolFlags::LINES.bits(),
            0
        );
    }

    #[test]
    fn symbol_kind_from_raw() {
        assert_eq!(SymbolKind::from_raw(0x01), SymbolKind::Variable);
        assert_eq!(SymbolKind::from_raw(0x02), SymbolKind::Label);
        assert_eq!(SymbolKind::from_raw(0x04), SymbolKind::Function);
        assert_eq!(SymbolKind::from_raw(0x10), SymbolKind::Constant);
        assert_eq!(SymbolKind::from_raw(0x99), SymbolKind::Other(0x99));
    }

    #[test]
    fn source_location_display() {
        let loc = SourceLocation {
            file: "main.c".into(),
            line: 42,
        };
        assert_eq!(loc.to_string(), "main.c:42");
    }
}
