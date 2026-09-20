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
- **REST / MCP / Swagger / DAQ:** 8080 (`api_port`, `api_port disabled` turns it off). See [REST, MCP and Python API](#rest-mcp-and-python-api).
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

**Performance.** Every operation is one short job on the core thread. Waiting (`wait_state`, the rest of `halt`) happens on the API's own threads, not on the core thread. Memory transfers are limited to 64 KB per call. The API subscribes to the core events once and drops events for slow stream clients. With no API client connected, the only extra work is one thread that receives and drops the core events. `load_symbols` runs on an API thread, never on the core thread: loading a large ELF takes seconds and would otherwise freeze run control. With no DAQ session open the sampler costs one `Option` comparison per loop iteration; while one is open, sample decoding happens on a thread per session, so the core thread only moves bytes. The core loop always sleeps: it never busy-waits for a DAQ deadline, and the sampler corrects its own rate instead, so 1 kHz costs ~8 % of a core. A point that is due when the run-state poll runs is read by the poll's own batch instead of costing a second round trip. `cargo build --no-default-features` builds the server without the API (no tokio/axum).

**Security.** There is no authentication. The port binds to `bindto` (localhost by default). Requests with an `Origin` header from other sites are rejected, so a web page cannot drive the debugger. The DAQ WebSocket upgrade goes through the same check, which matters more there: browsers exempt WebSocket from CORS entirely, so the `Origin` check is the only defence. A local client can still start a sustained high-rate read loop on the probe; the caps below and the "no socket, no sampling" rule bound it. The core thread is never busy-waited, so the worst case is probe traffic and one batch of added latency, not a pinned CPU.

Python ([python/](../python)): `pip install -e python`, then:

```python
from aurix_dbg import Client
dbg = Client("http://127.0.0.1:8080")
dbg.halt("tc37x.cpu0")
print(hex(dbg.read_register("tc37x.cpu0", "pc").value))
data = dbg.read_bytes(0x70000000, 256)   # any length, split into 64 KB calls
for ev in dbg.events(): print(ev)       # halts, resumes, ... of all clients
```

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

A point gives exactly one of `expression` or `address` (with `size`), and at most one of `rate_hz` or `interval_us`. `decode: false` streams raw hex. `target` reads through that core's view (HSM-private memory, MCS program RAM) instead of the system bus; those points cannot share the batch, so keep them slow.

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

  `daq` on the console shows, per point, the period asked for, the period the scheduler settled on and the interval actually achieved.

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

## Architecture

| Crate | Responsibility |
|---|---|
| `tas` | Pure-Rust TAS client: batched PL0 reads and writes, device connect/reset, user pins (SRST). |
| `jim` | Jim Tcl 0.83 (OpenOCD's copy) built with `cc`, with a safe `Interp` API. |
| `core` (aurix-core) | Adapter trait (`tas_client`, `sim`), TriCore target (OCDS, halt/resume/step, triggers, TL1), OpenOCD command set and event model, reset sequence, polling, periodic sampling (DAQ), CSA unwinder, flash driver and image loader. |
| `symbols` | DWARF (GHS DWARF 2 and GCC): functions, globals, line table, types, locals with location lists, C expression evaluator with assignment. |
| `tcf` | TCF frontend: wire protocol, services, source-level stepping, expressions, disassembly (Capstone). Also holds the debug session (breakpoints, symbols, stepping state) that the API shares. |
| `api-spec` | The API definition: operations, types, documentation; OpenAPI and Python generators. |
| `api` | REST, MCP, Swagger UI, event stream and the DAQ WebSocket (axum on its own thread); its build script writes the generated files. |
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

- **Offline:** `cargo test --workspace --exclude aurix-dbg`. Covers TAS packet encoding, TCF framing, the real cfg files against the simulator (event order, TL1 writes, resets), DWARF queries and expressions on the GHS ELF, image and flash layout, and Capstone decoding. It also covers the API: consistency of the operation table and the generated OpenAPI document, the shared breakpoint table (`tcf/tests/session.rs`), and REST, MCP, a DAQ WebSocket client and a TCF client on one simulated session (`api/tests/sim_http.rs`). The sampler itself is covered by `core/tests/daq_sim.rs`, which drives the tick with its own clock: rates, coalescing, budget truncation, backpressure, change detection and the socket-scoped lifetime.
- **Hardware:** run through the extension's debug adapter against a running server, with the board free (no OpenOCD):
  - `node tools/dap_smoke.js`: run control, registers, SFR memory, monitor.
  - `node tools/dap_m5.js`: source breakpoint, call stack, step over/into/out.
  - `node tools/dap_m6.js`: locals, register/global/struct writes, enum names, live watch.
  - `node tools/dap_m8.js`: disassembly and reset.
  - `node tools/tcf_smoke.js`: raw TCF services.
  - `node tools/dap_external_bp.js [tcf_port] [api_port]`: API breakpoints in VS Code's Breakpoints view (also runs against the simulator).
  - DAQ: `pip install -e python`, then sample a changing global by name and check the rate and jitter from the `t_us` deltas. Verified on a TC377 board with an application running: every increment of a 5 ms counter caught over 5 s at 1 kHz requested, `float32` and enum globals decoded, and a 1 kHz session left a concurrent REST `read_memory` unchanged (median 21.0 ms either way).

## Not yet implemented

- gdb RSP frontend (contract in [docs/gdb-frontend.md](docs/gdb-frontend.md)).
- DFLASH/UCB programming and PFLASH protection unlock.
- A RAM flash loader.
- A TCF Flash service; flashing goes through the Tcl `program` command.
- Frame names for code without DWARF functions (the frame ID is shown).
- Memory writes to SMPU-protected RAM.
- Target-side DAQ timestamps. Sampling is timed on the host; reading the STM in the same batch would let a client correlate the two clocks and measure the read latency. Jitter-free sampling would need MCDS trace (an ED device) or an agent on the target.
- A DAQ point that follows a pointer. Addresses are resolved once at subscribe time, so `p->x` samples wherever `p` pointed then.
