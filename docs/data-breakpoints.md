# Data breakpoints

A data breakpoint (watchpoint) halts a core when it writes, reads or accesses a variable or an address. The core halts right after the access, at the next instruction.

## Setting one

| Where | How |
|---|---|
| Variables or Watch view | Right-click → **Break on Value Change** / **Break on Value Read** / **Break on Value Access** |
| Breakpoints view | **+** → data breakpoint at an address (VS Code 1.90 and later) |
| Live Watch view or panel | Right-click a row → the same three entries; works while the target runs |
| Editor | Right-click a variable, or first select an expression such as `g_motor.speed` or `buf[3]`; also in the Command Palette |

Conditions work as for other breakpoints, e.g. `g_mode == 3`; `@N` limits it to core N.

## Which variable is watched

- **Global variables:** by name, on every TriCore core. Resolved again after an ELF rebuild; kept between sessions.
- **Local variables** and what a local pointer points to: by address, on that core only, for the rest of the session.
- **Core-local RAM** (`0xD000_0000`): the copy of the core whose variable you picked, e.g. `CRASH_state@2`.
- **From the editor:**
  - Core halted: the name is looked up in the frame selected in the Call Stack. A local is shown by address, e.g. `0x7003fe20 [write]`.
  - Target running: the name means what it means in the Live Watch.

## Where they show up

Data breakpoints set from the Live Watch, the editor, Claude or a script appear in the Breakpoints view as function breakpoints named `expression [access]`, e.g. `g_count [write]`. Disable or remove them there. They end with the debug session.

## Limits

- Each core has 8 hardware events, shared with code breakpoints. A data breakpoint on more than one byte takes two. When none are left, the breakpoint is shown unverified with the reason.
- Caught: an access that starts inside the variable. Not caught: one that starts below it and reaches into it, e.g. a 64-bit copy of the surrounding struct.
- The core halts on every hit, also when the condition is false: a data breakpoint on a busy variable slows the target down.
