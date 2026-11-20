# bluebox

Talks to an iSYSTEM BlueBox directly over WinUSB, without winIDEA or `isystemConnect.dll`:

| Probe | USB id |
|---|---|
| iC5700 | `06F9:5700` |
| iC5000 | `06F9:3200` |

It backs the server's `bluebox` adapter driver (`crates/core/src/bluebox_adapter.rs`). The driver moves memory only; halt, step and breakpoints go through the TriCore OCDS registers, as with a miniWiggler.

Layouts are in the module docs (`frame`, `message`, `memory`, `lowlevel`, `kernel`, `attach`, `fpga`, `reset`, `daq`, `health`). The tests check them against captured bytes; `tests/data/attach_*.txt` hold the link bring-up, without kernel data.

## Requirements

- **winIDEA 9.21.417 installed.** The kernel, service numbers, session value and attach are that version's (`kernel::WINIDEA_VERSION`); service numbers change between versions and the probe does not report them. The install provides:
  - the kernel: `iC6kKrnl.s` (iC5700) or `iC3kKrnl.s` (iC5000);
  - the iC5000's debug FPGA image (in `ICWHW32.HRS`). Nothing of it is in this repository.
- **iC5700 FPGA image of 9.21.417.** The iC5700 keeps its debug FPGA image in flash; another winIDEA version leaves its own behind. If the driver says so, connect once with winIDEA 9.21.417, which loads its image. This crate does not load iC5700 images.
- **winIDEA closed** while the server uses the probe.
- **TC37x.** The built-in attach fits a TC37x with the captured debug setup. Another derivative or setup needs a captured attach (`bluebox attach_replay`).

Kernel search (`kernel::find_kernel`), first match:
1. `bluebox kernel <file>`;
2. `bluebox winidea <folder>`;
3. `%WINIDEA_DIR%`, `C:\winIDEA`, `C:\winIDEA*`, `C:\iSYSTEM\*`, Chocolatey's `lib\winidea.*`. Chocolatey's folder: `%ChocolateyInstall%`, else the parent of the `bin` holding a `choco.exe` on `PATH`, else `C:\ProgramData\chocolatey`.

The version is taken from the install's `deploy.json`. Without a 9.21.417 install the error lists the versions found.

## Server use

```tcl
adapter driver bluebox
adapter serial 01ABC                        ;# only with several probes
bluebox winidea C:/tools/winIDEA_9_21_417   ;# optional: a winIDEA 9.21.417 install (kernel, iC5000 FPGA image)
bluebox kernel C:/winIDEA/iC6kKrnl.s        ;# optional: the kernel file itself (iC3kKrnl.s for an iC5000), unchecked
bluebox attach_replay C:/traces/x.jsonl     ;# optional: another target or setup; step defaults to "connect: begin_attach"
bluebox health_period 100                   ;# optional: ms between health queries, 0 = off
probe_lost_timeout 1000                     ;# optional: end the sessions after the probe is gone this long, 0 = never
```

All `bluebox` commands (`status`, `health`, `debug_mode`, `daq`, `daq_offload`, `drop_target`, `raw`, `scr_mem`, …): [commands.md](../../docs/commands.md#bluebox-adapter-adapter-driver-bluebox).

- **Defaults** unless the cfg sets them: run-state poll every 15 ms (`polling_period`), DAQ coalescing gap 512 B (`daq gap`).
- **Connect** (`init`) does not reset the target and runs the full bring-up and attach every time (the probe keeps no session across connects).
- **Target not answering at connect** (unpowered, held in reset, reset by a watchdog): `init` still succeeds; the cores stay unexamined and the driver retries once a second. `reset`, or `reset_config connect_assert_srst`, tries the probe's reset at once.
- **Reset:** `reset run|halt|init` always halt cpu0 at its start address. `reset run` is a halting reset followed by a resume of the halted cores.
- **Trigger line 1:** the attach turns on capture-and-hold on TL1, so a halt holds all cores until it is cleared. Use `ocds sync tl1` (set by `tc3xx_ocds.cfg`), which clears it at resume and step; without it the cores stay held.
- **Peripherals keep running while the cores are halted** unless suspended (`suspend add <module>`). An application with timing or ADC safety checks may reset the chip after a resume otherwise.

### DAP clock (`adapter speed`)

| Probe | Clock | Range | Service |
|---|---|---|---|
| iC5700, active probe | 100 MHz / integer, rounded down | 1–25 MHz | `//7//1// <Hz>` |
| iC5700, no active probe | 100 MHz / integer, rounded down | 1–25 MHz | `//4///7/ 01 00 <kHz>` |
| iC5000 | 30 MHz / (n + 1), rounded down | 2–30 MHz | `//5///2/ 8e 00 00 b0 <n> ff` |

- `adapter speed 30000` runs at 25 MHz on an iC5700.
- Attach, reconnects and resets run at 10 MHz (an attach at 25 MHz can fail to bring a target into debug mode). The configured clock applies once the target answers; the driver falls back to 10 MHz if the target does not answer there.
- A change after `init` applies at once; resets keep it.
- iC5700 above 25 MHz (33.3 MHz): wrong data, marked good, until the next connect; hence the 25 MHz cap.
- iC5000 below 2 MHz: flash reads return failed bytes.
- Above ~12 MHz the USB round trips dominate; the iC5000 spends ~0.5 ms per access whatever the clock.

### Recovery

| Lost | Detected by | Action |
|---|---|---|
| Target (reset, power loss) | a failed batch, then a failed `SCU_CHIPID` read | resend the attach's connect part (from its last `L`, ~290 exchanges, 0.4 s), at most once a second; reported as a reset (`take_resets`) |
| Probe (USB or protocol error, BlueBox power cycle) | the USB transfer | reopen and run the whole connect (kernel load and attach, ~4.6 s), at most every 3 s; no reset reported |

- The health query (every `bluebox health_period` ms, ~1.15 ms, no target access) names the cause: no target supply, a reset, no debug access. While the supply is missing, reconnects are held back (one every 5 s); its return starts one at once.
- A probe lost through USB (unplugged, powered off) ends the frontends' sessions after `probe_lost_timeout`.
- `bluebox drop_target` resets the target and leaves the connection broken, to test recovery.

### Known limits

- After `reset run` the driver briefly loses the target and reconnects (0.6–0.8 s).
- A reset that keeps the debug connection is noticed only when the connection breaks (reads keep working for ~0.2 s after the restart).
- With an iC5700 active probe, program flash reads (any size) returned failed bytes and the chip restarted; RAM and registers were not affected.
- No reset without halt (winIDEA's `resetAndRun()` also resets with halt).
- JTAG id not read: `DeviceInfo.device_id` is 0.
- The kernel does not survive a session: the next connect usually reloads it (~260 ms). A proper close is not decoded.

## Protocol reference

### Connect sequence

1. **Link bring-up** (`lowlevel`): hello and handshake. Without a running kernel the driver loads it (~300 ms), starts it and sets the session (`//1///29/`, 22-byte value). Byte-identical to winIDEA's apart from its stale padding.
2. **Kernel version** (`Probe::kernel_state`): a running kernel is asked with `//1///26/` and `//1///28/` (session read).
   - 9.21.417's kernel answers `02 …` and returns the session value.
   - Another version's kernel echoes both. It is stopped (`L`, `//1///3/ 00`, `////// 01`) and replaced (`tests/data/attach_ic5000_foreign.txt`). Send nothing else to it.
   - The `I` state alone cannot tell the versions apart (`2f` for both).
3. **Debug FPGA** (`fpga`), iC5700: after the attach's first three messages, `//1///37/` returns the image's size and MD5. 9.21.417 needs `0x316fd8` bytes, MD5 `ed36b5cf…`. Another image stops the driver with an error.
4. **Target connection** (`attach`): winIDEA 9.21.417's hot attach, sent as exchanges with this session's toggles:

   | Setup | Script | Exchanges | Time |
   |---|---|---|---|
   | iC5000 | `src/attach_tc37x_ic5000.txt` | 261 | ~2.3 s |
   | iC5700, no active probe | `src/attach_tc37x_system.txt` | 3532 | ~2.3 s |
   | iC5700, active probe | `src/attach_tc37x.txt` | 3705 | ~2.4 s |

   - The two iC5700 scripts are identical up to and including the node-5 scan (`//5///22/ 00 … c8`). Its node count (2 with an active probe, 1 without) picks the script to continue with (`Setup::script`).
   - A replayed attach captured in the other setup (`Script::captured_setup`) stops after the scan with an error, unless its first part is the built-in one.
   - On an iC5700 whose active probe is powered by the target, an unpowered board's scan finds no probe; the session runs on the attach without one.
   - **Do not replay a trace byte for byte** (`Replay::play`): that resets the target on every connect. Send exchanges with live toggles (`Probe::attach`).

**Probe `I` state:**

| Value | Meaning | Driver |
|---|---|---|
| `00` | no kernel | load it |
| `2f` | kernel ready | check its version |
| `01` | handshake ran, no kernel followed | load it at once. Do not send the kernel restart (`////// 01`) here: the probe stops answering until a power cycle. |
| `55` | booting, right after power-on | wait |
| `40`, `82`, other | unknown | wait up to 20 s, then restart the kernel; a probe that does not take the restart needs a power cycle |

A probe stuck in an unknown state may need a full power-off (unplug power and USB; a soft power cycle keeps the stuck kernel).

### Version-bound values

Same in both versions: frames, the hello, memory request and reply data, node 5, the reset `//7///7/`, the kernel transport, the iC5000's FPGA image. Different:

| | 9.21.417 (used) | 9.21.356 |
|---|---|---|
| Host id in `G` | `a6 a1` | `a6 64` |
| Session set / kernel check / session read | `//1///29/`, `//1///26/`, `//1///28/` | `//1///28/`, `//1///25/`, `//1///27/` |
| Memory service | `//2///14/` | `//2///16/` |
| Tail after each memory result | 28 bytes | 24 bytes |
| Reset reconnect | `//2///34/` | `//2///38/` |
| Breakpoint service | `//3///7/` | `//3///10/` |
| iC5700 FPGA status | `//1///37/` | `//1///35/` |
| iC5000 FPGA upload after | `//1///43/` | `//1///41/` |
| SoC / core tables | `//2///26/` (7378 B), `//3/…/6/` (5699 B) | `//2///30/` (7337 B), `//3/…/9/` (5663 B) |

Node-1 to node-3 services and some body sizes (`//1///13/`: 9 bytes, was 8) differ, so every captured script belongs to one version.

### iC5000 differences

Same frames, messages and services; the memory service is byte for byte the same. The model comes from the USB id (`Model`).

- **Padding:** frame payloads padded to 2 bytes (`Model::frame_pad`). Padded to 4, the first `E` command hangs the probe until a power cycle.
- **Short answers:** a low-level answer of N bytes is read with capacity N rounded up to even (`08` included). `L`, `W` and `K` answer `33` alone, the session message `01`.
- **Kernel:** `iC3kKrnl.s` (~290 ms to load), its own session value. `I` state without a kernel has the two top bits set (`ef`, `eb`); with one, `2f`.
- **Debug FPGA:** uploaded on every attach, after `//1///43/`: `//1///7/ 02`, five `//1///10/` chunks (`<length u32 LE> <data>`), `//1///9/ 02`. The image (266,599 bytes) is stored as-is in winIDEA's `ICWHW32.HRS`, entry `01 83 12 12 09 ff <length>`; `fpga::ic5000::find_image` picks it by length and hash.
- **Attach:** no node scan (setup "without active probe"). Node 5 is set up with `//5///1/`, `//5///2/`, `//5///30/`, `//5///33/`.
- **Pauses:** scripts keep winIDEA's pauses of 3 ms or more (`w<ms>`, `Exchange::pause_ms`). The 10 ms after `//5///1/` is needed.
- **Unreachable target:** the built-in attach does not reset it (winIDEA's does, up to 8 times); `reset halt` brings it back.
- **Reset reconnect** waits 110 ms after the reset; sent at once, the connection drops for good.

### Reset

`reset` (`src/reset.rs`), three messages:
1. `//2///7/ 01`: prepares the reset.
2. `//7///7/` with eight zero bytes: the reset.
3. `//2///34/ 01 00 00 00`: reconnects; the reply carries cpu0's `DBGSR` and PC.

The chip resets and cpu0 halts at its start address (`Adapter::reset_always_halts`).

### Batched access

Memory service `//2///14/` (layout in `memory.rs`):
- **Request:** a u32 count, then per access the single request's access block (166 + N bytes), then 12 zero bytes.
- **Reply:** count at 0x1f, results packed from 0x23, each `u32 0, B, bitmap[B], N, data[N]` followed by 28 bytes. The bitmap has one bit per byte (set = failed).
- The probe runs every access of a message, also after one fails; only that one is flagged. A write's bus error shows on the next access.
- Each access costs ~0.45 ms inside the probe at any clock.
- A reply over 61,440 bytes comes back empty.
- **Access flags:** the driver sends `01 01 01` (real-time direct access) for everything; it reports refused writes. winIDEA's own `00 01 00` / `00 01 01` hide a refused write (acknowledged, register unchanged).

`Probe::access` plans messages with `memory::plan`:
- reads share a message: at most 64 accesses and a 60,000-byte reply; ops over 8 KiB are split;
- a write gets its own message, followed by a check read of `STM0_TIM0`, so it runs only after everything before it succeeded and nothing follows before its outcome is known;
- results stop at the first failed op, reported like a TAS packet (the rest consequential).

### DAQ (the probe's sampler)

`src/daq.rs`: the kernel samples a list of bus addresses in a loop and buffers changes; the host drains the buffer.

| Service | Use |
|---|---|
| `//1///19/` | info |
| `//1///20/` | enable / disable / status |
| `//1///21/` | configure: 1, 2, 4 or 8-byte items at a tier of max, 1 ms, 10 ms, 100 ms or 1 s |
| `//1///22/` | read: 19-byte samples, µs timestamps of the probe's clock |

- A sample is sent only when the value changed (and once per item after a configure).
- An unreadable item sends nothing and slows every loop pass (8 µs to ~600 µs).
- 256 items, a buffer of 4096 samples. One 4-byte item at max: ~3,700 samples/s.
- The driver offers it to the core as `Adapter::probe_daq`; the DAQ registry puts plain bus points of up to 64 bytes on it, including the Live Watch's live sets. A live set's tier follows the view's interval (200 ms → 100 ms tier). An unmapped address falls back to a host read with its error.
- `bluebox daq` shows it; `bluebox daq_offload off` withdraws it. A probe that refuses the DAQ services withdraws it by itself.

## Prototype

```bash
# winIDEA must be closed; MinGW on PATH for the GNU toolchain (see rust-toolchain.toml)
cargo run -p bluebox --example probe -- --attach [--watch 2]           # bring-up + built-in attach
    [--then <trace> <step> [--then-only a-b,c] [--then-skip a-b]]...  # then captured steps, e.g. "reset: reset()"
cargo run -p bluebox --example probe -- --script <trace.jsonl> [--until-a] [--skip a-b,c] [--drop path:b0:len]
    [--list | --export <file>] [--watch 2]                              # try or reduce a captured attach
cargo run -p bluebox --example probe -- --replay <trace.jsonl>          # byte-for-byte playback (resets the target)
    [--kernel C:/winIDEA/iC6kKrnl.s] [--restart-kernel] [--rewrite 0x70000018] [--map ADDR LEN]
cargo run -p bluebox --example probe -- --fpga-status                  # iC5700 FPGA image size and MD5
cargo run -p bluebox --example probe -- --attach --daq 0xf0001010:4:10ms,0xf0036140:4:max [--daq-secs 2]
```

- `--watch`: reads chip id, STM0 uptime and cpu0 `DBGSR` every 200 ms; an uptime near zero means the attach reset the target.
- After the attach: `--reset-halt` resets; `--chipid-loop <n>` reads `SCU_CHIPID` every 20 ms and reports changes; `--speed-test` times reads.
- `--script <trace> --export <file>` reduces a captured attach to a script for `bluebox attach_replay`.
- `BLUEBOX_TRACE=1` logs every USB transfer to stderr (first 24 bytes; `BLUEBOX_TRACE=2`: all).

Captured traces come from the Frida capture tools (`capture.py` + `scenarios.py`); they are not in this repository.
