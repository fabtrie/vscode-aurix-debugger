//! The session operations: one board, served by its worker. Each `///` comment is the
//! documentation of the REST route, the MCP tool, the Rust trait method and the Python client
//! method.
//!
//! The routes are the worker's own. The hub serves them at `/api/v1/sessions/{session}/...`
//! (and `/api/v1/session/...` while exactly one session is open); see [`OpMeta::hub_path`].
//!
//! [`OpMeta::hub_path`]: crate::OpMeta::hub_path

use crate::types::*;

api! {
    trait DebuggerApi; table OPS; dispatch dispatch; scope Session;

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

    /// The call stack of a halted target: each frame's PC, stack pointer, return address,
    /// function and source line, top frame first.
    ///
    /// The same frames VS Code shows: TriCore frames come from the CSA chain, Cortex-M (HSM) ones
    /// from the function prologues; a GTM MCS channel has at most two frames, the SCR one. While
    /// VS Code shows the core at a trace row (instruction trace replay), the frames are the
    /// replay's. Functions and lines need loaded symbols (`load_symbols`). Fails with
    /// `not_halted` while the target runs and with `busy` during a source-level step of VS Code.
    op stack_trace [run_control]: GET "/api/v1/targets/{target}/stack" (StackTraceRequest) -> StackTrace;

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
    /// otherwise. Data linked to the non-cached PFLASH addresses (0xA...) is programmed at the
    /// cached ones (0x8...). UCB data and data outside the flash banks (e.g. linked to RAM) in the
    /// images are skipped with a warning. Only the sectors the images touch are erased. Blocks the debug
    /// server while it runs.
    op flash_write [flash]: POST "/api/v1/flash/write" (FlashWriteRequest) -> CommandOutput;

    /// Compare flash contents with an image file (`verify_image`).
    op flash_verify [flash]: POST "/api/v1/flash/verify" (FlashVerifyRequest) -> CommandOutput;

    /// Read the TC3xx UCBs (user configuration blocks) from the device and decode them.
    ///
    /// Each UCB comes with its confirmation state (ORIG, COPY, installed), its risk class, and its
    /// words and fields: value, text, default (delivery state; Infineon data: the chip's first
    /// readout), whether it differs from the default or between ORIG and COPY, staged changes,
    /// and for protection words the value the DMU installed (`pending_reset` when they differ).
    /// Passwords and HSM keys are masked; words the DMU protects are reported unreadable.
    op ucb_read [ucb]: GET "/api/v1/ucb" (UcbReadRequest) -> UcbData;

    /// Stage UCB changes by field (or discard them). Nothing is written to the device.
    ///
    /// The staged changes are shared with the other clients: the VS Code UCB panel shows them, and
    /// a person writes them from there after the review. Returns the staged UCBs.
    op ucb_stage [ucb]: POST "/api/v1/ucb/stage" (UcbStageRequest) -> UcbData;

    /// Review the staged UCB changes, or the UCB records of image files: the changes by field,
    /// the problems the checks found (blocking ones first), the risk (`reversible`, `password`,
    /// `permanent`) and the confirmation the write needs.
    ///
    /// Writing is not part of this API: UCBs are written only after a person confirmed the
    /// review, in the VS Code UCB panel or the Tcl console (`ucb write`), and only in a session
    /// started with UCB writing enabled.
    op ucb_review [ucb]: POST "/api/v1/ucb/review" (UcbReviewRequest) -> UcbData;

    /// Compare the device's UCBs with the UCB records of image files (e.g. a post-build HEX), field
    /// by field: per UCB whether the files hold it (ORIG, COPY, both, none), how many fields
    /// differ, and per field the device and the file value (passwords only as set/zero). Read-only
    /// UCBs are reported but cannot be applied.
    op ucb_compare [ucb]: POST "/api/v1/ucb/compare" (UcbCompareRequest) -> UcbData;

    /// Arm a miniMCDS trace of one core (TC37x production device; CPU0 so far): the program flow
    /// (optionally only while the PC is inside a function or address range) or every write into an
    /// address range. Stop: `halt` (the last 8 KB up to the core's halt; the debugger stops it),
    /// `full` (the first 8 KB), `manual` (the last 8 KB before `trace_stop`). The trace RAM holds
    /// about 80 µs of unfiltered program flow. Shared with traced DAQ points: fails while they
    /// use it.
    op trace_arm [trace]: POST "/api/v1/trace/arm" (TraceConfigRequest) -> TraceData;

    /// Arm a trace automatically whenever the traced core runs (a run, not a step) and read the
    /// recording when it halts, so every halt comes with the trace up to it (and its
    /// instruction trace and replay). `config` null ends it; the answer is the current setting.
    op trace_auto [trace]: POST "/api/v1/trace/auto" (TraceAutoRequest) -> TraceData;

    /// Check a trace configuration without arming: the resources it takes and the window to
    /// expect, or why it cannot be armed.
    op trace_check [trace]: POST "/api/v1/trace/check" (TraceConfigRequest) -> TraceData;

    /// Whether a trace is armed, by whom, how full the trace RAM is and whether it finished (a
    /// trace stopped on halt finishes when the core halts), and the last recording.
    op trace_status [trace]: GET "/api/v1/trace" (NoArgs) -> TraceData;

    /// Stop the armed trace (if it still runs) and read the trace RAM.
    op trace_stop [trace]: POST "/api/v1/trace/stop" (NoArgs) -> TraceData;

    /// The last recording, decoded: program flow as instructions with function, file and line
    /// (`Columns` names the fields of each `Steps` row; gaps are objects), or data trace writes
    /// with address, value and the variable hit. `max`: the newest rows only.
    op trace_read [trace]: GET "/api/v1/trace/read" (TraceReadRequest) -> TraceData;

    /// The last program recording as instructions, a page at a time: each row (`I`) with its time
    /// (`T`, ns from the first), kind (`K`: `i` instruction, `e` interrupt entry, `g` gap), address
    /// (`A`), mnemonic and operands (`M`, `O`), function and file (`F`, `Fl`: indices into
    /// `Functions`, `Files`), line (`L`), call depth (`D`), branch taken (`B`), the registers it
    /// wrote (`W`: `[name, value or null, "exact" | "mem" | null, known bits]`) and its memory
    /// accesses (`X`). Registers are rebuilt from the state at the halt: `exact`, `mem` (taken from
    /// memory at the halt) or unknown (null). `Replay` summarises how much could be rebuilt.
    op trace_insns [trace]: GET "/api/v1/trace/insns" (TraceInsnsRequest) -> TraceData;

    /// All registers after one row of the instruction trace, each with how sure it is and where
    /// it came from (`Why`), and the call stack at that point.
    op trace_regs [trace]: GET "/api/v1/trace/regs" (TraceRegsRequest) -> TraceData;

    /// The next row of the instruction trace matching `query`: an address (`0x80001234`), a
    /// register value (`d15=0xC`), a register write (`a4 changes`), or text in the instruction,
    /// function or file name. `from`: start after this row; `back`: search backwards.
    op trace_find [trace]: GET "/api/v1/trace/find" (TraceFindRequest) -> TraceData;

    /// Switch reverse debugging on for a TriCore core: before every action (run, step, source
    /// step, register or memory write) the core's registers and the RAM regions are saved, and
    /// `reverse_back` / `reverse_continue` write an earlier state back into the halted core, which
    /// then really runs from there. Regions: the ELF's writable sections, the core's stacks and
    /// context save area, plus `regions`, without `exclude`. While on it owns the miniMCDS (each
    /// action is recorded; manual traces are refused). Not rewound: peripherals, timers, other
    /// cores. The answer has the regions and what one checkpoint costs (`CheckpointMs`).
    op reverse_enable [reverse]: POST "/api/v1/reverse/enable" (ReverseEnableRequest) -> ReverseData;

    /// Switch reverse debugging off (the history is dropped, the trace released).
    op reverse_disable [reverse]: POST "/api/v1/reverse/disable" (NoArgs) -> ReverseData;

    /// Whether reverse debugging is on, for which core, how many checkpoints and the regions.
    op reverse_status [reverse]: GET "/api/v1/reverse" (NoArgs) -> ReverseData;

    /// The checkpoints, oldest first: the action that followed each (`Action`), where it started
    /// (`Pc`, `At`) and ended (`EndPc`), whether it was recorded.
    op reverse_history [reverse]: GET "/api/v1/reverse/history" (NoArgs) -> ReverseData;

    /// Step Back on the hardware: `line` (default) to the start of the previous source line,
    /// stepping backwards over calls; `instruction`; `action` (before the whole last action). A
    /// point inside an action is reached by restoring its checkpoint and running there again;
    /// `Diverged` reports a run again that took another path (it stops on a real state). The
    /// report lists what was restored and what not (`NotUndone`: peripheral registers).
    op reverse_back [reverse]: POST "/api/v1/reverse/back" (ReverseBackRequest) -> ReverseData;

    /// Reverse Continue on the hardware: back to the last breakpoint or data breakpoint hit in the
    /// history, or to its start.
    op reverse_continue [reverse]: POST "/api/v1/reverse/continue" (NoArgs) -> ReverseData;

    /// Go to checkpoint `index` of the history (and to row `row` of the action's recording).
    op reverse_goto [reverse]: POST "/api/v1/reverse/goto" (ReverseGotoRequest) -> ReverseData;

    /// Stage values from the compared files: everything, one UCB, one field or the password.
    /// Nothing is written; the result is the updated comparison (`taken` per field).
    op ucb_apply [ucb]: POST "/api/v1/ucb/apply" (UcbApplyRequest) -> UcbData;

    /// Export a selection of UCBs (whole 512-byte UCBs) to an Intel HEX file: by name, state
    /// (e.g. only `unlocked`), group, or only those changed from the delivery state.
    op ucb_export [ucb]: POST "/api/v1/ucb/export" (UcbExportRequest) -> UcbData;

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

    /// What the loaded symbols say about an address: the function, offset and source line of
    /// code there, and the global or static variable data there lies in.
    ///
    /// Reads no target memory. Addresses are as linked: a variable in core-local RAM linked at
    /// its `0xD000_0000` alias is found by that address, not by a core's global one. Fails with
    /// `not_found` when no symbols are loaded.
    op lookup_address [symbols]: GET "/api/v1/symbols/address" (LookupAddressRequest) -> AddressInfo;

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
    /// Without `frame` it works while the cores run: there is no stack frame, so names are
    /// globals and statics, as in a VS Code live watch. Core-local RAM is the copy of the core an
    /// `@N` names, or of the one core the symbol file is assigned to; `address` is the system-bus
    /// address of that copy. With `frame` (a halted `target`) names are the frame's locals and
    /// parameters first, as in the VS Code Watch view; a value in a register has no `address`.
    ///
    /// Fails with `not_found` for an unknown name, an `@N` that names no TriCore core, a frame
    /// the call stack does not have, or when no symbols are loaded; with `bad_request` for a
    /// variable in core-local RAM that needs an `@N` (the message lists the choices) or an `@N`
    /// that contradicts `target`; with `not_halted` / `busy` for a `frame` of a running or
    /// stepping target; and with `memory` when the target memory cannot be read.
    op evaluate [variables]: GET "/api/v1/expressions" (EvaluateRequest) -> VariableValue;

    /// Write a variable by its expression and read it back.
    ///
    /// Works while the cores run, like `write_memory`; a bit field is merged into its storage
    /// unit. With `frame` it writes a local or parameter; one in a register only in the top frame
    /// (a caller's registers are saved copies). Connected debugger clients (VS Code) are told to
    /// refresh their views. Core-local RAM and the errors are as for `evaluate`.
    op write_variable [variables]: PUT "/api/v1/expressions" (WriteVariableRequest) -> VariableValue;

    /// The locals and parameters of a stack frame of a halted target, with their values.
    ///
    /// The variables the VS Code Variables view shows for the frame. Each item's `expression`
    /// goes to `evaluate`, `write_variable` and `symbol_children` with the same `frame`. A local
    /// whose location the debug information does not give at the frame's PC (optimized out) is
    /// listed with an `error` instead of a value. Fails with `not_halted` / `busy` as
    /// `stack_trace` does and with `not_found` for a frame the call stack does not have; without
    /// symbols for the frame's code the list is empty.
    op list_locals [variables]: GET "/api/v1/targets/{target}/locals" (ListLocalsRequest) -> LocalList;

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

    /// Run the launch steps on this session: what a VS Code launch configuration does.
    ///
    /// In this order, each only when asked for: load the symbols, load the ORTI file, program
    /// the flash (after a halting reset, as the flash needs it), reset, set the breakpoints, run
    /// to `run_to`, halt all cores (`stop_on_entry`). The steps stop at the first failure, which
    /// leaves the session open to inspect.
    ///
    /// Returns once the steps are done or after `wait_s` seconds with phase `starting`;
    /// `wait_session` waits for the rest. The session keeps the arguments for `relaunch`.
    op launch [session]: POST "/api/v1/launch" (LaunchRequest) -> LaunchStatus;

    /// Run the last launch again, e.g. after a rebuild: flash (incremental), reset, breakpoints,
    /// `run_to`.
    op relaunch [session]: POST "/api/v1/relaunch" (RelaunchRequest) -> LaunchStatus;

    /// Wait until the launch steps are done and return how they went.
    op wait_session [session]: GET "/api/v1/launch" (WaitSessionRequest) -> LaunchStatus;

    /// Take the control lease of the session for the calling client.
    ///
    /// While another client holds it, run control (halt, resume, step, reset), register, memory
    /// and variable writes, flashing, launches and Tcl commands fail with `busy`; reading never
    /// waits for it. Nobody holds it unless a client takes it. It expires after `ttl_s` unless
    /// taken again, and is released when its client ends.
    op acquire_control [session]: POST "/api/v1/control" (ControlRequest) -> ControlInfo;

    /// Release the control lease (only its holder can).
    op release_control [session]: DELETE "/api/v1/control" (NoArgs) -> ControlInfo;

    /// End an API client: remove its breakpoints and watchpoints and release its lease.
    ///
    /// The hub calls it when an MCP session ends; a script can call it for itself when done.
    op end_client [session]: POST "/api/v1/clients/end" (ClientRef) -> EndClientResult;
}
