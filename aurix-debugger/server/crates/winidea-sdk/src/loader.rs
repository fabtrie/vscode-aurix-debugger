//! Programming the target from winIDEA's download list.
//!
//! The server keeps its own DWARF reader, so winIDEA must load **code only**
//! and no symbols: two symbol tables for one target would disagree the moment
//! the files differ, and the frontends read ours.
//!
//! # Why the list is configured through the option tree
//!
//! `CLoaderController` also exposes `addToDownloadList` and
//! `downloadFromList`, but their parameters are not self-describing -- an
//! `i32` list selector, a `bool`, and one or two strings whose meaning the
//! P/Invoke metadata does not record (SWIG names them `jarg2..jarg5`). Setting
//! a file's *symbols* flag by guessing a boolean is exactly the mistake that
//! would not fail loudly: it would silently load a second symbol table.
//!
//! So the file list is written through the option tree instead
//! ([`crate::IdeController::set_option_json`] on `/IDE/Debug.DownloadFiles`),
//! where each field is named, and only the argument-free entry points here are
//! used: [`LoaderController::download`] and
//! [`LoaderController::download_without_code`].

use crate::runtime::{self, Error, Owned};
use crate::{ConnectionMgr, WinIdea};

/// The option-tree path holding winIDEA's download files.
pub const DOWNLOAD_FILES_OPTION: &str = "/IDE/Debug.DownloadFiles";

/// `/IDE/Debug.DownloadFiles.DefaultFile` is -1 when no file is configured,
/// which is also how the SDK reports "no symbol table" to the symbol queries.
pub const DEFAULT_FILE_OPTION: &str = "/IDE/Debug.DownloadFiles.DefaultFile";

/// Programming and verifying, from the list winIDEA holds.
pub struct LoaderController<'a> {
    inner: Owned<'a>,
}

impl<'a> LoaderController<'a> {
    pub fn new(winidea: &'a WinIdea, conn: &'a ConnectionMgr<'a>) -> Result<Self, Error> {
        let sdk = winidea.raw();
        // SAFETY: `conn` outlives this controller, so its handle stays valid.
        let ptr = unsafe { (sym!(sdk, new_CLoaderController))(conn.as_ptr()) };
        // SAFETY: `ptr` is a fresh CLoaderController.
        let inner = unsafe { Owned::adopt(winidea, ptr, sdk.delete_CLoaderController)? };
        Ok(Self { inner })
    }

    /// Programs the configured download files into the target.
    pub fn download(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle; takes no arguments.
        let rc = unsafe { (sym!(self.inner.sdk(), CLoaderController_download))(self.inner.ptr) };
        runtime::check()?;
        Ok(rc)
    }

    /// Loads only the debug information, programming nothing.
    ///
    /// The opposite of what this server wants -- it is here because its
    /// existence is what confirms [`LoaderController::download`] programs code.
    pub fn download_without_code(&mut self) -> Result<i32, Error> {
        // SAFETY: live handle; takes no arguments.
        let rc =
            unsafe { (sym!(self.inner.sdk(), CLoaderController_downloadWithoutCode))(self.inner.ptr) };
        runtime::check()?;
        Ok(rc)
    }

    /// Asks winIDEA to verify what it downloaded.
    ///
    /// The server verifies with its own `verify_image` instead, which reads the
    /// image back through ordinary memory and so reports the same thing on
    /// either backend. This is here for parity, not because it is used.
    pub fn verify_download(&mut self) -> Result<(), Error> {
        // SAFETY: live handle; takes no arguments.
        unsafe { (sym!(self.inner.sdk(), CLoaderController_verifyDownload))(self.inner.ptr) };
        runtime::check()
    }
}
