# aurix-dbg

An AURIX TC3xx debug server written in Rust. It talks to the target through Infineon's `tas_server` (miniWiggler, no DAS, no OpenOCD process) and runs the **unchanged OpenOCD cfg scripts**. Frontends sit on top of a protocol-neutral core. The first frontend is **TCF**, for the VS Code extension `vscode-aurix-tcf-debugger`. A gdb RSP frontend is planned; see [docs/gdb-frontend.md](docs/gdb-frontend.md).

Compared with OpenOCD + gdb + cppdbg, this gives VS Code writable registers and variables, a live watch while the target runs, source breakpoints and stepping on all cores, disassembly, reset, and flashing.

## Build

Requirements (Windows):
- rustup with the `stable-x86_64-pc-windows-gnu` toolchain (pinned in `rust-toolchain.toml`);
- MSYS2 mingw64 gcc in `C:\msys64\mingw64\bin`, which builds Jim Tcl and Capstone;
- network access through the proxy set in `.cargo/config.toml` / `env.ps1`.

```powershell
cd server
. .\env.ps1          # PATH (mingw64, cargo) and proxy
cargo build          # target\debug\aurix-dbg.exe
cargo test --workspace --exclude aurix-dbg
```

Stop a running `aurix-dbg.exe` before `cargo build`, because Windows locks the exe.

## Run

The command-line options are the same as OpenOCD's:

```powershell
cd C:\workspace\test\aurix-debugger
.\server\target\debug\aurix-dbg.exe -s third_party/openocd/tcl `
    -f board/infineon/kit_a2g_tc377tx_3v3_trb.cfg -f tc37x_ocds_init.cfg [-c cmd] [-d3] [-l log.txt]
```

| Option | Meaning |
|---|---|
| `-f` / `-c` / `-s` | Script, command, search directory; handled in order, like OpenOCD. `init` runs automatically at the end. |
| `-d<n>`, `-l <file>` | Debug level 0..3, log file. |
| `--adapter sim` | Built-in TC37x simulator instead of TAS. Used by the tests. |

Ports:
- **Telnet Tcl console:** 4444 (`telnet_port`).
- **TCF:** 1534 (`tcf_port`).
- **REST / MCP / Swagger:** 8080 (`api_port`, `api_port disabled` turns it off). See [REST, MCP and Python API](#rest-mcp-and-python-api).
- **Bind address:** 127.0.0.1 by default (`bindto`).

Send console commands from a script:

```powershell
aurix-dbg.exe --remote 4444 targets "reg pc" "mdw 0x70013954 4"
```

## VS Code

Use a launch configuration of type `aurix-tcf` with `"agentFlavor": "aurix-dbg"`. See `.vscode/launch.json`, configuration "AURIX TC37x (aurix-dbg, TCF over TAS)".

| Attribute | Meaning |
|---|---|
| `autoStartAgent`, `agentPath` | Start `aurix-dbg.exe`, or reuse one that is already listening. |
| `searchDirs`, `configFiles`, `configCommands` | Passed as `-s`, `-f`, `-c`. |
| `program` | ELF with debug information (symbols, source breakpoints, variables). |
| `enableFlashing`, `flashFile` | Run `program {flashFile} preverify verify reset` before the session. `flashFile` defaults to `program`. |

Debug console: `monitor <tcl command>`, for example `monitor tc3xx_stm_status`.

## REST, MCP and Python API

One HTTP port (`api_port`, default 8080) serves:

| Path | Content |
|---|---|
| `/api/v1/...` | REST API: targets, run control, registers, memory, breakpoints, watchpoints, flash, Tcl. |
| `/docs` | Swagger UI; `/openapi.json` is the OpenAPI 3.1 document. |
| `/mcp` | MCP server (Streamable HTTP). Register it with `claude mcp add --transport http aurix http://127.0.0.1:8080/mcp`. |
| `/api/v1/events` | Server-sent events: halts, resumes, resets, breakpoint changes and log lines of all clients. |

**One source of truth.** The operations are declared once in [crates/api-spec/src/ops.rs](crates/api-spec/src/ops.rs), with their request and response types in [types.rs](crates/api-spec/src/types.rs). The `///` comments there are the documentation of all outputs. `cargo build` generates the rest from them:
- the REST routes, the MCP tools and their JSON schemas, and the `DebuggerApi` trait that [crates/api](crates/api) implements;
- [openapi/openapi.json](openapi/openapi.json);
- the Python client's [_generated.py](../python/aurix_dbg/_generated.py).

The generated files are checked in, and a file is only written when its content changes. To add an operation, add a line to `ops.rs` and implement the new trait method; the compiler reports what is missing.

**Shared session.** REST, MCP and VS Code (TCF) share one session:
- Run control goes through the same core operations and events. A halt from the API shows up in VS Code, and a resume in VS Code shows up in `wait_state` and on the event stream.
- Breakpoints live in one table. Two clients on the same address share one hardware breakpoint. `Breakpoints.set` only replaces the calling client's own breakpoints. API breakpoints stay until they are removed.
- VS Code shows API breakpoints in its Breakpoints view. A breakpoint at the start of a source line is shown as a source breakpoint; any other is shown as a function breakpoint named by symbol or address, e.g. `0x8021a9de`. Removing, disabling or enabling it in VS Code changes it on the server, and removing it through the API removes it from the view. When the debug session ends, the extension takes them out of the view again, so they are not saved with the workspace. VS Code's own function breakpoints accept a symbol, an address or `file:line`.
- Memory and register writes from the API send `Memory.memoryChanged` / `Registers.registerChanged`. The extension then refreshes its variable, watch and register views.
- During a source-level step from VS Code, API `resume`, `step` and `reset` fail with `busy`, and `halt` cancels the step, like the pause button.

**Performance.** Every operation is one short job on the core thread. Waiting (`wait_state`, the rest of `halt`) happens on the API's own threads, not on the core thread. Memory transfers are limited to 64 KB per call. The API subscribes to the core events once and drops events for slow stream clients. With no API client connected, the only extra work is one thread that receives and drops the core events. `cargo build --no-default-features` builds the server without the API (no tokio/axum).

**Security.** There is no authentication. The port binds to `bindto` (localhost by default). Requests with an `Origin` header from other sites are rejected, so a web page cannot drive the debugger.

Python ([python/](../python)): `pip install -e python`, then:

```python
from aurix_dbg import Client
dbg = Client("http://127.0.0.1:8080")
dbg.halt("tc37x.cpu0")
print(hex(dbg.read_register("tc37x.cpu0", "pc").value))
data = dbg.read_bytes(0x70000000, 256)   # any length, split into 64 KB calls
for ev in dbg.events(): print(ev)       # halts, resumes, ... of all clients
```

## Architecture

| Crate | Responsibility |
|---|---|
| `tas` | Pure-Rust TAS client: batched PL0 reads and writes, device connect/reset, user pins (SRST). |
| `jim` | Jim Tcl 0.83 (OpenOCD's copy) built with `cc`, with a safe `Interp` API. |
| `core` (aurix-core) | Adapter trait (`tas_client`, `sim`), TriCore target (OCDS, halt/resume/step, triggers, TL1), OpenOCD command set and event model, reset sequence, polling, CSA unwinder, flash driver and image loader. |
| `symbols` | DWARF (GHS DWARF 2 and GCC): functions, globals, line table, types, locals with location lists, C expression evaluator with assignment. |
| `tcf` | TCF frontend: wire protocol, services, source-level stepping, expressions, disassembly (Capstone). Also holds the debug session (breakpoints, symbols, stepping state) that the API shares. |
| `api-spec` | The API definition: operations, types, documentation; OpenAPI and Python generators. |
| `api` | REST, MCP, Swagger UI and event stream (axum on its own thread); its build script writes the generated files. |
| `aurix-dbg` | The executable: OpenOCD-style CLI, telnet console, TCF and API listeners. |

Only the core thread touches the adapter and the Tcl interpreter. Frontends use `ServerHandle::call` (closures run on the core thread) and subscribe to core events.

## Cfg scripts and other derivatives

The server itself holds no board or application settings. The cfg scripts come in three layers:

| File | Content |
|---|---|
| OpenOCD target cfg (`target/infineon/tc3?x.cfg`, via the board cfg) | TriCore cores with `-coreid`/`-dbgbase`, PFLASH banks. |
| [`tc3xx_ocds.cfg`](../tc3xx_ocds.cfg) | Generic TC3xx multi-core setup: TL1 break/step/continue, STM suspend, HSM and MCS targets, HSM start after a halting reset, `tc3xx_connect_reset_halt`, diagnostics. It takes DBGSR (`-dbgbase` + 0x1FD00) and the per-core CBS_TRCx/STMx/WDTCPUx registers (`-coreid`) from the target cfg. |
| [`tc37x_ocds_init.cfg`](../tc37x_ocds_init.cfg) | This project (TC377 TRB board, app): adapter speed, DFLASH banks, MCS modules used, notes on the application. |

For another board or derivative, copy `tc37x_ocds_init.cfg`, pass the target cfg's chip name to `tc3xx_ocds_setup` and adjust its settings. Options of `tc3xx_ocds.cfg` (set before `tc3xx_ocds_setup`) are listed at the top of the file: `tc3xx_hsm_target`, `tc3xx_mcs_modules`, `tc3xx_mcs_ramsize`, `tc3xx_hsm_autostart`, `tc3xx_hsm_ready_ms`, `tc3xx_gtm_suspend`, `tc3xx_step_timers`.

Console procs: `tc3xx_step_timers run|frozen`, `tc3xx_connect_reset_halt ?tries? ?delay_ms?`, `tc3xx_stm_status`, `tc3xx_wdt_status`.

TriCore 1.8 (TC4x) is rejected.

## Tcl commands

The commands the cfg scripts use behave as in OpenOCD. Multi-core policy stays in the cfg files, for example TL1 sync halt, STM suspend and resume/step sequencing in `tc3xx_ocds.cfg`, and the core fires `examine-end`, `halted`, `resume-start/end`, `step-start/end` and the reset events at the OpenOCD points.

- **Configuration:** `adapter`, `tas-client host`, `transport`, `reset_config`, `ifxdap newtap`, `ocmts create`, `target create/names/current/types`, `<target> configure/cget/curstate/invoke-event/arp_*`, `targets`, `flash bank/banks`, `init`, `tcf_port`, `telnet_port`, `gdb_port`, `tcl_port`, `api_port`, `bindto`. Target types: `tricore`, `cortex_m` (HSM), `gtm_mcs` (`-module` 0..9, `-channel` 0..7, `-ramsize <bytes>`: MCS RAM per module, default 12 KB as on the TC37x; it applies to all channels of the module).
- **Execution:** `halt`, `resume`, `step`, `reset [run|halt|init]`, `poll`, `wait_halt`, `reg`, `bp`/`rbp`/`wp`/`rwp`, `mdw/mdh/mdb`, `mww/mwh/mwb`, `read_memory`, `write_memory`, `<target> tricore tl1_halt on|off`, `<target> tricore tl1_step on|off`, `<target> tricore irq_mask off|on|step step_over step_out`, `echo`, `sleep`, `version`, `shutdown`.

## Multi-core stepping (`tricore tl1_step`)

With `<target> tricore tl1_step on` (set by `tc3xx_ocds.cfg`), an instruction step runs **all** cores for the stepped instruction, not only the stepping one:

1. The core makes sure TL1 is latched (capture-and-hold), pulsing it if needed.
2. The cfg's `step-start` releases the other cores' debug halt; TL1 still suspends them.
3. The core arms the step trigger, releases the stepping core (still held by TL1) and clears the TL1 hold: all cores start in the same cycle.
4. The step trigger halts the core and pulses BRKOUT: TL1 latches and stops all cores in the same cycle.

Without `tl1_step` (OpenOCD), the other cores stay in debug halt and only the stepping core runs.

**STMs.** On the TC377 an STM with `OCS.SUS=2` stops while its CPU is suspended by TL1 *or* has `DBGSR.SUSP` set (measured with raw register writes). A trigger event loads `DBGSR.SUSP` from `TRxEVT.SUSP`; the core carries the current SUSP into the step release and the step trigger, so the cfg decides:

- `tc3xx_step_timers run` (default): SUSP stays clear. TL1 stops and restarts the STMs in the same cycle as the cores, so they stop at halt, run with continue, and advance together while the cores execute a step.
- `tc3xx_step_timers frozen`: SUSP is set on every core while halted, so the STMs also stay stopped during steps.

Switch at runtime with `monitor tc3xx_step_timers frozen` or `run`.

## Interrupts while stepping (`tricore irq_mask`)

`<target> tricore irq_mask step step_over step_out` makes steps run with interrupts masked, like winIDEA's `DisableIRQ_Step`, `DisableIRQ_StepOver` and `DisableIRQ_RunUntilReturn`. `tc3xx_ocds.cfg` enables all three for every core. `tricore irq_mask off` (cfg, `configCommands` or `monitor`) turns it off.

- **When:** a step of a selected kind (instruction step or step into = `step`, source step over = `step_over`, step out = `step_out`) clears ICR.IE on that core. ICR.IE stays cleared while halted between steps. Continue restores it on every core before the cfg releases them.
- **What the application sees:** reading ICR (Registers view, `reg icr`) shows the application's IE, not the masked value. Writing ICR while masked sets the application's IE. `mdw` on the CSFR address shows the real chip value.
- **Instructions that change IE:** a stepped `enable`, `disable`, `disable Dn`, `restore Dn`, `rfe`, `bisr` or `syscall`/trap updates the application's IE instead of being undone on continue. `disable Dn` gets the application's IE in Dn. A trap or `bisr` saves the application's IE in PCXI.PIE, so its RFE restores the right value.
- **Hardware findings (TC377, TC1.6.2P):** CALL copies ICR.IE into PCXI.PIE and leaves ICR.IE unchanged. RET does not restore ICR.IE from PIE. Masked calls therefore leave nothing behind. Only contexts restored by RFE (traps, interrupts) need PIE fixed.
- **Limit:** during a masked step over or step out, the callee runs with interrupts masked. If it disables interrupts and does not restore them before the step ends, the debugger can't tell and restores the IE from before the step on continue.
- **Flash:** `flash info|probe|erase_check|erase_sector|write_image|write_images|verify_image <bank> ...`, `verify_image`, `program <file> [preverify] [verify] [reset] [exit] [offset]`, `aurix_eflash fallback`. PFLASH and DFLASH0/1 banks (`tc3x_eflash`); DFLASH banks support `erase_sector` and `erase_check` only, the DF0 bank size is the device's DF0 size (256 KB on the TC377). `tc37x_ocds_init.cfg` defines them as `dfls0` and `dfls1` (HSM data, 128 KB).

Unknown commands fail with `not supported by aurix-dbg`.

## TCF services

Locator, ContextQuery, RunControl (suspend/resume, instruction and source steps), Memory, Registers (read/write), Breakpoints (address, function, file/line), StackTrace (CSA unwinding), LineNumbers, Symbols, Expressions (locals, globals, casts, assign, live evaluation while running), Disassembly (TriCore via Capstone), MemoryMap (loads the ELF), ContextReset, and Tcl (the debug console `monitor` command).

Context IDs:
- **Chip:** `S0.D0`.
- **Core:** `S0.D0.C<coreid>`.
- **Stack frame:** `…C0.F<n>`.

## Flashing (TC3xx PFLASH, `tc3x_eflash`)

The driver runs the DMU command sequences over the debug interface, without a RAM loader. All cores are halted first.

- **`flash write_image [erase] [incremental] <file>`:** DMU-erased sectors are programmed directly. Other touched sectors are erased first, which requires `erase`. `incremental` skips sectors whose image pages already match, but it reads flash, and reading unprogrammed PFLASH raises an ECC error that the application's SMU may turn into a reset.
- **`flash write_images [erase] [incremental] [unlock] [verify] <file> ...`:** Merges the files into one image (where they overlap, later files win) and writes it like `flash write_image`. Writing the files one by one would erase data of earlier files in shared 16 KB sectors, and incremental writes would reprogram those sectors every time. With `incremental`, `verify` reads back only the programmed sectors, since the unchanged ones were just compared; otherwise the whole image. Reading runs at about 470 KB/s at `adapter speed 5000` and about 2 MB/s at 30000 (set by `tc37x_ocds_init.cfg`), so comparing large images dominates. The VS Code extension flashes `flashFiles` with this command (`verifyFlash` selects `verify`).
- **`verify_image`:** reads only the image bytes. Flash is read through the non-cached 0xA segment.
- **`program … preverify`:** leaves flash untouched when the image already matches.
- **Supported images:** ELF (load segments), Intel HEX, raw binary. UCB banks are not supported.
- **Banks:** the bank layout (busy bit, physical sectors) is looked up by `SCU_CHIPID.CHID`. TC36x/37x/38x/39x/3Ex use a table, like OpenOCD's `aurix_eflash`. Other TC3xx variants (e.g. TC33x, TC35x) use the common scheme with a warning: PFn at 0x80000000 + n × 3 MB, up to 3 MB, busy bit 2 + n. A `flash bank` may be smaller than its table entry, for devices with less flash than the largest one in their family. DFLASH0 (0xAF000000, up to 1 MB) and DFLASH1 (0xAFC00000, 128 KB) are the same on all variants.

## Tests

- **Offline:** `cargo test --workspace --exclude aurix-dbg`. Covers TAS packet encoding, TCF framing, the real cfg files against the simulator (event order, TL1 writes, resets), DWARF queries and expressions on the GHS ELF, image and flash layout, and Capstone decoding. It also covers the API: consistency of the operation table and the generated OpenAPI document, the shared breakpoint table (`tcf/tests/session.rs`), and REST, MCP and a TCF client on one simulated session (`api/tests/sim_http.rs`).
- **Hardware:** run through the extension's debug adapter against a running server, with the board free (no OpenOCD):
  - `node tools/dap_smoke.js`: run control, registers, SFR memory, monitor.
  - `node tools/dap_m5.js`: source breakpoint, call stack, step over/into/out.
  - `node tools/dap_m6.js`: locals, register/global/struct writes, enum names, live watch.
  - `node tools/dap_m8.js`: disassembly and reset.
  - `node tools/tcf_smoke.js`: raw TCF services.
  - `node tools/dap_external_bp.js [tcf_port] [api_port]`: API breakpoints in VS Code's Breakpoints view (also runs against the simulator).

## Not yet implemented

- gdb RSP frontend (contract in [docs/gdb-frontend.md](docs/gdb-frontend.md)).
- DFLASH/UCB programming and PFLASH protection unlock.
- A RAM flash loader.
- A TCF Flash service; flashing goes through the Tcl `program` command.
- Frame names for code without DWARF functions (the frame ID is shown).
- Memory writes to SMPU-protected RAM.
