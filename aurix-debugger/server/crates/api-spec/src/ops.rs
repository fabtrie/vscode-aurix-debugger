//! The operations. Each `///` comment is the documentation of the REST route, the MCP tool, the
//! Rust trait method and the Python client method.

use crate::types::*;

api! {
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

    /// List the watchpoints (data breakpoints) of all targets.
    op list_watchpoints [watchpoints]: GET "/api/v1/watchpoints" (NoArgs) -> WatchpointList;

    /// Set a watchpoint: the target halts when it accesses the address.
    op add_watchpoint [watchpoints]: POST "/api/v1/watchpoints" (AddWatchpointRequest) -> WatchpointInfo;

    /// Remove a watchpoint. Returns the remaining watchpoints.
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
    /// started afterwards replaces what was loaded here, and the other way round.
    op load_symbols [symbols]: POST "/api/v1/symbols" (LoadSymbolsRequest) -> SymbolsInfo;

    /// The symbol files loaded per target (by this API or by a VS Code debug session).
    op symbols [symbols]: GET "/api/v1/symbols" (NoArgs) -> SymbolsInfo;

    /// Run a command of the OpenOCD-compatible Tcl console (like the telnet console).
    op tcl [tcl]: POST "/api/v1/tcl" (TclRequest) -> CommandOutput;
}
