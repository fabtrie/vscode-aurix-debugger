# Change Log

All notable changes to the "AURIX™ Debugger" extension will be documented in this file.

## 0.8.0

### Added

- Trace panel: a Timeline of the calls in a program recording: the call depth as rows over time, with overview, zoom, pan, measuring and find, and a side panel with each call's path, times, source, instructions and Show in debugger
- trace option "with the task / ISR (ORTI)": the OS variables that say which task or ISR runs are recorded together with the program flow, so the Timeline shows every call in its task or ISR, on one timeline or with a lane per task and ISR
- MCP and REST: `trace_calls`, the calls of the last program recording with their call depth, times and task or ISR; `trace_arm` `context` for the task / ISR recording; Tcl `mcds arm -program -data <addr> <len>` records a data range together with the program flow

### Changed

- Trace panel: the tabs (Timeline, List, Instructions), the shown tab's tools and the recording controls share one header row (two on a narrow panel); the configuration is a short summary that opens as a popover, and one button arms or stops and reads

### Fixed

- a stop-on-halt recording timed the code before the halt by when the trace was read, so a recording of microseconds could span seconds
- the debug server stopped when an update failed to restart it, leaving no server; an update installed from inside an app container (such as the Claude desktop app's shell) broke the installation and is now refused

## 0.7.0

### Added

- MCP and REST: `list_globals` `search` for search boxes: globals and struct members by any part of their name, by the beginnings of their words (`dcs` finds `DriveCounterStamp`), by source file or by address, best matches first; members and elements name their `parent`, and the ancestors of a match come along as context, so the results can be shown as a tree
- MCP and REST: a `list_globals` `query` with `.` or `[` finds struct members and array elements by their path (`g_motor.spe*`, `g_buf[3]`)

### Changed

- Add Live Watch Expression: the debug server searches the variables and struct members as you type and sends only the best matches, so the picker no longer lags in large programs; array elements are found by their path (`g_buf[3]`)

## 0.6.0

### Added

- MCP and REST: call stack, locals and parameters of a frame, the source location of the PC, address lookup, and peripheral registers by name from the SVD description (read, write fields by value name)
- MCP `reference`: the cfg guide and the Tcl command reference, without an open session
- attach modes for every board (`"configCommands": ["attach_mode halt", "init"]`; also passive, hot, reset_halt, reset_run)
- trace option "and on steps": Step Over/Into/Out and instruction steps are recorded too, joined to the earlier trace, so Step Back works after a step
- reverse debugging: the instruction trace shows the whole history, action by action ("Whole history" in the Trace panel), and the History tab shows what each checkpoint cost

### Changed

- **Breaking:** one generic `tc3xx_ocds.cfg` for every TC3xx board, with two examples (`tc375_lite_example.cfg` without an SBC, `tc37x_fs26_example.cfg` with an FS26) in place of `tc37x_ocds_init.cfg`; the TL1 multi-core sync, peripheral suspend, HSM start and attach moved into the debug server (`ocds sync`, `suspend add`, `hsm autostart`, `attach_mode`). Copied board cfgs need to be redone
- **Breaking:** the HSM is a core to debug only with `tc3xx_hsm_target 1`; an HSM that boots is released after a reset without it
- flashing goes through a RAM flash loader on cpu0: much faster, above all over the BlueBox
- faster halts and steps: the Variables, Live Watch and Registers views read in batches, an instruction step over the BlueBox takes three probe messages instead of about a dozen, and reverse debugging needs far fewer probe accesses per action
- peripheral suspend is written as soon as a module can take it, also while the application runs, instead of at the next halt
- the Peripheral view gets its definitions from the debug server; the extension no longer unpacks the SVD files into a 100+ MB cache folder

### Fixed

- a reset during a long Tcl command or wait was answered only after it, so the application started without seeing the debugger
- a failed reset made the next one skip the reset and halt in the application
- a halting reset over TAS that lost its halt now resets again (at most twice)
- after a halting reset the bootloader could hang waiting for the HSM
- halted TL1 cores were shown running in VS Code after a Step Back
- the trace set to arm on every run by the launch configuration showed as off in the Trace panel, and missed the run to the first breakpoint
- the FS26 watchdog disarm no longer depends on application variables of the running build
- reading SCR or HSM peripherals without their target read unrelated system bus addresses; refused ENDINIT-protected writes now say why

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
