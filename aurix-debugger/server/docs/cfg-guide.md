# Writing cfg scripts and profiles

How to set up aurix-dbg for a board: which scripts a session loads, what goes in a board cfg, and
how a hub profile names them. The hub serves this text through MCP and REST (`reference`, topic
`cfg`); the Tcl commands are in [commands.md](commands.md) (`reference`, topic `commands` or a
command name).

## How a session starts

A session (one worker process, one board) runs OpenOCD-style options in order:

1. `-s <dir>` search directories. The bundled OpenOCD scripts (`openocd/tcl` next to the hub) always
   come first; `search_dirs` adds more.
2. `-f <file>` cfg scripts (`config_files`), then `-c <command>` Tcl commands (`commands`).
3. `init`: connects to the probe and the device, examines the cores and starts the servers. A cfg
   that ends without `init` (and without `noinit`) gets one after the last script and command.

Commands that must come before `init` (`attach_mode`, `adapter serial`, settings read by the
scripts) go in a cfg or in `commands`. A relative script name is looked up in the search
directories, the last one added first, then in the worker's working directory (the profiles
directory for a profile). Absolute paths with forward slashes are the safest.

## The layers

| Layer | File | What it does |
|---|---|---|
| OpenOCD board cfg | bundled, `board/infineon/<kit>.cfg` | Selects the probe driver (`tas_client`), `reset_config`, `adapter speed`, then sources the target cfg. |
| OpenOCD target cfg | bundled, `target/infineon/tc3?x.cfg` | Declares the tap (`ifxdap newtap ... -expected-id`), the OCMTS, the TriCore cores (`target create <chip>.cpuN tricore -coreid N -dbgbase ...`) and the PFLASH banks. |
| Board cfg | yours, e.g. `my_board.cfg` | Board settings and policy: probe speed, safety SBC, extra targets, the peripherals to suspend. Loads `tc3xx_ocds.cfg`. |
| `tc3xx_ocds.cfg` | aurix-dbg, copy next to the board cfg | Generic TC3xx part: finds the chip from the target names, creates the optional HSM, GTM MCS and SCR targets and the DFLASH banks, turns on TL1 sync and step interrupt masking, loads `<tc3xx_sbc>_sbc.cfg`. |
| `fs26_sbc.cfg` | aurix-dbg, copy next to it when the board has an NXP FS26 | Reset and halt policy for the FS26 watchdog (`reset_hook`, `halt_check_hook`, cpu0 event handlers). |

`tc3xx_ocds.cfg` and `fs26_sbc.cfg` are not installed with the hub: they come from the aurix-dbg
repository, and `reference` returns their text (topics `tc3xx_ocds.cfg`, `fs26_sbc.cfg`) to save
next to the board cfg. The board cfg loads them relative to itself:

```tcl
script [file join [file dirname [info script]] tc3xx_ocds.cfg]
```

### Bundled board cfgs (TC3xx)

| Board cfg | Chip cfg | Note |
|---|---|---|
| `board/infineon/kit_a2g_tc375_lite.cfg` | `tc37x.cfg` | Also fits a TC377 (JTAG ID 0x10207083). |
| `board/infineon/kit_a2g_tc377tx_3v3_trb.cfg` | `tc37x_EXT.cfg` | Expects ID 0x10208083 (TC37x EXT); refused on a plain TC377 over TAS: use the Lite Kit cfg there. |
| `board/infineon/kit_a2g_tc366_5v_trb.cfg` | `tc36x.cfg` | |
| `board/infineon/kit_a2g_tc387_3v3_trb.cfg`, `kit_a2g_tc387_3v3_tft.cfg` | `tc38x.cfg` | |
| `board/infineon/kit_a2g_tc397_5v_trb.cfg` | `tc39x.cfg` | |

For a board without a kit cfg, write the first lines yourself:

```tcl
source [find interface/tas_client.cfg]
reset_config srst_only
source [find target/infineon/tc37x.cfg]
```

TriCore 1.8 (TC4x) is refused.

## A board cfg

Without a safety SBC (e.g. the TC375 Lite Kit):

```tcl
script [file join [file dirname [info script]] tc3xx_ocds.cfg]
suspend add cores
```

With an NXP FS26 and the optional targets:

```tcl
adapter speed 30000              ;# kHz; 30 MHz is the miniWiggler V3 maximum
set tc3xx_sbc fs26               ;# loads fs26_sbc.cfg
# set tc3xx_hsm_target 1         ;# debug the HSM
# set tc3xx_mcs_modules {0 1}    ;# GTM MCS0 and MCS1 channels as targets
script [file join [file dirname [info script]] tc3xx_ocds.cfg]
suspend add cores
# suspend add EVADC
# suspend add GPT120 hard
```

Load it after the OpenOCD board cfg: `config_files = ["board/infineon/kit_a2g_tc375_lite.cfg",
"C:/work/my_board.cfg"]`.

### Settings of `tc3xx_ocds.cfg`

Set as Tcl variables before the `script` line; those marked * may also be set after it, before
`init` (e.g. in `commands`).

| Variable | Meaning | Default |
|---|---|---|
| `tc3xx_sbc` | Safety-SBC policy: loads `<name>_sbc.cfg`, e.g. `fs26`; `""` none. | `""` |
| `tc3xx_hsm_target` * | 1: create `<chip>.hsm` to debug the HSM, halted and resumed with the cores. Not needed for the HSM release after a reset. | 0 |
| `tc3xx_mcs_modules` * | GTM MCS modules to create `<chip>.mcs<m>.ch0..ch7` for, e.g. `{0 1}`. | `{}` |
| `tc3xx_mcs_ramsize` | MCS RAM bytes per module. | 12 KB (TC37x) |
| `tc3xx_scr_target` * | 1: create `<chip>.scr`, the standby controller (adapter `bluebox` or `tas_client`). The SCR must run in OCDS SoC DAP boot mode (started by the application or `tc3xx_scr_enable`). On a BlueBox it selects debug mode DAP Standard. | 0 |
| `tc3xx_dflash` | 1: flash banks `dfls0` (DFLASH0) and `dfls1` (DFLASH1, HSM data). | 1 |
| `tc3xx_dflash0_size` | DFLASH0 size in bytes; known for tc37x, other chips: set it or `dfls0` is left out. | |

The FS26 settings (`sbc_rstb_wired`, `sbc_disarm_delay_ms`, ...) are listed at the top of
`fs26_sbc.cfg`.

### Commands a board cfg typically uses

Details: `reference` with the command name, or `help <command>` on the Tcl console.

- `adapter speed <kHz>`, `adapter serial <serial>` (several probes plugged in).
- `attach_mode passive|hot|halt|reset_halt|reset_run`: how `init` attaches (default `hot`: no reset,
  no halt). Usually set per launch (`commands` or VS Code `configCommands`), not in the board cfg.
- `suspend add <module>|cores [soft|hard|off]`: peripherals stopped while the cores are halted, by
  SVD name. `cores` is the STMs of the cores: an OS whose tick timer runs on through a halt may
  report errors or reset the board after the resume. Nothing is suspended unless listed. Add what
  the application needs (ADC, timers, CAN), one module at a time. The server writes each OCS only
  when the write can land; never write OCS registers with `mww` (a refused write can reset the board).
- `ocds sync tl1|none` (`tc3xx_ocds.cfg` sets `tl1`: all cores halt, resume and step together),
  `ocds step all|single`, `ocds step_timers run|frozen`.
- `hsm autostart on|off`: release the HSM from its boot ROM breakpoint after a halting reset (on).
- `<target> tricore irq_mask off|on|step step_over step_out` (`tc3xx_ocds.cfg` masks interrupts
  during all three kinds of step).
- `reset_config connect_assert_srst`: connect with a reset and halt at the reset vector. Needed for
  a device locked with a debug password (`adapter debug_password -env <variable>`, never the words
  in a cfg under version control) and for an application that keeps the debugger out.
- `polling_period <ms>`, `probe_lost_timeout <ms>`.

### Probes

The kit cfgs select `tas_client` (miniWiggler or DAS through a TasServer, started when needed). The
other drivers replace it through the session's `adapter` (`--adapter`), so the same cfg serves
every probe:

| `adapter` | Probe | Note |
|---|---|---|
| `tas_client` | miniWiggler, DAS | `tas-client host <host> [<port>]` for a TasServer on another machine. |
| `bluebox` | iSYSTEM BlueBox over USB, no winIDEA | `bluebox winidea <folder>` names the winIDEA 9.21 install the probe firmware is taken from. |
| `winidea` | BlueBox through a running winIDEA | winIDEA owns run control: TL1, suspend, HSM, MCS and SCR are left to it (`tc3xx_ocds.cfg` skips them). `winidea workspace <path.xjrf>`. |
| `sim` | built-in TC37x model | No hardware: to check that a cfg loads and to try commands. |

A cfg can branch on the driver: `if {[adapter name] eq "winidea"} { ... }`.

### Events and policy

- `<target> configure -event <event> <body>` runs Tcl on `examine-end`, `halted`, `resumed`,
  `reset-start`, `reset-init`, `reset-end` and the other events listed under `<target> invoke-event`.
- `reset_hook <proc>` sends every reset request (Tcl, VS Code, API, `program`) to `<proc> <mode>`; a
  `reset` inside it runs the built-in sequence. `halt_check_hook <proc>` returns why a halt is not
  safe now. `fs26_sbc.cfg` shows both.
- Put the logic in procs: event bodies are not guaranteed to run at global level.
- Document the procs a user calls, so that `help` lists them:

  ```tcl
  add_help_text my_board_status "Show the board's supply and SBC state."
  add_usage_text my_board_status ""
  ```

The server itself sets up OCDS, the TL1 routing and the suspend list again after every reset;
a cfg does not need handlers for that.

## Profiles

A profile saves a session's connection and launch steps on the hub machine, so `open_session` needs
only its name. Profiles are `<name>.toml` files in the directory `list_profiles` reports
(`%APPDATA%\aurix-dbg\profiles` on Windows):

```toml
[connect]
config_files = ["board/infineon/kit_a2g_tc375_lite.cfg", "C:/work/my_board.cfg"]
commands = ["attach_mode halt"]   # -c, after the scripts
search_dirs = []                  # -s; the bundled OpenOCD scripts are always searched
adapter = "tas_client"            # optional: tas_client, bluebox, winidea, sim
probe = ""                        # optional: probe serial; a second session on it is refused

[launch]                          # optional: run on open_session, like a VS Code launch
symbols = ["C:/work/build/app.elf"]
flash = { files = ["C:/work/build/app.hex"] }
reset = { halt = false }
run_to = "main"
```

The `[launch]` fields are those of the `launch` operation (`symbols`, `orti`, `flash`, `reset`,
`breakpoints`, `run_to`, `stop_on_entry`, `timeout_s`, `wait_s`). `ucb_write = true` in `[connect]`
allows writing UCBs; only a person can set it (a profile or the VS Code launch configuration),
never an MCP client.

The same scripts in a VS Code launch configuration: `"configFiles"` and `"configCommands"`.

## Trying a cfg

1. Without hardware: `open_session` with `connect: {config_files: [...], adapter: "sim"}`. The
   model is a plain TC37x (JTAG ID 0x10207083): try the cfg with a TC37x target cfg, e.g. the
   Lite Kit's.
2. On the board: `open_session` with the real adapter. Opening a session connects and may reset
   the target: ask the user first.
3. When `init` fails, `session_log` shows the worker's output: the failing script line, the probe
   and the device ID.
4. With the session open, the `tcl` operation runs console commands: `help`, `targets`,
   `attach_status`, `suspend status`, `tc3xx_status`, `sbc_status`.
5. `close_session`, edit the cfg, open again: scripts are read only when a session starts.
