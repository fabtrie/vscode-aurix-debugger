//! Drift guard: loading must survive a DLL that does not export everything.
//!
//! These bindings are generated from ONE SDK version (9.21.417), but the DLL
//! they load is a client of whatever winIDEA is installed, and the export set
//! genuinely differs between versions -- 9.21.417 introduced the entire
//! `CDAQConfig` family (DAQ acquisition modes) that 9.21.356 has no trace of.
//!
//! Resolving every symbol with `?` therefore made ONE absent export abort the
//! whole load with an unattributed `GetProcAddress failed`, taking down ~6k
//! working exports with it. The contract pinned here is the fix: absent exports
//! bind as `None` and are named by `missing()`, so only the feature that needs
//! one fails, and it fails saying which.
//!
//! Regenerating `ffi.rs` with eager `?` resolution would break this test.

use std::ffi::OsStr;

use winidea_sdk::WinIdea;

/// A real DLL that is emphatically NOT the SDK.
///
/// Every OS-flavoured Windows build has `kernel32`, so this needs no fixture and
/// no winIDEA install: it exports thousands of symbols, none of them SWIG's, and
/// therefore stands in for the extreme case of a version mismatch.
#[cfg(windows)]
const FOREIGN_DLL: &str = "kernel32.dll";

/// A DLL missing every SDK export must still LOAD, reporting what is absent --
/// the whole point of tolerant resolution.
///
/// It is rejected only at `register_callbacks`, which is deliberate: SWIG's
/// exception and string callbacks are the one part of the ABI that is not
/// optional, since without them no error and no string can cross the boundary.
/// So the observable contract is "fails with MissingSymbol naming a callback",
/// NOT the old "GetProcAddress failed" with nothing attributable.
#[cfg(windows)]
#[test]
fn a_foreign_dll_fails_naming_the_symbol_not_getprocaddress() {
    let err = match WinIdea::load_from(OsStr::new(FOREIGN_DLL)) {
        Ok(_) => panic!("kernel32 is not the winIDEA SDK; loading it must not succeed"),
        Err(e) => e,
    };

    let msg = err.to_string();
    assert!(
        msg.contains("SWIGRegister"),
        "a mismatched DLL must fail naming the missing export, so the version \
         skew is diagnosable; got: {msg}"
    );
}

/// The bundled DLL -- the version these bindings were generated from -- must
/// resolve EVERY export. A non-empty `missing()` here means `pinvoke.json` and
/// `lib/x64/isystemConnect.dll` have drifted apart.
///
/// Skipped when the DLL is absent (CI has no winIDEA checkout) rather than
/// failing, so the guard costs nothing where it cannot run.
///
/// Pinned because the generator's `resolve()` also aliases `__SWIG_0___` to the
/// unsuffixed export: that fallback must never be needed HERE (the bundled DLL
/// is the version the bindings came from), so an empty `missing()` also proves
/// the alias is not quietly papering over a generator/DLL drift.
#[cfg(windows)]
#[test]
fn the_bundled_dll_resolves_every_export() {
    let dll = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("lib")
        .join("x64")
        .join("isystemConnect.dll");
    if !dll.is_file() {
        eprintln!("skipping: {} not present", dll.display());
        return;
    }

    let winidea = match WinIdea::load(&[dll.parent().unwrap().to_path_buf()]) {
        Ok(w) => w,
        Err(e) => panic!("the bundled SDK must load: {e}"),
    };

    assert!(
        winidea.missing().is_empty(),
        "the bundled DLL should export everything these bindings bind, but {} \
         are missing (first few: {:?}) -- pinvoke.json and the DLL have drifted",
        winidea.missing().len(),
        &winidea.missing()[..winidea.missing().len().min(5)],
    );

    // The older-SDK exports must NOT count as missing here: a modern DLL is not
    // expected to have them, and folding them in would report version skew on
    // an exact match. Absent from `missing()` is the contract.
    assert!(
        !winidea
            .missing()
            .iter()
            .any(|s| s.contains("CDAQController_configure___")),
        "legacy-only exports leaked into missing(); they are resolved into a \
         throwaway list precisely so a matching DLL reports a clean bill"
    );
}

/// The loader must NOT alias `__SWIG_0` to the unsuffixed export.
///
/// Tempting, because SWIG suffixes a name only when the C++ method is
/// overloaded: 9.21.417 has `CDAQController_configure__SWIG_0/1` where 9.21.356
/// has a bare `CDAQController_configure`, so the names look like the same
/// function under two spellings.
///
/// They are not ABI-compatible. Binding 9.21.356's unsuffixed `configure` as
/// 9.21.417's `__SWIG_0` **crashed the process inside `configure`** -- no error,
/// no panic, verified against a real 9.21.356 winIDEA. An export's shape does
/// not establish its parameter semantics, and a silent crash is far worse than
/// the loud `MissingSymbol` the alias was meant to avoid.
///
/// Asserted against the generated source: the alias would only *fire* on an
/// older DLL, so reintroducing it would pass every other test on a machine that
/// only has the bundled one.
/// The legacy `configure` must stay bound with its true **void** return.
///
/// This is the exact shape that crashed: 9.21.356's unsuffixed
/// `CDAQController_configure___` returns `void`, while 9.21.417's overloads
/// return an IntVector of item handles. Typing the old export like the new one
/// reads a garbage pointer out of the return register, which the caller then
/// adopts and frees -- a silent process crash inside `configure`.
///
/// So the `-> ()` here is not cosmetic; it is the fix. Pinned against the
/// generated source because it only matters on an older DLL.
#[test]
fn the_legacy_configure_is_bound_as_returning_void() {
    let ffi = include_str!("../src/generated/ffi.rs");
    assert!(
        ffi.contains(
            "pub CDAQController_configure_legacy_void: Option<unsafe extern \"C\" fn(*mut ::std::ffi::c_void, *mut ::std::ffi::c_void) -> ()>"
        ),
        "the legacy DAQ `configure` binding lost its `-> ()` return (or the \
         field vanished); binding it like the modern overload crashes the \
         process on winIDEA 9.21.356"
    );
}

#[test]
fn the_loader_does_not_alias_swig_0_to_the_unsuffixed_export() {
    let ffi = include_str!("../src/generated/ffi.rs");
    assert!(
        !ffi.contains(r#"strip_suffix("__SWIG_0___")"#),
        "the `__SWIG_0___` -> unsuffixed alias is back in resolve(); it crashes \
         the process on winIDEA 9.21.356 rather than failing with MissingSymbol"
    );
}
