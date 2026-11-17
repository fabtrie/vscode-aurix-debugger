# Change Log

All notable changes to the "AURIX™ Debugger" extension will be documented in this file.

## Unreleased

### Added

- on-chip trace on TC37x (miniMCDS) over the miniWiggler: Trace panel for program flow and data trace, Live Watch "Trace every write", traced lanes in the OS Profile
- OS Profile: a sampling rate per lane
- instruction trace: every executed instruction with source lines and the registers rebuilt from the halt; Step Back and Reverse Continue replay the trace in the debugger
- trace option "Arm on every run, stop on halt" (launch configuration `"trace": {"Auto": true}`)

### Fixed

- Continue from a breakpoint through the server's resume stopped at the same breakpoint again before executing anything; the debugger now steps past it first

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
