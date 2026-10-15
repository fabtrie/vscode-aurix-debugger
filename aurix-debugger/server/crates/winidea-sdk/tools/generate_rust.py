# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Generate the Rust FFI layer for isystemConnect.dll from pinvoke.json.

Emits ONE file, `src/generated/ffi.rs`, containing:
  - a `Sdk` struct of `libloading::Symbol` function pointers, one per export
  - a `Sdk::load(path)` that resolves every symbol from the DLL

Design notes
------------
* Every object is an opaque `*mut c_void` (SWIG's `HandleRef`). We never need
  any C++ layout, which is what makes this sound without the real headers.
* `string` params are `*const c_char`; `string` RETURNS are also `*const c_char`
  but are produced through SWIG's registered string callback -- see
  `runtime.rs` for the ownership rules.
* Nothing here is `unsafe`-free: this module is the raw ABI. The safe wrappers
  live above it.
* Skip-and-report: a signature using a type we have no rule for is NOT emitted;
  it is written to `unhandled.md` instead. Emitting a guess would produce code
  that compiles and then corrupts memory.
"""

from __future__ import annotations

import json
import re
import sys
from collections import Counter
from pathlib import Path

# IL type -> Rust extern "C" type. Only these are considered ABI-safe.
TYPE_MAP = {
    "void": "()",
    "handle": "*mut ::std::ffi::c_void",
    "native int": "*mut ::std::ffi::c_void",
    "string": "*const ::std::ffi::c_char",
    "bool": "bool",
    "char": "u16",          # IL `char` is UTF-16 code unit
    "int8": "i8",
    "uint8": "u8",
    "int16": "i16",
    "uint16": "u16",
    "int32": "i32",
    "uint32": "u32",
    "int64": "i64",
    "uint64": "u64",
    "float32": "f32",
    "float64": "f64",
}

# SWIG's callback registrars take function pointers, whose IL delegate types
# have no primitive form. Their signatures are fixed by SWIG's csharp.swg and
# verified against the IL:
#   ExceptionDelegate         -> void Invoke(string message)
#   ExceptionArgumentDelegate -> void Invoke(string message, string paramName)
#   SWIGStringDelegate        -> string Invoke(string message)
# The exception registrar takes 11 delegates, the argument variant 3.
CB = "unsafe extern \"C\" fn(*const ::std::ffi::c_char)"
CB_ARG = "unsafe extern \"C\" fn(*const ::std::ffi::c_char, *const ::std::ffi::c_char)"
CB_STR = "unsafe extern \"C\" fn(*const ::std::ffi::c_char) -> *mut ::std::ffi::c_char"

CALLBACK_REGISTRARS = {
    "SWIGRegisterExceptionCallbacks_isystemConnect": [CB] * 11,
    "SWIGRegisterExceptionArgumentCallbacks_isystemConnect": [CB_ARG] * 3,
    "SWIGRegisterStringCallback_isystemConnect": [CB_STR],
}

# Skip only the managed-side string helpers, which are not real exports.
HAND_WRITTEN = re.compile(r"^SWIG_csharp_string")

# Exports that exist ONLY in OLDER SDKs, bound so one build can drive several
# winIDEA versions. `pinvoke.json` describes the newest SDK, so anything an older
# one spells differently -- or types differently -- has to be declared here; the
# loader binds each as `Option`, so a DLL without it simply reports it missing.
#
# Signatures are taken from that older SDK's OWN P/Invoke metadata (extract it
# exactly as for the current one). Never infer one from the modern export: the
# whole reason this table exists is that the two disagree.
#
#   name -> (export, ret, [param types])
LEGACY_EXPORTS = {
    # 9.21.356 and earlier: `configure` was not overloaded, so SWIG emitted it
    # WITHOUT a `__SWIG_n` suffix -- and it returned **void**, where 9.21.417's
    # overloads return an IntVector of item handles.
    #
    # That return type is the trap. Calling the old export through the new
    # signature reads a garbage pointer out of the return register, which the
    # caller then adopts and frees: a silent process crash inside `configure`,
    # reproduced against a real 9.21.356 winIDEA. Binding it separately, with
    # its true `void` return, is what makes the old DLL safe to drive.
    "CDAQController_configure_legacy_void": (
        "CSharp_sifisystemficonnect_CDAQController_configure___",
        "()",
        ["*mut ::std::ffi::c_void", "*mut ::std::ffi::c_void"],
    ),
}

RUST_KEYWORDS = {
    "as", "break", "const", "continue", "crate", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
    "move", "mut", "pub", "ref", "return", "self", "static", "struct", "super",
    "trait", "true", "type", "unsafe", "use", "where", "while", "async",
    "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
}


def rust_ident(name: str) -> str:
    n = re.sub(r"\W", "_", name)
    if not n or n[0].isdigit():
        n = "_" + n
    return n + "_" if n in RUST_KEYWORDS else n


def map_type(t: str) -> str | None:
    return TYPE_MAP.get(t)


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    fns = json.loads(Path(sys.argv[1]).read_text())
    out = Path(sys.argv[2])
    report = out.parent.parent.parent / "tools" / "unhandled.md"

    emitted, unhandled = [], []

    for f in fns:
        if HAND_WRITTEN.search(f["name"]):
            continue

        # Callback registrars: types come from SWIG's fixed delegate shapes,
        # not from the IL (function pointers have no IL primitive form).
        if f["export"] in CALLBACK_REGISTRARS:
            sig = CALLBACK_REGISTRARS[f["export"]]
            emitted.append({
                "export": f["export"],
                "field": rust_ident(f["export"]),
                "ret": "()",
                "params": [(f"cb{i}", t) for i, t in enumerate(sig)],
                "class": "",
            })
            continue

        ret = map_type(f["ret"])
        if ret is None:
            unhandled.append((f, f"unsupported return type `{f['ret']}`"))
            continue

        params, bad = [], None
        for i, p in enumerate(f["params"]):
            rt = map_type(p["type"])
            if rt is None:
                bad = f"unsupported param type `{p['type']}`"
                break
            pname = rust_ident(p["name"] or f"arg{i}")
            params.append((pname, rt))
        if bad:
            unhandled.append((f, bad))
            continue

        emitted.append({
            "export": f["export"],
            "field": rust_ident(f["name"]),
            "ret": ret,
            "params": params,
            "class": f["class"],
        })

    # Field names must be unique; the export name already is.
    seen = Counter(e["field"] for e in emitted)
    for e in emitted:
        if seen[e["field"]] > 1:
            e["field"] = rust_ident(e["export"])

    out.parent.mkdir(parents=True, exist_ok=True)
    with out.open("w", newline="\n") as fh:
        w = fh.write
        w("// @generated by tools/generate_rust.py -- DO NOT EDIT.\n")
        w("//\n")
        w("// Raw FFI bindings for the winIDEA C# SDK's native library\n")
        w("// (isystemConnect.dll). Types come from the P/Invoke metadata in\n")
        w("// IConnectCSLib.dll, so every signature here is what the DLL was\n")
        w("// actually compiled with -- none of it is inferred.\n")
        w("//\n")
        w(f"// {len(emitted)} exports bound.\n")
        # `type_complexity` fires on the callback registrars, whose parameters
        # are genuinely fn-pointer-of-fn-pointer; that shape is dictated by the
        # C ABI and cannot be simplified without hiding the real signature.
        w("#![allow(non_snake_case, clippy::too_many_arguments, clippy::type_complexity)]\n\n")
        w("use libloading::Library;\n\n")

        w("/// Resolve one export, recording it as missing rather than failing.\n")
        w("///\n")
        w("/// A free generic function, NOT inlined per symbol: `load` resolves ~6k\n")
        w("/// exports, and emitting the match arms inline made the function body so\n")
        w("/// large that **rustc itself overflowed its stack** while optimising it\n")
        w("/// (`cargo test`, opt-level 3). One call per symbol keeps it compiling.\n")
        w("///\n")
        w("/// Deliberately does NOT alias `foo__SWIG_0___` to the unsuffixed `foo___`.\n")
        w("/// SWIG only suffixes a name when the C++ method is overloaded, so an\n")
        w("/// older DLL does spell the single-overload case without a suffix -- but\n")
        w("/// matching export SHAPES does not make the functions ABI-compatible.\n")
        w("/// Tried and reverted: 9.21.356's bare `CDAQController_configure___` bound\n")
        w("/// as 9.21.417's `configure__SWIG_0` **crashed the process** inside\n")
        w("/// `configure` -- silently, no error, no panic. A loud `MissingSymbol` is\n")
        w("/// strictly better. Version-specific signatures belong in a version-aware\n")
        w("/// binding, not a name guess.\n")
        w("///\n")
        w("/// # Safety\n")
        w("/// `T` must be the export's true signature; `name` must be NUL-terminated.\n")
        w("unsafe fn resolve<T: Copy>(\n")
        w("    lib: &Library,\n")
        w("    symbol: &[u8],\n")
        w("    name: &'static str,\n")
        w("    missing: &mut Vec<&'static str>,\n")
        w(") -> Option<T> {\n")
        w("    // SAFETY: guaranteed by the caller.\n")
        w("    if let Ok(s) = unsafe { lib.get::<T>(symbol) } {\n")
        w("        return Some(*s);\n")
        w("    }\n")
        w("\n")
        w("    missing.push(name);\n")
        w("    None\n")
        w("}\n\n")

        w("/// Every export of `isystemConnect.dll`, resolved at load time.\n")
        w("///\n")
        w("/// Held together with the `Library` it borrows from; `_lib` must be\n")
        w("/// declared last so it outlives the symbols (drop order is\n")
        w("/// declaration order).\n")
        w("pub struct Sdk {\n")
        for e in emitted:
            args = ", ".join(t for _, t in e["params"])
            w(
                f"    pub {e['field']}: "
                f"Option<unsafe extern \"C\" fn({args}) -> {e['ret']}>,\n"
            )
        for name, (_export, ret, params) in sorted(LEGACY_EXPORTS.items()):
            args = ", ".join(params)
            w(f"    /// Older-SDK export; see `LEGACY_EXPORTS` in the generator.\n")
            w(f"    pub {name}: Option<unsafe extern \"C\" fn({args}) -> {ret}>,\n")
        w("    /// Exports this DLL did not provide, in binding order.\n")
        w("    missing: Vec<&'static str>,\n")
        w("    _lib: Library,\n")
        w("}\n\n")

        w("impl Sdk {\n")
        w("    /// Load the DLL and resolve every export.\n")
        w("    ///\n")
        w("    /// Returns a `Box` because `Sdk` is large: one function pointer per\n")
        w(f"    /// export is ~{len(emitted) * 8 // 1024} KB, which must never live on the stack.\n")
        w("    /// Each symbol is written straight into the heap allocation via a raw\n")
        w("    /// pointer, so no intermediate copy of the struct is ever materialised.\n")
        w("    ///\n")
        w("    /// Exports the DLL does not provide are bound as `None` and listed by\n")
        w("    /// [`Sdk::missing`] -- only a DLL that fails to load at all is an\n")
        w("    /// error. See the note in the body for why.\n")
        w("    ///\n")
        w("    /// # Safety\n")
        w("    /// The caller must ensure `path` refers to a genuine\n")
        w("    /// `isystemConnect.dll`; loading an arbitrary library and calling\n")
        w("    /// through these signatures is undefined behaviour.\n")
        w("    pub unsafe fn load(path: &std::ffi::OsStr) -> Result<Box<Self>, libloading::Error> {\n")
        w("        unsafe {\n")
        w("            let lib = Library::new(path)?;\n")
        w("\n")
        w("            // Uninitialised heap allocation, populated field by field.\n")
        w("            // `Box::new(Self { .. })` would build the whole struct on the\n")
        w("            // stack first and overflow it.\n")
        w("            let mut boxed: Box<::std::mem::MaybeUninit<Self>> =\n")
        w("                Box::new(::std::mem::MaybeUninit::uninit());\n")
        w("            let p = boxed.as_mut_ptr();\n")
        w("\n")
        w("            // A missing export is recorded, not fatal. The SDK's export\n")
        w("            // set changes between winIDEA versions (9.21.417 added the\n")
        w("            // whole `CDAQConfig` family, for example), so resolving every\n")
        w("            // symbol with `?` made ONE absent export abort the entire load\n")
        w("            // with an unattributed `GetProcAddress failed`. Binding each as\n")
        w("            // `None` instead degrades exactly the feature that needs it and\n")
        w("            // keeps the other ~6k working.\n")
        w("            let mut missing: Vec<&'static str> = Vec::new();\n")
        for e in emitted:
            args = ", ".join(t for _, t in e["params"])
            w(
                f"            (&raw mut (*p).{e['field']}).write(resolve::<unsafe extern \"C\" fn({args}) -> {e['ret']}>(\n"
            )
            w(f"                &lib, b\"{e['export']}\\0\", \"{e['export']}\", &mut missing));\n")
        w("\n")
        w("            // Older-SDK exports. Resolved into a THROWAWAY `missing` list:\n")
        w("            // a modern DLL is not expected to have them, so counting them\n")
        w("            // would make `missing()` cry version skew on a perfect match.\n")
        w("            let mut legacy_absent: Vec<&'static str> = Vec::new();\n")
        for name, (export, ret, params) in sorted(LEGACY_EXPORTS.items()):
            args = ", ".join(params)
            w(
                f"            (&raw mut (*p).{name}).write(resolve::<unsafe extern \"C\" fn({args}) -> {ret}>(\n"
            )
            w(f"                &lib, b\"{export}\\0\", \"{export}\", &mut legacy_absent));\n")
        w("            drop(legacy_absent);\n")
        w("\n")
        w("            (&raw mut (*p).missing).write(missing);\n")
        w("\n")
        w("            // `_lib` last: it owns the memory the symbols point into, so it\n")
        w("            // must outlive them (drop order is declaration order).\n")
        w("            (&raw mut (*p)._lib).write(lib);\n")
        w("\n")
        w("            // Every field is now initialised.\n")
        w("            Ok(Box::from_raw(Box::into_raw(boxed).cast::<Self>()))\n")
        w("        }\n")
        w("    }\n")
        w("\n")
        w("    /// Exports this DLL did not provide.\n")
        w("    ///\n")
        w("    /// Empty for a DLL matching the version these bindings were generated\n")
        w("    /// from. A non-empty list means the loaded DLL is a different winIDEA\n")
        w("    /// version -- names it so the mismatch is diagnosable instead of\n")
        w("    /// surfacing later as an obscure failure.\n")
        w("    pub fn missing(&self) -> &[&'static str] {\n")
        w("        &self.missing\n")
        w("    }\n")
        w("}\n")

    with report.open("w", newline="\n") as fh:
        fh.write("# Unhandled exports\n\n")
        fh.write(
            "Generated by `tools/generate_rust.py`. These exports were **not**\n"
            "bound because their signature uses a type with no ABI-safe mapping.\n"
            "Emitting a guess would compile and then corrupt memory, so they are\n"
            "skipped deliberately. Hand-wrap them in `runtime.rs` if needed.\n\n"
        )
        if not unhandled:
            fh.write("None -- every export was bound.\n")
        else:
            fh.write(f"{len(unhandled)} unhandled:\n\n")
            fh.write("| export | reason |\n|---|---|\n")
            for f, why in unhandled:
                fh.write(f"| `{f['name']}` | {why} |\n")

    print(f"emitted   : {len(emitted)} bindings -> {out}")
    print(f"unhandled : {len(unhandled)} -> {report}")
    for f, why in unhandled[:10]:
        print(f"    ? {f['name']}: {why}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
