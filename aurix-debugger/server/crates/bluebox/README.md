# bluebox

Talks to an iSYSTEM BlueBox (iC5700, USB `06F9:5700`) directly over WinUSB, without winIDEA or `isystemConnect.dll`. It backs the server's `bluebox` adapter driver (`crates/core/src/bluebox_adapter.rs`).

The protocol was read off USB captures of winIDEA 9.21.356 driving a TC377. The module docs (`frame`, `message`, `memory`, `lowlevel`, `kernel`, `attach`) give the layouts. The tests check them against captured bytes; `tests/data/attach_*.txt` hold winIDEA's link bring-up, without kernel data.

## Connecting

1. **Link bring-up** (`lowlevel`): hello and handshake. After a power cycle no kernel runs; the driver loads `iC6kKrnl.s` from the winIDEA install (2.45 MB, about 260 ms), starts it and sets the session. This is byte-identical to winIDEA's, apart from its stale padding bytes.
2. **Target connection** (`attach`): `src/attach_tc37x.txt`, the messages winIDEA 9.21.356 sent after the kernel when attaching to a TC377 right after a power cycle. 3582 exchanges, sent with this session's toggles, take about 3.2 s.

Neither step resets the target. Every `init` runs both: the probe keeps no session across connects.

## What works (tested on hardware, TC377TP, iC5700 A)

- Connecting with nothing but the probe and `iC6kKrnl.s`: after a power cycle of the BlueBox and with the kernel running, both through `aurix-dbg`. The target kept running (STM0 uptime kept counting).
- Memory read and write through the memory service `//2///16/`, with a per-byte failure bitmap.
  - Reads from 1 byte to 64 KiB, split into 8 KiB requests.
  - About 1.2 ms per small access; 0.3 MB/s on long reads.
  - A refused write (read-only register) is reported failed. A write of RAM reads back.
- Run control through OCDS register accesses, with the board cfg plus `tc37x_ocds_init.cfg` (MCS and HSM targets off). Tested with the earlier replay-based attach:
  - `init` examines all three cores.
  - `halt` takes 17 ms and stops cpu0 and cpu1 together over trigger line 1.
  - `step` advances the PC, about 35 ms each.
  - `resume` works.
  - A hardware breakpoint (`bp ... hw`) is hit; `rbp` removes it.

## What doesn't work yet

- **The attach is a captured script, not decoded.**
  - Its tables describe the SoC and the debug setup of the workspace it was captured with: `//2///30/` (7337 bytes), `//3/<core>/9/` (5663 bytes each), `/////6/` (4107 bytes). So it fits a TC37x with those settings. Another derivative or setup needs a capture, used with `bluebox attach_replay <trace.jsonl>`, or reduced to a script with `examples/probe.rs --script <trace> --export`.
  - Shorter scripts connected on a probe that earlier sessions had set up: down to 53 messages. After a power cycle they failed and left the probe hung until the next power cycle. The node-5 setup and winIDEA's follow-up after the connect turned out to be needed then.
- **Do not replay a trace byte for byte** (`Replay::play`): that reset the target chip on every connect. Sent as exchanges with live toggles (`Probe::attach`), the same messages do not.
- **Version-bound values.** The kernel, the node-1 service numbers, the host id in `G`, the session value and the attach all belong to winIDEA 9.21.356.
- **The kernel does not survive our sessions.** After a run of this crate the next handshake usually finds no kernel; winIDEA's close leaves it running. The driver reloads it (260 ms). A proper close is not decoded.
- **The `I` answer** showed a third value once (`01`, besides `00` = no kernel and `2f` = kernel). It is treated as "kernel running"; that worked.
- **FPGA image.** The debug FPGA keeps its image across power cycles; winIDEA replaces it only when the hash differs (another winIDEA version). This crate does not check or load it.
- **Reset.** `reset` fails on this driver.
- **JTAG id.** Not read; `DeviceInfo.device_id` is 0.
- **Trigger setup.** The attach enables capture-and-hold on trigger line 1, so any halt holds all cores until `CBS_TLCHS` is written. Load `tc3xx_ocds.cfg`, whose resume and step hooks do that. Without it, the cores stay held.
- **Resets after resume (open).** Twice, the target dropped off the debug port right after `resume` released trigger line 1, and the chip restarted. The suspect is the board's SBC watchdog. After such a reset the driver does not reconnect by itself: restart the server.

## Prototype

```bash
# winIDEA must be closed; MinGW on PATH for the GNU toolchain (see rust-toolchain.toml)
cargo run -p bluebox --example probe -- --attach [--watch 2]           # bring-up + built-in attach
cargo run -p bluebox --example probe -- --script <trace.jsonl> [--until-a] [--skip a-b,c] [--drop path:b0:len]
    [--list | --export <file>] [--watch 2]                              # try or reduce a captured attach
cargo run -p bluebox --example probe -- --replay <trace.jsonl>          # byte-for-byte playback (resets the target)
    [--kernel C:/winIDEA/iC6kKrnl.s] [--restart-kernel] [--rewrite 0x70000018] [--map ADDR LEN]
```

`--watch` reads the chip id, STM0 uptime and cpu0 `DBGSR` every 200 ms; an uptime near zero means the attach reset the target.

## Server

```tcl
adapter driver bluebox
adapter serial 01ABC                        ;# only with several probes
bluebox kernel C:/winIDEA/iC6kKrnl.s        ;# optional: the default is C:\winIDEA or %WINIDEA_DIR%
bluebox attach_replay C:/traces/x.jsonl     ;# optional: another target or setup; step defaults to "connect: begin_attach"
```

The traces come from the Frida capture tools (`capture.py` + `scenarios.py`). They aren't in this repository.
