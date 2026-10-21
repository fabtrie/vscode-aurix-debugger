# aurix-dbg

An AURIX TC3xx debug server written in Rust. It talks to the target through Infineon's `tas_server` (miniWiggler, no DAS, no OpenOCD process) or, on the `winidea` backend, through an iSYSTEM **BlueBox** driven by the winIDEA SDK -- see [Backends](#backends-tas_client-sim-winidea). Either way it runs the **unchanged OpenOCD cfg scripts**. Frontends sit on top of a protocol-neutral core. The first frontend is **TCF**, for the VS Code extension `vscode-aurix-tcf-debugger`. A gdb RSP frontend is planned; see [docs/gdb-frontend.md](docs/gdb-frontend.md).

Compared with OpenOCD + gdb + cppdbg, this gives VS Code writable registers and variables, a live watch while the target runs, source breakpoints and stepping on all cores, disassembly, reset, and flashing.

## Build

Requirements (Windows):
- rustup with the `stable-x86_64-pc-windows-gnu` toolchain (pinned in `rust-toolchain.toml`);
- MSYS2 mingw64 gcc in `C:\msys64\mingw64\bin`, which builds Jim Tcl and Capstone;
- network access: the build downloads Jim Tcl
  (crate `jimtcl`) and OpenOCD's Tcl scripts (crate `openocd-tcl`, into `target/openocd-tcl/`) from
  GitHub; set `JIMTCL_SRC_DIR` / `OPENOCD_TCL_DIR` to local copies to build offline.

```powershell
cd server
$env:PATH = "C:\msys64\mingw64\bin;$env:PATH"   # gcc for Jim Tcl and Capstone
cargo build          # target\debug\aurix-dbg.exe
cargo test --workspace --exclude aurix-dbg
```

Stop a running `aurix-dbg.exe` before `cargo build`, because Windows locks the exe.

## Run

The command-line options are the same as OpenOCD's:

```powershell
cd aurix-debugger
.\server\target\debug\aurix-dbg.exe -s server/target/openocd-tcl/tcl `
    -f board/infineon/kit_a2g_tc377tx_3v3_trb.cfg -f tc37x_ocds_init.cfg [-c cmd] [-d3] [-l log.txt]
```

`server/target/openocd-tcl/tcl` holds OpenOCD's board/target scripts, fetched by `cargo build`. The VS Code extension ships the same folder and passes it as a search directory itself.

| Option | Meaning |
|---|---|
| `-f` / `-c` / `-s` | Script, command, search directory; handled in order, like OpenOCD. `init` runs automatically at the end. |
| `-d<n>`, `-l <file>` | Debug level 0..3, log file. |
| `--adapter sim` | Built-in TC37x simulator instead of TAS. Used by the tests. |

Ports:
- **Telnet Tcl console:** 4444 (`telnet_port`).
- **TCF:** 1534 (`tcf_port`).
- **REST / MCP / Swagger / DAQ:** 8080 (`api_port`, `api_port disabled` turns it off). See [REST, MCP and Python API](#rest-mcp-and-python-api).
- **Bind address:** 127.0.0.1 by default (`bindto`).

Send console commands from a script:

```powershell
aurix-dbg.exe --remote 4444 targets "reg pc" "mdw 0x70013954 4"
```

## VS Code

Use a launch configuration of type `aurixdbg`; the extension's [README](../../vscode-aurix-tcf-debugger/README.md#launch-configuration) has an example.

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
| `/api/v1/...` | REST API: targets, run control, registers, memory, [variables by name](#variables-by-name), breakpoints, watchpoints, flash, Tcl. |
| `/api/v1/info` | Liveness probe: server version, API revision, process ID, probe connection, targets, loaded symbol files. Never waits for the core. |
| `/api/v1/os/...` | [AUTOSAR OS awareness](#autosar-os-awareness-orti): the objects an ORTI file describes, read from the running target. |
| `/docs` | Swagger UI; `/openapi.json` is the OpenAPI 3.1 document. |
| `/mcp` | MCP server (Streamable HTTP). Register it with `claude mcp add --transport http aurix http://127.0.0.1:8080/mcp`. |
| `/api/v1/events` | Server-sent events: halts, resumes, resets, breakpoint changes and log lines of all clients. |
| `/api/v1/daq` | WebSocket: [periodic sampling](#periodic-sampling-daq) of variables, each at its own rate. |

**One source of truth.** The operations are declared once in [crates/api-spec/src/ops.rs](crates/api-spec/src/ops.rs), with their request and response types in [types.rs](crates/api-spec/src/types.rs). The `///` comments there are the documentation of all outputs. `cargo build` generates the rest from them:
- the REST routes, the MCP tools and their JSON schemas, and the `DebuggerApi` trait that [crates/api](crates/api) implements;
- [openapi/openapi.json](openapi/openapi.json);
- the Python client's [_generated.py](../python/aurix_dbg/_generated.py).

The generated files are checked in, and a file is only written when its content changes. To add an operation, add a line to `ops.rs` and implement the new trait method; the compiler reports what is missing.

DAQ is deliberately *not* an operation: a streaming session is of no use as an MCP tool and would only hold a socket open, and putting its configuration on the WebSocket is what ties a session's lifetime to its connection. Its protocol is therefore hand-written and specified below instead of in the OpenAPI document.

**Shared session.** REST, MCP and VS Code (TCF) share one session:
- Run control goes through the same core operations and events. A halt from the API shows up in VS Code, and a resume in VS Code shows up in `wait_state` and on the event stream.
- Breakpoints live in one table. Two clients on the same address share one hardware breakpoint. `Breakpoints.set` only replaces the calling client's own breakpoints. API breakpoints stay until they are removed.
- VS Code shows API breakpoints in its Breakpoints view. A breakpoint at the start of a source line is shown as a source breakpoint; any other is shown as a function breakpoint named by symbol or address, e.g. `0x8021a9de`. Removing, disabling or enabling it in VS Code changes it on the server, and removing it through the API removes it from the view. When the debug session ends, the extension takes them out of the view again, so they are not saved with the workspace. VS Code's own function breakpoints accept a symbol, an address or `file:line`.
- Memory and register writes from the API send `Memory.memoryChanged` / `Registers.registerChanged`. The extension then refreshes its variable, watch and register views.
- During a source-level step from VS Code, API `resume`, `step` and `reset` fail with `busy`, and `halt` cancels the step, like the pause button.
- Symbols can be loaded from the API (`load_symbols`), not only by a VS Code debug session. There is one symbol set per core and the last writer wins: a VS Code session started afterwards replaces what the API loaded, and the other way round. `symbols` reports what is loaded.

**Performance.** Every operation is one short job on the core thread, except `info`, which is none: it reads a status the core thread publishes after every job and poll, so it answers while a flash job or a long Tcl script holds the core. Waiting (`wait_state`, the rest of `halt`) happens on the API's own threads, not on the core thread. Memory transfers are limited to 64 KB per call. The API subscribes to the core events once and drops events for slow stream clients. With no API client connected, the only extra work is one thread that receives and drops the core events. `load_symbols` runs on an API thread, never on the core thread: loading a large ELF takes seconds and would otherwise freeze run control. With no DAQ session open the sampler costs one `Option` comparison per loop iteration; while one is open, sample decoding happens on a thread per session, so the core thread only moves bytes. The core loop always sleeps: it never busy-waits for a DAQ deadline, and the sampler corrects its own rate instead, so 1 kHz costs ~8 % of a core. A point that is due when the run-state poll runs is read by the poll's own batch instead of costing a second round trip. `cargo build --no-default-features` builds the server without the API (no tokio/axum).

**Security.** There is no authentication. The port binds to `bindto` (localhost by default). Requests with an `Origin` header from other sites are rejected, so a web page cannot drive the debugger. The DAQ WebSocket upgrade goes through the same check, which matters more there: browsers exempt WebSocket from CORS entirely, so the `Origin` check is the only defence. A local client can still start a sustained high-rate read loop on the probe; the caps below and the "no socket, no sampling" rule bound it. The core thread is never busy-waited, so the worst case is probe traffic and one batch of added latency, not a pinned CPU.

Python ([python/](../python)): `pip install -e python`, then:

```python
from aurix_dbg import Client
dbg = Client("http://127.0.0.1:8080")
dbg.halt("tc37x_ext.cpu0")
print(hex(dbg.read_register("tc37x_ext.cpu0", "pc").value))
data = dbg.read_bytes(0x70000000, 256)   # any length, split into 64 KB calls
for ev in dbg.events(): print(ev)       # halts, resumes, ... of all clients
```

### Variables by name

Globals can be browsed, read and written by name over REST and MCP, without a VS Code session:

```text
GET /api/v1/info                                            liveness probe
GET /api/v1/globals?query=*speed*&offset=0&limit=100        list_globals
GET /api/v1/expressions/children?expression=g_motor         symbol_children
GET /api/v1/expressions?expression=g_motor.speed_rpm        evaluate
PUT /api/v1/expressions  {"expression": "g_motor.mode", "value": "MODE_RUN"}   write_variable
```

```python
dbg.load_symbols(files=[r"C:\build\app.elf"])
for g in dbg.list_globals(query="*speed*").items:
    print(g.expression, g.type_name, g.sampleable)
print(dbg.evaluate(expression="g_motor.speed_rpm").value)
dbg.write_variable(expression="g_motor.mode", value="MODE_RUN")
```

- **Browsing.** `list_globals` matches a case-insensitive glob (`*`, `?`) against the whole name of every global of the loaded symbols and returns one page, sorted by name; `total` counts all matches. The sorted list is built once per symbol set, so a search over 10^5 globals is one pass over the names, and only the page's types are looked up. `symbol_children` lists struct and union members (those of an anonymous member lifted to where C finds them) and array elements, paged the same way. Pointers are not followed: their target can move, so evaluate `*p` or `p->x` instead.
- **Every item says how to use it:** `expression` to pass to the other operations and to a DAQ point, `class` (`uint`, `int`, `float`, `bool`, `enum`, `pointer`, `array`, `struct`, `union`, `function`, `unknown`), `size`, `address`, `has_children`, and `sampleable` - a DAQ point streams it as a typed value (a scalar of at most 8 bytes) rather than hex.
- **An expression names exactly one variable.** Two `static` variables of the same name in different files, or one in the bootmanager ELF as well, are found by name in a fixed order. The ones the name does not find get a symbol reference such as `${E0/GV:12}` as their `expression`, which `evaluate`, `write_variable`, `symbol_children` and DAQ points all accept.
- **The evaluator is the one VS Code uses** (TCF Expressions), without a stack frame: globals, statics, members, elements, casts and arithmetic, while the cores run.
- **Core-local RAM needs its core.** A variable linked to the core-local alias (`0xD000_0000` DSPR, `0xC000_0000` PSPR) exists once per TriCore core, and on the system bus the alias means nothing. Which copy an expression means:
  - a trailing **`@N`** (core ID, as in a Live Watch) names it: `CRASH_state@2`, `g_motor.speed@1`, `${E0/GV:12}@2`. `@` cannot occur in C, so the suffix is unambiguous. It is allowed on any expression; on memory that is not core-local it only selects that core's symbols. `@hsm` evaluates for the HSM. An `@N` that names no TriCore core is `not_found`, one that contradicts `target` a `bad_request`;
  - otherwise, if the ELF is **assigned to exactly one core**, that core's: the plain name works. A file is assigned with `load_symbols(files=["cosi.elf@0"])` (`@N,M` for several), as `symbolFiles` does in VS Code; `info` lists it as `local_cores`. Loading a file without the suffix clears its assignment;
  - otherwise (assigned to several cores, or to none) there is **no single variable**, and the bare name is refused rather than guessed: `` `CRASH_state` is in core-local RAM (0xD0037E8A) and cosi.elf is not assigned to a single core; use `CRASH_state@0`, `CRASH_state@1` or `CRASH_state@2` `` (the file's cores if it has several, else every TriCore core). `list_globals` lists such a variable as a group, `per_core: true` with no `address` and `sampleable: false`; its `symbol_children` are the copies, `core 0` = `CRASH_state@0` and so on, each with its type and address. A struct copy's members keep the suffix: `g_s@2` has `g_s.a@2`.

  `target` selects symbols and the HSM/MCS memory view, not the copy. Every `address` reported is the system-bus one: `CRASH_state@1` at `0xD0000100` is at `0x60000100`. A VS Code Live Watch is unchanged: it has a core, the one it evaluates for.
- **Values are decoded as DAQ decodes them:** `value` is a number, a bool, an enumerator name, `nan`/`inf`/`-inf`, or hex for aggregates and anything over 8 bytes, with the bytes as read in `raw_hex`. A bit field reads its storage unit and reports `bit_offset` and `bit_size`.
- **Writes** take a number, `true`/`false`, an enumerator name, or a decimal or `0x` string (exact for 64-bit values, which a JSON number is not); a struct or array takes its bytes as hex. The value is range-checked against the type or bit field, a bit field is merged into its storage unit, and the variable is read back. Connected VS Code clients get `Memory.memoryChanged`.
- **Errors:** `not_found` (404) without symbols, for an unknown name or an `@N` that names no TriCore core, `bad_request` (400) for an expression that does not parse, a value that does not fit, or core-local RAM that needs an `@N`, `memory` (422) when the target memory cannot be read.
- **`info`** is what a client polls to see that the server is up: `api_version` is an integer raised whenever an operation or field is added or changed, `core_lag_ms` the time since the core thread last finished a job or poll (it grows while one long operation holds it), and `symbols_loading` is set while `load_symbols` parses an ELF.
- Query parameters are URL-encoded as usual; note that `+` in an expression must be sent as `%2B`, or it arrives as a space.

### Periodic sampling (DAQ)

`ws://127.0.0.1:8080/api/v1/daq` samples variables while the target runs, **each at its own rate**, and streams their decoded values. There is no hardware DAQ behind this: TAS offers only MCDS trace (which needs an ED device, not a miniWiggler) and a channel that the target firmware would have to serve. The sampler is host-side polled reads, merged into one batch per tick on the core thread.

**Sampling runs only while the socket is open.** Configuration travels on the same socket, so a session cannot outlive its client: close the socket, or crash, and the session is removed and the target is left alone. There is no reaper and no idle timeout.

Client frames (`id` correlates the reply):

```json
{"op":"subscribe","id":1,"mode":"every_tick","queue":256,"points":[
    {"expression":"g_motor.speed_rpm","rate_hz":1000},
    {"expression":"g_state","interval_us":100000,"name":"state",
     "mode":"on_change","resend_after_us":1000000},
    {"address":"0x70000100","size":4,"rate_hz":200,"name":"ticks"}]}
{"op":"set_rate","id":2,"point":1,"rate_hz":500}
{"op":"set_mode","id":3,"point":2,"mode":"every_tick"}
{"op":"add","id":4,"points":[...]}    {"op":"remove","id":5,"points":[1,2]}
{"op":"pause","id":6}                 {"op":"resume","id":7}   {"op":"status","id":8}
```

A point gives exactly one of `expression` or `address` (with `size`), and at most one of `rate_hz` or `interval_us`. `decode: false` streams raw hex. `target` uses that core's symbols; on the HSM or an MCS channel it also reads through that core's view (HSM-private memory, MCS program RAM) instead of the system bus, and those points cannot share the batch, so keep them slow.

**Core-local RAM is sampled over the system bus.** An `expression` resolves exactly as in [`evaluate`](#variables-by-name), `@N` and the rules for core-local RAM included: `CRASH_state@1` at `0xD0000100` becomes a read of `0x60000100`, which batches with every other point at full speed instead of going through core 1's view. The `config` frame reports that translated `address`. A bare name that needs an `@N` gets the same message as `evaluate` as its point `error`. A TriCore's view differs from the system bus only in that alias, so a raw `address` with a TriCore `target` is translated for that core and read over the bus as well.

Server frames:

```json
{"type":"config","id":1,"start_wall_ms":1758280000000,"points":[
  {"id":1,"name":"g_motor.speed_rpm","address":2147489344,"size":4,
   "requested_interval_us":1000,"interval_us":1000,"rate_hz":1000.0,
   "type_name":"float","encoding":"float","mode":"every_tick"},
  {"name":"typo","requested_interval_us":1000,"interval_us":0,
   "error":"`typo`: unknown symbol"}]}

{"type":"samples","t_us":124000,"points":[1,3],"values":[1450.5,42]}
{"type":"overrun","t_us":125000,"dropped":12}
{"type":"gap","t_us":126000,"missed":98}
{"type":"point_error","point":3,"message":"PL0_ERR_DATA","muted":false}
{"type":"state","t_us":127000,"state":"stalled"}
{"type":"changed","id":2,"point":1,"interval_us":2000,"rate_hz":500.0}
{"type":"error","id":4,"code":"bad_request","message":"..."}
```

Samples are columnar: `points[i]` pairs with `values[i]`, and a tick carries only the points that were due. `t_us` is microseconds since `start_wall_ms`, taken after the batch returns.

- **Rates** are rounded up to a 100 µs grid and every point reports the period it was granted. The grid is that fine so ordinary rates come out exact: on a 1 ms grid, 400 Hz (2500 µs) would have been pulled down to 333 Hz.

- **A rate is a floor, not a grid.** You get **at least** what you asked for; individual samples land where they land.

  A sleep cannot resolve better than the OS timer. On Windows that is ~512 µs whatever you ask for — a request of 500 µs, 700 µs or 900 µs all return after ~1025 µs, and `timeBeginPeriod(1)` does not change it. Aiming a sleep straight at a 1 ms deadline therefore always lands late and delivers ~20 % under the request.

  So the sampler does not aim at the deadline. It schedules on a *shorter* period than you asked for and corrects that period from the rate it actually achieves. The correction needs no model of where the time went — timer overshoot, probe round trip, a run-state poll that got in the way all show up as one number. Measured at 1 kHz: **~990 samples/s for ~8 % of a core**. Samples land where they land (p95 ~3.5 ms, max ~7 ms at 1 kHz), but each carries its own timestamp, so the *placement* is known even when the *spacing* varies. **Do not assume a fixed step between consecutive values** — use `t_us`.

  **What the correction costs: about 4 % of a core.** Aiming a sleep straight at the deadline with no correction delivers 684 samples/s for 7 %; correcting delivers ~990 for ~11 %.

  **Over-delivering does not make it cheaper.** The core loop flattens out around 9 % whatever the rate, but decoding and writing each sample does not, so total CPU tracks the delivered count: 512 samples/s → 6 %, 1004 → 10 %, 1303 → 13 %, 1859 → 17 %. The cheapest way to satisfy "at least 1 kHz" is therefore to land just above it, which is what the controller does; asking for far more only costs more.

  **The ceiling is ~1.5 kHz**, because a sleep of *any* non-zero length costs a full ~512 µs quantum. Above that the delivered rate stops tracking the request; the probe itself would go to ~6 kHz.

  There used to be a second mode, `pacing: exact`, which held the requested period by busy-waiting the core thread below 3 ms. It was removed: measured at 1 kHz it burned 64–68 % of a core to deliver **fewer** samples (868/s) than the sleeping scheduler does at 8 %, and at 100 Hz and 10 Hz the two were indistinguishable. What it bought — evenly spaced samples and the 1.5–6 kHz band — was never worth a busy core. A `pacing` key in a `subscribe` or `add` frame is now rejected rather than ignored, so a script that still sets it is told.

  `daq` on the console shows, per point, the period asked for, the period the scheduler settled on, the interval it *achieved* over its steady ticks, and the interval actually *delivered* — the mean over every sample since the first, stalls included. Only the last is what a client receives: `achieved` leaves stalls out by design, so a sampler that keeps losing whole periods still looks on time there.

- **Windows 11 can take the 1 ms timer away.** A process that leaves its power throttling to the system (every process, unless it opts out) may have its `timeBeginPeriod` request ignored whenever Windows decides it is background work — and a server started hidden by VS Code is exactly that. The request then holds while the server is busy and lapses when it goes quiet, and every wait rounds up to the default 15.6 ms. Seen on a TC38x: a 1 kHz point delivered ~190 samples/s, in bursts of three with a 15.6 ms hole after each, while `sleep 1` on the console still took 1.5 ms. The server therefore opts out at startup (`SetProcessInformation(ProcessPowerThrottling)` with `IGNORE_TIMER_RESOLUTION` and `EXECUTION_SPEED` in the control mask and a zero state): its timer requests are always honoured and it never runs as EcoQoS. If `delivered` sits near 15600 µs anyway, something else is overriding it.

- **Timestamps are trustworthy even when spacing is not.** Checked against the target's own STM clock (`0xF0001010`, 10 ns/tick) sampled in the same batch: the host timestamps track it to within 25 ppm over a run.

- **The run-state poll is merged into the sampler.** `poll_all` of a 20-target configuration (3 cores, HSM, 16 MCS channels) runs every `polling_period` (10 ms) and holds the core thread for ~1.4 ms while its batch is in flight. Any DAQ point that is already due when it starts is queued into *that* batch rather than costing a second round trip right behind it. Measured at 1 kHz, `daq merge 0` versus `1` back to back:

  | 1 kHz | merge off | merge on |
  |---|---|---|
  | delivered | 837–879 samples/s | **989–996 samples/s** |
  | core thread | 7–9 % | 8–10 % |

  At 100 Hz and 10 Hz it makes no measurable difference either way: a DAQ deadline rarely lands inside a poll, and 1.4 ms is a small part of a 10 ms period.

  The DAQ reads are queued *after* the run-state reads on purpose. A PL0 packet marks every access after a failing one "consequential", so a bad DAQ address can never blank the poll it is riding in — and DAQ read errors never reach `mark_poll_failed`. `daq merge 0` on the console turns it off for a comparison.

  **What is left is the poll's own duration**, and it shows up as jitter rather than as a lower rate — the controller schedules around it. Sweeping `polling_period` at 1 kHz:

  | `polling_period` | delivered | p95 spacing |
  |---|---|---|
  | 10 ms (default) | 985 samples/s | 3525 µs |
  | 20 ms | 987 | 2501 µs |
  | 50 ms | 999 | 2085 µs |
  | 200 ms | 1000 | 2054 µs |

  The rate needs no tuning — it is within 1.5 % of the request at every setting. Raising `polling_period` only tightens the spacing, and it plateaus around 2 ms because the residual there is the ~512 µs sleep quantum, not the poll. The cost of raising it is that a halt takes correspondingly longer to be noticed by every frontend, so it is left at 10 ms.

- **What a tick costs is bus accesses, not points.** Timed flat out on a TC377 over a miniWiggler, one tick of N separate 4-byte reads against one read of N words:

  | N | N scattered reads | µs per access | one block of N words | µs per word |
  |---|---|---|---|---|
  | 1 | 123 µs | — | 123 µs | — |
  | 8 | 525 µs | 57.4 | 124 µs | 0.12 |
  | 16 | 983 µs | 57.3 | 126 µs | 0.18 |
  | 32 | 1969 µs | 59.5 | 149 µs | 0.84 |
  | 64 | 3957 µs | 60.9 | 196 µs | 1.17 |

  **~123 µs for the first access, ~57 µs for each one after it, and well under a microsecond per extra byte inside a read already being made.** The per-access figure is flat across the probe's 32-reads-per-packet boundary (`daq` prints it), so it is not packet count, not USB turnaround and not anything the host schedules — it is spent inside the probe. Host-side reordering or pipelining cannot recover it; all due reads already travel in one batch, and `tas::client::execute` already keeps 8 packets in flight.

- **Coalescing gap.** Because one avoided access pays for about a hundred bytes, the sampler reads small holes rather than making a second access. `daq gap <bytes>` sets how far it may reach; the default is 96 B. Twelve points, timed flat out:

  | hole between points | gap 0 | gap 96 | |
  |---|---|---|---|
  | 4 B | 740 µs | 189 µs | **3.9×** |
  | 12 B | 735 µs | 235 µs | **3.1×** |
  | 28 B | 737 µs | 306 µs | **2.4×** |
  | 60 B | 740 µs | 450 µs | **1.7×** |
  | 92 B | 739 µs | 598 µs | 1.2× |
  | 124 B | 753 µs | 756 µs | 1.0× |

  Break-even is a hole of ~105 B, so the default sits just below it and merging can never lose. **The gap is never applied in peripheral space** (`0xF0000000`+), where reading a byte nobody asked for can clear a flag; points that are genuinely adjacent still merge there, because that reads only what was asked for. `daq gap 0` restores the old strictly-adjacent behaviour.

  What this buys at 1 kHz, for variables 32 B apart as a struct would lay them out:

  | points | 32 B apart | 2 KB apart (cannot merge) |
  |---|---|---|
  | 12 | 1022 /s | 1011 /s |
  | 24 | 1013 /s | 579 /s |
  | 48 | 933 /s | 300 /s |

  So laying the sampled variables out together moves the 1 kHz ceiling from ~12 points to ~40. Past that the limit stops being accesses and becomes bytes: a 96-point span 32 B apart is 3 KB per tick, of which seven eighths is hole. Tight layouts stay far better — 128 *adjacent* words is 512 B and sustains 129 000 values/s.

  A stall that does stop the sampler — flashing, a long Tcl script, a reset — is not hidden: it is counted in periods and reported as a `gap`.

- **Adding points does not cost the sampler.** Per-thread CPU at 1 kHz, raw hex:

  | shape | values/s | core thread | decoder thread | WS runtime |
  |---|---|---|---|---|
  | 1 × 4 B | 1 009 | 3.8 % | 0.5 % | 3.3 % |
  | 16 × 32 B | 16 166 | 3.3 % | 4.6 % | 3.3 % |
  | 64 × 8 B | 64 874 | 4.8 % | 6.9 % | 2.8 % |
  | 128 × 4 B | 129 879 | 4.6 % | 8.9 % | 4.3 % |

  **The core thread is flat** at 3–5 % however many points there are — it only moves bytes. Everything that grows is downstream, on the per-session decoder thread and the socket. Typed decoding is not the cost either (`decode: true` is ~1 point of CPU more than raw hex); it is JSON construction. So point count costs stream CPU, never sampling quality.

- **Effect on everyone else.** A REST `read_memory` measured 0.54 ms median with no session, 0.54 ms at 1 kHz, 0.59 ms at 5 kHz, and 1.14 ms with 16 scattered points at 5 kHz — i.e. the worst case adds about one DAQ batch, as intended. The 10 ms run-state poll of a 20-target configuration is *not* the limit either: raising `polling_period` to 100 ms changed the achieved rate by under 10 %.
- **`mode`** is `every_tick` (default) or `on_change`. **On-change saves stream bandwidth, not probe traffic** — the point is still read every period, because that is how a change is noticed. The first read is always delivered as a baseline, and the baseline is dropped on a device reset and on `resume`, so a client never keeps believing a stale value. A quiet `on_change` point sends nothing at all; set `resend_after_us` if a plot needs a floor on the gap. For a bit field the comparison is masked to the field's own bits, so a neighbour in the same storage word does not trigger it.
- **Encodings** come from the DWARF type: `int`, `uint`, `float`, `bool`, `enum` (the enumerator name, or the number if it matches none) and `hex` for aggregates, unknown types and anything over 8 bytes. JSON has no NaN, so a non-finite float is the string `nan`, `inf` or `-inf` rather than `null`; a `uint` above 2^53 is exact in the frame but loses precision in `JSON.parse`, so use `decode: false` for 64-bit counters read by JavaScript.
- **Only globals and statics.** A frame local lives in a register and exists only while the target is halted, so it is rejected at subscribe time. Names need symbols: `load_symbols`, or an open VS Code debug session. A point that fails to resolve gets an `error` and `interval_us: 0`; the rest of the session still runs.
- **Backpressure and gaps.** Each session has a bounded queue (`queue`, default 256 ticks). A client that falls behind loses the newest messages and is told how many with `overrun`. When a tick does not fit the per-tick budget it is truncated and reported as `stalled`, and the points it drops **rotate**, so the budget is shared instead of permanently starving the highest addresses - which is where the peripherals are. Periods lost to a stall are reported as `gap` and never replayed as a burst. Only an interval longer than **four periods** counts: running a little late every tick is what the scheduler corrects for, not loss. At 1 kHz the run-state poll produces a steady trickle (~14 periods/s per point, which is about the gap between the rate asked for and the rate delivered); a burst means something owned the core thread.
- **Caps.** 8 sessions, 256 points per session, 1024 bytes per point, and per tick at most 8 KB and two response packets. A tick over budget is truncated and reports `state: stalled`; the skipped points keep their advanced deadline.

```python
from aurix_dbg import Client
dbg = Client()
dbg.load_symbols(files=[r"C:\build\app_dev.elf"])        # no VS Code needed

with dbg.daq(("g_motor.speed_rpm", 1000),                # 1 kHz, every tick
             {"expression": "g_state", "rate_hz": 10,    # polled at 10 Hz, sent when it changes
              "mode": "on_change", "resend_after_us": 1_000_000},
             {"address": 0x7000B400, "size": 4, "rate_hz": 200, "name": "ticks"}) as daq:
    for p in daq.points:
        print(p)
    for b in daq:
        print(b.t_us, b.values, "DROPPED" if b.dropped else "")
        if b.t_us > 10_000_000:
            break
# leaving the `with` closes the socket, which stops the sampling
```

## AUTOSAR OS awareness (ORTI)

An AUTOSAR/OSEK generator writes an **ORTI file** beside the ELF describing the OS objects: tasks, ISRs, stacks, alarms, resources, spinlocks, events. `load_orti` reads one and the debugger can then show them.

```python
dbg.load_symbols(files=["app.elf"])
dbg.load_orti(file="GenData/Os_Trace.ORT")
for o in dbg.os_objects("TASK", live_only=True).objects:
    print(o.name, {a.name: a.value for a in o.attributes})
```

Both frontends serve it: the REST operations above, and the `AurixOS` [TCF service](#tcf-services) that the VS Code extension uses. They share one loaded file, because they share one session.

Nothing about the OS is built in. The file *declares* its own object types and attributes, so a MICROSAR file's `vs_ISR`, `vs_SPINLOCK` and `vs_OS_CONFIG` come through exactly like the standard OSEK ones. `os` lists whatever was declared.

**Versions.** ORTI 2.1 and 2.2 are the published OSEK/VDX specification. **2.3 is not published**: it is Lauterbach's unofficial SMP extension, where the `OS` attributes become arrays indexed by logical core — `RUNNINGTASK[0]`, `RUNNINGTASK[1]` — so one file describes a multicore system. Vector's generator adds `vs_SMP_NUMCPU` and `vs_COREID[]`, which is what lets a reader map ORTI core indices onto debug targets. Indexed attributes are accepted whatever the `VERSION` block claims. AUTOSAR ARTI is the official successor and is not implemented.

**What a value costs.** Every attribute is a C expression, and they fall into four classes, decided once when the file is loaded:

| Class | Example | Cost per refresh |
|---|---|---|
| prose | `ACTION = "SET_EVENT task ev"` | none — never evaluated |
| constant | `STACK = "&OsCfg_Stack_…"`, `SIZE = "sizeof(…_Dyn)"` | none — resolved at load |
| one address | `RUNNINGTASK[0] = "OsCfg_Trace_OsCore0_Dyn.CurrentTask"` | one read |
| computed | `STATE = "…OsState == 2 ? ( … ? 0 : …State ) : 0xFF"` | the reads the expression makes |

On the MICROSAR file this repo is developed against, that splits 1 522 attributes into 613 one-address, 252 computed, 360 constant and 297 prose — so most of a task table costs nothing to refresh. `live_only=true` skips the two free classes; `reads` in the response reports what the call actually cost.

Only the declaration says which class an attribute is in: `CTYPE` and `ENUM` values are code, `STRING` values are text. Two things follow that are easy to get wrong and are worth knowing when reading the output:

- **Priority enums are an inverse map.** `PRIORITY` is declared `ENUM ["700" = 0, "680" = 1, …]`, so the field on the target holds an *ordinal* and the enumerator name is the priority to display. `vs_HomePriorityInt` is the same field read raw.
- **Enumerator values are often addresses.** `RUNNINGTASK`'s enumerators are `"IdleTask_OsCore0" = "&OsCfg_Trace_IdleTask_OsCore0"`. Those are evaluated against DWARF at load time, which is what turns the sampled pointer into a task name — and into a `link` to the object it names.


**Stack coverage.** `os_stacks` (REST) and `getStacks` (TCF) measure how much of each OS stack has ever been used. The OS fills every stack with a pattern at startup, so the high-water mark is the far end of the region that still holds it — and `SIZE`, `BASEADDRESS`, `STACKDIRECTION` and `FILLPATTERN` are ORTI-specified attribute names, so nothing about the OS is assumed.

This is the one expensive part of OS awareness: finding a mark means reading the untouched part of a stack. It is therefore on demand only, never on a refresh timer, and `names` narrows it to the stacks you care about (measuring one of the 50 stacks in this file costs 2 reads against 100).

A stack the debugger cannot read is reported with its `error` rather than as unused — on this board the SMPU hides CPU0 stack RAM below `0x7000B400` from TAS, and reporting 0% there would be a lie. A pattern-looking hole inside live data is counted as used, so the answer errs high rather than low.




**One live-update loop.** The Live Watch, the AUTOSAR OS view and the OS Objects panel share a single scheduler (`src/liveUpdate.ts` in the extension), so they behave the same way and there is one rate to reason about:

- One interval for all of them (`aurix-debugger.liveWatch.intervalMs`, the `liveWatchIntervalMs` launch option, or the status-bar item). 200 ms to 10 s.
- **A view that is not on screen reads nothing.** A collapsed sidebar section, a hidden tab, a panel in the background: no bus accesses at all. For the OS Profile panel that also stops the sampler, not just the drawing.
- Pause means *only on halt*, not *stop*: a paused view still updates when a core stops, the way the Watch view does. Stops of several cores coalesce into one refresh.
- The next refresh is timed from the start of the previous one, so a slow read delays the next instead of queueing another behind it.

**Sampled task profiling.** `startProfile` puts one [DAQ](#periodic-sampling-daq) point per core on `RUNNINGTASK` in on-change mode: the point is still *read* every period, but only *delivered* when the value differs. Two cores at 100 µs is two reads per tick, coalesced into one bus access, and the stream carries only the switches. `getProfile <sinceUs>` then returns the transitions since the caller last asked, plus the occupancy each task has accumulated — so a panel redrawing four times a second is independent of the sampling rate.

This is **not a trace**. There is no MCDS on a miniWiggler, so a switch shorter than the sampling period is not seen at all, and one that straddles a sample is attributed to whichever task the sample caught. The result is a statistical profile, and the panel says so on its face.

The other views do *not* use DAQ, deliberately. A task table is 613 single-address attributes — well past the 256-point session cap — and refreshing it once a second costs about as much as the sampler would. Sampling is worth its machinery where the rate has to be high; for a table, a one-shot read per refresh is simpler and no more expensive.

**Task-filtered breakpoints.** A breakpoint whose condition reads `task == <name>` (or `task != <name>`, or just a task name) halts only when that task is running. The VS Code extension implements this on top of the awareness: right-click a breakpoint → *Edit Breakpoint* → *Expression*, or use *Halt Only in Task…* from the editor context menu, which offers the tasks of the loaded file.

It is not free. The breakpoint is a plain hardware breakpoint, so the core really does halt on every hit and the adapter resumes it again when the wrong task got there — the same trade-off TRACE32 makes. A hit the adapter cannot attribute to a task is reported rather than swallowed: a stop the user cannot explain is better than one that never arrives.

A conditional attribute's read set depends on which branch is live, so an attribute planned while a core was down is planned again the first time it is read. Loading an ORTI file before the board is powered is therefore harmless; `errors` lists what has not resolved yet.

## Architecture

| Crate | Responsibility |
|---|---|
| `tas` | Pure-Rust TAS client: batched PL0 reads and writes, device connect/reset, user pins (SRST). |
| `jimtcl` | Builds Jim Tcl 0.83 from the upstream release (download, generated `configure` output, `cc`). |
| `jim` | Safe `Interp` API over Jim Tcl. |
| `openocd-tcl` | Fetches OpenOCD's Tcl scripts (pinned commit of the tricore-oss fork) into `target/openocd-tcl/`; used by the tests and packaged with the VS Code extension. |
| `core` (aurix-core) | Adapter trait (`tas_client`, `sim`), TriCore target (OCDS, halt/resume/step, triggers, TL1), OpenOCD command set and event model, reset sequence, polling, periodic sampling (DAQ), CSA unwinder, flash driver and image loader. |
| `symbols` | DWARF (GHS DWARF 2 and GCC): functions, globals, line table, types, locals with location lists, C expression evaluator with assignment. |
| `orti` | ORTI files: the AUTOSAR/OSEK OS awareness description a generator emits. Parser (generic over the object types a file declares), and planning each attribute into the reads it costs. |
| `tcf` | TCF frontend: wire protocol, services, source-level stepping, expressions, disassembly (Capstone). Also holds the debug session (breakpoints, symbols, stepping state) that the API shares. |
| `api-spec` | The API definition: operations, types, documentation; OpenAPI and Python generators. |
| `api` | REST, MCP, Swagger UI, event stream and the DAQ WebSocket (axum on its own thread); its build script writes the generated files. |
| `aurix-dbg` | The executable: OpenOCD-style CLI, telnet console, TCF and API listeners. |
| `winidea-sdk` | Rust bindings to the iSYSTEM winIDEA SDK (`isystemConnect.dll`), loaded at runtime. Used by the `winidea` backend. |

Only the core thread touches the adapter and the Tcl interpreter. Frontends use `ServerHandle::call` (closures run on the core thread) and subscribe to core events.

## Backends (`tas_client`, `sim`, `winidea`)

`adapter driver <name>`, or `--adapter <name>` to override whatever the cfg says. `adapter name` reports the one actually in use, which is how a cfg branches.

| Driver | Probe | Run control |
|---|---|---|
| `tas_client` | miniWiggler through `tas_server` | ours: [`crates/core/src/tricore.rs`](crates/core/src/tricore.rs) writes the OCDS debug registers |
| `sim` | built-in TC37x model | as above, against the model |
| `winidea` | **iSYSTEM BlueBox**, through a winIDEA instance | winIDEA's |

### How the winIDEA backend differs

winIDEA is itself a debugger and owns OCDS, so it will not let a second master drive the debug registers underneath it. The backend therefore sits at two levels:

- **As an `Adapter`** it serves ordinary memory (`DataController::read_memory` / `write_memory`, with `REAL_TIME | DIRECT` so a running target is not disturbed). That alone is enough for DAQ, OS awareness, CSA stack unwinding, expressions, disassembly, `verify_image` and console `mdw` / `mww` — they need memory, not OCDS.
- **As a `RunControl`** (`crates/core/src/target.rs`) it takes over halt, resume, step, registers, breakpoints and watchpoints, so the OCDS register paths are never reached. The hook lives inside `Core`, not in `ops.rs`, because `ops.rs` is not the only route to the silicon: the TCF call stack and source stepping call `Core::read_regs` directly.

Only TriCore cores are taken over. The HSM (through its debug bridge) and the GTM MCS channels use no OCDS registers, so they keep working over plain bus accesses on either backend.

### Tcl on this backend

Unsupported **actions** fail loudly; **declarations** are accepted and reported. The split is forced by the cfg chain: `board/infineon/kit_a2g_tc377tx_3v3_trb.cfg` sources `interface/tas_client.cfg` and sets `adapter speed 5000`, and those are third-party files nobody can add an `if` to.

| | Behaviour |
|---|---|
| `adapter speed`, `reset_config`, `transport select`, `flash bank`, `target create` | accepted, listed by `winidea status` and summarised at `init` |
| `tas-client …`, `tricore tl1_halt` / `tl1_step` / `irq_mask` / `triggers` | error, naming the adapter |
| `flash probe` / `info` / `erase_check` / `erase_sector` | error: winIDEA programs from its own download list, so there is no flash controller for us to drive |
| a `mww` into a core debug register or the CBS trigger fabric | error; reads of the same addresses are allowed, because `tc3xx_connect_reset_halt` and the `tc3xx_stm_status` / `tc3xx_wdt_status` diagnostics read them |

`tc3xx_ocds.cfg` guards itself with `tc3xx_ocds_available` and skips only the OCDS **event wiring** — it still creates every target and still releases the HSM, which is plain SFR and is what starts cpu1 on this board.

### Commands

```
winidea workspace <path>                      # the primary instance's .xjrf (optional, see below)
winidea start if-required|existing|always     # default: if-required
winidea status
```

**Further cores.** winIDEA debugs CPU0 in the window you opened and every other core in a window of its own. The server attaches a core when it examines it, as TAS examines every core at `init`: it looks the core up in winIDEA's session topology (on a TC377 `CPU0`–`CPU2`, `SCR`, `HSM`; TriCore core *n* is `CPU<n>`) and attaches it through the session (`CSessionCtrl::instance_attach`). winIDEA opens that core's window, creating its workspace if the project has none — that is normal. A core whose application has not started it yet (cpu1 right after a reset, or an unused cpu2) has no running core in its window until it is started; it is reported as halted (boot-halt), as over TAS, and switches to running once the application starts it.

**Choosing the primary.** With core windows open several winIDEA instances run side by side, and attaching to an arbitrary one drove the wrong window on hardware. The primary is `winidea workspace` when set; otherwise the running instance the core windows are named after (`Project.xjrf` for `Project_CPU1.xjrf`). Two unrelated projects open at once fail the connect and list them. With nothing running, the start mode decides: `if-required` launches one, `existing` fails. These are the SDK's own modes (`CConnectionConfig`), applied through `ConnectionMgr::connect_with`; the `connectMRU` family ignores them and always launches.

**Registers.** The GPRs, PCXI, PSW and PC are winIDEA registers by name. The other system and debug CSFRs (ICR, FCX, BTV, DBGSR, …) are not in winIDEA's register namespace; they are read from the core's CSFR window instead, and are read-only on this backend.

**The HSM is winIDEA's.** It is core 4 of winIDEA's topology, so `tc3xx_ocds.cfg` creates no HSM target on this backend and does not release it after a halting reset. Examining it from here enables halting debug on it, after which a halt of any TriCore core can stop it through TL1 with nothing to resume it (seen on hardware).

**Synchronous break is winIDEA's.** With winIDEA's multi-core synchronous break on, a halt of one core stops the others (and the HSM, if winIDEA debugs it) through the CBS TL1 capture-and-hold routing. That routing is winIDEA's own setting — it comes back after clearing it — so the server leaves it alone. Code on a halted core that waits for a stopped HSM does not finish a step.

### Flashing

`program`, `flash write_image` and `flash write_images` hand their files to winIDEA (option tree `/IDE/Debug.DownloadFiles`, then `CLoaderController::download`), with **code only and no symbols**: this server keeps its own DWARF reader, and two symbol tables for one target would disagree as soon as the files differ. `verify_image` stays ours on both backends — it reads the image back through ordinary memory, so both report the same thing. `incremental`, `unlock` and the per-sector write statistics are TAS-only.

### Example

```powershell
aurix-dbg.exe --adapter winidea -s server/target/openocd-tcl/tcl `
    -f board/infineon/kit_a2g_tc377tx_3v3_trb.cfg -f tc37x_ocds_init.cfg `
    -c "winidea workspace C:/ws/app.xjrf" -c "init"
```

From VS Code, no new launch attribute is needed — `configCommands` already forwards `-c`:

```json
"configCommands": ["adapter driver winidea", "winidea workspace C:/ws/app.xjrf", "init"]
```

### Status

Run against a BlueBox, winIDEA 9.21.356 and a TC377 (bindings and DLL are 9.21.417; no SDK DLL ships with that winIDEA):

| | Result |
|---|---|
| Attach, instance choice, version | works; the primary is found among its core windows |
| Run control | cpu0 and cpu1: step (PC +4, single-step), resume, halt (debug-request), breakpoint. Halting one core halts the others while winIDEA's synchronous break is on (see above) |
| Watchpoint by address | write watchpoint on a live RAM word halts with reason watchpoint |
| Registers | all 52 read (35 by name, 17 through the CSFR window); writes to the latter refused |
| Memory | reads of a running target, write-back; debug-window reads allowed |
| Reset | `reset halt`, `reset init`, `reset run` |
| HSM | left to winIDEA (see above) |
| Further cores | attached through the session by topology name; cpu1 and cpu2 attached |
| Flashing | **not tried on hardware** — a download replaces the board's firmware |
| DAQ | polled DAQ goes through the same reads; not measured end to end |

**Cost per call** (measured): status 0.16 ms running / 1.8 ms halted, register 40–240 µs, memory read **~4–7 ms regardless of size or access flags**, 64 KB in 75 ms (~0.9 MB/s). winIDEA's own run control is slow, whatever the step: stop ~220 ms, instruction step ~250 ms, source step 200–370 ms, run ~1.2 s — the time is spent inside winIDEA, also with a non-blocking timeout. Three things follow:

- **Source steps are winIDEA's.** Stepping a line instruction by instruction, as over TAS, costs ~250 ms per instruction. When winIDEA has loaded the same ELF itself (its project does), step over / into / out are handed to winIDEA as a whole (`RunControl::source_step`); the log says when they are not.
- **The poll reads little memory.** TriCore cores are asked for their status. `tc3xx_ocds.cfg` creates no MCS targets on this backend: polling all 16 channels took ~220 ms and kept the core thread busy behind every step. Plain-memory cores a cfg does create are polled an eighth per period (`RC_MEMORY_POLL_SHARE`). The poll period is 50 ms on this backend.
- Measured over TCF (VS Code's requests) with both changes: step over 300–540 ms, step into ~450 ms, instruction step ~300 ms, call stack 15–50 ms, pause ~220 ms. Before: step over 750 ms–11 s, pause 1.3 s.

Polled DAQ is bounded by the read cost — roughly 250 reads per second across all points — so winIDEA's hardware DAQ (`DaqController`) is the way to faster sampling; with winIDEA 9.21.356 note the SDK README's warning that its DAQ read fails against 9.21.417 bindings.


## Cfg scripts and other derivatives

The server itself holds no board or application settings. The cfg scripts come in three layers:

| File | Content |
|---|---|
| OpenOCD target cfg (`target/infineon/tc3?x.cfg`, via the board cfg) | TriCore cores with `-coreid`/`-dbgbase`, PFLASH banks. |
| [`tc3xx_ocds.cfg`](../tc3xx_ocds.cfg) | Generic TC3xx multi-core setup: TL1 break/step/continue, STM suspend, HSM and MCS targets, HSM start after a halting reset, `tc3xx_connect_reset_halt`, diagnostics. It takes DBGSR (`-dbgbase` + 0x1FD00) and the per-core CBS_TRCx/STMx/WDTCPUx registers (`-coreid`) from the target cfg. |
| [`tc37x_ocds_init.cfg`](../tc37x_ocds_init.cfg) | A TC377 TriBoard with an application: adapter speed, DFLASH banks, MCS modules used, notes on the application. |

For another board or derivative, copy `tc37x_ocds_init.cfg`, pass the target cfg's chip name to `tc3xx_ocds_setup` and adjust its settings. Options of `tc3xx_ocds.cfg` (set before `tc3xx_ocds_setup`) are listed at the top of the file: `tc3xx_hsm_target`, `tc3xx_mcs_modules`, `tc3xx_mcs_ramsize`, `tc3xx_hsm_autostart`, `tc3xx_hsm_ready_ms`, `tc3xx_gtm_suspend`, `tc3xx_step_timers`.

Console procs: `daq` (what the DAQ sessions are sampling, read-only), `tc3xx_step_timers run|frozen`, `tc3xx_connect_reset_halt ?tries? ?delay_ms?`, `tc3xx_stm_status`, `tc3xx_wdt_status`.

TriCore 1.8 (TC4x) is rejected.

## Tcl commands

The commands the cfg scripts use behave as in OpenOCD. Multi-core policy stays in the cfg files, for example TL1 sync halt, STM suspend and resume/step sequencing in `tc3xx_ocds.cfg`, and the core fires `examine-end`, `halted`, `resume-start/end`, `step-start/end` and the reset events at the OpenOCD points.

- **Configuration:** `adapter`, `tas-client host`, `transport`, `reset_config`, `ifxdap newtap`, `ocmts create`, `target create/names/current/types`, `<target> configure/cget/curstate/invoke-event/arp_*`, `targets`, `flash bank/banks`, `init`, `tcf_port`, `telnet_port`, `gdb_port`, `tcl_port`, `api_port`, `bindto`. Target types: `tricore`, `cortex_m` (HSM), `gtm_mcs` (`-module` 0..9, `-channel` 0..7, `-ramsize <bytes>`: MCS RAM per module, default 12 KB as on the TC37x; it applies to all channels of the module).
- **Execution:** `halt`, `resume`, `step`, `reset [run|halt|init]`, `poll`, `wait_halt`, `reg`, `bp`/`rbp`/`wp`/`rwp`, `mdw/mdh/mdb`, `mww/mwh/mwb`, `read_memory`, `write_memory`, `<target> tricore tl1_halt on|off`, `<target> tricore tl1_step on|off`, `<target> tricore irq_mask off|on|step step_over step_out`, `echo`, `sleep`, `version`, `shutdown`, `daq` (read-only DAQ session status).

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
- **Flash:** `flash info|probe|erase_check|erase_sector|write_image|write_images|verify_image <bank> ...`, `verify_image`, `program <file> [preverify] [verify] [reset] [exit] [offset]`, `aurix_eflash fallback`.
 PFLASH and DFLASH0/1 banks (`tc3x_eflash`); DFLASH banks support `erase_sector` and `erase_check` only, the DF0 bank size is the device's DF0 size (256 KB on the TC377). `tc37x_ocds_init.cfg` defines them as `dfls0` and `dfls1` (HSM data, 128 KB).

Unknown commands fail with `not supported by aurix-dbg`.

## TCF services

Locator, ContextQuery, RunControl (suspend/resume, instruction and source steps), Memory, Registers (read/write), Breakpoints (address, function, file/line), StackTrace (CSA unwinding), LineNumbers, Symbols, Expressions (locals, globals, casts, assign, live evaluation while running), Disassembly (TriCore via Capstone), MemoryMap (loads the ELF), ContextReset, AurixOS ([OS awareness](#autosar-os-awareness-orti)), and Tcl (the debug console `monitor` command).

`AurixOS` carries the ORTI objects on the connection the debug adapter already has, so the extension needs no second port and the awareness dies with the session:

| Command | Arguments | Reply |
|---|---|---|
| `load` | `<file>`, `<target>` or null | the description below |
| `getInfo` | — | `{Loaded, File, Implementation, Version, Cores, Types[{Name, Description, Count, Attributes[{Name, Description, Kind, PerCore, Live}]}], Errors}` |
| `getObjects` | `<kind>`, `<name>` or null, `<liveOnly>` | `{Kind, Objects[{Name, Attributes[{Name, Core?, Value, Number?, Link?, Error?}]}], Reads}` |
| `getRunning` | — | `{Cores[{Core, CoreId, Task, Priority, Isr, LastError}], Reads}` |
| `startProfile` | `<periodUs>` | `{PeriodUs, Cores}` |
| `stopProfile` | — | `{Stopped}` |
| `getProfile` | `<sinceUs>` | `{Running, PeriodUs, NowUs, Transitions[{T, Core, Task}], Totals[{Core, Task, Us}], Dropped, Missed}` |
| `getStacks` | `<names>` or null | `{Stacks[{Name, Base, Size, Direction, Pattern, Used?, Free?, Percent?, Error?}], Reads}` |

`getRunning` is the one the adapter calls on every stop, to name each thread after the task it is running; it is a handful of reads.

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

- **Offline:** `cargo test --workspace --exclude aurix-dbg`. Covers TAS packet encoding, TCF framing, the real cfg files against the simulator (event order, TL1 writes, resets), DWARF queries and expressions on a GHS ELF, image and flash layout, and Capstone decoding. It also covers the API: consistency of the operation table and the generated OpenAPI document, the shared breakpoint table (`tcf/tests/session.rs`), and REST, MCP, a DAQ WebSocket client and a TCF client on one simulated session (`api/tests/sim_http.rs`). The variable operations run there against two MCS assembler files the test writes (their data labels are globals, one shadowing another); struct members and array elements need `AURIX_TEST_ELF`. The sampler itself is covered by `core/tests/daq_sim.rs`, which drives the tick with its own clock: rates, coalescing, budget truncation, backpressure, change detection and the socket-scoped lifetime.
- **Real files:** tests that need build output of a real application skip unless it is named in environment variables: `AURIX_TEST_ELF` (TriCore application ELF), `AURIX_TEST_FUNCTION` (a function in it, default `main`), `AURIX_TEST_ORTI` (its ORTI file), `AURIX_TEST_HSM_ELF`, `AURIX_TEST_APP_ELF` + `AURIX_TEST_BM_ELF` (application and bootloader), `AURIX_TEST_MCS_DIR` (MCS assembler sources with their GHS output).
- **Hardware:** run through the extension's debug adapter against a running server, with the board free (no OpenOCD). The scripts take the ELF and a breakpoint location as arguments or from `AURIX_TEST_ELF` / `AURIX_TEST_BP_LOC`; `dap_m6.js` needs a few more variables, listed in its header:
  - `node tools/dap_smoke.js`: run control, registers, SFR memory, monitor.
  - `node tools/dap_m5.js`: source breakpoint, call stack, step over/into/out.
  - `node tools/dap_m6.js`: locals, register/global/struct writes, enum names, live watch.
  - `node tools/dap_m8.js`: disassembly and reset.
  - `node tools/tcf_smoke.js`: raw TCF services.
  - `node tools/dap_external_bp.js [tcf_port] [api_port]`: API breakpoints in VS Code's Breakpoints view (also runs against the simulator).
  - DAQ: `pip install -e python`, then sample a changing global by name and check the rate and jitter from the `t_us` deltas. Verified on a TC377 board with an application running: every increment of a 5 ms counter caught over 5 s at 1 kHz requested, `float32` and enum globals decoded, and a 1 kHz session left a concurrent REST `read_memory` unchanged (median 21.0 ms either way).

## Not yet implemented

- gdb RSP frontend (contract in [docs/gdb-frontend.md](docs/gdb-frontend.md)).
- On the `winidea` backend: flashing and multi-core attach are implemented but not yet confirmed on hardware (see [Backends](#backends-tas_client-sim-winidea)).
- DFLASH/UCB programming and PFLASH protection unlock.
- A RAM flash loader.
- A TCF Flash service; flashing goes through the Tcl `program` command.
- Frame names for code without DWARF functions (the frame ID is shown).
- Memory writes to SMPU-protected RAM.
- Target-side DAQ timestamps. Sampling is timed on the host; reading the STM in the same batch would let a client correlate the two clocks and measure the read latency. Jitter-free sampling would need MCDS trace (an ED device) or an agent on the target.
- A DAQ point that follows a pointer. Addresses are resolved once at subscribe time, so `p->x` samples wherever `p` pointed then.
