# aurix-dbg

AURIX TC3xx debug server in Rust. It runs the unchanged OpenOCD cfg scripts and talks to the target through:
- Infineon's `tas_server` (miniWiggler; no DAS, no OpenOCD process);
- an iSYSTEM BlueBox, directly over USB or through winIDEA (see [Backends](#backends)).

Frontends: TCF (for the VS Code extension), REST, MCP, a Python client and a telnet Tcl console. A gdb RSP frontend is planned: [docs/gdb-frontend.md](docs/gdb-frontend.md).

## Build

Requirements (Windows):
- rustup with the `stable-x86_64-pc-windows-gnu` toolchain (pinned in `rust-toolchain.toml`);
- MSYS2 mingw64 gcc in `C:\msys64\mingw64\bin` (builds Jim Tcl and Capstone);
- network access: the build downloads Jim Tcl (crate `jimtcl`) and OpenOCD's Tcl scripts (crate `openocd-tcl`, into `target/openocd-tcl/`) from GitHub. To build offline, set `JIMTCL_SRC_DIR` / `OPENOCD_TCL_DIR` to local copies.

```powershell
cd server
$env:PATH = "C:\msys64\mingw64\bin;$env:PATH"   # gcc for Jim Tcl and Capstone
cargo build          # target\debug\aurix-dbg.exe, aurix-hub.exe
cargo test --workspace --exclude aurix-dbg
```

- Stop a running `aurix-dbg.exe` before `cargo build`: Windows locks the exe.
- `cargo build --no-default-features` builds the worker without the API (no tokio/axum).

## Run

Two programs:

| Program | Role |
|---|---|
| `aurix-hub` | One per machine, port **3001**. MCP, REST and Swagger UI for every client (Claude, VS Code, scripts); opens and closes sessions. Never touches a probe. |
| `aurix-dbg` | The worker of one board (one *session*): one Jim interpreter, one set of cfg scripts, one probe. Started by the hub; listens on free loopback ports and registers them with the hub. |

- A crashing worker takes down only its own board.
- A restarted hub keeps the workers; they register again.

```text
Claude (http) ─────────────────┐
Claude (stdio) ─ aurix-hub stdio ┤   http://127.0.0.1:3001  (token for other machines)
Python / curl / aurix-hub tcl ───┤
VS Code extension ───────────────┤
                                 ▼
                         aurix-hub ── spawn, proxy ──► aurix-dbg (board X)   aurix-dbg (board Y)
                                                        ▲ TCF: VS Code (direct, or relayed by the hub)
```

Start the hub by hand (the extension and `aurix-hub stdio` start it when needed; for a start at logon see [Installation](#installation)):

```powershell
.\server\target\debug\aurix-hub.exe serve --foreground     # Ctrl-C stops it
```

Open a session from a **profile** (`%APPDATA%\aurix-dbg\profiles\<name>.toml`) or from cfg scripts, through MCP (`open_session`), REST or Python:

```toml
# %APPDATA%\aurix-dbg\profiles\tc375-lite.toml
[connect]
config_files = ["board/infineon/kit_a2g_tc375_lite.cfg", "C:/work/my_board.cfg"]
commands = []          # -c
search_dirs = []       # -s; the bundled OpenOCD scripts are always searched
adapter = "tas_client" # optional: tas_client, winidea, bluebox, sim
probe = ""             # optional: serial number; a second session on it is refused

[launch]               # optional: run on open_session, like a VS Code launch
symbols = ["C:/work/build/app.elf"]
flash = { files = ["C:/work/build/app.hex"] }
reset = { halt = false }
run_to = "main"
```

```powershell
curl -X POST http://127.0.0.1:3001/api/v1/sessions -H "Content-Type: application/json" -d '{\"profile\": \"tc375-lite\"}'
.\server\target\debug\aurix-hub.exe tcl targets "reg pc" "mdw 0x70013954 4"   # Tcl console of the only session
```

Run a worker by hand (development; it registers with no hub). It takes OpenOCD's options:

```powershell
.\server\target\debug\aurix-dbg.exe -s server/target/openocd-tcl/tcl `
    -f board/infineon/kit_a2g_tc375_lite.cfg -f tc375_lite_example.cfg [-c cmd] [-d3] [-l log.txt]
```

| Option | Meaning |
|---|---|
| `-f` / `-c` / `-s` | Script, command, search directory; handled in order, like OpenOCD. `init` runs automatically at the end. |
| `-d<n>`, `-l <file>` | Debug level 0..3, log file. |
| `--adapter <name>` | Override the cfg's adapter driver. `sim`: built-in TC37x simulator (used by the tests). |
| `--hub <url> --session <id>` | Set by the hub: register with it. |

Ports:
- **Hub:** 3001 (`port` in `hub.toml`). Binds to 127.0.0.1 unless `hub.toml` says otherwise; see [Remote use](#remote-use).
- **Worker:** `telnet_port`, `tcf_port`, `api_port` default to 0 = a free port on 127.0.0.1. Shown in the worker log and in `list_sessions`. A worker started by the hub always binds to loopback.

`aurix-hub` commands: `serve`, `stdio`, `tcl`, `install`, `uninstall`, `stop [--now]`, `status` (`aurix-hub --help`).

Logs: `%LOCALAPPDATA%\aurix-dbg\logs\` (`hub.log`, `session-<id>.log` per worker, `stdio.log`).

## VS Code

Use a launch configuration of type `aurixdbg` (example in the extension's [README](../../README.md#quick-start)). The extension opens the session through the hub, starting the hub when needed, and connects to the worker's TCF port.

| Attribute | Meaning |
|---|---|
| `profile` | A hub profile; the attributes below override its parts. |
| `searchDirs`, `configFiles`, `configCommands`, `adapter`, `probe` | The session's `connect`: passed to the worker as `-s`, `-f`, `-c`, `--adapter`. |
| `session` | Attach to an open session (ID or name, e.g. one Claude opened) instead of opening one. |
| `enableFlashing`, `flashFiles`, `verifyFlash`, `resetType`, `resetSuspend`, `runTo` | Launch steps the hub runs before the debugger attaches (flash, reset, run to). |
| `sessionLifetime` | `close` (default): close the session when debugging ends, if this debug session opened it. `keep`: leave it open. |
| `hub`, `hubToken` | Another hub (default: setting `aurix-debugger.hub.url`, `http://127.0.0.1:3001`). TCF then goes through the hub's WebSocket relay. |

Debug Console: `monitor <tcl command>`, e.g. `monitor tc3xx_status`, `monitor sbc_status`.

## Installation

The extension ships `aurix-hub.exe`, `aurix-dbg.exe` and OpenOCD's scripts. `aurix-hub install` copies them to a fixed per-user location, which the logon start and MCP configurations use:

```text
%LOCALAPPDATA%\aurix-dbg\versions\<version>-<hash>\   the files of one build
%LOCALAPPDATA%\aurix-dbg\current                      junction to the installed build
%APPDATA%\aurix-dbg\hub.toml, profiles\              settings (never touched by install/uninstall)
```

- **Automatic:** the extension runs `aurix-hub install` at start (setting `aurix-debugger.hub.install`, default `auto`). It copies only when the bundled build differs, never downgrades, needs no admin rights.
- **Logon start:** command *AURIX: Start Debugger Hub at Logon* (`aurix-hub install --autostart`) writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\AurixDbgHub` = `"%LOCALAPPDATA%\aurix-dbg\current\aurix-hub.exe" serve` and starts the hub. *Don't Start Debugger Hub at Logon* removes it.
- **Updates:** a newer build moves `current`; the running hub restarts from it once no session is open. *AURIX: Restart Debugger Hub Now* applies it at once and closes the sessions. Hub and workers always come from the same build.
- **Uninstall:** the extension's `vscode:uninstall` hook runs `aurix-hub uninstall`: stops the hub, removes the logon start, removes the files unless another VS Code profile still uses them (`installs.json`).
- **Without VS Code** (e.g. a probe PC): copy the `extension\bin` folder of the `.vsix` (a zip) and run `aurix-hub.exe install --autostart`.
- **VS Code Remote-SSH / Remote-Tunnels:** the extension runs on the remote machine (`extensionKind: workspace`), so the commands install there.

**Claude:** *AURIX: Add Debugger MCP Server to Claude Config* writes the workspace `.mcp.json`, either

```json
{ "mcpServers": { "aurix-debugger": { "command": "C:\\Users\\<you>\\AppData\\Local\\aurix-dbg\\current\\aurix-hub.exe", "args": ["stdio"] } } }
```

or `{ "type": "http", "url": "http://127.0.0.1:3001/mcp" }` when the hub runs anyway. `aurix-hub stdio` starts the hub when none answers and ends its MCP session when Claude exits.

## Remote use

The hub can serve other machines (e.g. probes on a lab PC, clients on a laptop):

```toml
# %APPDATA%\aurix-dbg\hub.toml on the lab PC (aurix-hub install --remote creates it)
bind = "0.0.0.0"
port = 3001
token = "<random>"
allow_tcl = true    # false: other machines may not run Tcl commands
mcp_idle_min = 30   # end an MCP session after this many minutes without a request
```

- **Token:** clients not on loopback send `Authorization: Bearer <token>` on every route except `/health`. A non-loopback `bind` without a token is refused at startup.
- **Ports:** workers listen on loopback only; the hub is the one exposed port. `/internal/*` (worker registration) is for the hub machine only.
- **Tcl is shell access:** Jim has `exec` and `file`, so the token equals a login on that PC. `allow_tcl = false` refuses `tcl` (and `monitor`) from other machines.
- **Files:** paths in operations are on the hub machine. `POST /api/v1/files?name=app.elf` (raw body) stores a file and returns `file:<sha256>/app.elf`, usable wherever a path is. The extension uploads `configFiles` and `flashFiles` to a remote hub itself; Python has `Client.upload`.
- **VS Code:** TCF has no authentication, so the hub relays it over an authenticated WebSocket (`/api/v1/sessions/{session}/tcf`); the extension opens a local port for it.
- **MCP:** `claude mcp add --transport http aurix http://<lab-pc>:3001/mcp --header "Authorization: Bearer <token>"`, or `aurix-hub stdio --url http://<lab-pc>:3001 --token <token>`.

## REST, MCP and Python API

The hub's port (3001) serves:

| Path | Content |
|---|---|
| `/mcp` | MCP (Streamable HTTP, JSON responses). Register: `claude mcp add --transport http aurix http://127.0.0.1:3001/mcp`. |
| `/api/v1/hub`, `/api/v1/sessions`, `/api/v1/profiles` | `hub_info`, `list_sessions`, `open_session` (POST), `close_session` (DELETE `.../sessions/{session}`), `session_log`, `list_profiles`. |
| `/api/v1/sessions/{session}/...` | A session's operations (ID or name): targets, run control, registers, memory, [variables](#variables-by-name), breakpoints, watchpoints, flash, Tcl, `launch`, the control lease. `/api/v1/session/...` = the only open session (409 with the list when several are open). |
| `/api/v1/sessions/{session}/info` | Worker liveness: version, API revision, PID, probe connection, targets, symbol files, launch phase, lease holder. Never waits for the core. |
| `/api/v1/sessions/{session}/os/...` | [AUTOSAR OS awareness](#autosar-os-awareness-orti). |
| `/api/v1/sessions/{session}/events`, `/api/v1/events` | Server-sent events: a session's halts, resumes, resets, breakpoint changes, log lines; the hub's sessions opened, launched, closed. |
| `/api/v1/sessions/{session}/daq` | WebSocket: [periodic sampling](#periodic-sampling-daq). |
| `/api/v1/sessions/{session}/tcf` | WebSocket: the worker's TCF connection, for VS Code on another machine. |
| `/api/v1/files` | Uploads (see [Remote use](#remote-use)). |
| `/docs`, `/openapi.json` | Swagger UI, OpenAPI 3.1 document. |

The examples in the sections below use a worker's own paths (`/api/v1/...`). Through the hub, replace `/api/v1` with `/api/v1/sessions/{session}` (or `/api/v1/session`).

**Sessions.**
- `open_session` starts a worker from a profile and/or `connect` (cfg scripts, commands, search directories, adapter).
- It runs the **launch steps** when asked: symbols, ORTI, flash (after `reset init`), breakpoints, reset, `run_to`, `stop_on_entry`.
- It returns when the steps are done, or after `wait_s` with phase `starting`; `wait_session` waits for the rest.
- `relaunch` runs the same steps again (e.g. after a rebuild).
- A failed step leaves the session open; read its log with `session_log`.
- Over MCP, `session` is optional while one session is open. With none or several open, the result lists the profiles or sessions to choose from.

**Clients and the control lease.**
- Every client names itself with the header `X-Aurix-Client` (MCP: `mcp:<id>`, VS Code: `vscode:<id>`, Python: `py:<id>`).
- A client owns the breakpoints and watchpoints it sets (`client` in `list_breakpoints`). `end_client` removes them. The hub calls it when an MCP session ends (`DELETE /mcp`, or `mcp_idle_min` without a request).
- `acquire_control` takes the **control lease**. While another client holds it, run control, register/memory/variable writes, flashing, launches and Tcl fail with `busy`. Reads never wait for it. It expires after `ttl_s`. Nobody holds it unless a client takes it.

**Limits.**
- Memory transfers: 64 KB per call (the Python client splits longer ones).
- `info` answers even while a flash job or long Tcl script holds the core.
- `load_symbols` does not block run control.

**Security.**
- On this machine there is no authentication; other machines need the hub's token.
- Requests with an `Origin` header from other sites are rejected (hub and workers, DAQ WebSocket included), so a web page cannot drive the debugger.

**Adding an operation** (developers). Operations are declared once: session operations in [crates/api-spec/src/ops.rs](crates/api-spec/src/ops.rs), the hub's in [hub_ops.rs](crates/api-spec/src/hub_ops.rs), types in [types.rs](crates/api-spec/src/types.rs). Their `///` comments are the documentation of all outputs. `cargo build` generates:
- the REST routes, the MCP tools and their JSON schemas, the `DebuggerApi` / `HubApi` traits that [crates/api](crates/api) and [crates/hub](crates/hub) implement;
- [openapi/openapi.json](openapi/openapi.json);
- the Python client's [_generated.py](../python/aurix_dbg/_generated.py).

Generated files are checked in and written only when they change. Add a line to `ops.rs`, implement the new trait method; the compiler reports what is missing. A worker serves session operations at `/api/v1/...` and by name at `/api/v1/ops/{op}`. DAQ is not an operation: its WebSocket protocol is [specified below](#periodic-sampling-daq).

### Shared session

All clients of a session (REST, MCP, VS Code over TCF) share it:
- **Run control:** a halt from the API shows in VS Code; a resume in VS Code shows in `wait_state` and on the event stream.
- **Breakpoints:** one table. Two clients on one address share one hardware breakpoint. `Breakpoints.set` replaces only the calling client's own breakpoints. API breakpoints stay until removed.
- **Watchpoints:** same table; VS Code data breakpoints and API watchpoints share hardware. `list_watchpoints` also shows the Tcl console's (`wp`), owner `console`.
- **API breakpoints in VS Code's Breakpoints view:**
  - at the start of a source line: a source breakpoint;
  - elsewhere: a function breakpoint named by symbol or address (`0x8021a9de`);
  - a watchpoint: a function breakpoint named by what it watches (`g_count [write]`).
  - Removing, disabling or enabling it in VS Code changes it on the server, and the other way round. They leave the view when the debug session ends (not saved with the workspace).
  - VS Code's own function breakpoints accept a symbol, an address or `file:line`.
- **Conditions:** a `condition` (C expression) is evaluated by the server on the halted core, in its top frame. When it is zero the core resumes before any client hears of the halt (the event stream still shows halt and resume). Checked during a source step over a function too. Every hit is a real halt (a few ms).
- **Writes:** API memory and register writes send `Memory.memoryChanged` / `Registers.registerChanged`; VS Code refreshes its views.
- **Source steps:** while VS Code runs a source step, API `resume`, `step` and `reset` fail with `busy`; `halt` cancels the step.
- **Symbols:** `load_symbols` works without VS Code. One symbol set per core, last writer wins (a later VS Code session replaces what the API loaded, and vice versa). `symbols` reports what is loaded.

### Python

[python/](../python): `pip install -e python`, then:

```python
from aurix_dbg import Client
dbg = Client()                          # http://127.0.0.1:3001; Client(url, session=..., token=...)
dbg.open_session(profile="tc375-lite")  # or connect={"config_files": [...]}, launch={...}
dbg.halt("tc37x.cpu0")
print(hex(dbg.read_register("tc37x.cpu0", "pc").value))
data = dbg.read_bytes(0x70000000, 256)   # any length, split into 64 KB calls
for ev in dbg.events(): print(ev)       # halts, resumes, ... of all clients of the session
dbg.end()                               # remove this client's breakpoints, release its lease
```

### Variables by name

Browse, read and write globals by name over REST and MCP, without VS Code:

```text
GET /api/v1/info                                            liveness probe
GET /api/v1/globals?query=*speed*&offset=0&limit=100        list_globals
GET /api/v1/globals?search=speed&limit=50                   list_globals (search box)
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

- **`list_globals`:** case-insensitive glob (`*`, `?`) over the full name of every global; one page sorted by name; `total` counts all matches.
  - A query with `.` or `[` also finds struct/union members and array elements by their path: the part before the last separator is evaluated like a `symbol_children` parent (anonymous members lifted, `@N` suffix, `${E0/GV:12}` references, pointers not followed), and the last segment is globbed against its children. Hits follow the plain-name matches, in declaration/index order, with the whole path as `name` and `expression` (`Drive_Counter_0.DriveCounterStamp`, sampleable by that expression); a child of a `per_core` group is a group too.
  - Leading `*` belong to the last segment, so a search box's `*…*` wrapping works: `*g_motor.spe*` finds the members of `g_motor` containing `spe`, `g_motor.spe*` those starting with it, `g_motor.speed` exactly that one. Element segments match the index: `g_buf[3]` (also `*g_buf[3]*`) is element 3, `g_buf[1*` elements 1, 10–19, …; `g_s.` / `g_buf[` list all children.
  - In a `query`, wildcards in the parent (`*.speed`, `g_*.speed`, `g_buf[*].x`) are not supported, and a leading `*` is never a dereference (write `(*p).x`). The parent is a C expression, so it is case-sensitive. A parent that does not resolve, or has no such children, gives an empty page, not an error. To find a member by its own name, use `search`.
- **`list_globals` with `search`** (instead of `query`): search-box text, as the VS Code Live Watch picker and Autobus use it. It searches the globals and the members of the struct globals (nested up to 8 levels, at most 200 000 per ELF; no array elements) in an index built once per symbol load, and returns the best matches first: `DriveCounterStamp` finds `Drive_Counter_0.DriveCounterStamp`.
  - A name matches when it contains the text (case-insensitive), when `*` / `?` make it match, or when the text is a run of word beginnings (`dcs` → `DriveCounterStamp`); after those come names whose source file contains the text, or whose address starts with a `0x…` text. Order: whole name, prefix, start of a word, anywhere, word beginnings, file/address; then shorter names first, then by name.
  - A text with `.` or `[` whose parent the index holds no members of (array elements, deeper members, a `${…}` reference, an `@N` suffix) also gets the parent's matching children by evaluation, after the index's matches: `g_buf[3]`, `g_buf[1` (1, 10–19, …), `g_s.a@2`.
  - An empty text lists the globals by name, then the members. `name` of a member is its whole path; a member of a `per_core` group is a group too.
  - For a tree: every member and element carries `parent`, the `expression` of the variable it is in, exactly as that one's own `expression` reads (`g_s.a@2` → `g_s@2`). Ancestors of a match that are not matches on the page come along as `context: true` items (outermost first, once per page, before the first match below them); they count toward neither `limit` nor `total`. `DriveCounterStamp` gives `Drive_Counter_0` (context) and `Drive_Counter_0.DriveCounterStamp` (parent `Drive_Counter_0`). `symbol_children` and path queries set `parent` too.
- **`symbol_children`:** struct/union members (members of anonymous members lifted up, as in C) and array elements, paged. Pointers are not followed: evaluate `*p` or `p->x`.
- **Each item** has `expression` (pass it to the other operations and to DAQ points), `class` (`uint`, `int`, `float`, `bool`, `enum`, `pointer`, `array`, `struct`, `union`, `function`, `unknown`), `size`, `address`, `has_children`, `sampleable` (DAQ streams it as a typed value: a scalar of at most 8 bytes).
- **Same name twice** (statics in different files, or in a bootmanager ELF too): the name finds one in a fixed order; the others get a symbol reference like `${E0/GV:12}` as `expression`, accepted by `evaluate`, `write_variable`, `symbol_children` and DAQ points.
- **Evaluator:** the one VS Code uses: globals, statics, members, elements, casts, arithmetic, while the cores run. With `frame` on a halted core it evaluates in that stack frame (see [Call stack](#call-stack-and-code-locations)).
- **Core-local RAM** (`0xD000_0000` DSPR, `0xC000_0000` PSPR alias) exists once per TriCore core. Which copy an expression means:
  - a trailing **`@N`** (core ID) names it: `CRASH_state@2`, `g_motor.speed@1`, `${E0/GV:12}@2`. Allowed on any expression; on other memory it only selects that core's symbols. `@hsm` evaluates for the HSM. An `@N` naming no TriCore core is `not_found`; one contradicting `target` is `bad_request`;
  - else, if the ELF is **assigned to exactly one core**, that core's copy. Assign with `load_symbols(files=["app.elf@0"])` (`@N,M` for several), as `symbolFiles` does in VS Code; `info` lists it as `local_cores`. Loading a file without the suffix clears the assignment;
  - else the bare name is refused with a message listing the `@N` choices. `list_globals` shows such a variable as a group (`per_core: true`, no `address`, `sampleable: false`); its `symbol_children` are the copies (`core 0` = `CRASH_state@0`, …). Members keep the suffix: `g_s@2` has `g_s.a@2`.
  - `target` selects symbols and the HSM/MCS memory view, not the copy.
  - Reported addresses are system-bus addresses: `CRASH_state@1` at `0xD0000100` is at `0x60000100`.
- **Values:** a number, a bool, an enumerator name, `nan`/`inf`/`-inf`, or hex for aggregates and anything over 8 bytes; `raw_hex` has the bytes as read. A bit field reads its storage unit and reports `bit_offset`, `bit_size`.
- **Writes** take a number, `true`/`false`, an enumerator name, or a decimal or `0x` string (exact for 64-bit values); a struct or array takes hex bytes. Range-checked against the type or bit field, bit fields merged into their storage unit, read back. VS Code gets `Memory.memoryChanged`.
- **LMU/DLMU** globals linked to the cached segment (`0x9000_0000`..`0x97FF_FFFF`) are accessed at the same offset in segment `0xB` on every path (REST, MCP, TCF, Tcl memory commands, DAQ): a debugger write to `0x9…` is dropped and can reset the board. Reported addresses stay the linked ones; watchpoints trigger on them. The miniMCDS trace RAM (`0x9800_0000`) and PFLASH (`0x8`/`0xA`) are accessed as linked.
- **Errors:** `not_found` (404): no symbols, unknown name, or `@N` naming no TriCore core. `bad_request` (400): expression does not parse, value does not fit, or core-local RAM needs `@N`. `memory` (422): target memory unreadable.
- **`info`:** `api_version` (integer, raised when an operation or field changes), `core_lag_ms` (time since the core thread last finished a job; grows while a long operation runs), `symbols_loading`.
- URL-encode query parameters: `+` must be sent as `%2B`.

### Call stack and code locations

```text
GET /api/v1/targets/tc37x.cpu0/stack?max_frames=10          stack_trace
GET /api/v1/symbols/address?address=0x80001234               lookup_address
GET /api/v1/targets/tc37x.cpu0/locals?frame=1               list_locals
GET /api/v1/expressions?expression=buf.len&target=tc37x.cpu0&frame=1   evaluate in a frame
```

- **`stack_trace`:** the frames VS Code shows, top first. TriCore from the CSA chain, HSM from Cortex-M prologues, GTM MCS channel up to two frames, SCR one. Each frame: `pc` (return address for callers), `sp`, `return_address` where available, `location` (function, offset, file, line) with symbols. A caller's location is that of its call. While VS Code shows the core at a trace row, the frames are the replay's (`replay: true`). Needs a halted target (`not_halted`) not in a VS Code source step (`busy`).
- **Location in state results:** `state`, `halt`, `step`, `resume`, `wait_state`, `targets` and `reset` add `location` for a halted target whose PC the symbols describe (no target access; left out while symbols load).
- **`list_locals`:** locals and parameters of a frame (default 0) with values, as the VS Code Variables view; shadowed outer declarations left out. A local without a location at the frame's PC has an `error`.
- **`frame` on `evaluate`, `write_variable`, `symbol_children`** (with `target`, without `@N`): names resolve as in the VS Code Watch view: frame locals first, then globals. Register locals can be written only in the top frame; stack locals in any frame.
- **`lookup_address`:** addr2line over the loaded symbols: function, offset and line at an address (`return_address: true` looks up the call before a saved A11 or LR), and the variable the address lies in, with offset. No target access. Addresses as linked (core-local variables by their `0xD000_0000` address).

### Peripheral registers

```text
GET  /api/v1/svd                                       svd: the description, the devices available
POST /api/v1/svd  {"device": "TC39x"}                  load_svd (or {"file": "my.svd"})
GET  /api/v1/peripherals?query=STM*                    list_peripherals
GET  /api/v1/peripherals/GTM.TOM0.CH1.CTRL             describe_peripheral
GET  /api/v1/peripherals/SCU.CCUCON0/value             read_peripheral
PUT  /api/v1/peripherals/STM0.CMCON.MSIZE0/value  {"value": 31}   write_peripheral
GET  /api/v1/svd/definitions                           the peripheral viewer's definitions (VS Code)
```

- **Description:** the device's SVD file bundled with the extension (`svd/` next to the worker; `hub install` copies it), merged with its extra files (TC3xx HSM and SCR). One per session.
  - Without `load_svd`, the first peripheral operation loads the chip the target configuration names (`tc37x.cpu0` → `TC37x`, `tc37x_ext` → `TC37xEXT`).
  - VS Code loads the launch configuration's `device`.
  - `load_svd` with `file` loads your own SVD file.
- **Paths** name a peripheral, cluster, register or field, case-insensitive; arrays as the SVD names their copies (`STM0.CMP[0]`). `describe_peripheral` gives addresses, sizes, access, reset values, fields and value names without touching the target (paged).
- **Reading:**
  - only declared registers, each at its width, adjacent ones in one access; gaps are never read (reserved addresses can raise bus errors and SMU alarms);
  - fields decoded with value names;
  - skipped unless `force`: registers with a read side effect (SVD `readAction`, or a debug-view twin such as `ASCLIN0.RXDATA` / `RXDATAD`, `QSPI0.RXEXIT` / `RXEXITD`, `EVADC.G0RES[0]` / `G0RESD[0]`) and write-only registers;
  - an unreadable register has its own `error`; the rest are still read;
  - HSM peripherals and SCR SFRs go through their core.
- **Writing a field:** read, merge, write back; other `modifiedWriteValues` fields (write 1 to clear, …) are set to the neutral value. A field takes a number or a value name.
  - Read-only registers and fields are refused.
  - Read back; a value that did not stick is reported in `note`. ENDINIT / Safety ENDINIT protected registers ignore writes (this API does not do the unlock sequence). Over TAS a protected write is refused with an access error.
  - Needs the control lease when another client holds it; VS Code refreshes its views.
- **The SVD is trusted:** Infineon's TC3xx files declare `Pnn.OMR`, `OMSR`, `OMCR` read-only (they are write-only), so `write_peripheral` refuses them; use `write_memory`. SCR SFRs are refused in a session without an SCR target; HSM addresses without an HSM target.

### Watchpoints

`add_watchpoint`:
- `address` (with `length`, default 4) or `expression`;
- `target` (default: every TriCore core);
- `access`: `write`, `read`, `access`;
- `condition`: as for breakpoints (see [Shared session](#shared-session)).

An expression resolves on each core from its symbols, to the object's address and type size (bit field: its storage unit), and again when other symbols are loaded, so a rebuilt ELF moves the watchpoint with the variable. VS Code data breakpoints are the same breakpoints (TCF `AccessMode`, `Size`).

What the TriCore data triggers catch:
- **Start address only:** an access is caught when it starts inside the object (a byte store into its middle included). An access that starts below and reaches into it (e.g. a 64-bit copy of the enclosing struct) is not.
- **Cost:** a watchpoint wider than one byte uses two of the core's 8 hardware events (shared with breakpoints).
- **After the access:** the core halts with reason `watchpoint` after the store, PC at the next instruction.
- **Core-local RAM:** the local alias (`0xD000_0000`) and the global address of the same DSPR never match each other. A core-local variable is watched at its local address on its own core and at its global address on the others; `CRASH_state@2` picks core 2's copy.
- The HSM watches with its DWT, by address range. MCS channels have no watchpoints.

### Periodic sampling (DAQ)

`ws://127.0.0.1:3001/api/v1/sessions/{session}/daq` (or `/api/v1/session/daq`) samples variables while the target runs, **each at its own rate**, and streams decoded values.
- Host-side polled reads, merged into one batch per tick.
- A point in `"mode":"trace"` is recorded by the chip instead (every write, with target time): see [DAQ trace points](#daq-trace-points).
- **Sampling runs only while the socket is open.** Close the socket (or crash) and the session is removed. No idle timeout.

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

Points:
- exactly one of `expression` or `address` (with `size`); at most one of `rate_hz` or `interval_us`;
- `decode: false` streams raw hex;
- `target` uses that core's symbols. On the HSM or an MCS channel it also reads through that core's view (HSM-private memory, MCS program RAM); those points cannot share the batch, so keep them slow;
- an `expression` resolves as in [`evaluate`](#variables-by-name), including `@N`. Core-local RAM is read over the system bus (`CRASH_state@1` at `0xD0000100` → `0x60000100`), batched with the other points; `config` reports the translated `address`. A raw `address` with a TriCore `target` is translated the same way;
- a `pacing` key is rejected.

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

Samples are columnar: `points[i]` pairs with `values[i]`; a tick carries only the points that were due. `t_us` = microseconds since `start_wall_ms`, taken after the batch returns.

**Rates and timing:**
- Rates are rounded up to a 100 µs grid; each point reports the period it got.
- A rate is a floor: you get **at least** the requested rate. Spacing varies (p95 ~3.5 ms at 1 kHz). **Use `t_us`; do not assume a fixed step.**
- Ceiling: about 1.5 kHz (the Windows sleep quantum is ~0.5 ms).
- Host timestamps track the target's STM to within 25 ppm.
- `daq` on the console shows per point: period asked for, period scheduled, interval achieved (steady ticks), interval delivered (mean over all samples, stalls included). `delivered` is what a client receives.
- `delivered` near 15600 µs: something overrides the timer resolution (the server opts out of Windows power throttling at startup).
- Due points ride in the run-state poll's batch (`daq merge 1`, default; `daq merge 0` turns it off). The poll (`polling_period`, default 10 ms) adds jitter, not rate loss. Raising `polling_period` tightens spacing but delays halt detection.

**What a tick costs is bus accesses, not points:**
- miniWiggler: ~123 µs for the first access, ~57 µs for each further one, well under 1 µs per extra byte inside a read.
- The sampler merges points across holes up to `daq gap <bytes>` (default 96 B, BlueBox 512 B). Never in peripheral space (`0xF0000000`+), where over-reading can clear a flag; adjacent points still merge there. `daq gap 0`: adjacent only.
- **Lay sampled variables out together.** At 1 kHz: ~12 scattered points, ~40 points 32 B apart, 128 adjacent words.
- Point count costs stream CPU (decoding, JSON), not sampling quality.
- Other clients see at most one DAQ batch of added latency.
- A stall (flashing, long Tcl script, reset) is reported as a `gap`.

**Modes and values:**
- `mode`: `every_tick` (default) or `on_change`. On-change saves stream bandwidth, not probe traffic (still read every period). The first read is always sent; the baseline is dropped on a device reset and on `resume`. A quiet point sends nothing; `resend_after_us` sets a floor. Bit fields compare only their own bits.
- Encodings from the DWARF type: `int`, `uint`, `float`, `bool`, `enum` (enumerator name, or the number if none matches), `hex` for aggregates, unknown types and anything over 8 bytes. Non-finite floats are `"nan"`, `"inf"`, `"-inf"`. A `uint` above 2^53 loses precision in `JSON.parse`: use `decode: false` for 64-bit counters read by JavaScript.
- Only globals and statics; frame locals are rejected. Names need symbols (`load_symbols`, or a VS Code debug session). A point that fails to resolve gets `error` and `interval_us: 0`; the rest runs.

**Backpressure and caps:**
- Each session has a bounded queue (`queue`, default 256 ticks). A slow client loses the newest messages and gets `overrun`.
- A tick over budget is truncated and reported as `state: stalled`; dropped points rotate, so no point starves. Lost periods are reported as `gap` (only intervals over four periods), never replayed as a burst.
- Caps: 8 sessions, 256 points per session, 1024 bytes per point; per tick at most 8 KB and two response packets.

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

## On-chip trace (miniMCDS)

The TC37x production device has a small trace unit: 8 KB of trace RAM at `0x98000000`, filled by the chip while the core runs. The debugger configures and reads it over the normal DAP connection, so it works with a miniWiggler (no ED device, no trace probe). A trace is a window: about 80 µs of unfiltered program flow, about 600 writes of a data trace, longer when filtered.

| Kind | Content |
|---|---|
| Program trace (CPU0) | Executed instructions, with function, source line and target time. Optionally only inside one function or address range (outside: a gap). |
| Data trace | Every write into an address range (up to `0x1000` bytes), with value, size and target time. **Only CPU0's stores** are seen. |

| `stop` | Buffer | Keeps |
|---|---|---|
| `halt` | ring | the last 8 KB up to the core's halt, whatever halted it (breakpoint, debugger halt, another core's halt over TL1) |
| `full` | linear | the first 8 KB after arming |
| `manual` | ring | the last 8 KB before `trace_stop` (nothing recorded while the core is halted) |
| a function name | ring | until the PC enters the function (works together with a filter range) |

Time stamps:
- `tsu` (default): 150 MHz trace clock (6.67 ns). Program trace: a stamp every ~160 ns while the core runs; data trace: one per write.
- `ticks`: a tick per cycle, exact per instruction, but the buffer fills within milliseconds.

**REST/MCP:**

| Operation | Route | Does |
|---|---|---|
| `trace_check` | POST `/api/v1/trace/check` | Validate a configuration; report comparators and events it needs. No target access. |
| `trace_arm` | | Arm it. |
| `trace_status` | GET `/api/v1/trace` | `{Armed, Owner, Done, Fill, LostByReset, Last}`. `Fill` 0..1; a ring stays at 1 after its first lap. |
| `trace_stop` | | Stop and read the trace RAM. |
| `trace_read` | GET `/api/v1/trace/read?max=N` | Decode the last recording. |
| `trace_auto` | POST `/api/v1/trace/auto` | `{"config": {...}}` (null ends it): arm whenever the traced core is resumed (not stepped), read at the halt. A trace in use by another owner or still recording is left alone. |
| `trace_insns` | GET `/api/v1/trace/insns?from=&count=` | Rows with registers (below). |
| `trace_regs` | GET `/api/v1/trace/regs?at=` | All registers and the call stack after a row. |
| `trace_find` | | Search by address, register value (`d15=0xC`), register write (`a4 changes`) or text. |

```json
{"core":0,"kind":"program","ranges":[{"function":"Os_Task_10ms"}],"stop":"halt","timestamps":"tsu"}
{"kind":"data","ranges":[{"start":"0x70001000","len":16}],"stop":"manual"}
{"kind":"program","ranges":[{"function":"Fee_MainFunction"}],"stop":"Os_Task_5ms"}
```

TCF service `AurixTrace` takes the same object with capitalized keys (`{"Stop":{"Function":…}}` for a function).

Results:
- Program trace: `Functions` and `Files` tables and `Steps` rows `[addr, t_ns, function, file, line]` (indices into the tables), the newest `max` rows. Gaps are `{"Gap":"trace_off"|"lost"|"no_code"|"inconsistent"|"untraced","T":…}`.
- Data trace: `Events [{T, Addr, Value, Size, Access, Symbol}]`.
- Times are ns from the first time stamp; `StartNs` is that stamp on the trace clock.
- A stop-on-halt trace reaches the halt (`ReachesHalt`; `Halt` gives the halt point with function and line). If data was lost, the flow ends with an `untraced` gap.

**Instruction trace with registers** (`trace_insns`, `trace_regs`):
- Rows: instruction text, source location, call depth, branch outcome, registers written, memory accesses.
- Registers are rebuilt from the halt registers, backwards and forwards through registers and memory. Each has `M` (`exact`, `mem` or null = unknown) and `Why` (provenance).
- `mem`: read from the device as it is at the halt (another bus master may have changed it). Program flash comes from the ELF. Peripheral addresses are never read. Memory is used only while the core still stands where the recording stopped.
- Unknown: results of instructions the model does not know (mostly FPU), and everything after a gap (so a filtered trace gives few values).
- TCF: `AurixTrace insns|regs|find`; `replay {At}` / `replayMove {Kind}` show a trace row as the core's stack, registers and locals (VS Code's Step Back).

**Hardware behaviour:**
- Leaving a filter range is reported one instruction late (the first instruction outside, then a `trace_off` gap).
- A data trace stamps reads too: a range the application reads often costs more trace RAM per write.
- Ring laps: a busy range (e.g. a core's stack) fills the 8 KB in ~3 ms.
- The last few hundred ns before a halt or manual stop are flushed by the debugger; a DAQ trace flushes after 100 ms without new data.

### Reverse debugging

`reverse_enable` (POST `/api/v1/reverse/enable`) switches it on for a TriCore core.
- **Checkpoints:** before every action (run, instruction step, source step, register or memory write; not reset hooks and cfg event handlers) the core's registers and RAM regions are saved, kept as differences.
- **Regions:** the ELF's allocated writable RAM sections; the core's stacks and CSA by linker symbols (iLLD/HighTec `__USTACK0`/`__USTACK0_END`, `__ISTACK0`, `__CSA0`/`__CSA0_END`; TASKING `_lc_ub_ustack_tc0` …; others: `stack_symbols`, `csa_symbols`); plus `regions`, minus `exclude`.
- **Going back:** `reverse_back` (`{"granularity": "line"|"instruction"|"action"}`), `reverse_continue`, `reverse_goto` (`{"index", "row"}`) write an earlier state into the halted core, which then really runs from there.
- **Trace:** while on, the mode owns the miniMCDS (`trace_arm`, `trace_auto`, traced DAQ points refused; `reverse_enable` refused while a manual trace is set up). Every action except instruction steps and writes is recorded from its checkpoint to the halt.
- **What is restored:** registers that differ; stacks and CSA by difference; other RAM only where the core wrote (from the recording, or the instruction model for a step), else by difference (`ByDifference`, with `Why`). Stores to peripheral registers are listed in `NotUndone`.
- **A point inside an action:** the checkpoint is restored and the core runs to the row's address as often as the recording says (breakpoints out of the hardware; `max_hits` when the recording lost its start). The new recording and the exact registers are compared with the old (`Diverged`).
- **Protection:** the restore reads `CPUx_SPR_SPROT_*`, `CPUx_DLMU_SPROT_*` first. Bytes closed to the debugger are never written: in the core's own regions the restore is refused before anything changes; elsewhere they are listed in `NotUndone.Protected`. Everything written is read back. LMU/DLMU at the `0xB` alias.
- **History is cleared** by a reset, by flash programming, and when the core ran without the debugger (another client, a script). A run that is no action (e.g. after attaching to a running application) is recorded without a checkpoint (`reverse_status` `RecordingRun`).
- **TCF:** `AurixReverse` (`enable`, `disable`, `status`, `history`, `back`, `continue`, `goto`, event `changed`). An operation ends like a source step: Memory/Registers change events, then one `contextSuspended` (`Step back`, `Breakpoint (reverse)`, `Step back (start of history)`).
- **Tcl:** `reverse on -region <start> <len> [-private] … | off | status | history | back | goto <index> | peek <index> <addr> <len>` (restores whole actions by difference).

### DAQ trace points

A [DAQ](#periodic-sampling-daq) point with `"mode":"trace"` is recorded instead of polled: the session arms a data trace over the traced points (manual stop, ring), drains it every 2 ms while the target runs and sends every write:

```json
{"op":"add","id":9,"points":[{"expression":"g_state","mode":"trace"}]}
{"type":"trace","points":[4,4],"values":[1,2],"t_target_ns":[118200,118650]}
{"type":"trace_lost","t_us":130000,"messages":12,"laps":1}
{"type":"source","point":4,"source":"host","reason":"another client is tracing"}
```

- Falls back to polling at its rate, with a `source` frame saying why, when the trace is in use (one MCDS, owned by the first session that traces or by `trace_arm`) or the points span more than `0x1000` bytes.
- The whole range from lowest to highest traced point is recorded: writes between points cost trace RAM too.
- `trace_lost`: writes came faster than the drain (lost messages, ring laps).
- A reset ends a running trace (`LostByReset`).

### Trace from Tcl and VS Code

- **Tcl:** `mcds arm|plan|status|stop|decode|dump|init|auto` ([commands](docs/commands.md#trace-mcds-tc37x-minimcds)). `mcds plan` prints the register writes without touching the target; `mcds dump` saves the trace RAM with a winIDEA-compatible header; `mcds auto -program -stop halt` / `mcds auto off`.
- **VS Code:** *Show Trace* opens the Trace panel (configuration, resources, arm, result grouped by function with source links, CSV export); a status-bar item shows a running trace. A Live Watch instance can trace its expressions (interval menu → *Trace every write*). The OS Profile panel sets a rate or trace per lane. A launch configuration `trace` object arms a trace at session start.

## AUTOSAR OS awareness (ORTI)

`load_orti` reads the ORTI file an AUTOSAR/OSEK generator writes beside the ELF (tasks, ISRs, stacks, alarms, resources, spinlocks, events).

```python
dbg.load_symbols(files=["app.elf"])
dbg.load_orti(file="GenData/Os_Trace.ORT")
for o in dbg.os_objects("TASK", live_only=True).objects:
    print(o.name, {a.name: a.value for a in o.attributes})
```

- Served by REST (`/os/...`) and by the `AurixOS` [TCF service](#tcf-services); both share the session's loaded file.
- Object types and attributes come from the file (e.g. MICROSAR's `vs_ISR`, `vs_SPINLOCK`, `vs_OS_CONFIG`); `os` lists what was declared.
- **Versions:** ORTI 2.1, 2.2, and the unofficial 2.3 SMP extension (per-core arrays such as `RUNNINGTASK[0]`; Vector's `vs_SMP_NUMCPU` and `vs_COREID[]` map ORTI cores to targets). Indexed attributes are accepted whatever `VERSION` says. AUTOSAR ARTI is not implemented.
- **Cost of a value:** each attribute is classified at load:

  | Class | Example | Cost per refresh |
  |---|---|---|
  | prose | `ACTION = "SET_EVENT task ev"` | none, never evaluated |
  | constant | `STACK = "&OsCfg_Stack_…"`, `SIZE = "sizeof(…_Dyn)"` | none, resolved at load |
  | one address | `RUNNINGTASK[0] = "OsCfg_Trace_OsCore0_Dyn.CurrentTask"` | one read |
  | computed | `STATE = "…OsState == 2 ? ( … ? 0 : …State ) : 0xFF"` | the reads the expression makes |

  `live_only=true` skips the free classes; `reads` in the response reports what the call cost.
- `CTYPE` and `ENUM` values are code, `STRING` values are text.
- **Priority enums are an inverse map:** the target holds an ordinal; the enumerator name is the priority to display. `vs_HomePriorityInt` is the raw field.
- **Enumerator values are often addresses** (`"IdleTask_OsCore0" = "&OsCfg_Trace_IdleTask_OsCore0"`), resolved at load: this turns a pointer into a task name and a `link` to that object.
- An ORTI file can be loaded before the board is powered; `errors` lists what has not resolved yet.

**Stack coverage.** `os_stacks` (REST) / `getStacks` (TCF) report how much of each OS stack was ever used (from `SIZE`, `BASEADDRESS`, `STACKDIRECTION`, `FILLPATTERN`).
- On demand only (it reads the untouched part of each stack). `names` narrows it to the stacks you need.
- An unreadable stack (e.g. hidden by the SMPU) reports `error`, not 0 %.
- A pattern-like hole inside live data counts as used: the result errs high.

**Live update in VS Code.** Live Watch, the AUTOSAR OS view and the OS Objects panel share one scheduler:
- One interval for all (`aurix-debugger.liveWatch.intervalMs`, launch option `liveWatchIntervalMs`, or the status-bar item), 200 ms to 10 s.
- A view not on screen reads nothing (for the OS Profile panel, its sampler stops too).
- Pause = update only on halt. Halts of several cores coalesce into one refresh.
- The next refresh is timed from the start of the previous one; a slow read delays it rather than queueing.

**Sampled task profiling.** `startProfile <periodUs> <lanes>` puts one DAQ point per core on `RUNNINGTASK` (on-change). `getProfile <sinceUs>` returns transitions since then and per-task occupancy.
- Lanes: `[{"Core":0,"Attr":"RUNNINGTASK","Mode":"trace"|"off"|<periodUs>}]` (RUNNINGTASK per core, the ISR variables), each with its own rate.
- A sampled lane is a statistical profile: switches shorter than the period are missed.
- `trace`: recorded by the [miniMCDS](#on-chip-trace-minimcds), every switch exact. CPU0 lanes only; other cores stay sampled.

**Task-filtered breakpoints.** A breakpoint condition `task == <name>`, `task != <name>` or just a task name halts only in that task. In VS Code: *Edit Breakpoint* → *Expression*, or *Halt Only in Task…* in the editor context menu. The core halts on every hit and is resumed when the wrong task is running; a hit that cannot be attributed to a task is reported.

## Architecture

| Crate | Responsibility |
|---|---|
| `tas` | Pure-Rust TAS client: batched PL0 reads and writes, device connect/reset, user pins (SRST). |
| `bluebox` | iSYSTEM BlueBox over WinUSB, without winIDEA ([README](crates/bluebox/README.md)). |
| `winidea-sdk` | Bindings to the winIDEA SDK (`isystemConnect.dll`), loaded at runtime ([README](crates/winidea-sdk/README.md)). |
| `jimtcl` | Builds Jim Tcl 0.83 from the upstream release. |
| `jim` | Safe `Interp` API over Jim Tcl. |
| `openocd-tcl` | Fetches OpenOCD's Tcl scripts (pinned commit of the tricore-oss fork) into `target/openocd-tcl/`; used by the tests and packaged with the extension. |
| `core` (aurix-core) | Adapter trait and drivers, TriCore target (OCDS, halt/resume/step, triggers, TL1), OpenOCD command set and events, reset sequence, polling, DAQ, CSA unwinder, flash driver, image loader. |
| `symbols` | DWARF (GHS DWARF 2, GCC): functions, globals, line table, types, locals with location lists, C expression evaluator with assignment. |
| `mcds` | miniMCDS trace: register setup, message framing, data and program trace decoders, program flow reconstruction. |
| `orti` | ORTI parser (generic over declared object types) and per-attribute read planning. |
| `svd` | CMSIS-SVD: streaming reader from `.svd.gz`, merged extra files (HSM, SCR), register model, the peripheral viewer's definitions. |
| `tcf` | TCF frontend: protocol, services, source stepping, expressions, disassembly (Capstone); the debug session (breakpoints, symbols, stepping state) shared with the API. |
| `api-spec` | API definition: operations, types, docs; OpenAPI and Python generators. |
| `api` | Worker REST routes, event stream, DAQ WebSocket, launch steps, clients, lease, hub registration; writes the generated files. |
| `aurix-dbg` | The worker: OpenOCD-style CLI, telnet console, TCF and API listeners. |
| `hub` | `aurix-hub`: MCP/REST/Swagger, sessions and profiles, worker supervision, proxy and WebSocket relays (DAQ, TCF), uploads, token check, stdio wrapper, install/uninstall, logon start. |

Only the core thread touches the adapter and the Tcl interpreter. Frontends use `ServerHandle::call` (closures on the core thread) and subscribe to core events.

## Backends

Select with `adapter driver <name>` in the cfg, or `--adapter <name>` to override it. `adapter name` reports the driver in use (a cfg can branch on it).

| Driver | Probe | Run control |
|---|---|---|
| `tas_client` | miniWiggler through `tas_server` | the server ([`crates/core/src/tricore.rs`](crates/core/src/tricore.rs)) writes the OCDS registers |
| `bluebox` | iSYSTEM BlueBox over USB, no winIDEA ([crates/bluebox](crates/bluebox/README.md)) | the server, as for `tas_client` |
| `winidea` | iSYSTEM BlueBox through a running winIDEA | winIDEA's |
| `sim` | built-in TC37x model | the server, against the model |

**`tas_server` autostart:** if no `tas_server` runs on the local host, `init` starts one:
- from `TAS_INSTALL_DIR` (or its `bin` / `servers` folder; set by the DAS installer), else from `PATH`;
- after running `check_redistributable.exe` from the same folder;
- it keeps running after `aurix-dbg` exits (tray icon only);
- a `tas-client host` on another machine is never started.

### winIDEA backend

winIDEA owns OCDS, so the backend works at two levels:
- **Memory** through winIDEA (`REAL_TIME | DIRECT`, a running target is not disturbed): enough for DAQ, OS awareness, stack unwinding, expressions, disassembly, `verify_image`, `mdw` / `mww`.
- **Run control** by winIDEA: halt, resume, step, registers, breakpoints, watchpoints of TriCore cores. The HSM and GTM MCS channels use plain bus accesses as on the other backends.

Tcl on this backend:

| Command | Behaviour |
|---|---|
| `adapter speed`, `reset_config`, `transport select`, `flash bank`, `target create` | accepted, listed by `winidea status` and summarised at `init` |
| `tas-client …`, `tricore tl1_halt` / `tl1_step` / `irq_mask` / `triggers` | error, naming the adapter |
| `flash probe` / `info` / `erase_check` / `erase_sector` | error: winIDEA programs from its own download list |
| `mww` into a core debug register or the CBS trigger fabric | error (reads allowed) |

`tc3xx_ocds.cfg` checks `tc3xx_ocds_available` and skips only the OCDS setup; it still creates the targets.

```
winidea workspace <path>                      # the primary instance's .xjrf (optional)
winidea start if-required|existing|always     # default: if-required
winidea status
```

- **Primary instance:** `winidea workspace` when set; else the running instance the core windows are named after (`Project.xjrf` for `Project_CPU1.xjrf`). Two unrelated projects open at once fail the connect and are listed. With none running, `if-required` launches one, `existing` fails.
- **Further cores:** attached when examined, through winIDEA's session topology (TriCore core *n* = `CPU<n>`). winIDEA opens a window per core and creates its workspace if missing. A core not yet started by the application is reported halted (boot halt) until it starts.
- **Registers:** GPRs, PCXI, PSW, PC by name. Other CSFRs (ICR, FCX, BTV, DBGSR, …) are read from the CSFR window and are read-only.
- **HSM:** left to winIDEA; no HSM target is created. Examining it from here lets a TriCore halt stop it over TL1 with nothing to resume it.
- **Synchronous break:** winIDEA's setting. With it on, a halt of one core stops the others (and the HSM, if winIDEA debugs it). Code on a halted core that waits for a stopped HSM does not finish a step.
- **Flashing:** `program`, `flash write_image(s)` hand the files to winIDEA's download (code only, no symbols). S-records and files with an offset or type go as temporary Intel HEX. `verify_image` reads back through memory. `incremental`, `unlock` and per-sector statistics are TAS-only. Not tried on hardware.
- **Speed:** winIDEA's run control is slow (stop ~220 ms, instruction step ~250 ms, run ~1.2 s). Memory reads cost ~4–7 ms each whatever the size.
  - Source steps are handed to winIDEA when it has loaded the same ELF (its project); the log says when not.
  - No MCS targets are created; the poll period is 50 ms.
  - Polled DAQ manages ~250 reads/s across all points; use winIDEA's own DAQ (`DaqController`) for faster sampling.
- **Version:** the bindings are generated from the winIDEA 9.21.417 SDK; see the [winidea-sdk README](crates/winidea-sdk/README.md) on version skew.

```powershell
aurix-dbg.exe --adapter winidea -s server/target/openocd-tcl/tcl `
    -f board/infineon/kit_a2g_tc375_lite.cfg -f tc375_lite_example.cfg `
    -c "winidea workspace C:/ws/app.xjrf" -c "init"
```

From VS Code (`configCommands` forwards `-c`):

```json
"configCommands": ["adapter driver winidea", "winidea workspace C:/ws/app.xjrf", "init"]
```

## Cfg scripts and other derivatives

The server holds the chip mechanics; the cfg scripts hold board settings and policy:

| File | Content |
|---|---|
| OpenOCD target cfg (`target/infineon/tc3?x.cfg`, via the board cfg) | TriCore cores with `-coreid`/`-dbgbase`, PFLASH banks. |
| [`tc3xx_ocds.cfg`](../tc3xx_ocds.cfg) | Generic part of every TC3xx board cfg: finds the chip by target names; creates HSM (`tc3xx_hsm_target 1`), MCS and SCR targets; sets `ocds sync tl1` and step interrupt masking; defines the DFLASH banks; loads `<tc3xx_sbc>_sbc.cfg` when `tc3xx_sbc` names one; diagnostics (`tc3xx_status`, `tc3xx_wdt_status`, `tc3xx_scr_enable`). Its settings are listed at its top. |
| [`fs26_sbc.cfg`](../fs26_sbc.cfg) | Policy for boards with an NXP FS26 SBC (`tc3xx_sbc fs26`): every reset goes through its `reset_hook`; a halting reset is repeated until it holds; the FS26 watchdog is disarmed once after connecting (running reset before the first reset); a halt while the watchdog may be armed is watched and recovered; its `halt_check_hook` makes `attach_mode halt` reset first. `sbc_status` shows the state. |
| [`tc375_lite_example.cfg`](../tc375_lite_example.cfg), [`tc37x_fs26_example.cfg`](../tc37x_fs26_example.cfg) | Example board cfgs, without an SBC and with an FS26 (adapter speed, HSM target, STM suspend). |

For another board or derivative: copy an example, set what the board needs, load `tc3xx_ocds.cfg`. Step by step: [docs/cfg-guide.md](docs/cfg-guide.md). The hub serves the guide, the Tcl command reference and these scripts without a session through the `reference` operation (MCP, REST), so an AI client can write a board cfg before it connects.

Server commands for the cfgs (details: `help <command>`, [commands.md](docs/commands.md)):
- **`ocds sync tl1|none`**: all cores halt, resume and step together through TL1 ([below](#multi-core-run-control-ocds-sync-tl1)); `ocds tl1_members`, `ocds step all|single`, `ocds step_timers run|frozen`, `ocds tl1_force`.
- **`suspend add <module>|cores ?soft|hard|off?`**: peripherals stopped with the cores (OCS.SUS), by SVD name. Each OCS is written once the module is enabled and its ACCEN0 lets the debugger write, never before (a refused write can reset the board). `suspend status` shows each module (`pending`, `idle`, `done`, `locked`, `refused`).
- **After every reset** the server writes OCDS, the TL1 routing and the peripheral suspend again, before any handler.
- **`hsm autostart`, `hsm ready_ms`, `hsm status`**: after a halting reset the boot ROM holds the HSM at its first instruction. When the HSM boots (PROCONHSMCFG) or there is an HSM target, the server releases it and waits for HSM2HTS before latching TL1 or resuming. No HSM target needed. An HSM behind a closed debug window cannot be released (warning after a halting reset).
- **`attach_mode passive|hot|halt|reset_halt|reset_run`** (before `init`, e.g. `"configCommands": ["attach_mode halt", "init"]`), `attach_status`, `halt_all`, `halt_check_hook`, `reset_hook`, `reset_settle`.

Console procs: `daq` (DAQ sessions, read-only), `tc3xx_status`, `tc3xx_wdt_status`, `sbc_status`.

TriCore 1.8 (TC4x) is rejected.

## Tcl commands

The commands the cfg scripts use behave as in OpenOCD. The core fires `examine-end`, `halted`, `resume-start/end`, `step-start/end` and the reset events at the OpenOCD points; they are free for a board policy.

- Reference: [docs/commands.md](docs/commands.md) (generated from `crates/core/src/help.rs`; a test fails when a command has no entry).
- Built in: `help` lists commands, `help <command>` describes one (`help reset_config`), `help <word>` searches (`help srst`). In VS Code: `monitor help ...`.
- Unknown commands fail with `not supported by aurix-dbg`.

## Multi-core run control (`ocds sync tl1`)

With `ocds sync tl1` (set by `tc3xx_ocds.cfg`):
- Every BRKOUT (CPUs, HSM) drives TL1; capture-and-hold keeps it active.
- **Halt:** a halt of one core stops all TL1 members (`ocds tl1_members`: CPUs, HSM, DMA, HSSL0) and every suspended peripheral in the same cycle. A pause pulses TL1.
- **Resume:** all cores start in the same cycle.
- **Instruction step** (`ocds step all`, default): all cores run for the stepped instruction and stop together.
- `ocds step single`: only the stepping core runs; the others stay halted.
- `ocds sync none`: every core halts and runs alone.

**STMs** (with `OCS.SUS=2`) stop while their CPU is suspended by TL1 or has `DBGSR.SUSP` set:
- `ocds step_timers run` (default): STMs stop at a halt, run on continue, and advance with the cores during a step.
- `ocds step_timers frozen`: STMs also stay stopped during steps.

## Interrupts while stepping (`tricore irq_mask`)

`<target> tricore irq_mask step step_over step_out` runs steps with interrupts masked (like winIDEA's `DisableIRQ_Step`, `DisableIRQ_StepOver`, `DisableIRQ_RunUntilReturn`). `tc3xx_ocds.cfg` enables all three on every core. `tricore irq_mask off` (cfg, `configCommands` or `monitor`) turns it off.

- **Kinds:** `step` = instruction step and step into; `step_over` = source step over; `step_out` = step out.
- ICR.IE is cleared on the stepping core and stays cleared while halted between steps. Continue restores it on every core.
- Reading ICR (Registers view, `reg icr`) shows the application's IE; writing ICR while masked sets the application's IE. `mdw` on the CSFR address shows the real value.
- Stepped `enable`, `disable`, `disable Dn`, `restore Dn`, `rfe`, `bisr`, `syscall`/trap update the application's IE (`disable Dn` returns it in Dn; a trap or `bisr` saves it in PCXI.PIE).
- **Limit:** during a masked step over or step out, the callee runs masked. If it disables interrupts and does not restore them before the step ends, continue restores the IE from before the step.

## TCF services

Locator, ContextQuery, RunControl (suspend/resume, instruction and source steps), Memory, Registers, Breakpoints (address, function, file/line; data breakpoints with `AccessMode` and `Size` on an address or expression; `Condition`), StackTrace, LineNumbers, Symbols, Expressions (locals, globals, casts, assign, live evaluation while running), Disassembly (Capstone), MemoryMap (loads the ELF), ContextReset, AurixOS, AurixTrace ([trace](#on-chip-trace-minimcds): `arm`/`check <config>`, `status`, `stop`, `read <{Max}>`, `auto`, `insns`, `regs`, `find`, `replay`, `replayMove`), AurixReverse ([reverse debugging](#reverse-debugging)), Tcl (`monitor`).

`AurixOS` ([OS awareness](#autosar-os-awareness-orti)) runs on the debug adapter's connection; the awareness ends with the session:

| Command | Arguments | Reply |
|---|---|---|
| `load` | `<file>`, `<target>` or null | the `getInfo` reply |
| `getInfo` | — | `{Loaded, File, Implementation, Version, Cores, Types[{Name, Description, Count, Attributes[{Name, Description, Kind, PerCore, Live}]}], Errors}` |
| `getObjects` | `<kind>`, `<name>` or null, `<liveOnly>` | `{Kind, Objects[{Name, Attributes[{Name, Core?, Value, Number?, Link?, Error?}]}], Reads}` |
| `getRunning` | — | `{Cores[{Core, CoreId, Task, Priority, Isr, LastError}], Reads}` (called on every stop to name threads by task; a few reads) |
| `startProfile` | `<periodUs>`, `<lanes>` or null | `{PeriodUs, Cores, Lanes}` |
| `getProfileLanes` | — | the lanes the loaded ORTI file offers |
| `stopProfile` | — | `{Stopped}` |
| `getProfile` | `<sinceUs>` | `{Running, PeriodUs, NowUs, Lanes, Transitions[{T, Core, Lane, Task}], Totals[{Core, Lane, Task, Us}], Dropped, Missed, TraceLost}` |
| `getStacks` | `<names>` or null | `{Stacks[{Name, Base, Size, Direction, Pattern, Used?, Free?, Percent?, Error?}], Reads}` |

Context IDs: chip `S0.D0`, core `S0.D0.C<coreid>`, stack frame `…C0.F<n>`.

## Flashing (TC3xx PFLASH, `tc3x_eflash`)

Commands (full syntax in [commands.md](docs/commands.md#flash)): `flash info|probe|erase_check|erase_sector|write_image|write_images|verify_image <bank> ...`, `verify_image`, `program <file> [preverify] [verify] [reset] [exit] [offset]`, `flash loader on|off`, `aurix_eflash fallback`.

**RAM flash loader** (`flash loader on`, default):
- All cores are halted. `flash write_image(s)` copies a small loader to the start of cpu0's PSPR and its buffers to the start of its DSPR; cpu0 programs while the debugger moves data in 8 KB blocks.
- Much faster than driving the DMU over the debug interface (about 3.7 KB/s there), most of all on the BlueBox.
- The loader turns off the CPU0 and safety watchdogs until the next reset. cpu0 gets its registers back; the RAM keeps the loader.
- If it cannot start (no cpu0, RAM not writable, watchdogs that do not turn off), or with `flash loader off`, the driver runs the DMU sequences over the debug interface. The other `flash` commands always do.
- Source: `crates/core/src/flash_loader.s` (Green Hills `astri -nooptimize`); the simulator executes it.

Commands:
- **`flash write_image [erase] [incremental] <file>`:** DMU-erased sectors are programmed directly; other touched sectors need `erase`. `incremental` skips sectors that already match, but it reads flash, and reading unprogrammed PFLASH raises an ECC error the application's SMU may turn into a reset.
- **`flash write_images [erase] [incremental] [unlock] [verify] [-offset <offset>] [-type <type>] <file> ...`:** merges the files into one image (later files win) and writes it. Use it instead of writing files one by one, which would erase earlier files' data in shared 16 KB sectors.
  - `-offset` (added to the file's addresses; load address of a binary) and `-type` (`elf`, `ihex`, `s19`/`srec`, `bin`) apply to the next file. REST/MCP `flash_write` takes `{"path", "offset", "type"}` entries.
  - `verify`: with `incremental` reads back only the programmed sectors; otherwise the whole image. Reading runs at ~470 KB/s at `adapter speed 5000`, ~2 MB/s at 30000.
  - The VS Code extension flashes `flashFiles` with this command (`verifyFlash` selects `verify`).
- **`verify_image`:** reads only the image bytes, through the non-cached 0xA segment.
- **`program … preverify`:** leaves flash untouched when the image already matches.

Images and banks:
- **Formats:** ELF (load segments), Intel HEX, Motorola S-records (S1/S2/S3; lines starting with `;` or `#` are comments), raw binary. Without a type: ELF magic, extension (`.hex`, `.srec`/`.s19`/`.s28`/`.s37`/`.mot`/`.sx`) or first non-comment line.
- UCB data in an image is skipped with a warning (with `unlock`: an error). Program UCBs with the `ucb` commands ([commands.md](docs/commands.md#user-configuration-blocks-ucb-tc3xx)).
- **PFLASH layout** by `SCU_CHIPID.CHID`: TC36x/37x/38x/39x/3Ex from a table, like OpenOCD's `aurix_eflash`. Other TC3xx (e.g. TC33x, TC35x): common scheme with a warning (PFn at 0x80000000 + n × 3 MB, up to 3 MB, busy bit 2 + n). A `flash bank` may be smaller than its table entry.
- **DFLASH:** DFLASH0 (0xAF000000, up to 1 MB) and DFLASH1 (0xAFC00000, 128 KB), same on all variants; `erase_sector` and `erase_check` only. `tc3xx_ocds.cfg` defines `dfls0` (where the chip's DF0 size is known, or `tc3xx_dflash0_size` is set; TC377: 256 KB) and `dfls1` (HSM data, 128 KB).

## Tests

- **Offline:** `cargo test --workspace`. Uses the simulator (`--adapter sim`) for the cfg files, the API, TCF, DAQ, watchpoints, the hub with real worker processes, install/uninstall and remote access.
- **Real files:** tests that need a real application's build output skip unless these are set:

  | Variable | Content |
  |---|---|
  | `AURIX_TEST_ELF` | TriCore application ELF |
  | `AURIX_TEST_FUNCTION` | a function in it (default `main`) |
  | `AURIX_TEST_ORTI` | its ORTI file |
  | `AURIX_TEST_HSM_ELF` | HSM ELF |
  | `AURIX_TEST_APP_ELF` + `AURIX_TEST_BM_ELF` | application and bootloader |
  | `AURIX_TEST_MCS_DIR` | MCS assembler sources with their GHS output |
  | `AURIX_TEST_SREC` | an S-record image |

- **Hardware:** scripts in `tools/` drive the extension's debug adapter against a running worker (board free, no OpenOCD). Start the worker by hand on the ports they default to: `aurix-dbg ... -c "tcf_port 1534" -c "api_port 8080"` (or pass a hub session's ports). ELF and breakpoint location as arguments or `AURIX_TEST_ELF` / `AURIX_TEST_BP_LOC`; `dap_m6.js` needs more variables (see its header).
  - `node tools/dap_smoke.js`: run control, registers, SFR memory, monitor.
  - `node tools/dap_m5.js`: source breakpoint, call stack, step over/into/out.
  - `node tools/dap_m6.js`: locals, register/global/struct writes, enum names, live watch.
  - `node tools/dap_m8.js`: disassembly and reset.
  - `node tools/tcf_smoke.js`: raw TCF services.
  - `node tools/dap_external_bp.js [tcf_port] [api_port]`: API breakpoints in VS Code's Breakpoints view (also against the simulator).
  - `node tools/dap_watch.js [tcf_port] [api_port] [expression] [elf]`: data breakpoints and API watchpoints (also against the simulator; `HIT=1` waits for a hit on a running target).
  - DAQ: `pip install -e python`, sample a changing global by name, check rate and jitter from the `t_us` deltas.

## Not yet implemented

- gdb RSP frontend (contract in [docs/gdb-frontend.md](docs/gdb-frontend.md)).
- Flashing on the `winidea` backend is implemented but not confirmed on hardware.
- DFLASH programming and PFLASH protection unlock.
- A TCF Flash service (flashing goes through Tcl commands).
- Frame names for code without DWARF functions (the frame ID is shown).
- Memory writes to SMPU-protected RAM.
- Target-side DAQ timestamps (sampling is host-timed; jitter-free values come from the [miniMCDS trace](#on-chip-trace-minimcds), CPU0's writes only).
- DAQ points that follow a pointer (`p->x` is resolved once, at subscribe time).
