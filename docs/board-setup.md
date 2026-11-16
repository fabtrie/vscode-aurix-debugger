# Board setup

The debugger is configured with OpenOCD-style Tcl scripts. A launch configuration names them in `configFiles`, in order:

```json
"searchDirs": ["${workspaceFolder}/debug"],
"configFiles": ["board/infineon/kit_a2g_tc375_lite.cfg", "my_board.cfg"]
```

1. **An OpenOCD board script** for the chip, e.g. `board/infineon/kit_a2g_tc375_lite.cfg`. OpenOCD's scripts are bundled and always searched. The TC375 Lite script also fits other TC37x boards.
2. **Your board script**, which loads `tc3xx_ocds.cfg` (the TC3xx chip support, which you don't change) and sets what is special about your board.

## Your board script

[`tc37x_ocds_init.cfg`](../aurix-debugger/tc37x_ocds_init.cfg) is an example for a TC37x board with an NXP FS26 safety SBC. Copy it next to `tc3xx_ocds.cfg` and adapt it:

- **DAP clock:** `adapter speed 30000` (kHz). Lower it if the connection is unreliable, e.g. with a long cable.
- **Data flash banks:** the `flash bank dfls0 ...` lines make the DFLASH erasable (`flash erase_sector dfls0 0 last`). Adjust the sizes to your chip.
- **Peripherals to suspend while halted** (`tc3xx_periph`): peripherals whose timeouts or safety checks reset the board after a halt. Empty by default; the comments show the format.
- **Safety SBC:** the example loads `fs26_sbc.cfg` for the FS26 on its board. On a board without an FS26, leave out the two lines that load it. A board with another SBC or an external watchdog may need its own handling; the example and the comments in `fs26_sbc.cfg` show how it is done for the FS26.

Instead of copying, you can also keep the example and write a small script that sets your values and then loads it, as described at the top of the example.

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
