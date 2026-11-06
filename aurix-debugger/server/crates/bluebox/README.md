# bluebox

Talks to an iSYSTEM BlueBox (iC5700, USB `06F9:5700`, or iC5000, `06F9:3200`) directly over WinUSB, without winIDEA or `isystemConnect.dll`. It backs the server's `bluebox` adapter driver (`crates/core/src/bluebox_adapter.rs`).

The protocol was read off USB captures of winIDEA (9.21.356 first, now 9.21.417) driving a TC377. The module docs (`frame`, `message`, `memory`, `lowlevel`, `kernel`, `attach`) give the layouts. The tests check them against captured bytes; `tests/data/attach_*.txt` hold winIDEA's link bring-up, without kernel data.

## Connecting

Everything version-specific (kernel, service numbers, session value, attach) is winIDEA 9.21.417's (`kernel::WINIDEA_VERSION`). Service numbers differ from version to version and the probe does not tell them, so the crate works with that version only.

1. **Link bring-up** (`lowlevel`): hello and handshake. After a power cycle no kernel runs; the driver loads the kernel (`iC6kKrnl.s`, 2.59 MB, or `iC3kKrnl.s` on an iC5000, about 300 ms), starts it and sets the session. This is byte-identical to winIDEA's, apart from its stale padding bytes (`tests/data/attach_*.txt`).
   - The kernel comes only from a winIDEA 9.21.417 install. That's the version its `deploy.json` gives, in a folder from `bluebox winidea <folder>`, or found in `%WINIDEA_DIR%`, `C:\winIDEA`, `C:\winIDEA*`, `C:\iSYSTEM\*` or Chocolatey's `lib\winidea.*` (`kernel::find_kernel`).
   - Without one, the error lists the versions found and asks to install 9.21.417 or point to its folder.
2. **Kernel version** (`Probe::kernel_state`): a running kernel is asked with `//1///26/` and `//1///28/` whether it is 9.21.417's, with its session.
   - The `I` state alone can't tell: 9.21.417 saw `2f` with 9.21.356's kernel, as with its own.
   - Another version's kernel answers both with an echo. It is stopped (`L`, `//1///3/ 00`, `////// 01`) and replaced, as winIDEA does (`tests/data/attach_ic5000_foreign.txt`). Its service numbers differ, so nothing else may go to it.
3. **Debug FPGA** (`fpga`): on an iC5700, after the attach's first three messages, the status query `//1///37/` gives the image's size and MD5. 9.21.417 needs `0x316fd8` bytes with MD5 `ed36b5cf…`.
   - The image is kept in the probe's flash, so another winIDEA version leaves its own behind.
   - The driver then stops with an error that asks to connect once with winIDEA 9.21.417, which loads its image. The crate does not load iC5700 FPGA images. (The iC5000's is uploaded on every attach; see below.)
4. **Target connection** (`attach`): a script of winIDEA's attach, sent with this session's toggles.
   - **iC5000:** `src/attach_tc37x_ic5000.txt`, winIDEA 9.21.417's hot attach (261 exchanges, about 2.3 s); see "iC5000".
   - **iC5700 without active probe:** `src/attach_tc37x_system.txt`, winIDEA 9.21.417's hot attach on iC5700 B (3532 exchanges, about 2.3 s), from `no-ap-417-warm.jsonl`. It asks for the FPGA status after three messages, scans the nodes and sets the clock with `//4///7/ 01 00 <kHz>` (10,000 kHz), as with 9.21.356.
   - **iC5700 with active probe:** `src/attach_tc37x.txt`, winIDEA 9.21.417's hot attach on iC5700 B with the active probe (3705 exchanges, about 2.4 s), from `ap-417-warm.jsonl`. It sets the clock through the probe with `//7//1// <Hz>` next to `//4///7/`, 10 MHz each.
   - **With or without active probe:** the two attaches are the same up to and with their node-5 scan (`//5///22/ 00 … c8`), which answers with a node count, 2 with an active probe and 1 without, as winIDEA uses it. The driver sends that part, reads the count and continues with the matching script (`Setup::script`). A replayed attach captured in the other setup (`Script::captured_setup`) stops after the scan with an error, unless its first part is the built-in one.

### winIDEA 9.21.417 against 9.21.356

Captured on the iC5000 (`ic5000-417*.jsonl`) and, for the iC5700's bring-up, `first.jsonl`. Frames, the hello, the memory request and reply data, node 5, the reset `//7///7/`, the kernel transport and the iC5000's FPGA image are the same. Different:

| | 9.21.356 | 9.21.417 |
|---|---|---|
| Host id in `G` | `a6 64` | `a6 a1` |
| Session set / kernel check / session read | `//1///28/`, `//1///25/`, `//1///27/` | `//1///29/`, `//1///26/`, `//1///28/` |
| Memory service | `//2///16/` | `//2///14/` |
| Result in a memory reply | 36 bytes + bitmap + data | 40 bytes + bitmap + data (the tail is 4 bytes longer) |
| Reset reconnect | `//2///38/` | `//2///34/` |
| Breakpoint service | `//3///10/` | `//3///7/` |
| iC5700 FPGA status | `//1///35/` | `//1///37/` |
| iC5000 FPGA upload after | `//1///41/` | `//1///43/` |
| SoC / core tables | `//2///30/` (7337 B), `//3/…/9/` (5663 B) | `//2///26/` (7378 B), `//3/…/6/` (5699 B) |

Node-1 to node-3 services moved by different amounts, and some bodies changed size (`//1///13/` 9 bytes instead of 8), so every captured script belongs to its version. A cold 9.21.417 capture also showed winIDEA re-syncing the frame bits with a longer hello (`42 85 c9 05 42 89 c2`) on a probe our driver had used; the driver drains the pipe and uses the plain hello, which worked every time.

None of these resets the target. Every `init` runs them all: the probe keeps no session across connects.

### iC5000

The iC5000 (`06F9:3200`) speaks the same frames, messages and services; the memory service is byte for byte the same. The model comes from the USB ids (`Model`), and the bring-up and attach differ (`ic5000.jsonl`, `ic5000-warm.jsonl`, the iC5000):

- **Padding:** frame payloads are padded to 2 bytes, not 4 (`Model::frame_pad`). Padded to 4, the first `E` command hung the probe until a power cycle.
- **Short answers:** a low-level answer of N bytes is read with capacity N rounded up to even, `08` included, so an even-length answer loses its last byte (`L`, `W` and `K` answer `33` alone, the session message `01`).
- **Kernel:** `iC3kKrnl.s` from the same winIDEA install (2,453,128 bytes, loaded in about 290 ms), and its own session value. The `I` state without a kernel has the two top bits set (`ef`, `eb`), with one it is `2f`.
- **Debug FPGA:** loaded on every attach, warm or cold, after `//1///41/`: `//1///7/ 02`, five `//1///10/` chunks (`<length u32 LE> <data>`), `//1///9/ 02`. The image (266,599 bytes) is stored as-is in winIDEA's `ICWHW32.HRS`, an entry `01 83 12 12 09 ff <length>`; `fpga::ic5000::find_image` picks it by length and hash. Nothing of it is in this repository.
- **Attach:** `src/attach_tc37x_ic5000.txt`, winIDEA's hot attach with the target running (272 exchanges, about 2.6 s), without the FPGA upload. Node 5 is set up with other services (`//5///1/`, `//5///2/`, `//5///30/`, `//5///33/`); there is no node scan, so the setup is "without active probe".
- **Pauses:** winIDEA pauses between some messages, and the scripts now keep pauses of 3 ms or more (`w<ms>`, `Exchange::pause_ms`). Without the 10 ms after `//5///1/`, the probe's read-back of its node-5 setup answered differently.
- **Target state:** winIDEA's attaches that found the target unreachable reset it, up to 8 times. The built-in attach does not; on a target in such a state, `reset halt` brings it back.
- **DAP clock:** `//5///2/ 8e 00 00 b0 <n> ff`, 30 MHz / (n + 1) (see "DAP clock").

The kernel check is byte-identical to winIDEA's in the tests, but has not run on hardware through this crate yet. The prototype shows the FPGA status: `cargo run -p bluebox --example probe -- --fpga-status`.

## Reset

`reset` (`src/reset.rs`): three of the 151 messages winIDEA's `CExecutionController.reset()` sends. `//2///7/ 01`, then `//7///7/` with eight zero bytes (the reset), then `//2///38/ 01`, which reconnects and returns cpu0's `DBGSR` and PC. The chip resets and cpu0 halts at its start address (`0x800a8000` here).

On an iC5000 the reconnect waits 110 ms after the reset, as winIDEA does: sent at once, it found cpu0 halted, and the connection dropped about 30 ms later for good.

There is no reset without halt. The driver says so (`Adapter::reset_always_halts`): `reset run` runs as a halting reset, so `tc3xx_ocds.cfg`'s reset hooks see halted cores and start the HSM. Then the core resumes the halted cores through the normal resume path.

## DAP clock (`adapter speed`)

`//7//1// <Hz>` sets the DAP clock with an active probe, `//4///7/ 01 00 <kHz>` without one; the captured attach sends 10 MHz (the workspace's `DAPClockkHz`). It is 100 MHz divided by an integer (winIDEA: "Clock divider for 100MHz base clock"), so the driver rounds `adapter speed` down to such a value and keeps it between 1 and 25 MHz: the cfg's `adapter speed 30000` runs at 25 MHz.

The attach, reconnects and the reset always run at 10 MHz: an attach at 25 MHz failed when it had to bring a target into debug mode (winIDEA too starts slower, `InitScanClockkHz` 500). Once the target answers, the driver switches to the configured clock, and back to 10 MHz if the target does not answer there. A later `adapter speed` applies at once.

Measured on the TC377 board, 64 KiB of application code:

| Clock | 64 KiB | Data |
|---|---|---|
| 1 MHz | 376 ms | correct |
| 5 MHz | 110 ms | correct |
| 10 MHz | 75 ms | correct |
| 12.5 MHz | 71 ms | correct |
| 16.7 MHz | 62 ms | correct |
| 20 MHz | 59 ms | correct |
| 25 MHz | 56-58 ms | correct |
| 33.3 MHz | 54 ms | **200 bytes failed, data wrong**; afterwards the probe refused a lower clock and reads came back wrong but marked good, until a new connect |

**iC5000:** `//5///2/ 8e 00 00 b0 <n> ff` sets 30 MHz / (n + 1). Captured with the workspace's `DAPClockkHz` changed: 10,000 kHz gives `n` = 2, 5,000 gives 5, 7,000 gives 3 (winIDEA rounds up, to 7.5 MHz), 40,000 gives 0. Every attach starts at `n` = 0x1d, the initial 1 MHz. The driver rounds down instead and keeps between 2 and 30 MHz: at 1 MHz, 8 KiB flash reads came back with failed bytes, while from 2 to 30 MHz 256 KiB read back identical. The rate doesn't change with the clock: the iC5000 spends about 0.5 ms per access itself (about 100 KB/s on long reads, winIDEA's too). The attach and resets run at 10 MHz, as on the iC5700.

Above about 12 MHz the USB round trips dominate. A miniWiggler reaches 30 MHz on the same board, so the limit is the BlueBox setup: winIDEA may set more than the clock for higher rates (it has a `LimitDAPClock` option); a winIDEA capture at a higher clock would show.

## Recovery

The `bluebox` driver does what the TAS server does for a miniWiggler: it notices a lost connection and reconnects, and the core re-examines the targets.
- **Target not answering at connect** (not powered, held in reset, or reset over and over by an external watchdog that can't be turned off): `init` still succeeds, the cores stay unexamined, and the driver treats the target as lost from the start (below). A `reset` tries the probe's reset at once, as does `reset_config connect_assert_srst` at connect. On an iC5700 whose active probe takes its power from the target, the node scan of an unpowered board finds no probe, so the session runs on the attach without one.
- **Target lost** (it reset or lost power): a batch with failed accesses is followed by a read of `SCU_CHIPID`; if that fails too, the driver sends the attach's connect part again (from its last `L`, about 290 exchanges, 0.4 s), at most once a second, and reports a reset (`take_resets`).
- **Probe lost** (USB or protocol errors, e.g. a BlueBox power cycle): the probe is opened again and the whole connect runs (kernel load and attach, about 4.6 s), at most every 3 s. No reset is reported.
- `bluebox drop_target` resets the target and leaves the connection broken, to try the target case.

## Batched access

One `//2///16/` message carries several accesses: a u32 count, the single request's access block per access (166 + N bytes), and 12 zero bytes. The reply has the count at 0x1f and the results packed from 0x23 (`u32 0, B, bitmap[B], N, data[N]`, 24 bytes). Measured on the TC377 with the prototype's `--batch`:
- The probe runs every access of a message, also after one fails; only that one is flagged. A write's bus error shows on the next access.
- Each access costs about 0.45 ms inside the probe at 10 and 25 MHz alike, and with every access-flag variant: 16 reads take 8.5 ms, 64 take 30 ms, 256 take 114 ms (a single access 1.1 to 1.7 ms).
- Six 8 KiB reads take 33 ms (1.5 MB/s, a 55.5 KB reply); seven fail, since the reply (data plus a bitmap of N/8 bytes each) passes 61,440 bytes.

`Probe::access` (used by the driver) plans the messages with `memory::plan`: reads share a message of at most 64 accesses and a 60,000-byte reply; a write has a message of its own followed by the `STM0_TIM0` check read, so it runs only after everything before it succeeded and nothing runs after it before its outcome is known. The results stop at the first failed op, which the driver reports like a TAS packet (the rest consequential). Not yet run on hardware through the driver.

## What works with winIDEA 9.21.417 (iC5000, TC377TP, through `aurix-dbg`)

- Connect with its kernel running (recognized as its own) and after loading it; the target keeps running (uptime 1077 s, counting).
- Write and read back, halt, `reset halt`, `reset run` (cpu0 and the HSM run), `adapter speed` 30 MHz, recovery after `drop_target` (0.8 s).
- Fixed on the way: the 417 script armed winIDEA's leftover breakpoint (0x80120ae0) until the disarm used 417's breakpoint service; `step` was checked only before that fix.
- iC5700 B without active probe: connect with its kernel running and after loading it, the target keeps running (three connects in a row, uptime counting); write back and read, halt, `step` (0x800a8000, 4, 8), `reset halt`, `reset run` (cpu0 and the HSM run), 25 MHz, recovery after `drop_target` (0.6 s). 600 writes without a failure. Writing changing values into the application's RAM (0x70000000) restarted the chip now and then, through its watchdog: test writes should keep the data.
- After `reset run` the driver briefly lost the target and reconnected (0.6 to 0.8 s), on both probes with 9.21.417; not explained.
- iC5700 B with the active probe: detected by the node scan, attach 2.4 s, 25 MHz through `//7//1//`, `reset halt`, `step` (0x800a8000 to 4), `reset run` (cpu0, cpu1 and the HSM run), recovery after `drop_target` (0.4 s), 8 KiB reads of RAM (20 of 20 clean).
- **Flash reads restart the chip in this state.** With the active probe, reads of program flash (8 KiB, 1 KiB and 4 bytes alike) came back partly failed and the chip restarted (STM0 back near 0); winIDEA 9.21.417's own flash-read messages, replayed, did the same. RAM and registers were not affected. Flash reads without the active probe and on the iC5000 were not tried with 9.21.417; with 9.21.356 (iC5000, and the active probe on iC5700 A) 64 KiB to 256 KiB of flash read clean. Not explained; probably the board's application reacting to debugger flash access.
- iC5700 A could not be captured: winIDEA 9.21.417 refused it ("License has expired"); the active probe was moved to iC5700 B for the capture.
- Not run on hardware through the driver: another version's kernel being replaced (fixture only).
- The board fell several times into a state where nothing reached the target, winIDEA 9.21.417's attach included ("External reset detected - possible watchdog activity", then a reset). On a freshly powered board winIDEA's own attach resets it too.

## What works on the iC5000 with winIDEA 9.21.356 (TC377TP, through `aurix-dbg`)

- Connect after a power cycle and with the kernel running: kernel load, FPGA upload, attach, about 3.5 s in all. The target keeps running (STM0 uptime kept counting).
- `reset halt` (cpu0 halted at `0x800a8000`) and `reset run`, with the board cfg plus `tc37x_ocds_init.cfg`: cpu0, cpu1 and the HSM run, as after winIDEA's `resetAndRun()`. `reset halt` then `resume` does the same.
- Recovery after `bluebox drop_target`: reconnected in 1.3 s.
- `adapter speed` from 2 to 30 MHz, at `init` and later (7,000 kHz runs at 6 MHz), kept across resets.
- The target became unreachable three times (the attach ran, `SCU_CHIPID` failed): after the first, broken attempts, after it had sat halted at boot for minutes, and once after a `reset run` at 6 MHz that did not repeat. `reset halt` brought it back each time.
- Not tested: run control beyond halt and resume, breakpoints, the DAQ.

## What works (tested on hardware, TC377TP, iC5700 A)

- Recovery, through `aurix-dbg` with `tc37x_ocds_init.cfg`:
  - after `bluebox drop_target`: reconnected in 357 ms, "Device reset detected", all four cores re-examined, the application ran on;
  - after a BlueBox power cycle during a session: reconnected in 4.6 s once the probe was back, cores re-examined, the application kept running throughout (STM0 uptime unbroken).

- Connecting with nothing but the probe and `iC6kKrnl.s`: after a power cycle of the BlueBox and with the kernel running, both through `aurix-dbg`. The target kept running (STM0 uptime kept counting).
- Memory read and write through the memory service `//2///16/`, with a per-byte failure bitmap.
  - Reads from 1 byte to 64 KiB, split into 8 KiB requests.
  - About 1.2 ms per small access; 0.3 MB/s on long reads.
  - A refused write (read-only register) is reported failed. A write of RAM reads back.
- Reset, through `aurix-dbg` with the board cfg plus `tc37x_ocds_init.cfg` (MCS targets off):
  - `reset halt`: the chip restarts (STM0 uptime about 0.4 s), cpu0 halts at `0x800a8000`, the cfg's hooks run (HSM found at its boot-ROM breakpoint, STM suspend set). About 0.8 s.
  - `resume` after it starts the application: cpu0 and cpu1 run.
  - `reset run`: the chip restarts, cpu0 and cpu1 run. About 0.8 s.
  - With the board cfg alone, cpu1 stays in boot halt after any reset: the boot ROM halts the HSM on a halting reset, and only the cfg starts it again.
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
- **Version-bound values.** The kernel, the node-1 to node-3 service numbers, the host id in `G`, the session value, the memory reply's result length and the attach all belong to winIDEA 9.21.417.
- **The kernel does not survive our sessions.** After a run of this crate the next handshake usually finds no kernel; winIDEA's close leaves it running. The driver reloads it (260 ms). A proper close is not decoded.
- **The `I` answer** showed a third value once (`01`, besides `00` = no kernel and `2f` = kernel). It is treated as "kernel running"; that worked.
- **FPGA image.** The debug FPGA keeps its image across power cycles; winIDEA replaces it only when the hash differs (another winIDEA version). This crate does not check or load it.
- **Reset without halt.** Not found: winIDEA's `resetAndRun()` also resets with halt and runs afterwards.
- **JTAG id.** Not read; `DeviceInfo.device_id` is 0.
- **DAP clocks above 25 MHz** gave wrong data (see "DAP clock"); the driver caps at 25 MHz.
- **The probe's `I` state.** Seen: `00` (no kernel), `2f` (kernel ready), `55` (booting, right after power-on), `01` and `40`. The driver waits up to 20 s for `00` or `2f`, then restarts the kernel. Attaching in another state hung the probe until a full power-off: unplug power and USB, as a soft power-cycle kept the stuck kernel.
- **Trigger setup.** The attach enables capture-and-hold on trigger line 1, so any halt holds all cores until `CBS_TLCHS` is written. Load `tc3xx_ocds.cfg`, whose resume and step hooks do that. Without it, the cores stay held.
- **Resets after resume (fixed in the cfg: the application's ADC safety reset, peripherals now suspended with the cores).** Twice, the target dropped off the debug port right after `resume` released trigger line 1, and the chip restarted; the driver now reconnects. With `tc37x_ocds_init.cfg` a 3 s halt and resume did not restart the chip. Without it (DBGSR written directly) it did every time: the application restarts the chip, and about 0.2 s later the connection breaks.
  - With 9.21.417 (iC5700 B without active probe, 2026-11-05) it happened on most resumes after halting cpu1 or cpu2: with the cfg, and with the plain target cfg when the cores were halted one by one. Halting cpu0 alone never did it. STM0 kept counting through the halts, and the cfg reported `STM0_OCS write refused (STM0_ACCEN0=0x00000004)`: the application locks the STMs, so they are not suspended, and the application sees the time jump.
  - The traces show nothing wrong with the 9.21.417 migration (request and reply layout, channels, reset, attach writes all checked against every capture).
  - Measured on 2026-11-06 (`memory.rs`, "Access flags"): winIDEA's own access flags (`00 01 00`) do not get past the application's `STMx_ACCEN0` either; the probe only stops reporting the refused write. The driver keeps the SDK's `01 01 01`.
  - Not BlueBox-specific: the same with the miniWiggler and with the build from before the BlueBox work (eda9dba).
  - Cause, caught with hardware breakpoints (2026-11-06): the application resets the board itself. On cpu1 its EVADC interrupt reports an ADC safety error and requests a reset; OCDS is off afterwards. The ADC runs on while the cores are halted (winIDEA's workspace has `SuspendPeripheralsWhileStopped`).
  - Fix in the cfg, as winIDEA does it: `tc3xx_suspend_ocs` suspends peripherals with the cores (OCS.SUS, written at resume and at halt; after a reset resume waits up to `tc3xx_suspend_wait_ms` until the application has enabled the modules, the EVADC after 34 ms). It must be set before the halt: written only at the halt, a few ms of the ADC running were enough. The board cfg (`tc3xx_periph`) lists every module of the TC37x SVD whose OCS write the running application accepts: EVADC, GTM, CAN0/1, CONVCTRL, SMU, GPT12, QSPI2, QSPI4 (the other 26 OCS registers are off or locked).
  - Result through the miniWiggler with `tl1_step on`: instruction steps 5 of 5 and source-level steps through the extension's debug adapter 12 of 12 without a reset (before: 3 to 4 of 4, and 4 of 5 with only the EVADC suspended). Through the BlueBox (iC5700 B, no active probe): instruction steps 6 of 6 and source-level steps 6 of 6 without a reset.
  - The STM suspend needs `STMx_OCS` written while a reset holds the application: `tc3xx_connect_reset_halt` now runs the application 200 ms after a running reset (`tc3xx_connect_prerun_ms`), then resets with halt; its hooks set `STMx_OCS`, which stays set after the application locks `STMx_ACCEN0`, and the STMs then stop during a halt. An application reset clears it again.
- **Resets that keep the connection** go unnoticed: right after such a restart reads still worked for about 0.2 s. A reset only shows once the connection breaks.

## Prototype

```bash
# winIDEA must be closed; MinGW on PATH for the GNU toolchain (see rust-toolchain.toml)
cargo run -p bluebox --example probe -- --attach [--watch 2]           # bring-up + built-in attach
    [--then <trace> <step> [--then-only a-b,c] [--then-skip a-b]]...  # then captured steps, e.g. "reset: reset()"
cargo run -p bluebox --example probe -- --script <trace.jsonl> [--until-a] [--skip a-b,c] [--drop path:b0:len]
    [--list | --export <file>] [--watch 2]                              # try or reduce a captured attach
cargo run -p bluebox --example probe -- --replay <trace.jsonl>          # byte-for-byte playback (resets the target)
    [--kernel C:/winIDEA/iC6kKrnl.s] [--restart-kernel] [--rewrite 0x70000018] [--map ADDR LEN]
```

`--watch` reads the chip id, STM0 uptime and cpu0 `DBGSR` every 200 ms; an uptime near zero means the attach reset the target. After the attach, `--reset-halt` resets, `--chipid-loop <n>` reads `SCU_CHIPID` every 20 ms and reports when that changes, and `--speed-test` times reads.

`BLUEBOX_TRACE=1` logs every USB transfer to stderr, its first 24 bytes (`BLUEBOX_TRACE=2`: all of it).

## Server

```tcl
adapter driver bluebox
adapter serial 01ABC                        ;# only with several probes
bluebox winidea C:/tools/winIDEA_9_21_417                      ;# optional: a winIDEA 9.21.417 install (kernel, iC5000 FPGA image)
bluebox kernel C:/winIDEA/iC6kKrnl.s        ;# optional: the kernel file itself (iC3kKrnl.s for an iC5000)
bluebox attach_replay C:/traces/x.jsonl     ;# optional: another target or setup; step defaults to "connect: begin_attach"
```

The traces come from the Frida capture tools (`capture.py` + `scenarios.py`). They aren't in this repository.
