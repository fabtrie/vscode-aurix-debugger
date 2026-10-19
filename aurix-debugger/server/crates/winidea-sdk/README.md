# winidea-sdk

Rust bindings for the iSYSTEM **winIDEA** SDK (`isystem.connect`).

## Which SDK this binds, and why

iSYSTEM ships three SDKs. This crate binds the **C#** one — specifically its
native library `isystemConnect.dll` — because it is the only one that exposes a
usable C ABI.

| SDK | Shape | Usable from Rust? |
|---|---|---|
| C++ | 196 MB **static** lib, C++ classes, no `extern "C"` | ✗ |
| Python | `.pyd` exporting only `PyInit__connect` | ✗ needs CPython |
| **C#** | `isystemConnect.dll`, **6,094 flat `extern "C"` exports** | **✓** |

SWIG's **C#** backend emits a plain C wrapper per method, so the DLL is an
ordinary dynamic library — loaded at runtime with `libloading`, exactly like
`vector-driver` loads `vxlapi64.dll`. It has **no .NET/CLR dependency** (only
`MSVCP140` / `VCRUNTIME140` / `WS2_32`). SWIG's **Python** backend hides
everything behind `PyInit__connect`, which is why the Python SDK is a dead end
for FFI.

### Where the DLL comes from

`isystemConnect.dll` is iSYSTEM's proprietary library and is **not part of this
repository** (`lib/x64/isystemConnect.dll` is git-ignored). Get it from the
winIDEA C# SDK (`winIDEA.CS.SDK-9.21.417`, file
`isystem-connect-cs/lib/x64/isystemConnect.dll`) and copy it to
`lib/x64/isystemConnect.dll` in this crate, or point `WINIDEA_HOME` at a
winIDEA installation. It is loaded at runtime with `libloading` (nothing is
linked at build time), so this crate still compiles anywhere — the DLL only has
to exist when a connection is opened.

Use the SDK version these bindings were generated from: a mismatched winIDEA
fails with an obscure *winIDEA-internal path* error rather than a missing symbol
(see the `.ReadCompressed` note on `daq::DaqController::configure`).

Resolution order is in `default_search_paths()`, most specific first:

1. `AURIX_WINIDEA_SDK_PATH` — set by Electron main to the bundled copy
   (`resources/bin/winidea` packaged, `crates/winidea-sdk/lib/x64` in dev),
   mirroring how `AURIX_UV_PATH` works for uv.
2. `WINIDEA_HOME` — a user-pointed install.
3. This crate's own `lib/x64/`, baked in via `CARGO_MANIFEST_DIR`, so
   `cargo run --example smoke` works with no arguments.
4. A winIDEA installation under Program Files.
5. Next to the running executable.

### Why not the C++ SDK

Even with a hand-written `extern "C"` shim it cannot reach full coverage:

* Headers cover only **76 classes / 567 methods**, vs **451 classes** here. No
  DAQ, coverage, profiler, breakpoint, analyzer or test-case headers at all.
* The gap is not recoverable from the static lib. The DAQ *data* types
  (`CDAQSample`, `CDAQStatus`) have their accessors defined `inline` in headers
  that were never shipped, so they have **zero linkable symbols** — you could
  call `read()` but never decode a sample. Those same accessors are ordinary
  exports in `isystemConnect.dll`.

The C++ SDK is **not** vendored here — it was 1.3 GB of static libs that nothing
references, and it cannot deliver the coverage this crate needs. Everything
learned from its headers is captured in the code comments and this file. The
original drop is `winIDEA.CPP.SDK-*` if it is ever needed for reference.

## How the bindings are produced

`src/generated/ffi.rs` is generated from the **P/Invoke metadata inside the
managed `IConnectCSLib.dll`**. Every signature is what the DLL was actually
compiled with — nothing is inferred from docs or guessed from mangled names.

```
IConnectCSLib.dll ──ildasm──▶ csil.txt ─┐
                                        ├─▶ extract_pinvoke.py ─▶ pinvoke.json ─▶ generate_rust.py ─▶ src/generated/ffi.rs
isystemConnect.dll ─dumpbin──▶ csexp.txt ┘                                                          └─▶ verify_exports.py
```

Both inputs are required. **`ildasm` truncates string literals at 80
characters**, and several SWIG overloads share an 80-char prefix — e.g. both
`..._getSourceLocationAtPathSource__SWIG_0___` and `__SWIG_1___` appear in the IL
as the same truncated name. Types therefore come from the IL, while export
*names* are resolved against the DLL's own (authoritative) export table.

### Regenerating

```sh
ILDASM="/c/Program Files (x86)/Microsoft SDKs/Windows/v10.0A/bin/NETFX 4.8 Tools/ildasm.exe"
"$ILDASM" /TEXT /NOBAR IConnectCSLib.dll > tools/csil.txt
dumpbin /EXPORTS isystemConnect.dll      > tools/csexp.txt   # needs MSYS_NO_PATHCONV=1 in Git Bash

uv run --script tools/extract_pinvoke.py tools/csil.txt tools/csexp.txt tools/pinvoke.json
uv run --script tools/generate_rust.py   tools/pinvoke.json src/generated/ffi.rs
uv run --script tools/extract_enums.py   tools/csil.txt enums.json
uv run --script tools/verify_exports.py  src/generated/ffi.rs tools/csexp.txt
cargo test -p winidea-sdk    # enum drift guard
```

### What's in `tools/`

| File | Role |
|---|---|
| `enums.json` | **Required at build time** — `include_str!` into `tests/enum_values.rs` |
| `*.py` | The regeneration pipeline (only run when the SDK is updated) |
| `pinvoke.json` | Input to `generate_rust.py` — **gitignored**, regenerate on demand |
| `csexp.txt` | Export list; input to `verify_exports.py` — **gitignored**, regenerate on demand |
| `csil.txt` | 13 MB raw `ildasm` dump — **gitignored**, regenerate on demand |

Nothing here participates in `cargo build`, so the crate compiles with no Python
and no SDK installed. The one exception is `enums.json`, which a test embeds.
`pinvoke.json`, `csexp.txt` and `csil.txt` are derived from the iSYSTEM SDK and
not committed; run the first commands under *Regenerating* to recreate them
before running the generator or `verify_exports.py`.

### Enum constants

Discriminants are the one place a wrapper can be *silently* wrong: a bad symbol
name fails at load, a bad signature is ruled out by the P/Invoke metadata, but a
bad integer just selects the wrong sampling rate and reports success.
`tools/extract_enums.py` pulls all **1,702 constants across 263 enum types**
verbatim out of the IL, and `tests/enum_values.rs` pins every value the safe
wrappers depend on — so an SDK renumber fails the build instead of the hardware.

This has already caught three real bugs, none of which would have failed to
compile or link:

* **`EDetachFlags`** looked like a simple keep/close pair but is two bitfields
  (close-server in bits 0-3, autosave in bits 4-5). `DetachFlags` is now a struct
  of two enums.
* **`bHW_acc*`** (hardware-breakpoint access) lives at **bit 16**, not bits 0-1.
  A "read" watchpoint encoded as `1` would have set an unrelated flag.
* **`accessFlags`**, the first parameter of every memory/register/expression
  call, is `IConnectDebug::EAccessFlags` — *not* a core/context index, as the
  positional `int32` suggests. Only the Python SDK's prose says so.

The last one is why the Python SDK stays useful even though it can't provide
types: its docstrings are the only place some parameter *semantics* are written
down.

Generated code is **committed**, so building needs neither the SDK nor Python.

### Skip-and-report

`generate_rust.py` refuses to guess: any signature using a type without an
ABI-safe mapping is skipped and listed in `tools/unhandled.md` rather than
emitted. A plausible-looking wrapper for a misunderstood type would compile and
then corrupt memory. Currently **0 unhandled** — all 6,094 exports are bound.

## Verification status

| Check | Result |
|---|---|
| Bound symbols exist in the DLL | ✅ 6,094 / 6,094, none missing, none unbound |
| Signatures match the DLL | ✅ taken from its own P/Invoke metadata |
| Enum values match the SDK | ✅ pinned by `tests/enum_values.rs` |
| `cargo build` / `clippy` / `test` | ✅ clean |
| Symbols resolve against the real DLL | ✅ all 6,094, at runtime |
| SWIG callbacks accepted | ✅ registered without incident |
| String callback (`malloc`/`free`) | ✅ round-tripped a real string |
| Exception callback | ✅ a real C++ throw became a Rust `Err` |
| Handle construct + destruct | ✅ a real destructor ran via `Owned` |
| Connected to a running winIDEA | ✅ `examples/smoke.rs`, 21/21 passed |
| `CMemAddress` decode on a real target | ✅ `function_address("main")` |
| **DAQ acquisition from hardware** | ✅ real samples via `CDAQSample` accessors |

Everything the smoke test covers is empirically verified, not merely reasoned
about. Still unexercised: the ~6,000 exports reachable only through
`WinIdea::raw()`.

> **Match the winIDEA version to the SDK.** These bindings are generated from
> the **9.21.417** C# SDK. Against winIDEA **9.21.356**, DAQ `read()` failed with
> `Cannot resolve path: .ReadCompressed` even though the connection was healthy
> and DAQ worked in the winIDEA UI; on 9.21.417 the same code passed. A
> *winIDEA-internal path* error from an otherwise-working connection means
> version skew, not a bad configuration.

## Smoke test

`examples/smoke.rs` builds a standalone .exe that exercises the above and prints
PASS/FAIL per step. It is the fastest way to validate a new machine or a new SDK
version.

```sh
cargo build -p winidea-sdk --example smoke --profile dev-release
# -> backend/target/dev-release/examples/smoke.exe   (single self-contained .exe)
```

```text
smoke.exe                                    # load-only; no winIDEA needed
smoke.exe --dll C:\sdk\...\x64\isystemConnect.dll
smoke.exe --workspace C:\ws\proj.xjrf --symbol main --expression gCounter
smoke.exe -w C:\ws\proj.xjrf --daq gCounter,gState   # + DAQ acquisition
```

`--daq` is opt-in: it needs real debug hardware and perturbs the target, so it
never runs unless asked. It exercises the full cycle (attach → configure →
enable → read → disable → detach) and prints per-item sample counts.

Exit code is 0 only if every step that ran passed, so it drops straight into CI.
Each step prints its label *before* doing the work: if the process dies outright,
the last line names the culprit.

Copying `isystemConnect.dll` next to the .exe is enough — `default_search_paths`
checks the executable's own directory, so no install is required to test the
loading half.

## Using it

```rust
use winidea_sdk::{WinIdea, ConnectionMgr, daq::{DaqController, SamplingFlags}};

let winidea = WinIdea::load(&winidea_sdk::default_search_paths())?;

let mut conn = ConnectionMgr::new(&winidea)?;
conn.connect_mru("C:/ws/MyProject.xjrf")?;
println!("winIDEA {}", conn.version()?);

let mut daq = DaqController::new(&winidea, &conn)?;
daq.attach_hw()?;
daq.config_add("gCounter", SamplingFlags::Every1ms)?;
daq.enable_global(true)?;
for s in daq.read(256)? {
    println!("t={} idx={}", s.time_ns, s.index);
}
```

The DLL is found via `AURIX_WINIDEA_DLL`, then any caller-supplied directories,
then the OS loader's search path.

Controllers borrow the `ConnectionMgr` they were built from, so the connection
cannot be dropped while a controller still uses it.

### Coverage

| Module | Covers | Status |
|---|---|---|
| `connection` | connect (MRU/id/TCP/pid), disconnect, version, id | ✅ |
| `execution` | run, stop, reset, step, run-until, wait, CPU status | ✅ |
| `debug` | breakpoints (SW + HW), memory, registers, evaluate/modify | ✅ |
| `address` | symbol ↔ address ↔ source line, batch name lookup | ✅ |
| `data` | bulk memory read/write, symbol-table enumeration | ✅ |
| `daq` | configure, enable, read samples, info/status | ✅ |
| `ide` | window control, views, option tree, `invoke()` | ✅ |
| `value` | `CValueType` / `CMemAddress` decoding | ✅ |
| everything else | 6,094 raw exports via `WinIdea::raw()` | ⚙️ unwrapped |

Three SDK calls are deliberately **not** wrapped, because they are traps:
`commitOptions()` is deprecated and *"terminates debugging session in most
cases"*, `serviceCall()` is superseded by `invoke()`, and
`CDataController::canAccessMemory()` is retired ("no longer implemented"). Use
`IdeController::invoke` to apply options during a live session.

**Symbol lookup needs a download file.** The `data` enumerators and everything
in `address` raise `ICONNECT_E_BAD_ID` when no ELF is loaded — check
`/IDE/Debug.DownloadFiles.DefaultFile == -1`, or call `data::partitions()`,
which returns empty. That is winIDEA reporting an empty symbol table, not a
binding fault.

Symbol queries are also **scoped to a download file**: pass `Partition::Default`
or `Partition::Current`, not a bare `0` — index 0 is only valid if a partition
with that number exists.

`CDebugFacade` also duplicates run control; the wrapper deliberately omits those
so there is one obvious way to do each thing (`execution`).

When using `WinIdea::raw()`, call `WinIdea::take_error()` after each call — the
SDK reports failures through a callback rather than by unwinding, so ignoring it
silently discards errors.
