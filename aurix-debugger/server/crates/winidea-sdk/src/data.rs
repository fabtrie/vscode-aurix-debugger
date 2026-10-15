//! Bulk memory access and symbol enumeration.
//!
//! Where [`crate::debug::DebugFacade`] reads one value at a time,
//! `CDataController` moves whole memory blocks and enumerates the target's
//! symbol table. That makes it the right tool for dumping a region, checksumming
//! an image, or discovering what variables exist without knowing their names up
//! front.
//!
//! ```no_run
//! use winidea_sdk::{WinIdea, ConnectionMgr, data::DataController};
//!
//! # fn main() -> Result<(), winidea_sdk::Error> {
//! let winidea = WinIdea::load(&winidea_sdk::default_search_paths())?;
//! let mut conn = ConnectionMgr::new(&winidea)?;
//! conn.connect_mru("C:/ws/MyProject.xjrf")?;
//!
//! let mut data = DataController::new(&winidea, &conn)?;
//!
//! // Dump 256 bytes.
//! let block = data.read_memory(0, 0x8000_0000, 256)?;
//! println!("{} bytes, all readable: {}", block.data.len(), block.is_fully_readable());
//!
//! // What globals does this image have?
//! for v in data.variables()?.iter().take(10) {
//!     println!("{} : {} ({} bytes)", v.name, v.type_name, v.size);
//! }
//! # Ok(())
//! # }
//! ```

use crate::debug::AccessFlags;
use crate::runtime::{self, Error, Owned};
use crate::{ConnectionMgr, WinIdea};

/// Per-MAU read status returned alongside memory data.
///
/// `Access_OK`/`Access_Fail` from the SDK's `globdefs.h`; pinned by
/// `tests/enum_values.rs`.
const ACCESS_OK: u8 = 0;

/// A block of target memory, plus whether each unit could actually be read.
///
/// The SDK returns data and status in one buffer; this splits them. A partly
/// unreadable block is normal — reading across an unmapped region succeeds for
/// the mapped part and flags the rest, rather than failing outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryBlock {
    /// The bytes read. Length is `num_maus * bytes_per_mau`.
    pub data: Vec<u8>,
    /// One entry per MAU: `true` when that unit was read successfully.
    pub readable: Vec<bool>,
    /// Bytes per minimum addressable unit for this region.
    pub bytes_per_mau: u8,
}

impl MemoryBlock {
    /// Whether every unit in the block was read successfully.
    pub fn is_fully_readable(&self) -> bool {
        self.readable.iter().all(|ok| *ok)
    }

    /// Number of minimum addressable units in this block.
    pub fn num_maus(&self) -> usize {
        self.readable.len()
    }
}

/// A variable in the target's symbol table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    pub name: String,
    /// Fully qualified name, which disambiguates file-static variables.
    pub qualified_name: String,
    pub type_name: String,
    pub scope: String,
    /// Size in bytes.
    pub size: i64,
    /// Number of elements when this is an array, else 0.
    pub array_dimension: i64,
}

/// One node of an expression's type tree: a variable, struct field or array
/// element, as returned by [`ExpressionController::children`].
///
/// `expression` is the string to hand back to the SDK — to DAQ-configure this
/// member, or to expand it further. It is built by the caller from the parent's
/// expression, because the SDK reports only the member's own `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeNode {
    /// Member name (`gain`) or array element (`[3]`), not the full path.
    pub name: String,
    /// Full expression, e.g. `cfg.tuning.gain` or `buf[3]`. Use this for DAQ.
    pub expression: String,
    pub type_name: String,
    pub address: u64,
    pub mem_area: u8,
    /// Size in bytes.
    pub size: u32,
    /// Number of elements when this is an array, else 0.
    pub array_dimension: u32,
    /// Whether this node has children — a struct, union or array.
    ///
    /// Derived from the SDK actually reporting children rather than from the
    /// type name, so a typedef'd struct is still recognised as compound.
    pub is_compound: bool,
}

impl TypeNode {
    /// Whether this node can be acquired directly as a single DAQ item.
    ///
    /// Compound nodes cannot: DAQ samples a scalar location, so a struct must
    /// be expanded and its leaves acquired individually.
    pub fn is_acquirable(&self) -> bool {
        !self.is_compound && self.size > 0
    }
}

/// A function in the target's symbol table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    pub name: String,
    pub qualified_name: String,
    pub return_type: String,
    pub scope: String,
    pub module_index: i32,
}

/// A compilation module (source file) in the target image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    pub name: String,
    pub path: String,
}

/// Which download file (partition) a symbol query applies to.
///
/// A winIDEA session can have several download files loaded; symbol lookups are
/// scoped to one of them. The SDK's `PARTITION_CURRENT` / `PARTITION_DEFAULT`
/// sentinels are C# static fields rather than native exports, so they are
/// reproduced here.
///
/// Passing a plain `0` is **not** the same as [`Partition::Default`] — index 0
/// is only valid if a partition with that number actually exists, and the SDK
/// raises `ICONNECT_E_BAD_ID` otherwise. Enumerate with
/// [`DataController::partitions`] when in doubt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Partition {
    /// The default download file.
    #[default]
    Default,
    /// The currently selected download file.
    Current,
    /// An explicit partition index, as ordered by
    /// [`DataController::partitions`].
    Index(i32),
}

impl Partition {
    fn to_raw(self) -> i32 {
        match self {
            // Verified against IConnectCSLib.dll:
            //   PARTITION_DEFAULT = 0xFFFFFFFF (-1)
            //   PARTITION_CURRENT = 0xFFFFFFFE (-2)
            Self::Default => -1,
            Self::Current => -2,
            Self::Index(i) => i,
        }
    }
}

/// A download file loaded into the winIDEA session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionInfo {
    pub name: String,
    pub path: String,
}

/// Bulk memory and symbol-table access on a connected winIDEA instance.
pub struct DataController<'a> {
    inner: Owned<'a>,
}

impl<'a> DataController<'a> {
    /// Create a data controller bound to an existing connection.
    pub fn new(winidea: &'a WinIdea, conn: &'a ConnectionMgr<'a>) -> Result<Self, Error> {
        let sdk = winidea.raw();
        // SAFETY: `conn` outlives this controller, so its handle stays valid.
        let ptr = unsafe { (sym!(sdk, new_CDataController))(conn.as_ptr()) };
        // SAFETY: `ptr` is a fresh CDataController.
        let inner = unsafe { Owned::adopt(winidea, ptr, sdk.delete_CDataController)? };
        Ok(Self { inner })
    }

    fn sdk(&self) -> &crate::generated::ffi::Sdk {
        self.inner.sdk()
    }

    // --- memory -----------------------------------------------------------

    /// Bytes per minimum addressable unit for a memory area.
    ///
    /// Needed to interpret `read_memory` results on targets where a MAU is not
    /// one byte (some DSPs use 16- or 32-bit MAUs).
    pub fn bytes_per_mau(&mut self, mem_area: i32) -> Result<i32, Error> {
        // SAFETY: live handle.
        let n = unsafe {
            (sym!(self.sdk(), CDataController_getMemoryAreaBytesPerMAU))(self.inner.ptr, mem_area)
        };
        runtime::check()?;
        Ok(n)
    }

    /// Read a block of memory, assuming one byte per MAU.
    ///
    /// For targets with wider MAUs use [`DataController::read_memory_maus`].
    pub fn read_memory(
        &mut self,
        mem_area: u8,
        address: u64,
        num_bytes: u64,
    ) -> Result<MemoryBlock, Error> {
        self.read_memory_maus(AccessFlags::DEFAULT, mem_area, address, num_bytes, 1)
    }

    /// Read a block of memory with explicit MAU width and access flags.
    ///
    /// The SDK returns one buffer holding `num_maus * bytes_per_mau` data bytes
    /// followed by `num_maus` status bytes; this splits them into
    /// [`MemoryBlock::data`] and [`MemoryBlock::readable`].
    pub fn read_memory_maus(
        &mut self,
        flags: AccessFlags,
        mem_area: u8,
        address: u64,
        num_maus: u64,
        bytes_per_mau: u8,
    ) -> Result<MemoryBlock, Error> {
        if bytes_per_mau == 0 {
            return Err(Error::Sdk {
                kind: crate::ExceptionKind::Argument,
                message: "bytes_per_mau must be at least 1".into(),
            });
        }
        let sdk = self.sdk();

        // SAFETY: live handle. Returns a VectorBYTE we own.
        let p = unsafe {
            (sym!(sdk, CDataController_readMemory))(
                self.inner.ptr,
                flags.bits() as i32,
                mem_area,
                address,
                num_maus,
                bytes_per_mau,
            )
        };
        // SAFETY: `p` is a fresh VectorBYTE.
        let buf = unsafe { Owned::adopt(self.inner.winidea, p, sdk.delete_VectorBYTE)? };

        // SAFETY: live vector handle.
        let total = unsafe { (sym!(sdk, VectorBYTE_size))(buf.ptr) } as usize;
        runtime::check()?;

        let data_len = (num_maus as usize).saturating_mul(bytes_per_mau as usize);
        // Trust the SDK's actual buffer over the requested size: a short read
        // would otherwise index past the end.
        let data_len = data_len.min(total);

        let mut data = Vec::with_capacity(data_len);
        for i in 0..data_len {
            // SAFETY: 0 <= i < size.
            data.push(unsafe { (sym!(sdk, VectorBYTE_getitem))(buf.ptr, i as i32) });
        }
        runtime::check()?;

        let mut readable = Vec::with_capacity(num_maus as usize);
        for i in data_len..total {
            // SAFETY: 0 <= i < size.
            let st = unsafe { (sym!(sdk, VectorBYTE_getitem))(buf.ptr, i as i32) };
            readable.push(st == ACCESS_OK);
        }
        runtime::check()?;

        Ok(MemoryBlock {
            data,
            readable,
            bytes_per_mau,
        })
    }

    /// Write a block of memory, assuming one byte per MAU.
    pub fn write_memory(&mut self, mem_area: u8, address: u64, data: &[u8]) -> Result<(), Error> {
        self.write_memory_maus(AccessFlags::DEFAULT, mem_area, address, data, 1)
    }

    /// Write a block of memory with explicit MAU width and access flags.
    pub fn write_memory_maus(
        &mut self,
        flags: AccessFlags,
        mem_area: u8,
        address: u64,
        data: &[u8],
        bytes_per_mau: u8,
    ) -> Result<(), Error> {
        if bytes_per_mau == 0 {
            return Err(Error::Sdk {
                kind: crate::ExceptionKind::Argument,
                message: "bytes_per_mau must be at least 1".into(),
            });
        }
        let sdk = self.sdk();

        // SAFETY: creates an empty VectorBYTE we own.
        let v_ptr = unsafe { (sym!(sdk, new_VectorBYTE__SWIG_0))() };
        // SAFETY: `v_ptr` is a fresh VectorBYTE.
        let buf = unsafe { Owned::adopt(self.inner.winidea, v_ptr, sdk.delete_VectorBYTE)? };
        for b in data {
            // SAFETY: live vector handle.
            unsafe { (sym!(sdk, VectorBYTE_Add))(buf.ptr, *b) };
        }
        runtime::check()?;

        let num_maus = (data.len() / bytes_per_mau as usize) as u64;
        // SAFETY: live handle + buffer. The returned status vector is owned by
        // us; adopt it so it is freed rather than leaked.
        let st = unsafe {
            (sym!(sdk, CDataController_writeMemory))(
                self.inner.ptr,
                flags.bits() as i32,
                mem_area,
                address,
                num_maus,
                bytes_per_mau,
                buf.ptr,
            )
        };
        runtime::check()?;
        if !st.is_null() {
            // SAFETY: `st` is a VectorBYTE we own; dropping frees it.
            let _ = unsafe { Owned::adopt(self.inner.winidea, st, sdk.delete_VectorBYTE)? };
        }
        Ok(())
    }

    // --- symbol enumeration ------------------------------------------------

    /// The download files loaded in this session.
    ///
    /// Index into this list to build a [`Partition::Index`]. Useful when a
    /// session has several images and you need symbols from a specific one.
    pub fn partitions(&mut self) -> Result<Vec<PartitionInfo>, Error> {
        let sdk = self.sdk();

        // SAFETY: creates two empty StrVectors we own.
        let paths_ptr = unsafe { (sym!(sdk, new_StrVector__SWIG_0))() };
        // SAFETY: `paths_ptr` is a fresh StrVector.
        let paths = unsafe { Owned::adopt(self.inner.winidea, paths_ptr, sdk.delete_StrVector)? };
        // SAFETY: creates an empty StrVector we own.
        let names_ptr = unsafe { (sym!(sdk, new_StrVector__SWIG_0))() };
        // SAFETY: `names_ptr` is a fresh StrVector.
        let names = unsafe { Owned::adopt(self.inner.winidea, names_ptr, sdk.delete_StrVector)? };

        // SAFETY: live handles; the SDK fills both vectors.
        unsafe { (sym!(sdk, CDataController_getPartitions))(self.inner.ptr, paths.ptr, names.ptr) };
        runtime::check()?;

        // SAFETY: live vector handle.
        let n = unsafe { (sym!(sdk, StrVector_size))(names.ptr) };
        runtime::check()?;

        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            // SAFETY: 0 <= i < size for both vectors (the SDK fills them in
            // lockstep); `paths` is re-checked below in case it does not.
            let name = unsafe { (sym!(sdk, StrVector_getitem))(names.ptr, i as i32) };
            // SAFETY: `name` is null or a NUL-terminated string from the SDK.
            let name = unsafe { runtime::from_cstr(name) }.unwrap_or_default();

            // SAFETY: live vector handle.
            let path = if i < unsafe { (sym!(sdk, StrVector_size))(paths.ptr) } {
                // SAFETY: bounds just checked.
                let p = unsafe { (sym!(sdk, StrVector_getitem))(paths.ptr, i as i32) };
                // SAFETY: `p` is null or a NUL-terminated string from the SDK.
                unsafe { runtime::from_cstr(p) }.unwrap_or_default()
            } else {
                String::new()
            };
            runtime::check()?;
            out.push(PartitionInfo { name, path });
        }
        Ok(out)
    }

    /// Global variables in the default download file.
    pub fn variables(&mut self) -> Result<Vec<Variable>, Error> {
        self.variables_in(Partition::Default)
    }

    /// Functions in the default download file.
    pub fn functions(&mut self) -> Result<Vec<Function>, Error> {
        self.functions_in(Partition::Default)
    }

    /// Compilation modules in the default download file.
    pub fn modules(&mut self) -> Result<Vec<Module>, Error> {
        self.modules_in(Partition::Default)
    }

    /// Every global variable in the target image.
    ///
    /// This is a whole-image enumeration and can be large; it is the way to
    /// discover names when you do not already know them.
    pub fn variables_in(&mut self, partition: Partition) -> Result<Vec<Variable>, Error> {
        let sdk = self.sdk();

        // SAFETY: creates an empty VariableVector we own.
        let v_ptr = unsafe { (sym!(sdk, new_VariableVector__SWIG_0))() };
        // SAFETY: `v_ptr` is a fresh VariableVector.
        let vec = unsafe { Owned::adopt(self.inner.winidea, v_ptr, sdk.delete_VariableVector)? };

        // SAFETY: live handles; the SDK fills the vector. Scope 0 = default.
        unsafe {
            (sym!(sdk, CDataController_getVariables))(self.inner.ptr, partition.to_raw(), vec.ptr)
        };
        runtime::check()?;

        // SAFETY: live vector handle.
        let n = unsafe { (sym!(sdk, VariableVector_size))(vec.ptr) };
        runtime::check()?;

        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            // SAFETY: 0 <= i < size. The element is BORROWED from the vector,
            // so it must not be freed.
            let e = unsafe { (sym!(sdk, VariableVector_getitem))(vec.ptr, i as i32) };
            runtime::check()?;
            if e.is_null() {
                continue;
            }
            // SAFETY: `e` is a live CVariable owned by `vec`.
            let v = unsafe {
                Variable {
                    name: runtime::from_cstr((sym!(sdk, CVariable_getName))(e)).unwrap_or_default(),
                    qualified_name: runtime::from_cstr((sym!(sdk, CVariable_getQualifiedName))(e))
                        .unwrap_or_default(),
                    type_name: runtime::from_cstr((sym!(sdk, CVariable_getType))(e))
                        .unwrap_or_default(),
                    scope: runtime::from_cstr((sym!(sdk, CVariable_getScope))(e))
                        .unwrap_or_default(),
                    size: (sym!(sdk, CVariable_getNumBytes))(e),
                    array_dimension: (sym!(sdk, CVariable_getArrayDimension))(e),
                }
            };
            runtime::check()?;
            out.push(v);
        }
        Ok(out)
    }

    /// Every function in the target image.
    pub fn functions_in(&mut self, partition: Partition) -> Result<Vec<Function>, Error> {
        let sdk = self.sdk();

        // SAFETY: creates an empty FunctionVector we own.
        let v_ptr = unsafe { (sym!(sdk, new_FunctionVector__SWIG_0))() };
        // SAFETY: `v_ptr` is a fresh FunctionVector.
        let vec = unsafe { Owned::adopt(self.inner.winidea, v_ptr, sdk.delete_FunctionVector)? };

        // SAFETY: live handles; the SDK fills the vector.
        unsafe {
            (sym!(sdk, CDataController_getFunctions))(self.inner.ptr, partition.to_raw(), vec.ptr)
        };
        runtime::check()?;

        // SAFETY: live vector handle.
        let n = unsafe { (sym!(sdk, FunctionVector_size))(vec.ptr) };
        runtime::check()?;

        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            // SAFETY: 0 <= i < size; borrowed from the vector.
            let e = unsafe { (sym!(sdk, FunctionVector_getitem))(vec.ptr, i as i32) };
            runtime::check()?;
            if e.is_null() {
                continue;
            }
            // SAFETY: `e` is a live CFunction owned by `vec`.
            let f = unsafe {
                Function {
                    name: runtime::from_cstr((sym!(sdk, CFunction_getName))(e)).unwrap_or_default(),
                    qualified_name: runtime::from_cstr((sym!(sdk, CFunction_getQualifiedName))(e))
                        .unwrap_or_default(),
                    return_type: runtime::from_cstr((sym!(sdk, CFunction_getReturnType))(e))
                        .unwrap_or_default(),
                    scope: runtime::from_cstr((sym!(sdk, CFunction_getScope))(e))
                        .unwrap_or_default(),
                    module_index: (sym!(sdk, CFunction_getModuleIndex))(e),
                }
            };
            runtime::check()?;
            out.push(f);
        }
        Ok(out)
    }

    /// Every compilation module (source file) in the target image.
    pub fn modules_in(&mut self, partition: Partition) -> Result<Vec<Module>, Error> {
        let sdk = self.sdk();

        // SAFETY: creates an empty ModuleVector we own.
        let v_ptr = unsafe { (sym!(sdk, new_ModuleVector__SWIG_0))() };
        // SAFETY: `v_ptr` is a fresh ModuleVector.
        let vec = unsafe { Owned::adopt(self.inner.winidea, v_ptr, sdk.delete_ModuleVector)? };

        // SAFETY: live handles; the SDK fills the vector.
        unsafe {
            (sym!(sdk, CDataController_getModules))(self.inner.ptr, partition.to_raw(), vec.ptr)
        };
        runtime::check()?;

        // SAFETY: live vector handle.
        let n = unsafe { (sym!(sdk, ModuleVector_size))(vec.ptr) };
        runtime::check()?;

        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            // SAFETY: 0 <= i < size; borrowed from the vector.
            let e = unsafe { (sym!(sdk, ModuleVector_getitem))(vec.ptr, i as i32) };
            runtime::check()?;
            if e.is_null() {
                continue;
            }
            // SAFETY: `e` is a live CModule owned by `vec`.
            let m = unsafe {
                Module {
                    name: runtime::from_cstr((sym!(sdk, CModule_getName))(e)).unwrap_or_default(),
                    path: runtime::from_cstr((sym!(sdk, CModule_getPath))(e)).unwrap_or_default(),
                }
            };
            runtime::check()?;
            out.push(m);
        }
        Ok(out)
    }

    /// Default byte order of the connected target.
    pub fn default_endian(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let e = unsafe { (sym!(self.sdk(), CDataController_getDefaultEndian))(self.inner.ptr) };
        runtime::check()?;
        Ok(e)
    }
}

/// Expression type inspection: unfolding structs, unions and arrays.
///
/// [`DataController::variables`] lists a target's globals, but a global that is
/// a struct or an array is a single entry there — you cannot acquire it, only
/// its scalar leaves. This controller walks *into* such a variable, one level at
/// a time, which is what lets a picker expand `cfg` into `cfg.tuning.gain`.
///
/// ```no_run
/// use winidea_sdk::{WinIdea, ConnectionMgr, data::ExpressionController};
///
/// # fn main() -> Result<(), winidea_sdk::Error> {
/// # let winidea = WinIdea::load(&winidea_sdk::default_search_paths())?;
/// # let mut conn = ConnectionMgr::new(&winidea)?;
/// # conn.connect_mru("C:/ws/MyProject.xjrf")?;
/// let mut expr = ExpressionController::new(&winidea, &conn)?;
///
/// // One level down. Recurse on any child where `is_compound`.
/// for child in expr.children("gConfig")? {
///     println!("{} : {} ({} bytes)", child.expression, child.type_name, child.size);
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Ownership
///
/// `CDataController2` hands back objects that are **not** freed by a plain
/// `delete_*`: they are released through `CDataController2::release`, which
/// needs the controller itself. They therefore cannot use [`Owned`], and are
/// released explicitly instead.
pub struct ExpressionController<'a> {
    inner: Owned<'a>,
}

impl<'a> ExpressionController<'a> {
    /// Create an expression controller bound to an existing connection.
    pub fn new(winidea: &'a WinIdea, conn: &'a ConnectionMgr<'a>) -> Result<Self, Error> {
        let sdk = winidea.raw();
        // SAFETY: `conn` outlives this controller, so its handle stays valid.
        let ptr = unsafe { (sym!(sdk, new_CDataController2))(conn.as_ptr()) };
        // SAFETY: `ptr` is a fresh CDataController2.
        let inner = unsafe { Owned::adopt(winidea, ptr, sdk.delete_CDataController2)? };
        Ok(Self { inner })
    }

    fn sdk(&self) -> &crate::generated::ffi::Sdk {
        self.inner.sdk()
    }

    /// Direct children of an expression: struct/union fields, or array elements.
    ///
    /// Returns an empty vector for a scalar — that is the signal to stop
    /// recursing, and is not an error.
    ///
    /// `expression` is anything winIDEA can evaluate: a global's name, a
    /// qualified name, or a path this function previously produced.
    ///
    /// # Arrays vs structs
    ///
    /// The SDK treats these differently, and the difference is not documented.
    /// `getExpressionType` **never enumerates array elements**: given
    /// `buf` of type `T [512]` it reports the array's own type with *zero*
    /// children. Only an already-indexed expression (`buf[0]`) resolves to the
    /// element type and exposes that element's fields.
    ///
    /// So an array's children are **synthesized here** from its dimension,
    /// while a struct's come from the SDK. Verified live against winIDEA
    /// 9.21.417 (`examples/unfold_probe.rs`), which also established that the
    /// `flags` argument makes no difference to any of this — every value from
    /// `0` to `0xffff_ffff` behaves identically.
    pub fn children(&mut self, expression: &str) -> Result<Vec<TypeNode>, Error> {
        // As in `has_children`: clear any inherited exception first, or an
        // unrelated earlier failure surfaces as "this struct cannot be expanded".
        let _ = runtime::check();

        let ty = self.expression_type(expression)?;

        // Every path from here must release `ty`.
        let result = self.children_of(ty, expression);

        // SAFETY: `ty` is a live IExpressionType minted by this controller;
        // release__SWIG_1 is its matching destructor (confirmed from the C#
        // IL: `release(IExpressionType)` dispatches to overload 1).
        unsafe { (sym!(self.sdk(), CDataController2_release__SWIG_1))(self.inner.ptr, ty) };
        // A release failure must not mask the real result.
        let _ = runtime::check();

        result
    }

    /// Resolve an expression to an `IExpressionType` the caller must release.
    fn expression_type(&self, expression: &str) -> Result<*mut std::ffi::c_void, Error> {
        let c = runtime::to_cstring(expression)?;
        // The `flags` argument is inert for this call (proven by sweeping every
        // plausible value against real hardware), so 0 is as good as anything.
        // SAFETY: live handle; the SDK copies the string during the call.
        let ty = unsafe {
            (sym!(self.sdk(), CDataController2_getExpressionType))(self.inner.ptr, 0, c.as_ptr())
        };
        runtime::check()?;
        if ty.is_null() {
            return Err(Error::NullHandle);
        }
        Ok(ty)
    }

    /// Children of a resolved type: array elements, or struct fields.
    fn children_of(&self, ty: *mut std::ffi::c_void, parent: &str) -> Result<Vec<TypeNode>, Error> {
        let sdk = self.sdk();

        // The node's own IVariable says whether this is an array, and how long.
        // Borrowed from `ty` (cMemoryOwn=false), so it must not be released.
        // SAFETY: `ty` is a live IExpressionType.
        let self_var = unsafe { (sym!(sdk, IExpressionType_Expression))(ty) };
        runtime::check()?;

        if !self_var.is_null() {
            // SAFETY: `self_var` is a live IVariable borrowed from `ty`.
            let dim = unsafe { (sym!(sdk, IVariable_ArrayDimension))(self_var) };
            runtime::check()?;
            if dim > 0 {
                // SAFETY: as above.
                let (type_name, address, mem_area, total_size) = unsafe {
                    (
                        runtime::from_cstr((sym!(sdk, IVariable_TypeName))(self_var))
                            .unwrap_or_default(),
                        (sym!(sdk, IVariable_Address))(self_var),
                        (sym!(sdk, IVariable_MemArea))(self_var),
                        (sym!(sdk, IVariable_NumBytes))(self_var),
                    )
                };
                runtime::check()?;
                let mut elements = Self::synth_array_elements(
                    parent, dim, &type_name, address, mem_area, total_size,
                );
                // Every element shares one type, so a single probe of `[0]`
                // settles whether they are all compound. This matters: a
                // typedef'd struct (`ChannelType`, no `struct ` prefix) is
                // invisible to the name heuristic, and wrongly calling it a
                // scalar would offer a 12-byte struct as a DAQ item.
                if let Some(first) = elements.first()
                    && let Ok(compound) = self.has_children(&first.expression)
                {
                    for e in &mut elements {
                        e.is_compound = compound;
                    }
                }
                return Ok(elements);
            }
        }

        self.struct_fields(ty, parent)
    }

    /// Synthesize an array's elements.
    ///
    /// The SDK will not list these, so they are built from the dimension. Each
    /// element's address is interpolated from the array's base and its stride;
    /// whether an element is itself compound is left to the caller's expand,
    /// which resolves `parent[i]` for real.
    fn synth_array_elements(
        parent: &str,
        dim: u32,
        type_name: &str,
        base_address: u64,
        mem_area: u8,
        total_size: u32,
    ) -> Vec<TypeNode> {
        // `uint32 [8]` -> element type `uint32`.
        let elem_type = match type_name.find('[') {
            Some(i) => type_name[..i].trim().to_string(),
            None => type_name.to_string(),
        };
        let stride = if dim > 0 { total_size / dim } else { 0 };

        (0..dim)
            .map(|i| {
                let expression = format!("{parent}[{i}]");
                TypeNode {
                    name: format!("[{i}]"),
                    expression,
                    type_name: elem_type.clone(),
                    address: base_address + (i as u64) * (stride as u64),
                    mem_area,
                    size: stride,
                    array_dimension: 0,
                    // An element of an array of structs is compound, but the
                    // element type name alone cannot say so reliably — the
                    // picker finds out by expanding it.
                    is_compound: is_compound_type(&elem_type),
                }
            })
            .collect()
    }

    /// Whether an expression has any children — i.e. is compound.
    ///
    /// The authoritative test, as against the [`is_compound_type`] name
    /// heuristic: a typedef'd struct carries no `struct ` prefix in its type
    /// name, so only the SDK can say. Costs one round trip, so callers use it
    /// where a wrong answer would matter (offering a struct as a DAQ item)
    /// rather than for every node.
    pub fn has_children(&self, expression: &str) -> Result<bool, Error> {
        // Drain any exception left behind by an EARLIER call before starting.
        //
        // `runtime::PENDING` is process-global and `check()` both reads and
        // clears it, so an error nobody consumed is inherited by whoever checks
        // next. That matters here because callers probe many expressions in a
        // row on one controller, and on a real image plenty of them legitimately
        // fail to evaluate (`ICONNECT_E_BAD_ID` for a symbol not live at the
        // current PC). Without this, one such failure makes the *next*
        // expression report an error too — so a genuine struct is dropped from
        // the batch depending only on what happened to precede it, which
        // presents as compound/scalar flapping between scroll positions.
        let _ = runtime::check();

        let ty = self.expression_type(expression)?;

        let sdk = self.sdk();
        // SAFETY: `ty` is a live IExpressionType; the vector is borrowed.
        let vec = unsafe { (sym!(sdk, IExpressionType_Children))(ty) };
        let n = if runtime::check().is_ok() && !vec.is_null() {
            // SAFETY: live IVectorVariables handle.
            let n = unsafe { (sym!(sdk, IVectorVariables_size))(vec) };
            let _ = runtime::check();
            n
        } else {
            0
        };

        // SAFETY: `ty` came from this controller; release it exactly once.
        unsafe { (sym!(sdk, CDataController2_release__SWIG_1))(self.inner.ptr, ty) };
        let _ = runtime::check();

        Ok(n > 0)
    }

    /// Named fields of a struct or union, as the SDK reports them.
    ///
    /// Only meaningful for a non-array type: see [`ExpressionController::children`]
    /// for why arrays never reach here.
    fn struct_fields(
        &self,
        ty: *mut std::ffi::c_void,
        parent: &str,
    ) -> Result<Vec<TypeNode>, Error> {
        let sdk = self.sdk();

        // BORROWED from `ty` (the C# wrapper constructs it with
        // cMemoryOwn=false), so it must not be released.
        // SAFETY: `ty` is a live IExpressionType.
        let vec = unsafe { (sym!(sdk, IExpressionType_Children))(ty) };
        if runtime::check().is_err() || vec.is_null() {
            return Ok(Vec::new());
        }

        // SAFETY: live IVectorVariables handle.
        let n = unsafe { (sym!(sdk, IVectorVariables_size))(vec) };
        if runtime::check().is_err() {
            return Ok(Vec::new());
        }

        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            // SAFETY: 0 <= i < size; borrowed from the vector.
            let e = unsafe { (sym!(sdk, IVectorVariables_at))(vec, i) };
            // Deliberately NOT `?`. A per-field failure must skip that field, not
            // abort the struct: returning `Err` here discards every field already
            // collected, so one awkward member makes an otherwise fine struct
            // look unexpandable. Combined with the nested `has_children` below —
            // which can leave its own exception in the process-global latch —
            // that turned into "the twisty appears but expanding yields nothing".
            if runtime::check().is_err() || e.is_null() {
                continue;
            }

            // SAFETY: `e` is a live IVariable owned by `vec`.
            let (name, type_name, address, mem_area, size, dim) = unsafe {
                (
                    runtime::from_cstr((sym!(sdk, IVariable_Name))(e)).unwrap_or_default(),
                    runtime::from_cstr((sym!(sdk, IVariable_TypeName))(e)).unwrap_or_default(),
                    (sym!(sdk, IVariable_Address))(e),
                    (sym!(sdk, IVariable_MemArea))(e),
                    (sym!(sdk, IVariable_NumBytes))(e),
                    (sym!(sdk, IVariable_ArrayDimension))(e),
                )
            };
            // As above: a field whose accessors raised is skipped, not fatal. The
            // values already read are still used when the check passes.
            if runtime::check().is_err() {
                continue;
            }

            // The SDK reports only the field's own name, so the path the caller
            // needs is assembled here. An unnamed field (the SDK does emit
            // these for some anonymous members) cannot be addressed by name, so
            // it is skipped rather than producing a bogus `parent.` expression.
            if name.is_empty() {
                continue;
            }
            let expression = format!("{parent}.{name}");

            // A field is compound when it is itself an array, or has fields of
            // its own. The name heuristic misses typedef'd structs, so it is
            // only a fast path: anything it calls scalar is confirmed with the
            // SDK, because wrongly calling a struct scalar would offer it as a
            // DAQ item and acquire garbage.
            let is_compound = dim > 0
                || is_compound_type(&type_name)
                || self.has_children(&expression).unwrap_or(false);

            out.push(TypeNode {
                name,
                expression,
                type_name,
                address,
                mem_area,
                size,
                array_dimension: dim,
                is_compound,
            });
        }
        Ok(out)
    }
}

/// Whether a type name denotes a struct, union or array.
///
/// A heuristic on the *name*, used only to decide whether a picker offers an
/// expand twisty. Getting it wrong is recoverable: expanding a scalar returns
/// no children, and a compound wrongly treated as scalar still refuses to
/// acquire. The authoritative answer is whether `children()` returns anything.
fn is_compound_type(type_name: &str) -> bool {
    let t = type_name.trim();
    t.starts_with("struct ")
        || t.starts_with("union ")
        || t.starts_with("class ")
        || t.ends_with(']')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_block_readability() {
        let all_ok = MemoryBlock {
            data: vec![1, 2, 3, 4],
            readable: vec![true; 4],
            bytes_per_mau: 1,
        };
        assert!(all_ok.is_fully_readable());
        assert_eq!(all_ok.num_maus(), 4);

        // A partly-unreadable block is a normal outcome, not an error, so it
        // must be representable rather than collapsing to a failure.
        let partial = MemoryBlock {
            data: vec![1, 2, 3, 4],
            readable: vec![true, true, false, true],
            bytes_per_mau: 1,
        };
        assert!(!partial.is_fully_readable());
        assert_eq!(partial.num_maus(), 4);
    }

    #[test]
    fn compound_type_detection() {
        assert!(is_compound_type("struct Config"));
        assert!(is_compound_type("union Payload"));
        assert!(is_compound_type("uint8 [16]"));
        // Whitespace from the SDK must not defeat the prefix check.
        assert!(is_compound_type("  struct Config "));

        assert!(!is_compound_type("uint32"));
        assert!(!is_compound_type("float"));
        // A pointer is a scalar: it is one acquirable machine word, even though
        // what it points at is not.
        assert!(!is_compound_type("uint8 *"));
    }

    /// The SDK does not enumerate array elements, so they are synthesized. The
    /// element expressions must be ones the SDK accepts back, and the addresses
    /// must step by the true stride — an off-by-one here would DAQ-acquire the
    /// wrong memory, silently.
    #[test]
    fn array_elements_are_synthesized_with_stride() {
        // Stands in for a `uint32 [8]` at 0x2000, 32 bytes total.
        let winidea_free = ExpressionController::synth_array_elements(
            "g_revisions",
            8,
            "uint32 [8]",
            0x2000,
            0,
            32,
        );
        assert_eq!(winidea_free.len(), 8);
        assert_eq!(winidea_free[0].expression, "g_revisions[0]");
        assert_eq!(winidea_free[7].expression, "g_revisions[7]");
        assert_eq!(winidea_free[0].name, "[0]");
        // Element type drops the dimension suffix.
        assert_eq!(winidea_free[0].type_name, "uint32");
        // 32 bytes / 8 elements = 4-byte stride.
        assert_eq!(winidea_free[0].address, 0x2000);
        assert_eq!(winidea_free[1].address, 0x2004);
        assert_eq!(winidea_free[7].address, 0x201c);
        assert_eq!(winidea_free[0].size, 4);
        // A scalar element is directly acquirable; that is the whole point.
        assert!(winidea_free[0].is_acquirable());

        // An array of structs: the stride and expressions are what matter here.
        // `is_compound` from synthesis alone is only provisional — `children()`
        // overwrites it by probing element [0] against the SDK, because a
        // typedef'd element type (`ChannelType [4]`, no `struct ` prefix) is
        // invisible to the name heuristic.
        let structs = ExpressionController::synth_array_elements(
            "channels",
            4,
            "struct ChannelType [4]",
            0x3000,
            0,
            48,
        );
        assert_eq!(structs.len(), 4);
        assert_eq!(structs[2].expression, "channels[2]");
        assert_eq!(structs[2].address, 0x3000 + 2 * 12);
        assert_eq!(structs[2].size, 12);
        assert!(structs[2].is_compound);
        assert!(!structs[2].is_acquirable());
    }

    /// Only scalars with a real size can be a DAQ item: DAQ samples one memory
    /// location, so offering to acquire a struct would configure garbage.
    #[test]
    fn only_sized_scalars_are_acquirable() {
        let node = |type_name: &str, size, dim, compound| TypeNode {
            name: "x".into(),
            expression: "a.x".into(),
            type_name: type_name.into(),
            address: 0x2000,
            mem_area: 0,
            size,
            array_dimension: dim,
            is_compound: compound,
        };
        assert!(node("uint32", 4, 0, false).is_acquirable());
        assert!(!node("struct Cfg", 32, 0, true).is_acquirable());
        assert!(!node("uint8 [4]", 4, 4, true).is_acquirable());
        // A zero-size scalar has no location worth sampling.
        assert!(!node("void", 0, 0, false).is_acquirable());
    }

    /// A 16-bit MAU means data is twice as long as the status array; the two
    /// must not be conflated.
    #[test]
    fn wide_mau_lengths() {
        let block = MemoryBlock {
            data: vec![0; 8],
            readable: vec![true; 4],
            bytes_per_mau: 2,
        };
        assert_eq!(block.num_maus(), 4);
        assert_eq!(
            block.data.len(),
            block.num_maus() * block.bytes_per_mau as usize
        );
    }
}

impl<'a> DataController<'a> {
    /// Wraps a `DataController` handle that did not come from a [`ConnectionMgr`].
    ///
    /// For a handle obtained from an SDK call this crate does not wrap (via
    /// [`crate::WinIdea::raw`]).
    ///
    /// # Safety
    ///
    /// `ptr` must be a live `DataController`. `free` must be the matching destructor
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

impl<'a> DataController<'a> {
    pub(crate) fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.inner.ptr
    }
}
