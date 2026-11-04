# bluebox

Talks to an iSYSTEM BlueBox (iC5700, USB `06F9:5700`) directly over WinUSB, without winIDEA or `isystemConnect.dll`. It backs the server's `bluebox` adapter driver (`crates/core/src/bluebox_adapter.rs`).

The protocol was read off USB captures of winIDEA 9.21.356 driving a TC377. The module docs (`frame`, `message`, `memory`, `lowlevel`, `kernel`) give the layouts. The tests check them against captured bytes; `tests/data/attach_*.txt` hold winIDEA's link bring-up, without kernel data.

## What works (tested on hardware, TC377TP, iC5700 A)

- Opening the probe through WinUSB, with winIDEA closed.
- Memory read and write through the memory service `//2///16/`, with a per-byte failure bitmap.
  - Reads from 1 byte to 64 KiB, split into 8 KiB requests.
  - About 1.2 ms per small access; 0.3 MB/s on long reads.
  - A refused write (read-only register) is reported failed. A write of RAM reads back.
- Link bring-up (`lowlevel`), byte-identical to winIDEA's apart from its stale padding bytes:
  - hello and handshake, which tell whether the probe's kernel runs, and read the board ids;
  - after a power cycle, loading the kernel from `iC6kKrnl.s` in the winIDEA install (2.45 MB in about 260 ms), starting it, and setting the session.
- Playing back a captured winIDEA attach for the target connection (`replay`), after the bring-up.
- The server: `adapter driver bluebox`, `init`, `read_memory`.
- Run control through OCDS register accesses, with the board cfg plus `tc37x_ocds_init.cfg` (MCS and HSM targets off):
  - `init` examines all three cores.
  - `halt` takes 17 ms and stops cpu0 and cpu1 together over trigger line 1.
  - `step` advances the PC, about 35 ms each.
  - `resume` works.
  - A hardware breakpoint (`bp ... hw`) is hit; `rbp` removes it.
  - See the known issue under "What doesn't work yet".

## What doesn't work yet

- **Target connection.** Not decoded: after the bring-up, `bluebox attach_replay <trace.jsonl>` plays back an attach captured with the kernel running (`followup.jsonl`). A trace captured right after a power cycle loads the kernel itself and is refused. Without a replay, the probe must still be attached from an earlier session; a handshake would end that session, so the driver then skips it.
- **Version-bound values.** The kernel, the node-1 service numbers, the host id in `G` and the session value all belong to one winIDEA version (9.21.356 here). The kernel file and the trace must come from the same version.
- **The kernel does not survive our sessions.** After each run of this crate, the next handshake found no kernel, while winIDEA's close leaves it running. The driver reloads it (260 ms), so this only costs time; a proper close is not decoded.
- **The replayed attach resets the target.** Right after it, the chip's uptime (STM0) was 0.3 s; the same happened with winIDEA's own kernel. Reads in the first second can fail while the chip boots. Not investigated.
- **FPGA image.** The debug FPGA keeps its image across power cycles; winIDEA replaces it only when the hash differs (another winIDEA version). This crate does not check or load it.
- **Reset.** `reset` fails on this driver.
- **JTAG id.** Not read; `DeviceInfo.device_id` is 0.
- **The replay restores winIDEA's breakpoints and trigger setup.**
  - It armed a leftover winIDEA breakpoint at 0x80120ae0. `Replay::disarm_breakpoints` (used by the driver and the prototype) now sends it as cleared.
  - It also enables capture-and-hold on trigger line 1, so any halt holds all cores until `CBS_TLCHS` is written. Load `tc3xx_ocds.cfg`, whose resume and step hooks do that. Without it, the cores stay held.
- **Resets after resume (open).** Twice, the target dropped off the debug port right after `resume` released trigger line 1, and the chip restarted. Suspects:
  - the application's supervision: STM0/1 keep running during a halt, because the application's `ACCEN` refuses the debugger's `STMx_OCS` writes;
  - the SBC watchdog the cfg warns about.

  Not yet compared with the same sequence over TAS. After such a reset the driver does not reconnect by itself: restart with `bluebox attach_replay`.

## Prototype

```bash
# winIDEA must be closed; MinGW on PATH for the GNU toolchain (see rust-toolchain.toml)
cargo run -p bluebox --example probe -- --replay <warm trace.jsonl> [--rewrite 0x70000018]
    [--kernel C:/winIDEA/iC6kKrnl.s] [--restart-kernel] [--watch 4] [--map ADDR LEN]
cargo run -p bluebox --example probe -- --no-bringup   # probe already attached
```

## Server

```tcl
adapter driver bluebox
adapter serial 01ABC                                   ;# only with several probes
bluebox kernel C:/winIDEA/iC6kKrnl.s                   ;# optional: the default is C:\winIDEA or %WINIDEA_DIR%
bluebox attach_replay C:/traces/followup.jsonl         ;# step defaults to "connect: begin_attach"
```

The traces come from the Frida capture tools (`capture.py` + `scenarios.py`). They aren't in this repository.
