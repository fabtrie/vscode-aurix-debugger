# On-chip trace and reverse debugging

## On-chip trace

TC37x only, over the miniWiggler. The trace uses the chip's miniMCDS: an 8 KB buffer, which holds about 80 µs of unfiltered program flow and more with a filter. Only CPU0 is traced, so the data trace sees only CPU0's writes. No trace probe is needed.

### Trace panel

**Show Trace** opens the Trace panel.

| Setting | Choices |
|---|---|
| Record | All CPU0 code, one function or address range, or every write into a variable range (up to `0x1000` bytes) |
| Stop | On halt (the trace ends exactly at the breakpoint or halt), when the buffer is full, manually, or at a function |

- The panel checks the configuration as you type and shows the trace resources it uses.
- Configurations can be saved by name.
- **Arm on every run, stop on halt** arms the trace on every Continue and reads it on halt. Every breakpoint then comes with the trace up to it, and Step Back works right away.

### Results

- **Functions:** the executed code, grouped by function, with source lines and target times. Click a row to open the source. **Export CSV** saves the list.
- **Instructions:** every executed instruction, with its source line, what it wrote, and the registers at that instruction (rebuilt from the halt).
  - Each register value is marked exact, taken from memory at the halt, or unknown.
  - Values after a gap in the trace, and results of FPU instructions, are mostly unknown, so a filtered trace gives few register values.
  - Click a register to see where its value came from.
- **Data trace:** each write with its address, value, size, symbol and target time.

### Step Back replay

Without reverse debugging, **Step Back** and **Reverse Continue** replay the last trace:

- The editor, Variables, Watch, Registers, Call Stack and Disassembly show the state at that point. Nothing on the target changes.
- The status bar shows **Trace replay · view only**. The current line marks where the core really is.
- Step, Step Into and Continue move forward again, up to the halt. **Leave Trace Replay** goes straight back to the halt.

### Other uses

- **Live Watch:** **Trace every write** (in a view's interval menu) records the view's variables with the trace instead of reading them. You get every change, with target time. Only one view at a time can use it.
- **OS Profile:** CPU0's lanes can be traced instead of sampled, which catches every task switch.

### Launch configuration

`"trace": { ... }` arms a trace when the session starts. It takes the same keys as the panel, e.g.:

```json
"trace": { "Kind": "program", "Ranges": [{ "Function": "Motor_Update" }], "Stop": "halt" }
```

`"trace": { "Auto": true }` turns on **Arm on every run, stop on halt**.

## Reverse debugging

Enable it with `"reverse": true` in the launch configuration, or with **Enable Reverse Debugging**. **Step Back** and **Reverse Continue** then put the real core back into an earlier state, and it runs on from there.

### How it works

- Before every step, run, and register or memory write, the debugger saves the core's registers and RAM: the ELF's writable sections, the stacks and the context save area.
- **Step Back** goes to the start of the previous line, stepping back over calls. In instruction stepping mode it goes back one instruction.
  - Inside a Continue, it reruns from the start of the run up to that line.
  - If the rerun takes another path, it stops there and says so.
- **Reverse Continue** goes back to the last breakpoint or data breakpoint hit, or to the start of the history.
- The Trace panel's **History** tab lists the checkpoints and goes back to any of them. **Show Reverse Debugging History** opens it.
- **Go here** in the Instructions tab goes back to an instruction of the last action.
- At the start, the Debug Console shows how much RAM is saved and how long a checkpoint takes.

### What is not rewound

- Peripherals (their writes are listed in the Debug Console), timers, other cores, DMA.
- Use a reset to go back across initialisation.

### Protected RAM

RAM that is protected against other bus masters (TC3xx DSPR safety protection, `CPUx_SPR_SPROT`) also blocks the debugger.

- A Step Back that would write such RAM is refused, and nothing changes.
- At the start, the Debug Console lists the blocked areas.
- To fix this in a test build, add one more protection region per core that opens these areas to the debugger (master tags 28 and 30).

### Trace and reverse together

Reverse debugging uses the on-chip trace, so the Trace panel's own setup is off. `reverse` and `trace` cannot both be in a launch configuration.

### Options

`"reverse": { ... }` takes these options:

| Option | Default | Description |
|---|---|---|
| `history` | `50` | Checkpoints kept (`0`: all since the last reset) |
| `trace` | `true` | Record every action with the trace; `false`: whole actions only |
| `regions` | | RAM saved in addition: `{ "start", "size", "private", "name" }` |
| `exclude` | | RAM left out, e.g. large buffers: `{ "symbol" }` or `{ "start", "size" }` |
| `stackSymbols`, `csaSymbols` | iLLD/HighTec and TASKING symbols | Linker symbol pairs bounding the stacks and the context save area |
| `maxHits` | `200` | Most breakpoint hits a Step Back inside a run may count |
| `strict` | `false` | Refuse a Step Back that cannot restore everything (peripheral or unknown writes) |
