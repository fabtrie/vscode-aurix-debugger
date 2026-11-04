# bluebox

Talks to an iSYSTEM BlueBox (iC5700, USB `06F9:5700`) directly over WinUSB, without winIDEA or `isystemConnect.dll`. It backs the server's `bluebox` adapter driver (`crates/core/src/bluebox_adapter.rs`).

The protocol was read off USB captures of winIDEA 9.21.356 driving a TC377. The module docs (`frame`, `message`, `memory`) give the layouts, and the tests check them against captured bytes.

## What works (tested on hardware, TC377TP, iC5700 A)

- Opening the probe through WinUSB, with winIDEA closed.
- Memory read and write through the memory service `//2///16/`, with a per-byte failure bitmap.
  - Reads from 1 byte to 64 KiB, split into 8 KiB requests.
  - About 1.2 ms per small access; 0.3 MB/s on long reads.
  - A refused write (read-only register) is reported failed. A write of RAM reads back.
- Playing back a captured winIDEA attach to connect the probe to the target (`replay`).
- The server: `adapter driver bluebox`, `init`, `read_memory`.
- Run control through OCDS register accesses, with the board cfg plus `tc37x_ocds_init.cfg` (MCS and HSM targets off):
  - `init` examines all three cores.
  - `halt` takes 17 ms and stops cpu0 and cpu1 together over trigger line 1.
  - `step` advances the PC, about 35 ms each.
  - `resume` works.
  - A hardware breakpoint (`bp ... hw`) is hit; `rbp` removes it.
  - See the known issue under "What doesn't work yet".

## What doesn't work yet

- **Bring-up.** The probe must already be attached: by winIDEA, by an earlier run of this driver (the attach outlives the process), or by `bluebox attach_replay <trace.jsonl>` in the cfg.
  - The replay assumes the probe still holds its firmware.
  - In one of six runs, the first after winIDEA closed, the replay ended with target reads failing.
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
cargo run -p bluebox --example probe -- --replay <trace.jsonl> [--rewrite 0x70000018]
cargo run -p bluebox --example probe        # probe already attached
```

## Server

```tcl
adapter driver bluebox
adapter serial 01ABC                                   ;# only with several probes
bluebox attach_replay C:/traces/followup.jsonl         ;# optional; step defaults to "connect: begin_attach"
```

The traces come from the Frida capture tools (`capture.py` + `scenarios.py`). They aren't in this repository.
