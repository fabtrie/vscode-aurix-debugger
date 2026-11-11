# Changelog

Simple, human-readable release notes for this extension.

## Unreleased
- TC3xx standby controller (SCR, XC800): with `set tc3xx_scr_target 1` in `configCommands` the SCR is its own thread with disassembly, registers, stepping and instruction breakpoints. It is not halted with the TriCores. `@scr` picks it in breakpoint conditions, the Watch and the Live Watch.
- The SCR can be debugged over the miniWiggler (TasServer) too, through the SCR's boot ROM monitor; no setting needed.
- Peripheral View: the SCR's special function registers with their bit fields, paged SFRs included, as the `SCR_*` peripherals of the TC3xx devices (read while the SCR is halted).
- SCR source-level debugging from its SDCC debug file (`.cdb` in `symbolFiles`): source lines, line breakpoints, step over/into/out, global and local variables.
- Data breakpoints: Break on Value Change / Read / Access on variables and watch expressions, and at an address. Globals are watched by name on every TriCore core; core-local RAM as the copy of the chosen core.
- The Live Watch context menu has Break on Value Change / Read / Access too, also while the target runs.
- So does the editor context menu, on the variable under the cursor or the selected expression; locals are looked up in the selected stack frame.
- Breakpoint conditions are evaluated: any condition other than a task filter or `@core` is a C expression the debug server checks on each hit (`count > 100`), resuming the core when it is false. They used to be ignored with a warning.
- Renamed the extension to `fabtrie.aurix-debugger` (AURIX™ Debugger).
- Renamed the IDs: debug type `aurix-tcf` → `aurixdbg`; commands, settings and context keys `aurix-tcf.*` → `aurix-debugger.*`. Existing launch configurations, keybindings and settings need to be updated.
- aurix-dbg is the only debug server and is bundled with the extension; `agentPath` / `aurix-debugger.agentPath` still override it.
- Removed `program`: all ELF files go into `symbolFiles`, the images to flash into `flashFiles` (no longer defaults to the ELF). A launch configuration that still sets `program` is rejected with a hint.
- Removed the Debug Configuration panel (`Open Debug Configuration` command); use a launch configuration.
- Removed the previous debug agent, the AURIX™ Flasher and the launch attributes `agentFlavor`, `targetDevice`, `entryFunction`, `flashDownload`, `ppuFlashHex` and `cdspFlashHex`.
- Flashing: image data outside the flash banks of the cfg (e.g. initialized data a post-build HEX has in LMU RAM) is skipped with a warning instead of failing the whole write with "no flash bank for image data". An image with no data in any flash bank is still an error.
- Faster first debug session after starting VS Code: the XPeripherals view no longer parses the device SVD file itself, which blocked the debugger for 5–8 seconds on TC3xx devices. The SVD file is converted once, in the background, and read back in well under a second.

- iSYSTEM BlueBox (iC5700, iC5000) as a debug probe, straight over USB without winIDEA: `adapter driver bluebox` in `configCommands`. Memory, run control and resets go through the probe; the probe kernel (and the iC5000's FPGA image) is taken from a winIDEA 9.21.417 install (`bluebox winidea <folder>`, `%WINIDEA_DIR%`, the usual install folders or Chocolatey). A lost target connection, an external reset or a BlueBox power cycle is recovered without ending the session.
- Live Watch values on the BlueBox are sampled by the probe itself and only the changes are read, instead of reading every value per refresh.
- Target health in the status bar: no target power, reset (reconnecting), no debug access, no device or probe lost, with the probe's details and Vref in the tooltip. When the debug probe is gone, the debug session ends with "Debug probe lost" instead of retrying for ever.
- Password-protected TC3xx devices (`UCB_DBG`) can be debugged over the miniWiggler: `adapter debug_password <PW0> .. <PW7>`, or `adapter debug_password -env <variable>` to keep the password out of cfg files. It is never logged.
- Boards with an NXP FS26 safety SBC (`fs26_sbc.cfg`): attach modes `passive`, `hot`, `halt`, `reset_halt` and `reset_run` (`set sbc_attach_mode <mode>` before `init`); halts and resets that respect the FS26 watchdog; a halting reset is repeated until it holds.
- `reset_hook <proc>`: a cfg proc handles every reset request (the `reset` command, VS Code's reset buttons, flashing).
- Peripherals can be suspended while the cores are halted (`tc3xx_periph` in the board cfg), as winIDEA's "Suspend peripherals while stopped". They are only written when the module accepts the debugger's write.
- `monitor help` lists every Tcl command by topic, `monitor help <command>` describes one and `monitor help <word>` searches the descriptions. The same text is in the Tcl command reference (`aurix-debugger/server/docs/commands.md`).
- Running GTM MCS channels are no longer shown as paused after a TriCore breakpoint.
- Over the miniWiggler, `adapter speed` now also applies to the device connect, not only afterwards.
- **Breaking:** the TC3xx cfg scripts are split into chip mechanics (`tc3xx_ocds.cfg`, set up with `tc3xx_setup <chip>` instead of `tc3xx_ocds_setup`), the FS26 policy (`fs26_sbc.cfg`) and the board cfg. A board cfg copied from an earlier version needs to be redone; start from `tc37x_ocds_init.cfg`.
- **Breaking:** `tc37x_ocds_init.cfg` is now a generic example for a TC37x board with an FS26 and uses the TC375 Lite board cfg's target (JTAG ID 0x10207083). The GTM MCS channels are off by default (`set tc3xx_mcs_modules {0 1}` turns them on); the peripheral suspend list and the FS26 disarm check are empty, with examples. Keep your application's settings in a cfg of your own that sets them and then loads it.

## 0.1.0 - 2026-01-20
- First public release.
- Core debugging: breakpoints, step/continue, call stack, variables.
- Memory read/write and disassembly view.
- Launch configs, multi-core support, optional auto-start agent.
