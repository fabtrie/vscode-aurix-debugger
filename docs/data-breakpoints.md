# Data breakpoints

A data breakpoint (watchpoint) halts a core when it writes, reads or accesses a variable or an address. The core halts right after the access, at the next instruction.

## Setting one

| Where | How |
|---|---|
| Variables or Watch view | Right-click → **Break on Value Change** / **Break on Value Read** / **Break on Value Access** |
| Breakpoints view | **+** → data breakpoint at an address (VS Code 1.90 and later) |
| Live Watch view or panel | Right-click a row → the same three entries; works while the target runs |
| Editor | Right-click a variable, or select an expression such as `g_motor.speed` or `buf[3]` first; also in the Command Palette |

A condition works as for other breakpoints, e.g. `g_mode == 3`, and `@N` limits it to core N.

## Which variable is watched

- **Global variables** are watched by name on every TriCore core, so a write from any core is caught. They are resolved again when the ELF is rebuilt, and VS Code keeps them between sessions.
- **Local variables**, and what a local pointer points to, are watched at their address, on that core only, for the rest of the session.
- **Core-local RAM** (`0xD000_0000`) means the copy of the core whose variable you picked, e.g. `CRASH_state@2`.
- From the **editor**, while a core is halted, the name is looked up in the frame selected in the Call Stack; a local is shown by its address, e.g. `0x7003fe20 [write]`. While the target runs, the name means what it means in the Live Watch.

## How they show up

VS Code offers extensions no way to add data breakpoints to its Breakpoints view. Those set from the Live Watch, the editor, Claude or a script therefore show up as function breakpoints named `expression [access]`, e.g. `g_count [write]`. You can disable or remove them there; they end with the debug session.

## Limits

- Each core has 8 hardware events, shared with code breakpoints. A data breakpoint on more than one byte takes two. When none are left, the breakpoint is shown unverified with the reason.
- An access that starts inside the variable is caught. One that starts below it and reaches into it is not, e.g. a 64-bit copy of the struct around it.
- The core halts on every hit, also when the condition is false; a data breakpoint on a busy variable slows the target down.
