# Change Log

All notable changes to the "AURIX™ Debugger" extension will be documented in this file.

## 0.5.0

### Added

- on-chip trace on TC37x (miniMCDS) over the miniWiggler: Trace panel for program flow and data trace, Live Watch "Trace every write", traced lanes in the OS Profile
- instruction trace: every executed instruction with its source line and the registers rebuilt from the halt; Step Back and Reverse Continue replay it (view only)
- reverse debugging on the hardware (launch configuration `"reverse": true`): Step Back and Reverse Continue restore the core's registers and RAM, so the code really runs again from there
- trace option "Arm on every run, stop on halt"
- OS Profile: a sampling rate per lane

### Fixed

- editing a global in LMU/DLMU RAM could reset the board (the debugger wrote through the cached alias `0x9…`)
- after a reset with the cores halted, VS Code kept showing the old stop instead of the reset vector
- boards without the FS26 reset line wired were taken for wired ones when miniWiggler reads failed while the HSM ran
- resume over REST/MCP from a breakpoint stopped at the same breakpoint again
- a debug session that ends unexpectedly now says why (session log and hub)

## 0.4.0

### Added

- debugger hub: one always-on process shares the debug sessions between VS Code, Claude (MCP) and scripts (REST, Python)
- TC3xx UCBs: view, compare with HEX, edit, export and guarded write
- iSYSTEM BlueBox (iC5700, iC5000) over USB without winIDEA
- debugging of the TC3xx standby controller (SCR), with source-level debugging from SDCC files
- target health in the status bar
- password-protected TC3xx devices over the miniWiggler
- boards with an NXP FS26 safety SBC
- C++ variables by their qualified name in the Live Watch

### Changed

- **Breaking:** launch configurations connect through the hub; `autoStartAgent`, `agentPath`, `host` and `port` are removed
- **Breaking:** the TC3xx cfg scripts are split into chip, FS26 and board cfgs; copied board cfgs need to be redone

### Fixed

- flashing of projects linked to non-cached PFLASH addresses
- PC marker in the disassembly
- source breakpoints that could not be removed

## 0.3.0

### Added

- data breakpoints (break on value change, read, access)
- breakpoint conditions
- start the local TasServer when none is running

## 0.2.0

### Added

- Motorola S-record flashing
- offset and type per flash file

### Fixed

- Continue button while another core is halted

## 0.1.1

### Added

- variables by name over REST and MCP

### Fixed

- core-local variables

## 0.1.0

- Initial release, based on [vscode-aurix-tcf-debugger](https://github.com/ZeZhan/vscode-aurix-tcf-debugger) by ZeZhan
