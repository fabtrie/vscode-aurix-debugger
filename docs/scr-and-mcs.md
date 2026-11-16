# Standby controller (SCR) and GTM MCS channels

Besides the TriCore cores and the HSM, the debugger can debug the TC3xx standby controller and the channels of the GTM's MCS modules. Both are off by default.

## Standby controller (SCR)

The SCR is an XC800 (8051) core. It can be debugged over the miniWiggler and over the iSYSTEM BlueBox. Turn it on in the launch configuration:

```json
"configCommands": ["set tc3xx_scr_target 1"]
```

- The SCR shows in the Call Stack as its own thread (`tc37x.scr`), with disassembly, registers, stepping and instruction breakpoints.
- It is not halted together with the TriCores: a TriCore breakpoint or the toolbar's pause leaves it running, and an SCR breakpoint stops only the SCR. Pause it from its own thread.
- Its code addresses `0x0000`..`0xFFFF` are the SCR's own code space. **Go to Address or Symbol** and instruction breakpoints take the address on the core whose disassembly is shown.
- `x@scr` in the Watch or Live Watch reads `x` on the SCR; `@scr` in a breakpoint condition halts only the SCR.
- The SCR's special function registers are in the Peripheral view as the `SCR_*` peripherals, readable while the SCR is halted. Paged registers show every page.

### Source-level debugging

Add the SCR's SDCC debug file to `symbolFiles`, with the `.ihx` file of the same build beside it:

```json
"symbolFiles": ["${workspaceFolder}/build/app.elf", "${workspaceFolder}/scr/gen/scr_main.cdb"]
```

The sources are looked up next to the `.cdb` file and in the `src`, `inc` and `api` folders beside its folder. You then get source lines, line breakpoints (two), step over, into and out, global variables by name and the local variables of the current function.

SDCC does not record where in a function a local variable is valid: before its first assignment or after its last use, its register may hold something else. Locals the compiler removed show as optimized out. TriCore ELF files are never used for the SCR.

### Things to know

- The application must start the SCR in its OCDS boot mode (`PMSWCR4.SCRCFG = 0x0A`). Until it does, the SCR thread stays unexamined.
- Over the BlueBox, the SCR needs the device's debug mode DAP Standard. With the SCR on, the BlueBox uses it for the whole session, which limits the DAP clock to 10 MHz.
- Over the miniWiggler, after a chip reset the SCR takes no halt request until the debugger has reset it once: the first halt or breakpoint restarts the SCR program at `0x0000` (a warning says so). Breakpoints can be set while the SCR runs; it is halted for a moment to set them.

## GTM MCS channels

Name the MCS modules your application uses:

```json
"configCommands": ["set tc3xx_mcs_modules {0 1}"]
```

- Each module gives eight threads, `tc37x.mcs<m>.ch0` .. `ch7`. A channel shows in the Call Stack once the application has enabled it.
- A channel halts only itself: TriCore breakpoints and the pause button leave it running, and an MCS breakpoint stops only that channel.
- For source-level debugging, add the MCS assembler sources (`.asm`) to `symbolFiles`.
- Every module adds eight cores the debugger has to poll, so enable only the modules you debug.

Both can be combined: `"configCommands": ["set tc3xx_mcs_modules {0 1}", "set tc3xx_scr_target 1"]`.
