//! The operations. Each `///` comment is the documentation of the REST route, the MCP tool, the
//! Rust trait method and the Python client method.

use crate::types::*;

api! {
    /// What is serving: server version and API revision, process, probe connection, targets and
    /// loaded symbol files.
    ///
    /// Always answers at once, whatever the state - no adapter, no examined target, no symbols, or
    /// a core busy flashing: it reads what the core publishes and never queues a job on it. That
    /// makes it the liveness probe; `core_lag_ms` says whether other operations would wait.
    op info [server]: GET "/api/v1/info" (NoArgs) -> ServerInfo;

    /// List the targets (cores) with their run state.
    op targets [targets]: GET "/api/v1/targets" (NoArgs) -> TargetList;

    /// Get the run state of a target (and its PC when halted).
    op state [targets]: GET "/api/v1/targets/{target}" (TargetRef) -> TargetInfo;

    /// Halt a target. Returns once it is halted (up to one second).
    ///
    /// Does nothing if it is already halted. A source-level step from VS Code running on it is
    /// cancelled, like the VS Code pause button does.
    op halt [run_control]: POST "/api/v1/targets/{target}/halt" (TargetRef) -> TargetInfo;

    /// Resume a halted target, optionally at another address.
    ///
    /// Fails with `busy` while VS Code does a source-level step on it.
    op resume [run_control]: POST "/api/v1/targets/{target}/resume" (ResumeRequest) -> TargetInfo;

    /// Execute one instruction on a halted target.
    ///
    /// Fails with `busy` while VS Code does a source-level step on it.
    op step [run_control]: POST "/api/v1/targets/{target}/step" (TargetRef) -> TargetInfo;

    /// Wait until a target is halted (or running), e.g. after `resume` until a breakpoint hits.
    ///
    /// Fails with `timeout` if the state is not reached in time. Other clients are not blocked
    /// while waiting.
    op wait_state [run_control]: GET "/api/v1/targets/{target}/wait" (WaitRequest) -> TargetInfo;

    /// Reset the device through the debug adapter and run the reset handlers of the
    /// configuration (OpenOCD `reset run|halt|init`).
    ///
    /// Flash programming needs a preceding halting reset. Fails with `busy` while VS Code does
    /// a source-level step.
    op reset [run_control]: POST "/api/v1/reset" (ResetRequest) -> TargetList;

    /// Read the registers of a target (the core registers need a halted target).
    op read_registers [registers]: GET "/api/v1/targets/{target}/registers" (ReadRegistersRequest) -> RegisterList;

    /// Read one register.
    op read_register [registers]: GET "/api/v1/targets/{target}/registers/{name}" (RegisterRef) -> RegisterValue;

    /// Write one register. Core registers need a halted target.
    ///
    /// Connected debugger clients (VS Code) are told to refresh their views.
    op write_register [registers]: PUT "/api/v1/targets/{target}/registers/{name}" (WriteRegisterRequest) -> RegisterValue;

    /// Read memory (at most 65536 bytes per call).
    ///
    /// Reads through the system bus, or with `target` as that core sees memory (HSM-private
    /// memory, MCS program RAM). Some areas are protected at runtime (e.g. by the application's
    /// memory protection) and fail with `memory`.
    op read_memory [memory]: GET "/api/v1/memory" (ReadMemoryRequest) -> MemoryData;

    /// Write memory (at most 65536 bytes per call). PFLASH is written with `flash_write`.
    ///
    /// Connected debugger clients (VS Code) are told to refresh their views.
    op write_memory [memory]: PUT "/api/v1/memory" (WriteMemoryRequest) -> MemoryWritten;

    /// List the breakpoints of all clients (this API and VS Code).
    op list_breakpoints [breakpoints]: GET "/api/v1/breakpoints" (NoArgs) -> BreakpointList;

    /// Set a hardware breakpoint at an address, function or source line.
    ///
    /// The breakpoint is shared with the connected debugger clients: VS Code reports its hits.
    /// A function or source line breakpoint needs the symbols a VS Code debug session loaded;
    /// before that it is pending (see `error`) and is planted when they are loaded.
    op add_breakpoint [breakpoints]: POST "/api/v1/breakpoints" (AddBreakpointRequest) -> BreakpointInfo;

    /// Remove a breakpoint (also one set in VS Code). Returns the remaining breakpoints.
    op remove_breakpoint [breakpoints]: DELETE "/api/v1/breakpoints/{id}" (BreakpointRef) -> BreakpointList;

    /// List the watchpoints (data breakpoints) of all clients: the API's, VS Code's and those of
    /// the Tcl console.
    op list_watchpoints [watchpoints]: GET "/api/v1/watchpoints" (NoArgs) -> WatchpointList;

    /// Set a watchpoint on an address or a variable (`expression`): the target halts after an
    /// access to it, with reason `watchpoint` and the PC at the instruction after the access.
    ///
    /// A watchpoint wider than one byte takes two of the 8 hardware events of a core (shared with
    /// breakpoints) and catches every access that starts inside it; an access that starts below
    /// it and reaches into it is not caught. It shows up in VS Code's Breakpoints view.
    op add_watchpoint [watchpoints]: POST "/api/v1/watchpoints" (AddWatchpointRequest) -> WatchpointInfo;

    /// Remove a watchpoint (also one set in VS Code or the Tcl console). Returns the remaining
    /// watchpoints.
    op remove_watchpoint [watchpoints]: DELETE "/api/v1/watchpoints" (WatchpointRef) -> WatchpointList;

    /// Program image files into flash (`flash write_images`).
    ///
    /// Needs a preceding halting reset (`reset` with mode `halt`): the flash is protected
    /// otherwise. UCB data in the images is skipped. Only the sectors the images touch are
    /// erased. Blocks the debug server while it runs.
    op flash_write [flash]: POST "/api/v1/flash/write" (FlashWriteRequest) -> CommandOutput;

    /// Compare flash contents with an image file (`verify_image`).
    op flash_verify [flash]: POST "/api/v1/flash/verify" (FlashVerifyRequest) -> CommandOutput;

    /// Load symbol files (ELF) for the targets, as a VS Code debug session does.
    ///
    /// Makes `add_breakpoint` by `symbol` or `file` with `line` and the `expression` points of a
    /// DAQ session work without VS Code. Breakpoints that were pending are planted.
    ///
    /// There is one symbol set per core, and the last writer wins: a VS Code debug session
    /// started afterwards replaces what was loaded here, and the other way round. `path@N`
    /// assigns a file to the cores that run it, for its variables in core-local RAM.
    op load_symbols [symbols]: POST "/api/v1/symbols" (LoadSymbolsRequest) -> SymbolsInfo;

    /// The symbol files loaded per target (by this API or by a VS Code debug session).
    op symbols [symbols]: GET "/api/v1/symbols" (NoArgs) -> SymbolsInfo;

    /// List the global and static variables of the loaded symbols, filtered by a name pattern, one
    /// page at a time.
    ///
    /// The search runs on the server and stays fast for an ELF with 10^5 globals. Each item says
    /// whether a DAQ point can sample it and gives the expression to sample it by. A variable in
    /// core-local RAM whose file is not assigned to exactly one core is a `per_core` group: its
    /// children are the copies of the cores, `CRASH_state@0`, `CRASH_state@1`, ... Fails with
    /// `not_found` when no symbols are loaded (`load_symbols`).
    op list_globals [variables]: GET "/api/v1/globals" (ListGlobalsRequest) -> GlobalList;

    /// List the members of a struct or union, or the elements of an array, that an expression
    /// stands for, each with the expression that names it.
    ///
    /// Reads no memory unless the expression itself needs to (`p->s`, `buf[i]`). Pointers are not
    /// followed: evaluate `*p` or `p->x` instead. Arrays are paged with `offset` and `limit`. The
    /// children of a `per_core` group are its copies, one per core, each named with its `@N`.
    op symbol_children [variables]: GET "/api/v1/expressions/children" (SymbolChildrenRequest) -> SymbolChildren;

    /// Evaluate a C expression and read its value, decoded as a DAQ point decodes it.
    ///
    /// Works while the cores run: there is no stack frame, so names are globals and statics, as in
    /// a VS Code live watch. Core-local RAM is the copy of the core an `@N` names, or of the one
    /// core the symbol file is assigned to; `address` is the system-bus address of that copy.
    ///
    /// Fails with `not_found` for an unknown name, an `@N` that names no TriCore core, or when no
    /// symbols are loaded; with `bad_request` for a variable in core-local RAM that needs an
    /// `@N` (the message lists the choices) or an `@N` that contradicts `target`; and with
    /// `memory` when the target memory cannot be read.
    op evaluate [variables]: GET "/api/v1/expressions" (EvaluateRequest) -> VariableValue;

    /// Write a variable by its expression and read it back.
    ///
    /// Works while the cores run, like `write_memory`; a bit field is merged into its storage
    /// unit. Connected debugger clients (VS Code) are told to refresh their views. Core-local RAM
    /// and the errors are as for `evaluate`.
    op write_variable [variables]: PUT "/api/v1/expressions" (WriteVariableRequest) -> VariableValue;

    /// Load an ORTI file: the AUTOSAR/OSEK OS awareness description the OS generator emits.
    ///
    /// Every attribute is parsed as a C expression and resolved against the loaded symbols, so
    /// `load_symbols` (or a VS Code debug session) has to have run first. Attributes that cannot
    /// be resolved are reported in `errors` and retried when they are first read, so loading
    /// before the board is powered is harmless.
    op load_orti [os]: POST "/api/v1/os/orti" (LoadOrtiRequest) -> OsInfo;

    /// The loaded ORTI file: its object types, their attributes and how many cores it describes.
    ///
    /// Object types are whatever the file declared. A MICROSAR file adds `vs_ISR`, `vs_SPINLOCK`,
    /// `vs_EVENT` and `vs_OS_CONFIG` to the standard OSEK ones.
    op os [os]: GET "/api/v1/os" (NoArgs) -> OsInfo;

    /// Read the objects of one ORTI type with their current attribute values.
    ///
    /// Values come from the target as it is now. Attributes that are constants (a stack address,
    /// a `sizeof`) or literal text (an alarm action) cost no bus access.
    op os_objects [os]: GET "/api/v1/os/objects/{kind}" (OsObjectsRequest) -> OsObjectList;

    /// Measure stack coverage: how much of each OS stack has ever been used.
    ///
    /// The OS fills every stack with a pattern at startup, so the high-water mark is the far end
    /// of the region that still holds it. Finding it means reading the untouched part of every
    /// stack, so this is on demand and never part of a refresh - prefer `names` to narrow it.
    ///
    /// A stack the debugger cannot read (a region the memory protection hides) is reported with
    /// its `error` rather than as unused.
    op os_stacks [os]: GET "/api/v1/os/stacks" (OsStacksRequest) -> OsStackList;

    /// Run a command of the OpenOCD-compatible Tcl console (like the telnet console).
    op tcl [tcl]: POST "/api/v1/tcl" (TclRequest) -> CommandOutput;
}
