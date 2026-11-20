# winidea-sdk

Rust bindings for the iSYSTEM **winIDEA** SDK (`isystem.connect`), used by the server's `winidea` backend.

## Which SDK

This crate binds the **C#** SDK's native library `isystemConnect.dll`: the only one with a usable C ABI.

| SDK | Shape | Usable from Rust? |
|---|---|---|
| C++ | 196 MB **static** lib, C++ classes, no `extern "C"` | ✗ |
| Python | `.pyd` exporting only `PyInit__connect` | ✗ needs CPython |
| **C#** | `isystemConnect.dll`, **6,094 flat `extern "C"` exports** | **✓** |

- The DLL is loaded at runtime with `libloading`; nothing is linked at build time, so the crate compiles anywhere.
- No .NET/CLR dependency (only `MSVCP140`, `VCRUNTIME140`, `WS2_32`).
- The C++ SDK cannot give full coverage: its headers cover 76 classes / 567 methods (vs 451 classes here), with no DAQ, coverage, profiler, breakpoint, analyzer or test-case headers. The DAQ data types (`CDAQSample`, `CDAQStatus`) have only `inline` accessors in unshipped headers, so the static lib has no symbols for them.
- The Python SDK's docstrings document some parameter semantics found nowhere else (e.g. `accessFlags`).

## Getting the DLL

`isystemConnect.dll` is proprietary and **not in this repository** (`lib/x64/isystemConnect.dll` is git-ignored).

1. Take it from the winIDEA C# SDK 9.21.417: `winIDEA.CS.SDK-9.21.417`, file `isystem-connect-cs/lib/x64/isystemConnect.dll`.
2. Copy it to `lib/x64/isystemConnect.dll` in this crate, or point an environment variable at it (below).

`WinIdea::load(&default_search_paths())` tries, in order:
1. `AURIX_WINIDEA_DLL`: the DLL file itself;
2. `AURIX_WINIDEA_SDK_PATH`: the directory holding the DLL (set by whoever launches the server);
3. `WINIDEA_HOME`: a winIDEA install (`lib\x64\`, then its root);
4. this crate's `lib/x64/` (path baked in at compile time, so `cargo run --example smoke` needs no arguments);
5. `%ProgramFiles%` / `%ProgramFiles(x86)%`: `iSYSTEM\winIDEA`, `iSYSTEM\winIDEA Open` (`lib\x64\` and root), `iSYSTEM`;
6. the running executable's directory;
7. the OS loader's search path (`PATH`, …).

An entry of `extra` may name the directory or the DLL file.

## Version

**Use winIDEA 9.21.417**, the SDK version these bindings are generated from.
- Exports the loaded DLL lacks bind as `None` (listed by `WinIdea::missing()`); calling one returns `Error::MissingSymbol`. Everything else works.
- A *winIDEA-internal path* error from an otherwise working connection means version skew, not a bad configuration. Example: against winIDEA 9.21.356, DAQ `read()` fails with `Cannot resolve path: .ReadCompressed` (see `daq::DaqController::configure`).

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
for s in daq.read()? {
    println!("t={} idx={}", s.time_ns, s.index);
}
```

- Controllers borrow the `ConnectionMgr` they were built from; the connection cannot be dropped while a controller uses it.
- **Symbol lookup needs a download file.** `data` enumerators and `address` raise `ICONNECT_E_BAD_ID` when no ELF is loaded (`/IDE/Debug.DownloadFiles.DefaultFile == -1`; `data::partitions()` returns empty).
- **Symbol queries are scoped to a download file:** pass `Partition::Default` or `Partition::Current`, not `0` (index 0 is valid only if that partition exists).
- **Raw calls:** after each `WinIdea::raw()` call, call `WinIdea::take_error()`. The SDK reports failures through a callback; ignoring it discards errors.
- **Apply options in a live session** with `IdeController::invoke`.

### Coverage

| Module | Covers |
|---|---|
| `connection` | connect (MRU/id/TCP/pid), disconnect, version, id |
| `config` | connection configuration: which instance, whether to start one (`CConnectionConfig`) |
| `execution` | run, stop, reset, step, run-until, wait, CPU status |
| `debug` | breakpoints (SW + HW) by symbol, memory, registers, evaluate/modify |
| `breakpoints` | breakpoints and watchpoints by address |
| `address` | symbol ↔ address ↔ source line, batch name lookup |
| `data` | bulk memory read/write, symbol-table enumeration |
| `memareas` | which memory area an address belongs to |
| `daq` | configure, enable, read samples, info/status |
| `loader` | programming from winIDEA's download list |
| `session` | the SoC's cores, attaching further cores |
| `ide` | window control, views, option tree, `invoke()` |
| `value` | `CValueType` / `CMemAddress` decoding |
| everything else | 6,094 raw exports via `WinIdea::raw()` (unwrapped) |

Deliberately not wrapped:
- `commitOptions()`: deprecated, *"terminates debugging session in most cases"*; use `IdeController::invoke`.
- `serviceCall()`: superseded by `invoke()`.
- `CDataController::canAccessMemory()`: not available in the SDK.
- `CDebugFacade` run control: use `execution`.

## Smoke test

`examples/smoke.rs` exercises the bindings and prints PASS/FAIL per step. Use it to validate a new machine or SDK version.

```sh
cargo build -p winidea-sdk --example smoke
# -> server/target/debug/examples/smoke.exe
```

```text
smoke.exe                                    # load-only; no winIDEA needed
smoke.exe --dll C:\sdk\...\x64\isystemConnect.dll
smoke.exe --workspace C:\ws\proj.xjrf --symbol main --expression gCounter
smoke.exe -w C:\ws\proj.xjrf --daq gCounter,gState   # + DAQ acquisition
```

- `--daq` needs debug hardware and perturbs the target: attach, configure, enable, read, disable, detach; prints per-item sample counts.
- Exit code 0 only if every step that ran passed (usable in CI).
- Each step prints its label before running: if the process dies, the last line names the culprit.
- Copying `isystemConnect.dll` next to the .exe is enough for the load checks.

## How the bindings are produced

`src/generated/ffi.rs` is generated from the P/Invoke metadata in the managed `IConnectCSLib.dll`, so every signature is what the DLL was compiled with. Generated code is **committed**: building needs neither the SDK nor Python.

```
IConnectCSLib.dll ──ildasm──▶ csil.txt ─┐
                                        ├─▶ extract_pinvoke.py ─▶ pinvoke.json ─▶ generate_rust.py ─▶ src/generated/ffi.rs
isystemConnect.dll ─dumpbin──▶ csexp.txt ┘                                                          └─▶ verify_exports.py
```

Both inputs are needed: `ildasm` truncates string literals at 80 characters, and several SWIG overloads share an 80-character prefix (e.g. `..._getSourceLocationAtPathSource__SWIG_0___` / `__SWIG_1___`). Types come from the IL; export names from the DLL's export table.

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

### `tools/`

| File | Role |
|---|---|
| `enums.json` | **Needed at build time:** `include_str!` into `tests/enum_values.rs` |
| `*.py` | The regeneration pipeline (run only when the SDK changes) |
| `pinvoke.json` | Input to `generate_rust.py`; git-ignored, regenerate |
| `csexp.txt` | Export list, input to `verify_exports.py`; git-ignored, regenerate |
| `csil.txt` | Raw `ildasm` dump (13 MB); git-ignored, regenerate |
| `unhandled.md` | Exports `generate_rust.py` skipped (currently none) |

- `pinvoke.json`, `csexp.txt` and `csil.txt` come from the SDK: run the first two commands of *Regenerating* before the generator or `verify_exports.py`.
- **Skip-and-report:** `generate_rust.py` skips any signature with a type that has no ABI-safe mapping and lists it in `tools/unhandled.md`. All 6,094 exports are bound.

### Enum constants

A wrong discriminant compiles, links and silently selects the wrong option. `tools/extract_enums.py` extracts all 1,702 constants of 263 enum types from the IL, and `tests/enum_values.rs` pins every value the safe wrappers use: an SDK renumbering fails the tests.

Pitfalls the pinned values cover:
- **`EDetachFlags`** is two bit fields (close-server in bits 0-3, autosave in bits 4-5): `DetachFlags` is a struct of two enums.
- **`bHW_acc*`** (hardware breakpoint access) is at **bit 16**, not bits 0-1.
- **`accessFlags`**, the first parameter of every memory/register/expression call, is `IConnectDebug::EAccessFlags`, not a core index.

## Verification

| Check | How |
|---|---|
| Bound symbols exist in the DLL | `verify_exports.py`: 6,094 / 6,094 |
| Signatures match the DLL | taken from its P/Invoke metadata |
| Enum values match the SDK | `tests/enum_values.rs` |
| Load, callbacks, handles, connection, symbol lookup, DAQ on hardware | `examples/smoke.rs` |

Not exercised: the exports reachable only through `WinIdea::raw()`.
