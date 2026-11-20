# Board setup

The debugger is configured with OpenOCD-style Tcl scripts. A launch configuration names them in `configFiles`, in order:

```json
"searchDirs": ["${workspaceFolder}/debug"],
"configFiles": ["board/infineon/kit_a2g_tc375_lite.cfg", "my_board.cfg"]
```

1. **An OpenOCD board script** for the chip, e.g. `board/infineon/kit_a2g_tc375_lite.cfg`. OpenOCD's scripts are bundled and always searched. The TC375 Lite script also fits other TC37x boards.
2. **Your board script**, which sets what is special about your board and loads `tc3xx_ocds.cfg` (the part every TC3xx board shares, which you don't change).

## Your board script

Start from an example next to `tc3xx_ocds.cfg`:

- [`tc375_lite_example.cfg`](../aurix-debugger/tc375_lite_example.cfg): a board without a safety SBC (TC375 Lite Kit).
- [`tc37x_fs26_example.cfg`](../aurix-debugger/tc37x_fs26_example.cfg): a TC37x board with an NXP FS26 safety SBC.

What a board script sets (before `tc3xx_ocds.cfg` unless said otherwise):

- **DAP clock:** `adapter speed 30000` (kHz). Lower it if the connection is unreliable, e.g. with a long cable.
- **Safety SBC:** `set tc3xx_sbc fs26` loads `fs26_sbc.cfg`, which handles the FS26 watchdog and its reset line on every reset and halt. Without it no SBC file is loaded. A board with another SBC or an external watchdog may need its own policy file (`<name>_sbc.cfg` with a `<name>_setup` proc, plugged in through `reset_hook`, `halt_check_hook` and target events); `fs26_sbc.cfg` shows how.
- **HSM:** nothing to set for an application whose startup waits for the HSM. After a halting reset the boot ROM holds the HSM at its first instruction; the debugger reads whether the HSM boots (PROCONHSMCFG) and releases it. If the HSM firmware has closed its debug access it cannot be released, and the debugger says so: use a running reset then. `set tc3xx_hsm_target 1` adds the HSM as a core to debug (halted and resumed with the TriCore cores), in the board cfg or a launch configuration's `configCommands`.
- **Peripherals to suspend while halted** (after `tc3xx_ocds.cfg`): `suspend add cores` for the STMs, `suspend add EVADC`, `suspend add GPT120 hard`, `suspend add GTM off`, by their SVD names. For peripherals whose timeouts or safety checks reset the board after a halt. The debugger writes each one as soon as the module allows it (after every reset, and once the application has enabled it); none unless listed.
- **Multi-core:** all cores halt, resume and step together by default; `ocds sync none` (after `tc3xx_ocds.cfg`) lets each run alone, `ocds step single` steps only the stepping core.
- **Data flash banks:** `dfls0` and `dfls1` are defined for erasing (`flash erase_sector dfls0 0 last`). The DFLASH0 size is known for the TC37x; for another chip set `tc3xx_dflash0_size`.

The attach mode goes in the launch configuration, before `init`: `"configCommands": ["attach_mode halt", "init"]` (`passive`, `hot`, `halt`, `reset_halt`, `reset_run`; `monitor help attach_mode`).

## Probes

| Probe | Launch configuration | Notes |
|---|---|---|
| Infineon miniWiggler / DAP | nothing (default) | Needs Infineon's `tas_server` (Infineon DAS), which is started when it isn't running |
| iSYSTEM BlueBox (iC5700, iC5000) | `"adapter": "bluebox"` | Talks to the probe over USB without winIDEA, but takes the probe's firmware from a winIDEA 9.21.417 installation (found in the usual install folders, or set `bluebox winidea <folder>`) |

With several probes connected, `probe` selects one by its serial number; the hub refuses a second session on the same probe.

## Password-protected devices

A TC3xx device whose debug interface is locked with a password (`UCB_DBG`) is unlocked during the connect:

```tcl
adapter debug_password -env AURIX_DEBUG_PW
reset_config connect_assert_srst
```

The environment variable holds the eight password words (hex, PW0 first), so the password does not end up in a file under version control. It is never logged. Unlocking needs a reset during the connect, which `connect_assert_srst` does.

## More

- The Tcl commands for board scripts and the Debug Console: [Tcl command reference](../aurix-debugger/server/docs/commands.md)
- In the Debug Console, `monitor help <word>` searches the commands, e.g. `monitor help reset`.
