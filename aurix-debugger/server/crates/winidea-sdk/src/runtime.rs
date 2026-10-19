//! Hand-written runtime support around the generated FFI layer.
//!
//! Three things the generator cannot produce:
//!
//! 1. **DLL discovery** -- where `isystemConnect.dll` lives on this machine.
//! 2. **The SWIG callbacks.** The native library never throws across the ABI and
//!    never allocates the strings it returns; instead it calls back into the
//!    host. We must register those callbacks before any other call, or the first
//!    string return / first error will dereference a null function pointer.
//! 3. **Error propagation** -- turning a pending exception into a `Result`.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use libloading::Library;

use crate::generated::ffi::Sdk;

/// Errors from loading or calling the winIDEA SDK.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("isystemConnect.dll not found (looked in: {0})")]
    DllNotFound(String),

    #[error("failed to load {path}: {source}")]
    Load {
        path: PathBuf,
        #[source]
        source: libloading::Error,
    },

    /// An exception raised inside the SDK, reported through SWIG's callback.
    #[error("{kind}: {message}")]
    Sdk {
        kind: ExceptionKind,
        message: String,
    },

    #[error("could not start the loader thread: {0}")]
    LoaderThread(std::io::Error),

    /// A call needed an export this DLL does not provide.
    ///
    /// The SDK's export set differs between winIDEA versions, so the loader
    /// binds absent exports as `None` rather than failing outright. Reaching
    /// one means this particular feature needs a newer (or older) winIDEA than
    /// the DLL in use -- everything not touching it keeps working.
    #[error(
        "this winIDEA SDK build does not export `{0}` -- \
         the loaded isystemConnect.dll is a different version than this feature needs"
    )]
    MissingSymbol(&'static str),

    #[error("string returned by the SDK was not valid UTF-8")]
    InvalidUtf8,

    /// The host and the SDK must allocate and free on the same C runtime heap.
    #[error(
        "could not bind the C runtime `malloc` that the SDK frees its strings \
         with (tried: {0}) -- without it every string the SDK returns would be \
         allocated on one heap and freed on another"
    )]
    HostAllocator(String),

    #[error("a null handle was passed to the SDK")]
    NullHandle,
}

/// Which SWIG exception category the native side raised.
///
/// SWIG registers one callback per .NET exception type; we keep the same
/// granularity so callers can match on the kind rather than parse the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionKind {
    Application,
    Arithmetic,
    DivideByZero,
    IndexOutOfRange,
    InvalidCast,
    InvalidOperation,
    Io,
    NullReference,
    OutOfMemory,
    Overflow,
    System,
    Argument,
    ArgumentNull,
    ArgumentOutOfRange,
}

impl std::fmt::Display for ExceptionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Application => "application error",
            Self::Arithmetic => "arithmetic error",
            Self::DivideByZero => "divide by zero",
            Self::IndexOutOfRange => "index out of range",
            Self::InvalidCast => "invalid cast",
            Self::InvalidOperation => "invalid operation",
            Self::Io => "I/O error",
            Self::NullReference => "null reference",
            Self::OutOfMemory => "out of memory",
            Self::Overflow => "overflow",
            Self::System => "system error",
            Self::Argument => "invalid argument",
            Self::ArgumentNull => "null argument",
            Self::ArgumentOutOfRange => "argument out of range",
        };
        f.write_str(s)
    }
}

/// The pending exception, set by a callback and drained by [`check`].
///
/// SWIG's model is strictly "raise, then the host checks after the call
/// returns", so at most one can be outstanding per thread of execution. We use
/// a global rather than a thread-local because the DLL may invoke the callback
/// from one of its own worker threads.
static PENDING: Mutex<Option<(ExceptionKind, String)>> = Mutex::new(None);

fn raise(kind: ExceptionKind, msg: *const c_char) {
    let message = if msg.is_null() {
        String::new()
    } else {
        // SAFETY: SWIG passes a NUL-terminated string that stays valid for the
        // duration of the callback; we copy it immediately.
        unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    };
    if let Ok(mut slot) = PENDING.lock() {
        *slot = Some((kind, message));
    }
}

/// Take and clear the pending exception, if any.
pub(crate) fn check() -> Result<(), Error> {
    let taken = PENDING.lock().ok().and_then(|mut s| s.take());
    match taken {
        Some((kind, message)) => Err(Error::Sdk { kind, message }),
        None => Ok(()),
    }
}

macro_rules! exception_callback {
    ($name:ident, $kind:expr) => {
        unsafe extern "C" fn $name(msg: *const c_char) {
            raise($kind, msg);
        }
    };
}

exception_callback!(cb_application, ExceptionKind::Application);
exception_callback!(cb_arithmetic, ExceptionKind::Arithmetic);
exception_callback!(cb_divide_by_zero, ExceptionKind::DivideByZero);
exception_callback!(cb_index_out_of_range, ExceptionKind::IndexOutOfRange);
exception_callback!(cb_invalid_cast, ExceptionKind::InvalidCast);
exception_callback!(cb_invalid_operation, ExceptionKind::InvalidOperation);
exception_callback!(cb_io, ExceptionKind::Io);
exception_callback!(cb_null_reference, ExceptionKind::NullReference);
exception_callback!(cb_out_of_memory, ExceptionKind::OutOfMemory);
exception_callback!(cb_overflow, ExceptionKind::Overflow);
exception_callback!(cb_system, ExceptionKind::System);

/// The `...Argument...` family takes an extra parameter name argument.
macro_rules! argument_callback {
    ($name:ident, $kind:expr) => {
        unsafe extern "C" fn $name(msg: *const c_char, param: *const c_char) {
            let p = if param.is_null() {
                String::new()
            } else {
                // SAFETY: as in `raise` -- valid for the callback's duration.
                unsafe { CStr::from_ptr(param) }
                    .to_string_lossy()
                    .into_owned()
            };
            let m = if msg.is_null() {
                String::new()
            } else {
                // SAFETY: as above.
                unsafe { CStr::from_ptr(msg) }
                    .to_string_lossy()
                    .into_owned()
            };
            let combined = if p.is_empty() {
                m
            } else {
                format!("{m} (parameter `{p}`)")
            };
            if let Ok(mut slot) = PENDING.lock() {
                *slot = Some(($kind, combined));
            }
        }
    };
}

argument_callback!(cb_argument, ExceptionKind::Argument);
argument_callback!(cb_argument_null, ExceptionKind::ArgumentNull);
argument_callback!(cb_argument_out_of_range, ExceptionKind::ArgumentOutOfRange);

/// SWIG asks the *host* to allocate every string it hands back.
///
/// The contract (from SWIG's `csharp.swg`): we return a buffer the native side
/// takes ownership of and frees with `free()`. We therefore allocate with
/// `malloc`, NOT with Rust's allocator -- freeing a Rust allocation with
/// `free()` is undefined behaviour.
///
/// It must also be the *same C runtime's* `malloc` -- see [`init_host_allocator`].
unsafe extern "C" fn cb_string(s: *const c_char) -> *mut c_char {
    if s.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: SWIG passes a valid NUL-terminated string.
    let len = unsafe { CStr::from_ptr(s) }.to_bytes_with_nul().len();
    let Some(malloc) = host_malloc() else {
        // Never reached: `load_from` binds the allocator before registering the
        // callbacks, so a string can only come back after that succeeded.
        return std::ptr::null_mut();
    };
    // SAFETY: malloc returns either null or a `len`-byte allocation.
    let buf = unsafe { malloc(len) } as *mut c_char;
    if buf.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: both pointers are valid for `len` bytes and do not overlap.
    unsafe { std::ptr::copy_nonoverlapping(s, buf, len) };
    buf
}

type MallocFn = unsafe extern "C" fn(usize) -> *mut std::ffi::c_void;

/// Keeps the CRT module alive for as long as the callback can fire.
static HOST_CRT: OnceLock<Library> = OnceLock::new();
/// `malloc` of the CRT the SDK frees with, as an address (fn pointers are not
/// `const`-constructible in a static, and this is only ever set once).
static HOST_MALLOC: AtomicUsize = AtomicUsize::new(0);

fn host_malloc() -> Option<MallocFn> {
    match HOST_MALLOC.load(Ordering::Acquire) {
        0 => None,
        // SAFETY: only ever stored from a `Symbol<MallocFn>` resolved below.
        addr => Some(unsafe { std::mem::transmute::<usize, MallocFn>(addr) }),
    }
}

/// Binds the `malloc` that pairs with the `free()` the SDK calls on our strings.
///
/// These must come from the same C runtime, and by default they do not.
/// `isystemConnect.dll` imports `api-ms-win-crt-heap-l1-1-0.dll` (the UCRT),
/// while this crate built for `x86_64-pc-windows-gnu` with
/// `-C target-feature=+crt-static` binds a plain `malloc` to **`msvcrt.dll`** --
/// verified with `objdump -p`, which shows `malloc` imported from `msvcrt.dll`
/// in the executable and the UCRT heap API set in the DLL. Allocating on one
/// heap and freeing on the other is undefined behaviour: it does not fail
/// loudly, it corrupts quietly, which is why this is bound explicitly rather
/// than left to the linker.
///
/// Called once from [`WinIdea::load_from`] *before* the callbacks are
/// registered, so no string can come back while the slot is still empty.
fn init_host_allocator() -> Result<(), Error> {
    if HOST_MALLOC.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    // The DLL imports the heap API set, which forwards to `ucrtbase.dll`;
    // either name reaches the same heap. Prefer the concrete module.
    for name in HOST_CRT_NAMES {
        // SAFETY: a system C runtime, loaded by name; it runs no user code.
        let Ok(lib) = (unsafe { Library::new(name) }) else {
            continue;
        };
        let addr = {
            // SAFETY: `malloc` has the C signature `MallocFn` everywhere.
            let Ok(sym) = (unsafe { lib.get::<MallocFn>(b"malloc\0") }) else {
                continue;
            };
            *sym as usize
        };
        let _ = HOST_CRT.set(lib);
        HOST_MALLOC.store(addr, Ordering::Release);
        return Ok(());
    }
    Err(Error::HostAllocator(HOST_CRT_NAMES.join(", ")))
}

#[cfg(windows)]
const HOST_CRT_NAMES: &[&str] = &["ucrtbase.dll", "api-ms-win-crt-heap-l1-1-0.dll"];
#[cfg(not(windows))]
const HOST_CRT_NAMES: &[&str] = &["libc.so.6", "libc.so"];

/// Owns one SDK handle and releases it on drop.
///
/// Every SDK object is an opaque pointer minted by a `new_*`/getter export and
/// released by a matching `delete_*`. Pairing them here means handles can
/// neither leak nor be freed twice. Handles *borrowed* from a container are not
/// wrapped in this type -- they are owned by the container.
pub(crate) struct Owned<'a> {
    pub(crate) winidea: &'a WinIdea,
    pub(crate) ptr: *mut std::ffi::c_void,
    /// The destructor, or `None` if this DLL does not export it.
    ///
    /// `Option` because every export is optional (see [`crate::sym`]). In
    /// practice a handle can only exist if its *constructor* resolved, so a
    /// missing destructor is close to unreachable -- but leaking one handle is
    /// strictly better than unwrapping and aborting inside `Drop`.
    pub(crate) free: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
}

impl<'a> Owned<'a> {
    /// Adopt a handle returned by the SDK, mapping null to an error.
    ///
    /// # Safety
    /// `ptr` must be null or a live handle that `free` is the correct
    /// destructor for, and must not be owned by anything else.
    pub(crate) unsafe fn adopt(
        winidea: &'a WinIdea,
        ptr: *mut std::ffi::c_void,
        free: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    ) -> Result<Self, Error> {
        check()?;
        if ptr.is_null() {
            return Err(Error::NullHandle);
        }
        Ok(Self { winidea, ptr, free })
    }

    pub(crate) fn sdk(&self) -> &Sdk {
        &self.winidea.sdk
    }
}

impl Drop for Owned<'_> {
    fn drop(&mut self) {
        // A `None` destructor leaks the handle rather than aborting in `Drop` --
        // see the field's note; it means the DLL lacks an export whose
        // constructor it nonetheless provided.
        if let (false, Some(free)) = (self.ptr.is_null(), self.free) {
            // SAFETY: `ptr` came from the matching constructor and is freed
            // exactly once, here.
            unsafe { free(self.ptr) };
            // A destructor may itself report an error; discard it rather than
            // panic while unwinding.
            let _ = check();
        }
    }
}

/// A loaded winIDEA SDK: the DLL plus its resolved exports.
pub struct WinIdea {
    pub(crate) sdk: Box<Sdk>,
}

impl WinIdea {
    /// Exports the loaded DLL does not provide.
    ///
    /// Empty when the DLL matches the SDK these bindings were generated from
    /// (9.21.417). A non-empty list means a different winIDEA version is in use:
    /// the features touching those exports fail with [`Error::MissingSymbol`],
    /// everything else works. Worth logging once at load so version skew is
    /// visible up front rather than as a surprise mid-feature.
    pub fn missing(&self) -> &[&'static str] {
        self.sdk.missing()
    }

    /// Load `isystemConnect.dll`, searching the standard locations.
    ///
    /// Order: `AURIX_WINIDEA_DLL` env var, then each path in `extra`, then the
    /// OS loader's own search (`PATH`, app dir, ...).
    pub fn load(extra: &[PathBuf]) -> Result<Self, Error> {
        let mut tried = Vec::new();

        if let Some(p) = std::env::var_os("AURIX_WINIDEA_DLL") {
            let path = PathBuf::from(&p);
            tried.push(path.display().to_string());
            if path.is_file() {
                return Self::load_from(&p);
            }
        }

        for dir in extra {
            // Accept either the directory holding the DLL or the DLL itself:
            // the `winidea.sdkPath` setting is filled in by hand, and pointing
            // at the file is the likelier reading of "SDK path".
            let cand = if dir.is_file() {
                dir.clone()
            } else {
                dir.join(DLL_NAME)
            };
            tried.push(cand.display().to_string());
            if cand.is_file() {
                return Self::load_from(cand.as_os_str());
            }
        }

        // Fall back to the loader's search path.
        tried.push(format!("{DLL_NAME} (via PATH)"));
        match Self::load_from(OsStr::new(DLL_NAME)) {
            Ok(w) => Ok(w),
            Err(_) => Err(Error::DllNotFound(tried.join(", "))),
        }
    }

    /// Load from an exact path, then register the SWIG callbacks.
    pub fn load_from(path: &OsStr) -> Result<Self, Error> {
        init_host_allocator()?;
        let sdk = Self::load_symbols(path)?;
        let me = Self { sdk };
        me.register_callbacks()?;
        Ok(me)
    }

    /// Resolve all ~6k symbols on a thread with a generous stack.
    ///
    /// `Sdk::load` writes each symbol straight into a heap allocation, so the
    /// *struct* never lives on the stack. But it still declares one `Symbol`
    /// binding per export, and an unoptimised build gives each its own stack
    /// slot rather than reusing one -- roughly 6094 x 24 bytes, which blows the
    /// default 1 MB main-thread stack in debug builds. (Release builds collapse
    /// them and are fine.)
    ///
    /// Doing the work on a thread we size ourselves makes loading behave
    /// identically in debug and release, instead of only working when
    /// optimisations happen to be on.
    fn load_symbols(path: &OsStr) -> Result<Box<Sdk>, Error> {
        const STACK: usize = 16 * 1024 * 1024;

        let owned = path.to_os_string();
        let handle = std::thread::Builder::new()
            .name("winidea-sdk-load".into())
            .stack_size(STACK)
            .spawn(move || {
                // SAFETY: loading a DLL and calling into it is inherently
                // unsafe; the signatures come from the SDK's own P/Invoke
                // metadata.
                unsafe { Sdk::load(&owned) }.map_err(|source| Error::Load {
                    path: PathBuf::from(&owned),
                    source,
                })
            })
            .map_err(Error::LoaderThread)?;

        match handle.join() {
            Ok(r) => r,
            // The loader thread panicked -- almost certainly a stack overflow
            // that the guard page caught. Surface it rather than silently
            // returning "not found".
            Err(_) => Err(Error::DllNotFound(format!(
                "{} (symbol resolution panicked)",
                path.to_string_lossy()
            ))),
        }
    }

    /// Install SWIG's exception + string callbacks.
    ///
    /// Must run before any other call into the DLL: without it the native side
    /// calls through null function pointers the moment it needs to report an
    /// error or return a string.
    ///
    /// These three are the one part of the ABI that is NOT optional. Every other
    /// export may be absent on a different winIDEA version and is reported per
    /// feature, but without these no exception and no string can cross the
    /// boundary at all -- so a DLL lacking them is not usable as an SDK and is
    /// rejected at load rather than crashing on the first error.
    fn register_callbacks(&self) -> Result<(), Error> {
        // SAFETY: every callback below has the exact signature SWIG declares,
        // is `extern "C"`, and never unwinds (all of them only copy bytes).
        unsafe {
            sym!(self.sdk, SWIGRegisterExceptionCallbacks_isystemConnect)(
                cb_application,
                cb_arithmetic,
                cb_divide_by_zero,
                cb_index_out_of_range,
                cb_invalid_cast,
                cb_invalid_operation,
                cb_io,
                cb_null_reference,
                cb_out_of_memory,
                cb_overflow,
                cb_system,
            );
            sym!(
                self.sdk,
                SWIGRegisterExceptionArgumentCallbacks_isystemConnect
            )(cb_argument, cb_argument_null, cb_argument_out_of_range);
            sym!(self.sdk, SWIGRegisterStringCallback_isystemConnect)(cb_string);
        }
        Ok(())
    }
}

#[cfg(windows)]
const DLL_NAME: &str = "isystemConnect.dll";
#[cfg(not(windows))]
const DLL_NAME: &str = "libisystemConnect.so";

/// Convert a Rust string into a NUL-terminated buffer for the SDK.
pub(crate) fn to_cstring(s: &str) -> Result<CString, Error> {
    CString::new(s).map_err(|_| Error::InvalidUtf8)
}

/// Read a string returned by the SDK.
///
/// # Safety
/// `p` must be null or a valid NUL-terminated string owned by the SDK.
pub(crate) unsafe fn from_cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: guaranteed by the caller.
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// Best-effort guesses for where `isystemConnect.dll` lives.
///
/// Ordered most- to least-specific, so an explicit override always wins:
///
/// 1. `AURIX_WINIDEA_SDK_PATH` — the directory holding the DLL, set by
///    whoever launches the server.
/// 2. `WINIDEA_HOME` — a user-pointed winIDEA install.
/// 3. This crate's `lib/x64/`, resolved at compile time, where a developer
///    copies the DLL (see README.md). Makes `cargo run --example smoke` work
///    with no arguments and no install.
/// 4. A real winIDEA installation under Program Files.
/// 5. Next to the running executable, for a portable deployment.
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();

    // The explicit path wins, so a host running a different winIDEA version
    // can point at its own matching SDK, which is the only way to get full
    // coverage (see the ".ReadCompressed" note on `daq::AcquisitionMode`).
    if let Some(dir) = std::env::var_os("AURIX_WINIDEA_SDK_PATH") {
        v.push(PathBuf::from(dir));
    }

    if let Some(dir) = std::env::var_os("WINIDEA_HOME") {
        let home = PathBuf::from(dir);
        v.push(home.join("lib").join("x64"));
        v.push(home);
    }

    // The copy in this crate's lib/x64. `CARGO_MANIFEST_DIR` is baked in at
    // compile time, so this resolves for any binary built from this workspace
    // regardless of its working directory.
    v.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("lib")
            .join("x64"),
    );

    for key in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Some(base) = std::env::var_os(key) else {
            continue;
        };
        let base = Path::new(&base);
        for product in ["winIDEA", "winIDEA Open"] {
            let root = base.join("iSYSTEM").join(product);
            v.push(root.join("lib").join("x64"));
            v.push(root);
        }
        v.push(base.join("iSYSTEM"));
    }

    // Alongside the running executable, for a portable deployment that ships
    // the DLL next to the binary.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        v.push(dir.to_path_buf());
    }

    v
}

/// The name of the DLL this build looks for.
pub fn dll_name() -> OsString {
    OsString::from(DLL_NAME)
}

#[cfg(all(test, windows))]
mod host_allocator_tests {
    use super::*;

    /// The SDK frees our strings with the UCRT's `free()`, so [`cb_string`] must
    /// allocate with the UCRT's `malloc` -- not the one a plain `extern "C"`
    /// binding resolves to.
    ///
    /// This is not a theoretical concern on this toolchain. The crate is built
    /// `x86_64-pc-windows-gnu` with `-C target-feature=+crt-static`, where
    /// `objdump -p` shows the executable importing `malloc` from `msvcrt.dll`,
    /// while `isystemConnect.dll` imports `api-ms-win-crt-heap-l1-1-0.dll`.
    /// Those are two different heaps, and mixing them corrupts silently rather
    /// than failing, so the guarantee is pinned here instead of being assumed.
    #[test]
    fn host_malloc_is_the_ucrt_not_msvcrt() {
        init_host_allocator().expect("UCRT malloc must be bindable on Windows");
        let bound = host_malloc().expect("bound after init") as usize;

        let ucrt = unsafe { Library::new("ucrtbase.dll") }.expect("ucrtbase.dll loads");
        let ucrt_malloc = *unsafe { ucrt.get::<MallocFn>(b"malloc\0") }.expect("ucrtbase malloc");
        assert_eq!(bound, ucrt_malloc as usize, "must bind the UCRT allocator");

        // The trap this guards: msvcrt also exports `malloc`, at a different
        // address and on a different heap.
        let msvcrt = unsafe { Library::new("msvcrt.dll") }.expect("msvcrt.dll loads");
        let msvcrt_malloc = *unsafe { msvcrt.get::<MallocFn>(b"malloc\0") }.expect("msvcrt malloc");
        assert_ne!(
            bound, msvcrt_malloc as usize,
            "bound msvcrt's malloc -- its free() does not pair with the SDK's"
        );
    }

    /// A round trip through the real callback, at a size large enough that the
    /// allocator must go to the heap rather than hand back a cached block.
    #[test]
    fn cb_string_round_trips_a_large_string() {
        init_host_allocator().unwrap();
        let src = CString::new("w".repeat(64 * 1024)).unwrap();
        let out = unsafe { cb_string(src.as_ptr()) };
        assert!(!out.is_null());
        assert_eq!(unsafe { CStr::from_ptr(out) }.to_bytes().len(), 64 * 1024);

        // Free it the way the SDK does, through the same CRT.
        let ucrt = unsafe { Library::new("ucrtbase.dll") }.unwrap();
        let free = *unsafe { ucrt.get::<unsafe extern "C" fn(*mut std::ffi::c_void)>(b"free\0") }
            .expect("ucrtbase free");
        unsafe { free(out.cast()) };
    }
}
