# Tcl command reference

<!-- Generated from crates/core/src/help.rs by `UPDATE_DOCS=1 cargo test -p aurix-core reference`. Do not edit. -->

The commands aurix-dbg understands in cfg scripts, on the Tcl console (`telnet localhost 4444`), as `monitor` commands in the VS Code Debug Console or GDB, in `configCommands` and in debugger actions. They follow OpenOCD where OpenOCD has the same command.

The same text is available at runtime: `help` lists the commands, `help <command>` describes one and its subcommands, and `help <word>` searches the descriptions (`monitor help srst`). `usage <command>` shows only the arguments. Procs of cfg scripts appear there too when the script documents them with `add_help_text` and `add_usage_text`.

Notation: `<value>` is a value to fill in, `[...]` is optional, `a|b` is one of the choices.

## Contents

- [Session setup](#session-setup)
- [Probe (adapter)](#probe-adapter)
- [Taps and targets](#taps-and-targets)
- [Target objects (`<target> ...`)](#target-objects-target-)
- [Reset](#reset)
- [Run control](#run-control)
- [Memory and registers](#memory-and-registers)
- [Breakpoints and watchpoints](#breakpoints-and-watchpoints)
- [Core-specific (`tricore`, `cortex_m`, `gtm_mcs`, `xc800`)](#core-specific-tricore-cortex_m-gtm_mcs-xc800)
- [Flash](#flash)
- [Server, ports and logging](#server-ports-and-logging)
- [Scripting](#scripting)
- [TAS adapter (`adapter driver tas_client`)](#tas-adapter-adapter-driver-tas_client)
- [winIDEA adapter (`adapter driver winidea`)](#winidea-adapter-adapter-driver-winidea)
- [BlueBox adapter (`adapter driver bluebox`)](#bluebox-adapter-adapter-driver-bluebox)
- [Accepted for OpenOCD compatibility](#accepted-for-openocd-compatibility)

## Session setup

### `init`

```
init
```

Connect to the probe and the target, examine the cores and start the servers.

Runs the `init_targets` and `init_board` procs of the cfg first. A cfg that ends without `init` (and without `noinit`) gets one from the server once all scripts ran.

### `noinit`

```
noinit
```

Do not run `init` after the cfg scripts.

### `shutdown`

```
shutdown
```

Stop the server.

### `transport select`

```
transport select [ifxdap|dap|jtag]
```

Select the transport; without an argument, show it.

Every adapter talks DAP to the device, so the name is only checked. Default `ifxdap`.

### `transport list`

```
transport list
```

List the transports (`ifxdap`).

### `reset_config`

```
reset_config [<option> ...]
```

Set the reset options; without arguments, show them.

Of OpenOCD's options only two change anything:

- `connect_assert_srst`: `init` connects with a reset and halts the cores at the reset vector (TAS: reset-and-halt connect; BlueBox: probe reset right after the attach). Needed for a device locked with a debug password (`adapter debug_password`), and for a target whose application keeps the debugger out.
- `connect_deassert_srst`: connect to the running target without a reset (hot attach, the default).

The others (`srst_only`, `trst_and_srst`, `srst_nogate`, ...) are stored and shown, but have no effect. The winIDEA adapter ignores the setting: the winIDEA workspace decides how it connects.

## Probe (adapter)

### `adapter driver`

```
adapter driver <name>
```

Select the probe driver: `tas_client`, `sim`, `winidea`, `bluebox`.

`tas_client` talks to a TasServer (miniWiggler, DAS), `winidea` drives a winIDEA instance through its SDK, `bluebox` talks to an iSYSTEM BlueBox over USB without winIDEA, `sim` is the built-in simulated TC37x. The server's `--adapter` option overrides the cfg's choice. Must come before the driver's own commands.

### `adapter name`

```
adapter name
```

The driver in use (after any `--adapter` override).

Cfg scripts branch on it to skip what a backend cannot do: `if {[adapter name] eq "tas_client"} { ... }`.

### `adapter list`

```
adapter list
```

List the drivers this build supports.

### `adapter speed`

```
adapter speed [<kHz>]
```

Set the DAP clock; without an argument, show it.

Applies at once when given after `init`. The BlueBox rounds down to 100 MHz divided by an integer (1 to 25 MHz).

### `adapter serial`

```
adapter serial <serial>
```

Pick the probe by serial number when several are plugged in.

### `adapter debug_password`

```
adapter debug_password [none | -env <variable> | <PW0> .. <PW7>]
```

Set the debug password of a locked device (`UCB_DBG`); without an argument, say whether one is set.

The eight words are hex (`0x` optional), PW0 first; one token of 64 hex digits is taken as well. `-env` reads them from an environment variable, so a cfg under version control need not hold the password. The words are never logged. A locked device takes the password only in the window right after a reset, so the session also needs `reset_config connect_assert_srst`.

### `adapter srst|assert|deassert|usb|gpio`

```
adapter srst|assert|deassert|usb|gpio ...
```

Ignored with a warning: the probes drive reset themselves.

### `probe_lost_timeout`

```
probe_lost_timeout [<ms>]
```

How long the probe may be gone before the debug sessions end; without an argument, show it.

Covers an unplugged or powered-off BlueBox or miniWiggler and a TasServer that went away. `0` keeps reconnecting for ever. Default 1000 ms.

## Taps and targets

### `ifxdap newtap`

```
ifxdap newtap <chip> <tap> -irlen <n> [-expected-id <id>] ...
```

Declare a DAP tap `<chip>.<tap>`.

`init` checks the device's JTAG ID against the `-expected-id` values (none: no check). `-ircapture`, `-irmask`, `-enable`, `-disable`, `-ignore-version` and `-ignore-bypass` are accepted and ignored.

### `ifxdap names`

```
ifxdap names
```

List the declared taps.

### `jtag newtap`

```
jtag newtap <chip> <tap> -irlen <n> [-expected-id <id>] ...
```

Same as `ifxdap newtap`.

### `jtag names`

```
jtag names
```

List the declared taps.

### `jtag tapisenabled`

```
jtag tapisenabled <tap>
```

Always `1`.

### `ocmts create`

```
ocmts create <name> -chain-position <tap>
```

Declare the on-chip debug system (OCMTS) behind a tap.

Every TriCore target needs one (`target create ... -ocmts <name>`).

### `ocmts names`

```
ocmts names
```

List the declared OCMTS.

### `target create`

```
target create <name> <type> [<option> ...]
```

Create a target (a core) and a command of the same name.

Types: `tricore`, `cortex_m` (the HSM), `gtm_mcs` (a GTM MCS channel), `xc800` (the SCR). The options are those of `<target> configure`; a TriCore target needs `-ocmts`. The new target becomes the current one.

### `target names`

```
target names
```

List the targets.

### `target current`

```
target current
```

The current target, which commands without a target prefix act on.

### `target types`

```
target types
```

List the target types (`tricore cortex_m gtm_mcs xc800`).

### `target count`

```
target count
```

The number of targets.

### `target number`

```
target number <n>
```

The name of target number `<n>`.

### `target smp`

```
target smp ...
```

Ignored with a warning: the cfg scripts synchronise the cores (TL1).

### `targets`

```
targets [<name>]
```

Make `<name>` the current target; without an argument, list the targets and their state.

## Target objects (`<target> ...`)

### `<target> configure`

```
<target> configure <option> <value> ...
```

Change a target's options.

- `-event <event> <body>`: Tcl run on an event (see `<target> invoke-event`).
- `-coreid <n>`: the core number.
- `-dbgbase <address>`: the base of the core's debug registers.
- `-ocmts <name>`: the OCMTS the TriCore sits behind (cannot be changed later).
- `-chain-position <tap>`: the tap.
- `-module <0..9>`, `-channel <0..7>`, `-rambase <address>`, `-ramsize <bytes>`: GTM MCS only. `-ramsize` is the MCS RAM per module (default 12 KB, as on the TC37x) and applies to every channel of the module.
- `-defer-examine`: do not examine the core at `init` (cores that start later).
- `-work-area-*`, `-endian`, `-rtos`, `-gdb-port`, `-gdb-max-connections`: stored for `cget`, no effect.

### `<target> cget`

```
<target> cget <option> [<event>]
```

Read a target option back (`cget -event <event>` gives the handler).

### `<target> curstate`

```
<target> curstate
```

The run state: `halted`, `running`, `reset`, `unknown`.

### `<target> debug_reason`

```
<target> debug_reason
```

Why the target last halted.

### `<target> was_examined`

```
<target> was_examined
```

`1` once the core was examined.

### `<target> examine_deferred`

```
<target> examine_deferred
```

`1` when the target was created with `-defer-examine`.

### `<target> invoke-event`

```
<target> invoke-event <event>
```

Run the target's handler for an event now.

Events fired by the server: `examine-start`, `examine-end`, `examine-fail`, `halted`, `debug-halted`, `resume-start`, `resumed`, `resume-end`, `step-start`, `step-end`, `reset-start`, `reset-assert-pre`, `reset-assert` (replaces the probe's reset when set), `reset-assert-post`, `reset-deassert-pre`, `reset-deassert-post`, `reset-init`, `reset-end`.

### `<target> eventlist`

```
<target> eventlist
```

List the target's event handlers.

### `<target> arp_examine`

```
<target> arp_examine
```

Examine the core.

### `<target> arp_poll`

```
<target> arp_poll
```

Poll every target's run state.

### `<target> arp_halt`

```
<target> arp_halt
```

Request a halt without waiting for it.

### `<target> arp_resume`

```
<target> arp_resume
```

Resume the core.

### `<target> arp_step`

```
<target> arp_step
```

Step one instruction.

### `<target> arp_reset`

```
<target> arp_reset assert|deassert <halt>
```

One half of the reset sequence, for `reset-assert` handlers.

### `<target> arp_waitstate`

```
<target> arp_waitstate halted|running|reset|unknown <ms>
```

Wait until the core is in the given state.

### `<target> <command>`

```
<target> <command> ...
```

Run any other command on this target instead of the current one.

E.g. `cpu1 mdw 0x70000000 4`, `cpu1 halt`, `cpu2 tricore irq_mask off`.

## Reset

### `reset`

```
reset [run|halt|init]
```

Reset the chip (default `run`).

`halt` halts the cores at the reset vector; `init` also runs the targets' `reset-init` handlers. The sequence fires `reset-start`, `reset-assert-pre`/`-post`, `reset-deassert-pre`/`-post` and `reset-end` on every target. With `reset_hook` set, the hook proc gets the request instead. A probe that can only reset with a halt (BlueBox) resets as `reset halt` and resumes the cores after `reset-end` for `reset run`.

### `reset_hook`

```
reset_hook [<proc>]
```

Send every reset request to `<proc> <mode>`; without an argument, show the hook.

Requests from Tcl, the debugger frontends, the API and `program` all go to the proc. A `reset` inside the proc runs the built-in sequence. An empty name removes the hook. `fs26_sbc.cfg` uses this to reset through the FS26.

### `ocd_process_reset`

```
ocd_process_reset <mode>
```

Same as `reset <mode>` (OpenOCD's name for it).

## Run control

### `halt`

```
halt [<ms>]
```

Halt the current target and wait for it (default 5000 ms).

### `wait_halt`

```
wait_halt [<ms>]
```

Wait until the current target halts (default 5000 ms).

### `resume`

```
resume [<address>]
```

Resume the current target, at `<address>` if given.

### `step`

```
step [<address>]
```

Step one instruction on the current target, from `<address>` if given.

With `tricore tl1_step on`, all cores run for the stepped instruction.

### `poll`

```
poll [on|off]
```

Show the current target's state; `on`/`off` switches the background poll.

The background poll notices halts and resets; it runs every `polling_period`.

### `polling_period`

```
polling_period [<ms>]
```

Set how often the background poll runs; without an argument, show it.

Default 10 ms (15 ms on the BlueBox unless the cfg sets it).

## Memory and registers

### `mdw`

```
mdw ['phys'] <address> [<count>]
```

Display 32-bit words.

### `mdh`

```
mdh ['phys'] <address> [<count>]
```

Display 16-bit halfwords.

### `mdb`

```
mdb ['phys'] <address> [<count>]
```

Display bytes.

### `mww`

```
mww ['phys'] <address> <value> [<count>]
```

Write a 32-bit word (`<count>` times).

### `mwh`

```
mwh ['phys'] <address> <value> [<count>]
```

Write a 16-bit halfword (`<count>` times).

### `mwb`

```
mwb ['phys'] <address> <value> [<count>]
```

Write a byte (`<count>` times).

### `read_memory`

```
read_memory <address> 8|16|32|64 <count> ['phys']
```

Read `<count>` values and return them as a Tcl list (at most 65536).

### `write_memory`

```
write_memory <address> 8|16|32|64 <list> ['phys']
```

Write the values of a Tcl list.

### `reg`

```
reg [<register> [<value>]]
```

Show all registers of the current target, or read or write one by name or number.

### `daq`

```
daq [status | gap [<bytes>] | merge [0|1]]
```

Show the live sampling (DAQ) sessions and what the probe can take.

Sessions are opened on the DAQ WebSocket, not here. `gap` is how far the sampler may over-read to merge two points into one bus access (never in peripheral space); `merge 0` stops due points from riding in the run-state poll's batch (to measure what that is worth).

## Breakpoints and watchpoints

### `bp`

```
bp [<address> <length> ['hw']]
```

Set a hardware breakpoint on the current target; without arguments, list them.

### `rbp`

```
rbp all|<address>
```

Remove a breakpoint, or all of them.

### `wp`

```
wp [<address> <length> [r|w|a]]
```

Set a watchpoint (default `a`: any access); without arguments, list them.

Value and mask conditions are not supported. On the TriCore a watchpoint matches accesses that start inside the range; the end is exclusive.

### `rwp`

```
rwp all|<address>
```

Remove a watchpoint, or all of them.

## Core-specific (`tricore`, `cortex_m`, `gtm_mcs`, `xc800`)

### `tricore tl1_halt`

```
tricore tl1_halt [on|off]
```

Let a halt request also pulse trigger line TL1, so every core stops with this one.

`tc3xx_ocds.cfg` turns it on, so pause in the debugger stops all cores in the same cycle.

### `tricore tl1_step`

```
tricore tl1_step [on|off]
```

Run all cores for an instruction step, not only the stepping one.

`tc3xx_ocds.cfg` turns it on. The other cores are released by the cfg's `step-start` and stopped again by TL1 in the same cycle as the stepping core.

### `tricore irq_mask`

```
tricore irq_mask [off | on | step step_over step_out]
```

Mask interrupts during steps of the given kinds.

Like winIDEA's `DisableIRQ_Step`, `DisableIRQ_StepOver` and `DisableIRQ_RunUntilReturn`. `on` selects all three. The application keeps seeing its own ICR.IE in the Registers view. `tc3xx_ocds.cfg` enables all three.

### `tricore triggers`

```
tricore triggers
```

List the core's hardware triggers (TriCore TRx, Cortex-M FPB/DWT, MCS breakpoints).

### `tricore mdw`

```
tricore mdw <address> [<count>]
```

Display words as the core sees them (HSM RAM, boot ROM, local scratchpads).

### `cortex_m ...`

```
cortex_m ... triggers | mdw <address> [<count>]
```

Same as `tricore`, for the HSM; `triggers` lists FPB and DWT comparators.

### `gtm_mcs ...`

```
gtm_mcs ... triggers | mdw <address> [<count>]
```

Same as `tricore`, for an MCS channel.

### `xc800 ...`

```
xc800 ... triggers | mdw <address> [<count>]
```

Same as `tricore`, for the SCR: `triggers` lists its hardware breakpoints, `mdw` takes its virtual addresses (code 0x0000-0xFFFF, XDATA 0x10000, IRAM 0x20000-0x200FF, SFRs 0x40000-0x7FFFF with the page in bits 17:15 and the page register in bits 14:8, TC3xx UM Table 558).

Only while the SCR is halted: its debug monitor serves the accesses. The SFR window 0x40000-0x7FFFF also reaches the SCR from the other cores' TCF contexts (the Peripheral View reads through the TriCore).

## Flash

### `flash bank`

```
flash bank <name> <driver> <base> <size> <chip_width> <bus_width> <target> [<option> ...]
```

Declare a flash bank (drivers `tc3x_eflash`, `aurix_eflash`).

### `flash banks`

```
flash banks
```

List the flash banks (also `flash list`).

### `flash info`

```
flash info <bank>
```

Probe a bank and show its geometry (also `flash probe`).

### `flash erase_check`

```
flash erase_check <bank>
```

Show which sectors of a bank are erased.

### `flash erase_sector`

```
flash erase_sector <bank> <first> <last>|last
```

Erase sectors of a bank.

### `flash write_image`

```
flash write_image [erase] [incremental] [unlock] <file> [<offset> [<type>]]
```

Program an image (ELF, HEX, S-record, binary).

`incremental` only programs the sectors that differ. On the winIDEA adapter the file goes to winIDEA's own download.

### `flash write_images`

```
flash write_images [erase] [incremental] [unlock] [verify] [-offset <offset>] [-type <type>] <file> ...
```

Program several images in one go.

The files are merged first (later files win), so a file cannot erase what an earlier one programmed into a shared sector. `-offset` and `-type` apply to the file that follows them.

### `flash verify_image`

```
flash verify_image <file> [<offset> [<type>]]
```

Same as `verify_image`.

### `verify_image`

```
verify_image <file> [<offset> [<type>]]
```

Compare an image with the target's memory.

### `program`

```
program <file> [preverify] [verify] [reset] [exit] [<offset>]
```

Reset, program and optionally verify, reset and exit (OpenOCD's `program`).

Runs `init` first if needed, resets with `reset init`, and skips programming when `preverify` finds the image already there.

### `aurix_eflash fallback`

```
aurix_eflash fallback <bank> [on|off]
```

Accepted: the command sequences (OpenOCD's fallback mode) are always used.

## Server, ports and logging

### `tcf_port`

```
tcf_port [<port>|disabled]
```

TCP port of the TCF server (the VS Code extension); default 1534.

### `telnet_port`

```
telnet_port [<port>|disabled]
```

TCP port of the Tcl console; default 4444.

### `gdb_port`

```
gdb_port [<port>|disabled]
```

TCP port of the GDB server; disabled by default.

### `tcl_port`

```
tcl_port [<port>|disabled]
```

TCP port of OpenOCD's Tcl RPC server; disabled by default.

### `api_port`

```
api_port [<port>|disabled]
```

TCP port of the REST, MCP and WebSocket API; default 8080.

### `bindto`

```
bindto [<address>]
```

The address the servers listen on; default 127.0.0.1.

### `debug_level`

```
debug_level [0..4]
```

Set the log level: 0 errors, 1 warnings, 2 info (default), 3 debug.

### `log_output`

```
log_output <file>
```

Also write the log to a file.

### `version`

```
version
```

The server version.

## Scripting

### `help`

```
help [<command> | <word>]
```

List the commands, or describe one.

`help <command>` shows a command and its subcommands (`help adapter`, `help cpu0`). Anything else searches the names and descriptions: `help srst` finds `reset_config`. Cfg procs documented with `add_help_text` are included.

### `usage`

```
usage [<command>]
```

Show the arguments of a command (all commands without an argument).

### `add_help_text`

```
add_help_text <command> <text>
```

Document a command, e.g. a proc of a cfg script, for `help`.

### `add_usage_text`

```
add_usage_text <command> <text>
```

Give a command's arguments for `help` and `usage`.

### `script`

```
script <file>
```

Run a Tcl file, looked up in the search directories.

### `find`

```
find <file>
```

The path of a file in the search directories.

### `add_script_search_dir`

```
add_script_search_dir <directory>
```

Add a directory to search for `script` and `find`.

### `echo`

```
echo [-n] <text>
```

Print a message to the log.

### `sleep`

```
sleep <ms> [busy]
```

Wait.

## TAS adapter (`adapter driver tas_client`)

### `tas-client host`

```
tas-client host [<host> [<port>]]
```

The TasServer to connect to; without arguments, show it.

### `tas-client device_type`

```
tas-client device_type [<id>]
```

Operate the device as this JTAG ID; without an argument, show it.

For a device the access hardware does not report by itself, e.g. the SCR once the application started its OCDS SoC DAP mode. Must be set before `init`.

### `tas-client targets`

```
tas-client targets
```

List every device the TasServer enumerates.

### `tas-client jtag_scan`

```
tas-client jtag_scan {<ir_width> <ir> <dr_bits> <hex>} ...
```

Raw JTAG scans; returns the shifted-out data.

`<ir_width>` 0 keeps the IR; `<hex>` is shifted LSB first.

## winIDEA adapter (`adapter driver winidea`)

### `winidea workspace`

```
winidea workspace <path>
```

The winIDEA workspace (`.xjrf`) to open.

### `winidea start`

```
winidea start if-required|existing|always
```

Attach to a running winIDEA, start one, or both (default `if-required`).

### `winidea status`

```
winidea status
```

The connection, and the cfg settings this backend ignores.

## BlueBox adapter (`adapter driver bluebox`)

### `bluebox winidea`

```
bluebox winidea <folder>
```

A winIDEA 9.21.417 install to take the probe's kernel and FPGA image from.

### `bluebox kernel`

```
bluebox kernel <file>
```

The probe kernel file (`iC6kKrnl.s`) itself, unchecked.

### `bluebox attach_replay`

```
bluebox attach_replay <trace.jsonl> [<step>]
```

Replay a captured attach after the driver's own bring-up (another target or setup).

### `bluebox status`

```
bluebox status
```

The probe and target the driver connected to.

### `bluebox health`

```
bluebox health
```

Ask the probe about the target connection now: supply, reset latch, debug access.

### `bluebox health_period`

```
bluebox health_period [<ms>]
```

How often the background poll asks the probe about the target connection; 0 never. Default 100 ms.

### `bluebox debug_mode`

```
bluebox debug_mode [standard|wide]
```

The debug channel mode the attach sets up; set before `init`.

`wide` (DAP Wide, the default) is what the built-in attaches were captured with. `standard` (DAP Standard) is the only mode the SCR (`xc800` target) works in; the DAP clock then stays at 10 MHz or below. `tc3xx_scr_target 1` selects it.

### `bluebox daq`

```
bluebox daq
```

The probe's sampler and the items it samples.

### `bluebox daq_offload`

```
bluebox daq_offload [on|off]
```

Let the probe sample live values itself (default `on`), or read every point from the host.

### `bluebox drop_target`

```
bluebox drop_target
```

Test aid: reset the target and leave the connection broken, as after a reset the target did itself.

### `bluebox raw`

```
bluebox raw <path> [<hex body>]
```

Protocol aid: send one service message as it is and return the reply as hex.

### `bluebox scr_mem`

```
bluebox scr_mem <space> <address> <length> [<hex data>]
```

Protocol aid: read (with data: write) SCR memory in the probe's own terms.

Memory spaces: 0 code/XDATA, 1 directly addressed data (IRAM 0x00-0x7F, the SFRs of the pages the program selected), 2 IDATA, 4 XDATA, 12 the monitor's virtual address map (winIDEA's area 12; the SCR target uses it for upper IRAM and all SFRs). Prints the bytes and how many failed. While the SCR runs, the probe answers with zeros.

## Accepted for OpenOCD compatibility

### `init_targets`

```
init_targets
```

Called by `init`; a cfg may redefine it.

### `init_board`

```
init_board
```

Called by `init`; a cfg may redefine it.

### `using_jtag`

```
using_jtag
```

Always `0`.

### `using_swd`

```
using_swd
```

Always `0`.

### `using_hla`

```
using_hla
```

Always `0`.

### `gdb_memory_map`

```
gdb_memory_map ...
```

No effect.

### `gdb_flash_program`

```
gdb_flash_program ...
```

No effect.

### `gdb_breakpoint_override`

```
gdb_breakpoint_override ...
```

No effect.
