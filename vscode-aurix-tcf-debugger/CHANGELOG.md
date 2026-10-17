# Changelog

Simple, human-readable release notes for this extension.

## Unreleased
- Renamed the extension to `fabtrie.aurix-debugger` (AURIX™ Debugger).
- Renamed the IDs: debug type `aurix-tcf` → `aurixdbg`; commands, settings and context keys `aurix-tcf.*` → `aurix-debugger.*`. Existing launch configurations, keybindings and settings need to be updated.
- aurix-dbg is the only debug server and is bundled with the extension; `agentPath` / `aurix-debugger.agentPath` still override it.
- Removed `program`: all ELF files go into `symbolFiles`, the images to flash into `flashFiles` (no longer defaults to the ELF). A launch configuration that still sets `program` is rejected with a hint.
- Removed the Debug Configuration panel (`Open Debug Configuration` command); use a launch configuration.
- Removed the previous debug agent, the AURIX™ Flasher and the launch attributes `agentFlavor`, `targetDevice`, `entryFunction`, `flashDownload`, `ppuFlashHex` and `cdspFlashHex`.
- Faster first debug session after starting VS Code: the XPeripherals view no longer parses the device SVD file itself, which blocked the debugger for 5–8 seconds on TC3xx devices. The SVD file is converted once, in the background, and read back in well under a second.

## 0.1.0 - 2026-01-20
- First public release.
- Core debugging: breakpoints, step/continue, call stack, variables.
- Memory read/write and disassembly view.
- Launch configs, multi-core support, optional auto-start agent.
