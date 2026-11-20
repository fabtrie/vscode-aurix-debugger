# Standby controller (SCR) and GTM MCS channels

The debugger can also debug the TC3xx standby controller and the channels of the GTM's MCS modules. Both are off by default.

## Standby controller (SCR)

XC800 (8051) core. Works over the miniWiggler and the iSYSTEM BlueBox. Turn it on in the launch configuration:

```json
"configCommands": ["set tc3xx_scr_target 1"]
```

- Own thread in the Call Stack (`tc37x.scr`), with disassembly, registers, stepping and instruction breakpoints.
- Not halted with the TriCores: TriCore breakpoints and the toolbar's pause leave it running; an SCR breakpoint stops only the SCR. Pause it from its own thread.
- Code addresses `0x0000`..`0xFFFF` are the SCR's own code space. **Go to Address or Symbol** and instruction breakpoints use the address space of the core whose disassembly is shown.
- `x@scr` in the Watch or Live Watch reads `x` on the SCR; `@scr` in a breakpoint condition halts only the SCR.
- Special function registers: the `SCR_*` peripherals in the Peripheral view, readable while the SCR is halted. Paged registers show every page.

### Source-level debugging

Add the SCR's SDCC debug file (`.cdb`) to `symbolFiles`, with the `.ihx` file of the same build beside it:

```json
"symbolFiles": ["${workspaceFolder}/build/app.elf", "${workspaceFolder}/scr/gen/scr_main.cdb"]
```

- Sources are looked up next to the `.cdb` file and in the `src`, `inc` and `api` folders beside its folder.
- Supported: source lines, line breakpoints (two), step over/into/out, global variables by name, locals of the current function.
- Locals may show a wrong value before their first assignment or after their last use (SDCC does not record where a local is valid). Locals the compiler removed show as optimized out.
- TriCore ELF files are never used for the SCR.

### Requirements and limits

- The application must start the SCR in its OCDS boot mode (`PMSWCR4.SCRCFG = 0x0A`). Until then the SCR thread stays unexamined.
- BlueBox: the SCR needs the device's debug mode DAP Standard. With the SCR on, the BlueBox uses it for the whole session, which limits the DAP clock to 10 MHz.
- miniWiggler: after a chip reset the SCR takes no halt request until the debugger has reset it once. The first halt or breakpoint restarts the SCR program at `0x0000` (a warning says so).
- Breakpoints can be set while the SCR runs; it is halted briefly to set them.

## GTM MCS channels

Name the MCS modules your application uses:

```json
"configCommands": ["set tc3xx_mcs_modules {0 1}"]
```

- Each module gives eight threads, `tc37x.mcs<m>.ch0` .. `ch7`. A channel appears in the Call Stack once the application has enabled it.
- A channel halts only itself: TriCore breakpoints and the pause button leave it running; an MCS breakpoint stops only that channel.
- Source-level debugging: add the MCS assembler sources (`.asm`) to `symbolFiles`.
- Each module adds eight cores to poll: enable only the modules you debug.

Both together: `"configCommands": ["set tc3xx_mcs_modules {0 1}", "set tc3xx_scr_target 1"]`.
