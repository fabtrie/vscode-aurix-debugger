//! Minimal safe bindings to Jim Tcl 0.83 (the interpreter OpenOCD embeds).
//!
//! The interpreter is single threaded (`!Send`). Commands are `Fn` closures so
//! that a command may re-enter the interpreter (e.g. `resume` firing an event
//! handler that calls `mww`), keep mutable state in `RefCell`s.

// Links libjimtcl, which the C shim calls into.
use jimtcl as _;

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};

#[repr(C)]
struct RawInterp {
    _private: [u8; 0],
}

extern "C" {
    fn rj_create() -> *mut RawInterp;
    fn rj_free(interp: *mut RawInterp);
    fn rj_eval(interp: *mut RawInterp, script: *const c_char, global: c_int) -> c_int;
    fn rj_eval_file(interp: *mut RawInterp, path: *const c_char) -> c_int;
    fn rj_result(interp: *mut RawInterp, len: *mut c_int) -> *const c_char;
    fn rj_error_message(interp: *mut RawInterp, len: *mut c_int) -> *const c_char;
    fn rj_set_result(interp: *mut RawInterp, s: *const c_char, len: c_int);
    fn rj_create_command(interp: *mut RawInterp, name: *const c_char, data: *mut c_void) -> c_int;
    fn rj_delete_command(interp: *mut RawInterp, name: *const c_char) -> c_int;
    fn rj_set_global(interp: *mut RawInterp, name: *const c_char, value: *const c_char, len: c_int) -> c_int;
    fn rj_get_global(interp: *mut RawInterp, name: *const c_char, len: *mut c_int) -> *const c_char;
}

pub const JIM_OK: c_int = 0;
pub const JIM_ERR: c_int = 1;

/// Result of a command implemented in Rust: `Ok(result)` or `Err(message)`.
pub type CmdResult = Result<String, String>;

type BoxedCmd = Box<dyn Fn(&Interp, &[&str]) -> CmdResult>;

/// A Jim Tcl interpreter.
pub struct Interp {
    raw: *mut RawInterp,
    owned: bool,
    _not_send: PhantomData<*mut ()>,
}

impl Default for Interp {
    fn default() -> Self {
        Self::new()
    }
}

impl Interp {
    /// Creates an interpreter with the core commands and static extensions.
    pub fn new() -> Interp {
        let raw = unsafe { rj_create() };
        assert!(!raw.is_null(), "could not create Jim interpreter");
        Interp { raw, owned: true, _not_send: PhantomData }
    }

    /// Evaluates a script at the current call frame.
    pub fn eval(&self, script: &str) -> CmdResult {
        let s = cstring(script);
        let rc = unsafe { rj_eval(self.raw, s.as_ptr(), 0) };
        self.finish(rc)
    }

    /// Evaluates a script at global level (like OpenOCD event handlers).
    pub fn eval_global(&self, script: &str) -> CmdResult {
        let s = cstring(script);
        let rc = unsafe { rj_eval(self.raw, s.as_ptr(), 1) };
        self.finish(rc)
    }

    /// Sources a file at global level.
    pub fn eval_file(&self, path: &str) -> CmdResult {
        let s = cstring(path);
        let rc = unsafe { rj_eval_file(self.raw, s.as_ptr()) };
        self.finish(rc)
    }

    fn finish(&self, rc: c_int) -> CmdResult {
        let mut len = 0;
        if rc == JIM_OK {
            let p = unsafe { rj_result(self.raw, &mut len) };
            Ok(to_string(p, len))
        } else if rc == JIM_ERR {
            let p = unsafe { rj_error_message(self.raw, &mut len) };
            Err(to_string(p, len))
        } else {
            // return/break/continue at top level: treat the result as a value.
            let p = unsafe { rj_result(self.raw, &mut len) };
            Ok(to_string(p, len))
        }
    }

    /// Registers (or replaces) a command implemented in Rust.
    pub fn register<F>(&self, name: &str, f: F)
    where
        F: Fn(&Interp, &[&str]) -> CmdResult + 'static,
    {
        let boxed: Box<BoxedCmd> = Box::new(Box::new(f));
        let data = Box::into_raw(boxed) as *mut c_void;
        let n = cstring(name);
        unsafe { rj_create_command(self.raw, n.as_ptr(), data) };
    }

    pub fn delete_command(&self, name: &str) {
        let n = cstring(name);
        unsafe { rj_delete_command(self.raw, n.as_ptr()) };
    }

    pub fn command_exists(&self, name: &str) -> bool {
        self.eval(&format!("info commands {{{name}}}")).map(|r| !r.is_empty()).unwrap_or(false)
    }

    pub fn set_global(&self, name: &str, value: &str) {
        let n = cstring(name);
        unsafe { rj_set_global(self.raw, n.as_ptr(), value.as_ptr() as *const c_char, value.len() as c_int) };
    }

    pub fn get_global(&self, name: &str) -> Option<String> {
        let n = cstring(name);
        let mut len = 0;
        let p = unsafe { rj_get_global(self.raw, n.as_ptr(), &mut len) };
        if p.is_null() {
            None
        } else {
            Some(to_string(p, len))
        }
    }
}

impl Drop for Interp {
    fn drop(&mut self) {
        if self.owned {
            unsafe { rj_free(self.raw) };
        }
    }
}

/// Quotes a string as a single Tcl list element.
pub fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._:-/+,@=".contains(c)) {
        return s.to_string();
    }
    let balanced = {
        let mut depth = 0i32;
        let mut ok = true;
        let mut prev_backslash = false;
        for c in s.chars() {
            match c {
                '\\' => prev_backslash = !prev_backslash,
                '{' if !prev_backslash => depth += 1,
                '}' if !prev_backslash => {
                    depth -= 1;
                    if depth < 0 {
                        ok = false;
                    }
                }
                _ => prev_backslash = false,
            }
            if c != '\\' {
                prev_backslash = false;
            }
        }
        ok && depth == 0 && !s.ends_with('\\')
    };
    if balanced {
        format!("{{{s}}}")
    } else {
        let mut out = String::with_capacity(s.len() + 8);
        for c in s.chars() {
            if "{}[]$\"\\; \t\n".contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }
}

/// Formats values as a Tcl list.
pub fn list<I, S>(items: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    items.into_iter().map(|s| quote(s.as_ref())).collect::<Vec<_>>().join(" ")
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap()
}

fn to_string(p: *const c_char, len: c_int) -> String {
    if p.is_null() {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(p as *const u8, len as usize) };
    String::from_utf8_lossy(bytes).into_owned()
}

#[no_mangle]
unsafe extern "C" fn rj_rust_dispatch(
    data: *mut c_void,
    interp: *mut RawInterp,
    argc: c_int,
    argv: *const *const c_char,
    lens: *const c_int,
) -> c_int {
    let cmd = &*(data as *const BoxedCmd);
    let wrapper = Interp { raw: interp, owned: false, _not_send: PhantomData };
    let owned: Vec<String> = (0..argc as usize)
        .map(|i| to_string(*argv.add(i), *lens.add(i)))
        .collect();
    let args: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
    let res = catch_unwind(AssertUnwindSafe(|| cmd(&wrapper, &args)));
    let (rc, msg) = match res {
        Ok(Ok(v)) => (JIM_OK, v),
        Ok(Err(e)) => (JIM_ERR, e),
        Err(p) => {
            let m = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panic".into());
            (JIM_ERR, format!("internal error in {}: {m}", args.first().copied().unwrap_or("?")))
        }
    };
    rj_set_result(interp, msg.as_ptr() as *const c_char, msg.len() as c_int);
    rc
}

#[no_mangle]
unsafe extern "C" fn rj_rust_free(data: *mut c_void) {
    drop(Box::from_raw(data as *mut BoxedCmd));
}

#[allow(dead_code)]
fn _assert_cstr(_: &CStr) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn openocd_style_constructs() {
        let i = Interp::new();
        let r = i
            .eval(
                "set l {}; lappend l a b; array set arr {k1 0xF881FD00}; \
                 set x [format 0x%08x [expr {$arr(k1) + 0x10}]]; \
                 if {[info exists arr(k1)]} { set y ok }; list $x $y [llength $l]",
            )
            .unwrap();
        assert_eq!(r, "0xf881fd10 ok 2");
    }

    #[test]
    fn rust_command_errors_are_catchable() {
        let i = Interp::new();
        let log = Rc::new(RefCell::new(Vec::new()));
        let l2 = log.clone();
        i.register("mww", move |_, args| {
            l2.borrow_mut().push(args[1..].join(" "));
            if args.len() != 3 {
                return Err("usage: mww address value".into());
            }
            if args[1] == "0xF00010E8" {
                return Err("Read/write access was not completely successful".into());
            }
            Ok(String::new())
        });
        let r = i.eval("if {[catch {mww 0xF00010E8 0x12000000} err]} { set err } else { set x ok }").unwrap();
        assert_eq!(r, "Read/write access was not completely successful");
        i.eval("mww 0xF0000494 0xB000003F").unwrap();
        assert_eq!(log.borrow().len(), 2);
    }

    #[test]
    fn reentrant_command() {
        let i = Interp::new();
        i.register("outer", |interp, _| interp.eval_global("inner"));
        i.register("inner", |_, _| Ok("from inner".into()));
        assert_eq!(i.eval("outer").unwrap(), "from inner");
    }

    #[test]
    fn error_contains_message() {
        let i = Interp::new();
        let e = i.eval("proc p {} { error boom }; p").unwrap_err();
        assert!(e.contains("boom"), "{e}");
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("tc37x.cpu0"), "tc37x.cpu0");
        assert_eq!(quote("a b"), "{a b}");
        assert_eq!(quote(""), "{}");
        let i = Interp::new();
        let l = list(["a b", "c{", "$x"]);
        assert_eq!(i.eval(&format!("llength [list {l}]")).unwrap(), "3");
    }
}
