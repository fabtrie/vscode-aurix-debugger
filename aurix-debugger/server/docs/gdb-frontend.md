# gdb RSP frontend: contract with the core

This document describes how a gdb remote serial protocol frontend plugs into aurix-dbg. The core already provides everything below; the frontend only translates the protocol. It follows the TCF frontend in `crates/tcf`.

## Placement and startup

- **Crate:** a new crate `crates/gdb`, a sibling of `crates/tcf`. It depends on `aurix-core` only.
- **Startup:** in `crates/aurix-dbg/src/main.rs`, start it like `tcf::start` when `Ctx.ports.gdb` is set (the `gdb_port` command; `bindto` is shared).
- **Port numbers:** as in OpenOCD, one port per target: `gdb_port` for the first examined target, then consecutive ports. This matches what users of the OpenOCD configuration expect (`localhost:3333` = cpu0).
- **Not needed:** symbols. The `symbols` crate is not used; gdb has its own DWARF.

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
- **Keep core-thread closures short:** polling, TCF and telnet share the thread. A long operation (flash write, `wait_state`) blocks the other frontends while it runs, which is acceptable but should be deliberate.
- **Don't evaluate Tcl while holding a borrow:** inside `cx.with(|c| …)`, never call `cx.interp`. Use the `ops::` functions, which follow this rule.

### Operations (`aurix_core::ops`, `aurix_core::flash`)

| gdb need | Core function |
|---|---|
| Halt (`^C`) | `ops::halt(cx, t)`. With `tricore tl1_halt on` from the cfg, all cores stop. |
| Continue (`c`, `vCont;c`) | `ops::resume(cx, t, addr: Option<u32>)`. Fires `resume-start/end`; the cfg releases the other cores. Restores interrupts masked for steps. |
| Single step (`s`) | `ops::step(cx, t, addr)`. Hardware-trigger based; masks a breakpoint at the PC. Runs with interrupts masked if `tricore irq_mask` includes `step`. For a gdb `next`/`finish` built from steps and runs, use `ops::step_as` / `ops::resume_as` with `RunMode::StepOver` / `StepOut`. |
| State | `ops::state(cx, t) -> (TargetState, DebugReason)`, `ops::wait_state` |
| Registers (`g`/`G`/`p`/`P`) | `ops::read_reg`/`write_reg` with an index into `arch::REGS` |
| Memory (`m`/`M`/`X`) | `ops::read_memory`/`write_memory`. Allowed while running. |
| Breakpoints (`Z0`/`Z1`) | `ops::add_breakpoint(cx, t, addr, len, BpKind::Hard)`, `remove_breakpoint` |
| Watchpoints (`Z2`–`Z4`) | `ops::add_watchpoint` / `remove_watchpoint` (`WatchAccess`) |
| Reset (`monitor reset …`) | `ops::reset(cx, "run" \| "halt" \| "init")`. Runs the full OpenOCD reset event sequence. |
| Flash load (`vFlashErase`/`vFlashWrite`/`vFlashDone`) | Collect the writes into `image::Image::from_sections`, then call `flash::write_image(cx, &img, true)` on `vFlashDone`. |
| `monitor <cmd>` (`qRcmd`) | `handle.tcl_with_output(cmd)`; send the lines as `O` packets. |

Targets are indices into `Ctx.targets`: cpu0, cpu1, cpu2 in cfg order. `TargetEntry.core.examined` tells whether a core is usable; a core in boot halt is halted and examined.

## Events

`aurix_core::Event` (broadcast to every subscriber):

| Event | gdb frontend |
|---|---|
| `Halted { target, reason, pc, trigger, .. }` | If the connection for `target` is waiting in `c`/`s`, send a stop reply: `T05` with `swbreak`/`hwbreak`/`watch:<addr>` from `reason` and `trigger`. With TL1, the other cores also halt and emit `Halted` with reason `DbgRq`. Report them only on their own connection. |
| `Resumed { target, .. }` | Informational; a script (cfg) resumed the core. |
| `ResetDetected` / `ResetDone` | The target was reset. Registers and triggers were re-armed by the core. A waiting `c` should get a stop reply once the core halts again. |
| `Examined`, `Log`, `Shutdown` | `Shutdown`: close the connections. `Log`: optional console output. |

Poll-based halt detection runs every 10 ms on the core thread, so the frontend does not poll.

## Registers and target description

Each `arch::REGS` entry carries `name`, `offset` (CSFR), `gdb` (gdb register number, if the register is in gdb's TriCore layout), `dwarf`, `group` and `description`. All registers are 32 bits, little-endian. The core group follows the `tricore-elf-gdb` numbering:
- D0–D15 = 0–15;
- A0–A15 = 16–31 (A10 = SP, A11 = RA);
- then `lcx`, `fcx`, `pcxi` (34), `psw` (35), `pc` (36), `icr`, `isp`, `btv`, `biv`, `syscon`;
- then `cpu_id`, `compat` and the debug group (`dbgsr`, `exevt`, …).

- Build the `g` packet and `target.xml` (`qXfer:features:read`) from the `gdb` numbers, not from the table order: the core group as the TriCore feature, registers without a gdb number (debug group) as a separate feature so `info registers` stays short.
- Check the numbers against `tricore-elf-gdb` (`maint print registers`) and OpenOCD's tricore register list before relying on them. The `g` packet must match gdb's expectation exactly.

## Memory map

Answer `qXfer:memory-map:read` from `Ctx.flash_banks`. Each bank becomes a `flash` region with `blocksize` 0x4000 (16 KB logical sectors). Mark RAM and SFR space as `ram` or leave it undefined.

With a flash region, gdb uses hardware breakpoints in flash automatically, which also fixes the auto-hw workaround needed in the OpenOCD launch configuration.

## Breakpoints

- **Hardware only:** TriCore has 8 trigger events per core, shared with watchpoints. Map `Z0` (software) to hardware breakpoints, as OpenOCD does, and return `E` when the core reports no free trigger.
- **Stepping:** `ops::step` temporarily uses one trigger. Keep one free, or accept that stepping fails when all 8 are in use.

## Board caveats that surface through gdb

- **Protected memory:** writes to SMPU-protected RAM (CPU0 stack on the TC377 board) are dropped without an error from TAS. The TCF frontend verifies RAM writes by read-back; do the same for `M`/`X` when the core is halted.
- **Flash protection:** PFLASH erase on the TC377 board fails with a protection error. `flash::write_image` returns it as an error; report `E` on `vFlashErase`/`vFlashDone`.
- **Unprogrammed flash:** reading it can reset the board through the SMU. Don't read memory speculatively, for example beyond the memory map.

## Testing

- **Offline:** use `--adapter sim` (`core/src/sim.rs`), as `crates/core/tests/cfg_sim.rs` does, with a small RSP client in the test.
- **Hardware:**

  ```
  tricore-elf-gdb app.elf -ex "target extended-remote :3333" -ex "info registers"
  ```

  Then `break`, `continue`, `step`, `bt` and `load`, comparing with the TCF frontend's results for the same halt.
