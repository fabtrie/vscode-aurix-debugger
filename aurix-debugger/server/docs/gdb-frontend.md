# gdb RSP frontend: contract with the core

How a gdb remote serial protocol frontend plugs into aurix-dbg. The core provides everything below; the frontend only translates the protocol. Model it on the TCF frontend (`crates/tcf`).

## Placement and startup

- **Crate:** `crates/gdb`, next to `crates/tcf`. Depends on `aurix-core` only.
- **Startup:** in `crates/aurix-dbg/src/main.rs`, start it like `tcf::start` when `Ctx.ports.gdb` is set (Tcl `gdb_port`; `bindto` is shared).
- **Ports:** as in OpenOCD, one per target: `gdb_port` for the first examined target, then consecutive ports (`localhost:3333` = cpu0).
- **Symbols:** not needed; gdb reads DWARF itself. Do not use the `symbols` crate.

## Core access

Everything that touches the target or Tcl runs on the core thread:

```rust
let handle: aurix_core::ServerHandle = server.handle();
// Run a closure on the core thread; None if the server stopped.
let pc = handle.call(move |cx| aurix_core::ops::read_reg(cx, target, aurix_core::arch::REG_PC));
// Tcl (monitor commands): result plus the lines logged while it ran.
let (result, lines) = handle.tcl_with_output("reset halt");
// Core events, one receiver per subscriber.
let events = handle.subscribe();
```

Rules:
- **Keep closures short:** polling, TCF and telnet share the core thread. A long operation (flash write, `wait_state`) blocks the other frontends while it runs.
- **No Tcl while holding a borrow:** inside `cx.with(|c| …)`, never call `cx.interp`. The `ops::` functions follow this rule.

### Operations (`aurix_core::ops`, `aurix_core::flash`)

| gdb packet | Core function |
|---|---|
| Halt (`^C`) | `ops::halt(cx, t)`. With `ocds sync tl1` (set by `tc3xx_ocds.cfg`) all cores stop. |
| Continue (`c`, `vCont;c`) | `ops::resume(cx, t, addr: Option<u32>)`. Fires `resume-start/end`; restores interrupts masked for steps. |
| Single step (`s`) | `ops::step(cx, t, addr)`. Uses a hardware trigger; masks a breakpoint at the PC; runs with interrupts masked if `tricore irq_mask` includes `step`. For `next`/`finish` built from steps and runs: `ops::step_as` / `ops::resume_as` with `RunMode::StepOver` / `StepOut`. |
| State | `ops::state(cx, t) -> (TargetState, DebugReason)`, `ops::wait_state(cx, t, want, ms)` |
| Registers (`g`/`G`/`p`/`P`) | `ops::read_reg` / `write_reg` with an index into `arch::REGS` |
| Memory (`m`/`M`/`X`) | `ops::read_memory` / `write_memory`. Allowed while running. |
| Breakpoints (`Z0`/`Z1`) | `ops::add_breakpoint(cx, t, addr, len, BpKind::Hard)`, `remove_breakpoint` |
| Watchpoints (`Z2`–`Z4`) | `ops::add_watchpoint(cx, t, addr, len, WatchAccess)`, `remove_watchpoint` |
| Reset (`monitor reset …`) | `ops::reset(cx, "run" \| "halt" \| "init")`. Runs the full OpenOCD reset event sequence. |
| Flash load (`vFlashErase`/`vFlashWrite`/`vFlashDone`) | Collect the writes into `image::Image::from_sections`; on `vFlashDone` call `flash::write_image(cx, &img, true)`. |
| `monitor <cmd>` (`qRcmd`) | `handle.tcl_with_output(cmd)`; send the lines as `O` packets. |

Targets are indices into `Ctx.targets`, in cfg order (cpu0, cpu1, …; then HSM, MCS and SCR targets when the cfg creates them). `TargetEntry.core.examined()` tells whether a core is usable; a core in boot halt is halted and examined.

## Events

`aurix_core::Event`, broadcast to every subscriber:

| Event | gdb frontend |
|---|---|
| `Halted { target, reason, pc, trigger, .. }` | If `target`'s connection waits in `c`/`s`, send a stop reply: `T05` with `swbreak`/`hwbreak`/`watch:<addr>` from `reason` and `trigger`. With TL1 sync the other cores halt too (`Halted`, reason `DbgRq`); report each only on its own connection. |
| `Resumed { target, .. }` | Informational: a script resumed the core. |
| `ResetDetected` / `ResetDone` | The target was reset; the core re-armed registers and triggers. A waiting `c` gets a stop reply once the core halts again. |
| `Shutdown` | Close the connections. |
| `Log` | Optional console output. |
| Others (`Examined`, `Unreachable`, `ProbeLost`, `MemoryChanged`, `RegisterChanged`, …) | Informational. |

The core polls for halts every 10 ms (`polling_period`); the frontend does not poll.

## Registers and target description

Each `arch::REGS` entry has `name`, `offset` (CSFR), `gdb` (gdb register number, if in gdb's TriCore layout), `dwarf`, `group`, `description`. All registers are 32-bit little-endian. Core group numbering follows `tricore-elf-gdb`:
- D0–D15 = 0–15;
- A0–A15 = 16–31 (A10 = SP, A11 = RA);
- `lcx`, `fcx`, `pcxi` (34), `psw` (35), `pc` (36), `icr`, `isp`, `btv`, `biv`, `syscon`;
- then `cpu_id`, `compat` and the debug group (`dbgsr`, `exevt`, …), without gdb numbers.

Build the `g` packet and `target.xml` (`qXfer:features:read`):
- from the `gdb` numbers, not the table order;
- the core group as the TriCore feature; registers without a gdb number as a separate feature, so `info registers` stays short;
- check the numbers against `tricore-elf-gdb` (`maint print registers`) and OpenOCD's TriCore register list: the `g` packet must match gdb exactly.

## Memory map

Answer `qXfer:memory-map:read` from `Ctx.flash_banks`:
- each bank: a `flash` region, `blocksize` 0x4000 (16 KB logical sectors);
- RAM and SFR space: `ram`, or undefined.

With flash regions, gdb uses hardware breakpoints in flash by itself.

## Breakpoints

- **Hardware only:** 8 trigger events per core, shared with watchpoints. Map `Z0` (software) to hardware breakpoints, as OpenOCD does. Return `E` when no trigger is free.
- **Stepping** takes one trigger temporarily: keep one free, or stepping fails when all 8 are in use.

## Target caveats

- **Protected RAM:** writes to SMPU-protected RAM are dropped without an error. Verify `M`/`X` writes by read-back when the core is halted, as the TCF frontend does.
- **Flash protection:** an erase of protected PFLASH fails; `flash::write_image` returns the error. Reply `E` to `vFlashErase`/`vFlashDone`.
- **Unprogrammed flash:** reading it can reset the board through the SMU. Do not read memory speculatively (e.g. beyond the memory map).

## Testing

- **Offline:** `--adapter sim` (`crates/core/src/sim.rs`), as `crates/core/tests/cfg_sim.rs` does, with a small RSP client in the test.
- **Hardware:**

  ```
  tricore-elf-gdb app.elf -ex "target extended-remote :3333" -ex "info registers"
  ```

  Then `break`, `continue`, `step`, `bt`, `load`; compare with the TCF frontend at the same halt.
