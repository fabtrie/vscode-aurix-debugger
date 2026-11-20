# Board setup

The debugger is configured with OpenOCD-style Tcl scripts, listed in order in `configFiles`:

```json
"searchDirs": ["${workspaceFolder}/debug"],
"configFiles": ["board/infineon/kit_a2g_tc375_lite.cfg", "my_board.cfg"]
```

1. **OpenOCD board script** for the chip, e.g. `board/infineon/kit_a2g_tc375_lite.cfg`. OpenOCD's scripts are bundled and always searched. The TC375 Lite script also fits other TC37x boards.
2. **Your board script:** what is special about your board; loads `tc3xx_ocds.cfg` (shared by every TC3xx board, don't change it).

## Your board script

Start from an example next to `tc3xx_ocds.cfg`:

- [`tc375_lite_example.cfg`](../aurix-debugger/tc375_lite_example.cfg): board without a safety SBC (TC375 Lite Kit)
- [`tc37x_fs26_example.cfg`](../aurix-debugger/tc37x_fs26_example.cfg): TC37x board with an NXP FS26 safety SBC

Settings (before `tc3xx_ocds.cfg` unless noted):

| Setting | Command | Notes |
|---|---|---|
| DAP clock | `adapter speed 30000` (kHz) | Lower it if the connection is unreliable, e.g. with a long cable |
| Safety SBC | `set tc3xx_sbc fs26` | Loads `fs26_sbc.cfg`: handles the FS26 watchdog and reset line on every reset and halt. Default: no SBC file. |
| HSM as a debug core | `set tc3xx_hsm_target 1` | Halted and resumed with the TriCore cores. Also in `configCommands`. |
| Peripheral suspend (after `tc3xx_ocds.cfg`) | `suspend add cores`, `suspend add EVADC`, `suspend add GPT120 hard`, `suspend add GTM off` | SVD names. For peripherals whose timeouts or safety checks reset the board after a halt. None unless listed. |
| Multi-core (after `tc3xx_ocds.cfg`) | `ocds sync none`, `ocds step single` | Default: all cores halt, resume and step together. `ocds sync none`: each core runs alone. `ocds step single`: only the stepping core steps. |
| Data flash banks | `set tc3xx_dflash0_size <bytes>` | `dfls0` and `dfls1` are defined for erasing (`flash erase_sector dfls0 0 last`). The DFLASH0 size is known for TC37x; set it for other chips. |

- **Other SBC or external watchdog:** write a policy file `<name>_sbc.cfg` with a `<name>_setup` proc, plugged in through `reset_hook`, `halt_check_hook` and target events. Use `fs26_sbc.cfg` as the template.
- **HSM:** nothing to set for an application whose startup waits for the HSM. After a halting reset the debugger releases the HSM if it boots. If the HSM firmware has closed its debug access, it cannot be released and the debugger says so: use a running reset.
- **Suspended peripherals** are written after every reset, as soon as the module allows it (some only once the application has enabled them).

## Attach mode

In the launch configuration, before `init`:

```json
"configCommands": ["attach_mode halt", "init"]
```

Modes: `passive`, `hot` (default), `halt`, `reset_halt`, `reset_run`. Details: `monitor help attach_mode`.

## Probes

| Probe | Launch configuration | Notes |
|---|---|---|
| Infineon miniWiggler / DAP | nothing (default) | Needs Infineon's `tas_server` (Infineon DAS); started when not running |
| iSYSTEM BlueBox (iC5700, iC5000) | `"adapter": "bluebox"` | Needs a winIDEA 9.21.417 installation for the probe's firmware (found in the usual install folders, or set `bluebox winidea <folder>`). winIDEA itself is not used. |

- Several probes connected: `probe` selects one by serial number.
- The hub refuses a second session on the same probe.

## Password-protected devices

For a TC3xx whose debug interface is locked with a password (`UCB_DBG`), add to the board script:

```tcl
adapter debug_password -env AURIX_DEBUG_PW
reset_config connect_assert_srst
```

- The environment variable holds the eight password words, hex, PW0 first. Keeps the password out of version control; never logged.
- `connect_assert_srst` is required: unlocking needs a reset during the connect.

## More

- Tcl commands for board scripts and the Debug Console: [Tcl command reference](../aurix-debugger/server/docs/commands.md)
- `monitor help <word>` in the Debug Console searches the commands, e.g. `monitor help reset`.
