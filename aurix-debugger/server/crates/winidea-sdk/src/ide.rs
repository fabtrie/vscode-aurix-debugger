//! Controlling the winIDEA application itself: windows, views, options.
//!
//! `CIDEController` is broader than its name suggests. It covers three things:
//!
//! * **Window control** — show, hide, move, raise the winIDEA window.
//! * **Views** — open/close the docked panes (Watch, Memory, Terminal, ...).
//! * **The option tree** — winIDEA's entire configuration, addressed by dotted
//!   paths like `Hardware.Emulator`. Find any option's path via winIDEA's
//!   *Help | Display Option…* menu.
//!
//! ```no_run
//! use winidea_sdk::{WinIdea, ConnectionMgr, ide::{IdeController, ViewType}};
//!
//! # fn main() -> Result<(), winidea_sdk::Error> {
//! let winidea = WinIdea::load(&winidea_sdk::default_search_paths())?;
//! let mut conn = ConnectionMgr::new(&winidea)?;
//! conn.connect_mru("C:/ws/MyProject.xjrf")?;
//!
//! let mut ide = IdeController::new(&winidea, &conn)?;
//! ide.bring_to_top()?;
//! ide.open_view(ViewType::Watch)?;
//! println!("emulator = {}", ide.option_str("Hardware.Emulator")?);
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeMap;

use crate::runtime::{self, Error, Owned};
use crate::{ConnectionMgr, WinIdea};

/// A dockable view in the winIDEA window.
///
/// Mirrors `CIDEController::ViewType`; values pinned by `tests/enum_values.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewType {
    Project,
    Symbols,
    Callstack,
    Locals,
    This,
    Disassembly,
    CoreRegisters,
    Terminal,
    Tasks,
    /// The SFR *list*.
    Sfrs,
    Output,
    Build,
    FindInFiles,
    Tools,
    Script,
    Progress,
    Watch,
    Memory,
    /// A single SFR view.
    Sfr,
    FrameworkPlugin,
    Document,
    SessionExplorer,
    /// The SDK's "no view" sentinel.
    None,
}

impl ViewType {
    fn to_raw(self) -> i32 {
        match self {
            Self::Project => 0,
            Self::Symbols => 1,
            Self::Callstack => 2,
            Self::Locals => 3,
            Self::This => 4,
            Self::Disassembly => 5,
            Self::CoreRegisters => 6,
            Self::Terminal => 7,
            Self::Tasks => 8,
            Self::Sfrs => 9,
            Self::Output => 10,
            Self::Build => 11,
            Self::FindInFiles => 12,
            Self::Tools => 13,
            Self::Script => 14,
            Self::Progress => 15,
            Self::Watch => 16,
            Self::Memory => 17,
            Self::Sfr => 18,
            Self::FrameworkPlugin => 19,
            Self::Document => 20,
            Self::SessionExplorer => 21,
            Self::None => 22,
        }
    }
}

/// Which winIDEA path to query with [`IdeController::path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathType {
    /// Directory containing `winIDEA.exe`.
    ExeDir,
    /// Directory containing the open workspace.
    WorkspaceDir,
    /// File name of the open workspace.
    WorkspaceFileName,
}

impl PathType {
    fn to_raw(self) -> i32 {
        match self {
            Self::ExeDir => 0,
            Self::WorkspaceDir => 1,
            Self::WorkspaceFileName => 2,
        }
    }
}

/// Controls the winIDEA application: window, views and options.
///
/// Borrows the [`ConnectionMgr`] it was built from.
pub struct IdeController<'a> {
    inner: Owned<'a>,
}

impl<'a> IdeController<'a> {
    /// Create an IDE controller bound to an existing connection.
    pub fn new(winidea: &'a WinIdea, conn: &'a ConnectionMgr<'a>) -> Result<Self, Error> {
        let sdk = winidea.raw();
        // SAFETY: `conn` outlives this controller, so its handle stays valid.
        let ptr = unsafe { (sym!(sdk, new_CIDEController))(conn.as_ptr()) };
        // SAFETY: `ptr` is a fresh CIDEController.
        let inner = unsafe { Owned::adopt(winidea, ptr, sdk.delete_CIDEController)? };
        Ok(Self { inner })
    }

    fn sdk(&self) -> &crate::generated::ffi::Sdk {
        self.inner.sdk()
    }

    // --- window -----------------------------------------------------------

    /// Raise the winIDEA window above other windows.
    pub fn bring_to_top(&mut self) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CIDEController_bringWinIDEAToTop))(self.inner.ptr) };
        runtime::check()
    }

    /// Minimise the winIDEA window.
    pub fn minimize(&mut self) -> Result<(), Error> {
        // SAFETY: live handle; the returned handle is a status object we do not
        // need, and the SDK does not transfer ownership of it.
        unsafe { (sym!(self.sdk(), CIDEController_minimize))(self.inner.ptr) };
        runtime::check()
    }

    /// Maximise the winIDEA window.
    pub fn maximize(&mut self) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CIDEController_maximize))(self.inner.ptr) };
        runtime::check()
    }

    /// Restore the window from minimised/maximised.
    pub fn restore(&mut self) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CIDEController_restore))(self.inner.ptr) };
        runtime::check()
    }

    /// Give the winIDEA window focus.
    pub fn activate(&mut self) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CIDEController_activate))(self.inner.ptr) };
        runtime::check()
    }

    /// Move and resize the winIDEA window, in screen pixels.
    pub fn move_window(&mut self, x: i32, y: i32, width: i32, height: i32) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CIDEController_move))(self.inner.ptr, x, y, width, height) };
        runtime::check()
    }

    /// Redraw the winIDEA UI.
    pub fn refresh_ui(&mut self) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CIDEController_refreshUI))(self.inner.ptr) };
        runtime::check()
    }

    // --- views ------------------------------------------------------------

    /// Open a docked view.
    pub fn open_view(&mut self, view: ViewType) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CIDEController_openView__SWIG_1))(self.inner.ptr, view.to_raw()) };
        runtime::check()
    }

    /// Open a named instance of a view, e.g. a specific Memory window.
    pub fn open_view_named(&mut self, view: ViewType, name: &str) -> Result<(), Error> {
        let n = runtime::to_cstring(name)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        unsafe {
            (sym!(self.sdk(), CIDEController_openView__SWIG_0))(self.inner.ptr, view.to_raw(), n.as_ptr())
        };
        runtime::check()
    }

    /// Close a docked view.
    pub fn close_view(&mut self, view: ViewType) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CIDEController_closeView__SWIG_1))(self.inner.ptr, view.to_raw()) };
        runtime::check()
    }

    /// Close every view.
    pub fn close_all_views(&mut self) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CIDEController_closeAllViews))(self.inner.ptr) };
        runtime::check()
    }

    /// Close every open document.
    pub fn close_all_documents(&mut self) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CIDEController_closeAllDocuments))(self.inner.ptr) };
        runtime::check()
    }

    /// Save every open document.
    pub fn save_all_documents(&mut self) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CIDEController_saveAllDocuments))(self.inner.ptr) };
        runtime::check()
    }

    /// Write the current window layout to a file.
    pub fn export_view_layout(&mut self, path: &str) -> Result<(), Error> {
        let p = runtime::to_cstring(path)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        unsafe { (sym!(self.sdk(), CIDEController_exportViewLayout))(self.inner.ptr, p.as_ptr()) };
        runtime::check()
    }

    /// Restore a window layout previously exported.
    pub fn import_view_layout(&mut self, path: &str) -> Result<(), Error> {
        let p = runtime::to_cstring(path)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        unsafe { (sym!(self.sdk(), CIDEController_importViewLayout))(self.inner.ptr, p.as_ptr()) };
        runtime::check()
    }

    // --- options ----------------------------------------------------------

    /// Read a string option, e.g. `/IOPEN/Hardware.Emulator`.
    ///
    /// # Option paths
    ///
    /// A path needs a **module prefix** — `/IOPEN/` for hardware/communication,
    /// `/IDE/` for the debugger and editor. A bare `Hardware.Emulator` does not
    /// resolve. Verified live:
    ///
    /// ```text
    /// /IOPEN/Hardware.Emulator      -> "iC6000"
    /// /IOPEN/Communication.Mode     -> "USB"
    /// /IDE/Debug.Symbols.Format.ANSI
    /// ```
    ///
    /// winIDEA's *Help | Display Option…* menu shows the path of anything in
    /// the UI.
    ///
    /// Only **leaf** options have a scalar value. A branch such as
    /// `/IOPEN/Hardware` fails here with `E_INVALIDARG`; read it with
    /// [`IdeController::option_json`] instead, which returns the whole subtree.
    pub fn option_str(&mut self, path: &str) -> Result<String, Error> {
        let p = runtime::to_cstring(path)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let s = unsafe {
            (sym!(self.sdk(), CIDEController_getOptionStr__SWIG_1))(self.inner.ptr, p.as_ptr())
        };
        runtime::check()?;
        // SAFETY: `s` is null or a NUL-terminated string from the SDK.
        Ok(unsafe { runtime::from_cstr(s) }.unwrap_or_default())
    }

    /// Read an integer option.
    pub fn option_int(&mut self, path: &str) -> Result<i64, Error> {
        let p = runtime::to_cstring(path)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let v = unsafe {
            (sym!(self.sdk(), CIDEController_getOptionInt__SWIG_1))(self.inner.ptr, p.as_ptr())
        };
        runtime::check()?;
        Ok(v)
    }

    /// Read a floating-point option.
    pub fn option_float(&mut self, path: &str) -> Result<f64, Error> {
        let p = runtime::to_cstring(path)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let v = unsafe {
            (sym!(self.sdk(), CIDEController_getOptionFloat__SWIG_1))(self.inner.ptr, p.as_ptr())
        };
        runtime::check()?;
        Ok(v)
    }

    /// Whether an option path exists.
    ///
    /// **Only reliable for leaf options.** Branch nodes such as
    /// `/IOPEN/Hardware` report `false` here even though
    /// [`IdeController::option_json`] happily returns their subtree, so do not
    /// use this as a general "can I read this path" gate.
    pub fn option_exists(&mut self, path: &str) -> Result<bool, Error> {
        let p = runtime::to_cstring(path)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let e = unsafe { (sym!(self.sdk(), CIDEController_optionExists))(self.inner.ptr, p.as_ptr()) };
        runtime::check()?;
        Ok(e)
    }

    /// Set a string option.
    ///
    /// Most options are read at the start of a debug session, so a change may
    /// not take effect until the session restarts. Note there is deliberately no
    /// `commit_options` wrapper: the SDK's `commitOptions()` is deprecated and
    /// *"terminates debugging session in most cases"*. To apply an option during
    /// a live session, use [`IdeController::invoke`] with the service function
    /// iSYSTEM support gives you.
    pub fn set_option_str(&mut self, path: &str, value: &str) -> Result<(), Error> {
        let p = runtime::to_cstring(path)?;
        let v = runtime::to_cstring(value)?;
        // SAFETY: live handle; the SDK copies both strings during the call.
        unsafe {
            (sym!(self.sdk(), CIDEController_setOption_str))(self.inner.ptr, p.as_ptr(), v.as_ptr())
        };
        runtime::check()
    }

    /// Set an integer option.
    pub fn set_option_int(&mut self, path: &str, value: i64) -> Result<(), Error> {
        let p = runtime::to_cstring(path)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        unsafe { (sym!(self.sdk(), CIDEController_setOption__SWIG_5))(self.inner.ptr, p.as_ptr(), value) };
        runtime::check()
    }

    /// Set a floating-point option.
    pub fn set_option_float(&mut self, path: &str, value: f64) -> Result<(), Error> {
        let p = runtime::to_cstring(path)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        unsafe {
            (sym!(self.sdk(), CIDEController_setOption_f64))(self.inner.ptr, p.as_ptr(), value)
        };
        runtime::check()
    }

    /// Set a boolean option.
    pub fn set_option_bool(&mut self, path: &str, value: bool) -> Result<(), Error> {
        let p = runtime::to_cstring(path)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        unsafe { (sym!(self.sdk(), CIDEController_setOption__SWIG_6))(self.inner.ptr, p.as_ptr(), value) };
        runtime::check()
    }

    /// Read an option subtree as JSON.
    pub fn option_json(&mut self, path: &str) -> Result<String, Error> {
        let p = runtime::to_cstring(path)?;
        // SAFETY: live handle; the SDK copies the string during the call.
        let s = unsafe { (sym!(self.sdk(), CIDEController_getOptionJSON))(self.inner.ptr, p.as_ptr()) };
        runtime::check()?;
        // SAFETY: `s` is null or a NUL-terminated string from the SDK.
        Ok(unsafe { runtime::from_cstr(s) }.unwrap_or_default())
    }

    /// Write an option subtree as JSON.
    pub fn set_option_json(&mut self, path: &str, json: &str) -> Result<(), Error> {
        let p = runtime::to_cstring(path)?;
        let j = runtime::to_cstring(json)?;
        // SAFETY: live handle; the SDK copies both strings during the call.
        unsafe {
            (sym!(self.sdk(), CIDEController_setOptionJSON))(self.inner.ptr, p.as_ptr(), j.as_ptr())
        };
        runtime::check()
    }

    // --- introspection ----------------------------------------------------

    /// A winIDEA path: its executable directory, workspace directory, or
    /// workspace file name.
    pub fn path(&mut self, which: PathType) -> Result<String, Error> {
        // SAFETY: live handle.
        let s = unsafe { (sym!(self.sdk(), CIDEController_getPath))(self.inner.ptr, which.to_raw()) };
        runtime::check()?;
        // SAFETY: `s` is null or a NUL-terminated string from the SDK.
        Ok(unsafe { runtime::from_cstr(s) }.unwrap_or_default())
    }

    /// The configured default download (program) file.
    pub fn default_download_file(&mut self) -> Result<String, Error> {
        // SAFETY: live handle.
        let s = unsafe { (sym!(self.sdk(), CIDEController_getDefaultDownloadFile))(self.inner.ptr) };
        runtime::check()?;
        // SAFETY: `s` is null or a NUL-terminated string from the SDK.
        Ok(unsafe { runtime::from_cstr(s) }.unwrap_or_default())
    }

    /// winIDEA's own license description.
    pub fn license_description(&mut self) -> Result<String, Error> {
        // SAFETY: live handle.
        let s = unsafe { (sym!(self.sdk(), CIDEController_getLicenseDescription))(self.inner.ptr) };
        runtime::check()?;
        // SAFETY: `s` is null or a NUL-terminated string from the SDK.
        Ok(unsafe { runtime::from_cstr(s) }.unwrap_or_default())
    }

    /// Number of clients currently attached to this winIDEA instance.
    pub fn num_clients(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle.
        let n = unsafe { (sym!(self.sdk(), CIDEController_getNumClients))(self.inner.ptr) };
        runtime::check()?;
        Ok(n)
    }

    /// Whether this is a winIDEA Open (free) build.
    pub fn is_winidea_open(&mut self) -> Result<bool, Error> {
        // SAFETY: live handle.
        let b = unsafe { (sym!(self.sdk(), CIDEController_isWinIDEAOpenVersion))(self.inner.ptr) };
        runtime::check()?;
        Ok(b)
    }

    // --- service functions ------------------------------------------------

    /// Call a function inside the winIDEA process.
    ///
    /// This is the SDK's current mechanism (`serviceCall` is deprecated). It is
    /// how options are applied during a live debug session, e.g.
    /// `invoke("/iOPEN/Core.Debug.ApplyInternalBPs", &[])`.
    ///
    /// **Failure is reported in the result, not as an error.** The SDK only
    /// raises for routing problems (winIDEA unreachable). A function that runs
    /// and fails returns `ResultB = "false"` with a message in `ResultS`, so
    /// this wrapper turns that convention into a proper `Err` for you.
    ///
    /// Use only as directed by iSYSTEM support — the function set is internal.
    pub fn invoke(
        &mut self,
        function_url: &str,
        params: &[(&str, &str)],
    ) -> Result<BTreeMap<String, String>, Error> {
        let sdk = self.sdk();
        let url = runtime::to_cstring(function_url)?;

        // Input parameters as a StrStrMap.
        // SAFETY: creates an empty StrStrMap we own.
        let in_ptr = unsafe { (sym!(sdk, new_StrStrMap__SWIG_0))() };
        // SAFETY: `in_ptr` is a fresh StrStrMap.
        let inputs = unsafe { Owned::adopt(self.inner.winidea, in_ptr, sdk.delete_StrStrMap)? };
        for (k, v) in params {
            let ck = runtime::to_cstring(k)?;
            let cv = runtime::to_cstring(v)?;
            // SAFETY: live map; the SDK copies both strings during the call.
            unsafe { (sym!(sdk, StrStrMap_setitem))(inputs.ptr, ck.as_ptr(), cv.as_ptr()) };
            runtime::check()?;
        }

        // SAFETY: live handle + map. Returns a StrStrMap we own.
        let out = unsafe {
            (sym!(sdk, CIDEController_invoke))(self.inner.ptr, url.as_ptr(), inputs.ptr)
        };
        // SAFETY: `out` is a fresh StrStrMap.
        let outputs = unsafe { Owned::adopt(self.inner.winidea, out, sdk.delete_StrStrMap)? };

        // SAFETY: `outputs.ptr` is a live StrStrMap.
        let map = unsafe { read_str_map(self.inner.winidea, outputs.ptr)? };

        // Apply the SDK's documented convention so callers get a Result rather
        // than having to remember to check two magic keys.
        if let Some(ok) = map.get("ResultB")
            && ok.eq_ignore_ascii_case("false")
        {
            let msg = map.get("ResultS").cloned().unwrap_or_default();
            return Err(Error::Sdk {
                kind: crate::ExceptionKind::Application,
                message: format!("{function_url} failed: {msg}"),
            });
        }
        Ok(map)
    }
}

/// Copy a `StrStrMap` into a Rust map.
///
/// # Safety
/// `ptr` must be a live `StrStrMap` handle.
unsafe fn read_str_map(
    winidea: &WinIdea,
    ptr: *mut std::ffi::c_void,
) -> Result<BTreeMap<String, String>, Error> {
    let sdk = winidea.raw();
    let mut out = BTreeMap::new();

    // SAFETY: live map handle.
    let n = unsafe { (sym!(sdk, StrStrMap_size))(ptr) };
    runtime::check()?;
    if n == 0 {
        return Ok(out);
    }

    // The map is keyed, not indexed, so it is walked with the SDK's own
    // iterator rather than by position.
    // SAFETY: live map handle; the iterator is released below.
    let it = unsafe { (sym!(sdk, StrStrMap_create_iterator_begin))(ptr) };
    runtime::check()?;

    for _ in 0..n {
        // SAFETY: the iterator is valid for `n` advances.
        let key = unsafe { (sym!(sdk, StrStrMap_get_next_key))(ptr, it) };
        if runtime::check().is_err() {
            break;
        }
        // SAFETY: `key` is null or a NUL-terminated string from the SDK.
        let Some(key) = (unsafe { runtime::from_cstr(key) }) else {
            break;
        };
        let ck = runtime::to_cstring(&key)?;
        // SAFETY: live map handle; the SDK copies the key during lookup.
        let val = unsafe { (sym!(sdk, StrStrMap_getitem))(ptr, ck.as_ptr()) };
        runtime::check()?;
        // SAFETY: `val` is null or a NUL-terminated string from the SDK.
        let val = unsafe { runtime::from_cstr(val) }.unwrap_or_default();
        out.insert(key, val);
    }

    // SAFETY: `it` came from create_iterator_begin on this same map.
    unsafe { (sym!(sdk, StrStrMap_destroy_iterator))(ptr, it) };
    runtime::check()?;

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_type_encoding() {
        // Spot-check the ends and a few in between; the full set is pinned
        // against the SDK's constants in tests/enum_values.rs.
        assert_eq!(ViewType::Project.to_raw(), 0);
        assert_eq!(ViewType::Watch.to_raw(), 16);
        assert_eq!(ViewType::Memory.to_raw(), 17);
        assert_eq!(ViewType::SessionExplorer.to_raw(), 21);
        assert_eq!(ViewType::None.to_raw(), 22);
    }

    #[test]
    fn path_type_encoding() {
        assert_eq!(PathType::ExeDir.to_raw(), 0);
        assert_eq!(PathType::WorkspaceDir.to_raw(), 1);
        assert_eq!(PathType::WorkspaceFileName.to_raw(), 2);
    }
}
