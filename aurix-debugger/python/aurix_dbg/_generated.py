# Generated from the aurix-dbg API definition (server/crates/api-spec) by `cargo build`. Do not edit.
"""Generated part of the aurix-dbg client: response types and API methods.

Control and inspect AURIX targets through the aurix-dbg debug server: run control, registers, memory, breakpoints, watchpoints, flash programming and the OpenOCD-compatible Tcl console.

The API shares its session with the other frontends: actions taken here are reflected in a connected VS Code debug session (TCF) and the other way round, e.g. a breakpoint set here shows up there and a halt from VS Code is seen in `wait_state` and the event stream.

Targets are named as in the OpenOCD configuration (e.g. `tc37x.cpu0`); `targets` lists them. Addresses accept numbers or strings in decimal or `0x` hex; memory contents are hex strings.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Literal
from urllib.parse import quote

API_VERSION = "0.1.0"
MAX_TRANSFER = 65536
"""Largest memory transfer of one read_memory / write_memory call in bytes."""


def _obj(cls, v):
    return None if v is None else cls.from_dict(v)


def _list(cls, v):
    return None if v is None else [cls.from_dict(x) for x in v]


def _args(d):
    return {k: v for k, v in d.items() if v is not None}


BreakpointOwner = Literal["api", "client"]
"""Who set a breakpoint."""

ErrorCode = Literal["bad_request", "not_found", "not_halted", "not_examined", "busy", "timeout", "memory", "adapter", "device_locked", "device_access", "resource", "failed"]
"""Error kind."""

RegisterGroup = Literal["core", "debug"]
"""Register group."""

ResetMode = Literal["run", "halt", "init"]
"""Reset mode."""

RunState = Literal["unknown", "running", "halted", "debug_running", "reset"]
"""Run state of a target."""

WaitFor = Literal["halted", "running"]
"""State to wait for."""

WatchAccess = Literal["read", "write", "access"]
"""Access that triggers a watchpoint."""


@dataclass
class ApiError:
    """Error of an operation (HTTP error responses, MCP tool errors)."""

    code: ErrorCode
    """What went wrong, for programs."""
    message: str
    """What went wrong, for people."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ApiError:
        return cls(
            code=d.get("code"),
            message=d.get("message"),
        )


@dataclass
class BreakpointInfo:
    """A breakpoint and its state."""

    enabled: bool
    hits: int
    """Number of hits reported to debugger clients."""
    id: str
    """Breakpoint ID."""
    instances: list[BreakpointInstance]
    """Where it is planted."""
    owner: BreakpointOwner
    address: int | None = None
    """Requested code address."""
    error: str | None = None
    """Why it is not planted (e.g. symbols not loaded yet: it is planted once they are)."""
    file: str | None = None
    """Requested source file."""
    line: int | None = None
    """Source line (the actual line with code, for a file/line breakpoint)."""
    symbol: str | None = None
    """Requested function name."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> BreakpointInfo:
        return cls(
            enabled=d.get("enabled"),
            hits=d.get("hits"),
            id=d.get("id"),
            instances=_list(BreakpointInstance, d.get("instances")),
            owner=d.get("owner"),
            address=d.get("address"),
            error=d.get("error"),
            file=d.get("file"),
            line=d.get("line"),
            symbol=d.get("symbol"),
        )


@dataclass
class BreakpointInstance:
    """Where a breakpoint is planted on one target."""

    address: int
    target: str
    """Target name."""
    error: str | None = None
    """Why it could not be planted (e.g. no free hardware breakpoint)."""
    file: str | None = None
    """Source file of the address, when the symbols are loaded (by a VS Code debug session)."""
    line: int | None = None
    """Source line of the address in `file`."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> BreakpointInstance:
        return cls(
            address=d.get("address"),
            target=d.get("target"),
            error=d.get("error"),
            file=d.get("file"),
            line=d.get("line"),
        )


@dataclass
class BreakpointList:
    """The breakpoints of all clients."""

    breakpoints: list[BreakpointInfo]

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> BreakpointList:
        return cls(
            breakpoints=_list(BreakpointInfo, d.get("breakpoints")),
        )


@dataclass
class CommandOutput:
    """Output of a command."""

    output: str
    """Log output and result."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> CommandOutput:
        return cls(
            output=d.get("output"),
        )


@dataclass
class MemoryData:
    """Memory contents."""

    address: int
    """Start address."""
    data: str
    """The bytes as hex string."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> MemoryData:
        return cls(
            address=d.get("address"),
            data=d.get("data"),
        )


@dataclass
class MemoryWritten:
    """A written memory range."""

    address: int
    """Start address."""
    length: int
    """Number of bytes written."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> MemoryWritten:
        return cls(
            address=d.get("address"),
            length=d.get("length"),
        )


@dataclass
class RegisterList:
    """Register values of a target."""

    registers: list[RegisterValue]
    target: str
    """Target name."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> RegisterList:
        return cls(
            registers=_list(RegisterValue, d.get("registers")),
            target=d.get("target"),
        )


@dataclass
class RegisterValue:
    """A register value."""

    description: str
    group: RegisterGroup
    name: str
    """Register name."""
    value: int

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> RegisterValue:
        return cls(
            description=d.get("description"),
            group=d.get("group"),
            name=d.get("name"),
            value=d.get("value"),
        )


@dataclass
class TargetInfo:
    """A target (core) and its state."""

    arch: str
    """Core architecture (`tricore`, `cortex_m`, `gtm_mcs`)."""
    examined: bool
    """The target has been examined (is accessible and under debug control)."""
    index: int
    """Index of the target (usable instead of the name)."""
    name: str
    """Target name (e.g. `tc37x.cpu0`)."""
    reachable: bool
    """False while the target has not been accessible for a while (e.g. not powered)."""
    reason: str
    """Why the target halted (`breakpoint`, `debug-request`, `single-step`, `reset`, ...),
    `not halted` while running."""
    state: RunState
    pc: int | None = None
    """Program counter, when halted."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> TargetInfo:
        return cls(
            arch=d.get("arch"),
            examined=d.get("examined"),
            index=d.get("index"),
            name=d.get("name"),
            reachable=d.get("reachable"),
            reason=d.get("reason"),
            state=d.get("state"),
            pc=d.get("pc"),
        )


@dataclass
class TargetList:
    """The targets."""

    targets: list[TargetInfo]

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> TargetList:
        return cls(
            targets=_list(TargetInfo, d.get("targets")),
        )


@dataclass
class WatchpointInfo:
    """A watchpoint."""

    access: WatchAccess
    address: int
    id: int
    length: int
    target: str
    """Target name."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> WatchpointInfo:
        return cls(
            access=d.get("access"),
            address=d.get("address"),
            id=d.get("id"),
            length=d.get("length"),
            target=d.get("target"),
        )


@dataclass
class WatchpointList:
    """The watchpoints of all targets."""

    watchpoints: list[WatchpointInfo]

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> WatchpointList:
        return cls(
            watchpoints=_list(WatchpointInfo, d.get("watchpoints")),
        )


class GeneratedClient:
    """The API operations; a subclass provides the transport (`_call`)."""

    def _call(self, method: str, path: str, query: dict[str, Any] | None, body: dict[str, Any] | None) -> Any:
        raise NotImplementedError

    def targets(self) -> TargetList:
        """List the targets (cores) with their run state.
        """
        return TargetList.from_dict(self._call("GET", "/api/v1/targets", None, None))

    def state(self, target: str) -> TargetInfo:
        """Get the run state of a target (and its PC when halted).

        Args:
            target: Target name (e.g. `tc37x.cpu0`) or index, as listed by `targets`.
        """
        return TargetInfo.from_dict(self._call("GET", f"/api/v1/targets/{quote(str(target), safe='')}", None, None))

    def halt(self, target: str) -> TargetInfo:
        """Halt a target. Returns once it is halted (up to one second).

        Does nothing if it is already halted. A source-level step from VS Code running on it is
        cancelled, like the VS Code pause button does.

        Args:
            target: Target name (e.g. `tc37x.cpu0`) or index, as listed by `targets`.
        """
        return TargetInfo.from_dict(self._call("POST", f"/api/v1/targets/{quote(str(target), safe='')}/halt", None, None))

    def resume(self, target: str, *, address: int | str | None = None) -> TargetInfo:
        """Resume a halted target, optionally at another address.

        Fails with `busy` while VS Code does a source-level step on it.

        Args:
            target: Target name or index.
            address: Continue at this address instead of the current PC.
        """
        return TargetInfo.from_dict(self._call("POST", f"/api/v1/targets/{quote(str(target), safe='')}/resume", None, _args({"address": address})))

    def step(self, target: str) -> TargetInfo:
        """Execute one instruction on a halted target.

        Fails with `busy` while VS Code does a source-level step on it.

        Args:
            target: Target name (e.g. `tc37x.cpu0`) or index, as listed by `targets`.
        """
        return TargetInfo.from_dict(self._call("POST", f"/api/v1/targets/{quote(str(target), safe='')}/step", None, None))

    def wait_state(self, target: str, *, state: WaitFor | None = None, timeout_ms: int | None = None) -> TargetInfo:
        """Wait until a target is halted (or running), e.g. after `resume` until a breakpoint hits.

        Fails with `timeout` if the state is not reached in time. Other clients are not blocked
        while waiting.

        Args:
            target: Target name or index.
            state: State to wait for (default `halted`).
            timeout_ms: Maximum wait in milliseconds (default 5000, at most 600000).
        """
        return TargetInfo.from_dict(self._call("GET", f"/api/v1/targets/{quote(str(target), safe='')}/wait", _args({"state": state, "timeout_ms": timeout_ms}), None))

    def reset(self, *, mode: ResetMode | None = None) -> TargetList:
        """Reset the device through the debug adapter and run the reset handlers of the
        configuration (OpenOCD `reset run|halt|init`).

        Flash programming needs a preceding halting reset. Fails with `busy` while VS Code does
        a source-level step.

        Args:
            mode: Reset mode (default `halt`).
        """
        return TargetList.from_dict(self._call("POST", "/api/v1/reset", None, _args({"mode": mode})))

    def read_registers(self, target: str, *, group: RegisterGroup | None = None) -> RegisterList:
        """Read the registers of a target (the core registers need a halted target).

        Args:
            target: Target name or index.
            group: Only this group (default `core`).
        """
        return RegisterList.from_dict(self._call("GET", f"/api/v1/targets/{quote(str(target), safe='')}/registers", _args({"group": group}), None))

    def read_register(self, target: str, name: str) -> RegisterValue:
        """Read one register.

        Args:
            target: Target name or index.
            name: Register name (e.g. `pc`, `d4`, `a10`, `psw`; aliases like `sp` work).
        """
        return RegisterValue.from_dict(self._call("GET", f"/api/v1/targets/{quote(str(target), safe='')}/registers/{quote(str(name), safe='')}", None, None))

    def write_register(self, target: str, name: str, *, value: int | str) -> RegisterValue:
        """Write one register. Core registers need a halted target.

        Connected debugger clients (VS Code) are told to refresh their views.

        Args:
            target: Target name or index.
            name: Register name.
            value: New value.
        """
        return RegisterValue.from_dict(self._call("PUT", f"/api/v1/targets/{quote(str(target), safe='')}/registers/{quote(str(name), safe='')}", None, _args({"value": value})))

    def read_memory(self, *, address: int | str, length: int, target: str | None = None) -> MemoryData:
        """Read memory (at most 65536 bytes per call).

        Reads through the system bus, or with `target` as that core sees memory (HSM-private
        memory, MCS program RAM). Some areas are protected at runtime (e.g. by the application's
        memory protection) and fail with `memory`.

        Args:
            address: Start address.
            length: Number of bytes (at most 65536).
            target: Read as this core sees memory (HSM-private memory, MCS program RAM); default: the system bus.
        """
        return MemoryData.from_dict(self._call("GET", "/api/v1/memory", _args({"address": address, "length": length, "target": target}), None))

    def write_memory(self, *, address: int | str, data: str, target: str | None = None) -> MemoryWritten:
        """Write memory (at most 65536 bytes per call). PFLASH is written with `flash_write`.

        Connected debugger clients (VS Code) are told to refresh their views.

        Args:
            address: Start address.
            data: The bytes as hex string (at most 65536 bytes).
            target: Write as this core sees memory (HSM-private memory, MCS program RAM); default: the system bus.
        """
        return MemoryWritten.from_dict(self._call("PUT", "/api/v1/memory", None, _args({"address": address, "data": data, "target": target})))

    def list_breakpoints(self) -> BreakpointList:
        """List the breakpoints of all clients (this API and VS Code).
        """
        return BreakpointList.from_dict(self._call("GET", "/api/v1/breakpoints", None, None))

    def add_breakpoint(self, *, address: int | str | None = None, file: str | None = None, line: int | None = None, symbol: str | None = None, targets: list[str] | None = None) -> BreakpointInfo:
        """Set a hardware breakpoint at an address, function or source line.

        The breakpoint is shared with the connected debugger clients: VS Code reports its hits.
        A function or source line breakpoint needs the symbols a VS Code debug session loaded;
        before that it is pending (see `error`) and is planted when they are loaded.

        Args:
            address: Code address.
            file: Source file (with `line`); needs the symbols loaded by a VS Code debug session.
            line: Source line in `file`.
            symbol: Function name; needs the symbols loaded by a VS Code debug session.
            targets: Targets (names or indices) to break on; default: all examined targets.
        """
        return BreakpointInfo.from_dict(self._call("POST", "/api/v1/breakpoints", None, _args({"address": address, "file": file, "line": line, "symbol": symbol, "targets": targets})))

    def remove_breakpoint(self, id: str) -> BreakpointList:
        """Remove a breakpoint (also one set in VS Code). Returns the remaining breakpoints.

        Args:
            id: Breakpoint ID.
        """
        return BreakpointList.from_dict(self._call("DELETE", f"/api/v1/breakpoints/{quote(str(id), safe='')}", None, None))

    def list_watchpoints(self) -> WatchpointList:
        """List the watchpoints (data breakpoints) of all targets.
        """
        return WatchpointList.from_dict(self._call("GET", "/api/v1/watchpoints", None, None))

    def add_watchpoint(self, *, address: int | str, target: str, access: WatchAccess | None = None, length: int | None = None) -> WatchpointInfo:
        """Set a watchpoint: the target halts when it accesses the address.

        Args:
            address: Data address.
            target: Target name or index.
            access: Triggering access (default `write`).
            length: Watched bytes (default 4).
        """
        return WatchpointInfo.from_dict(self._call("POST", "/api/v1/watchpoints", None, _args({"address": address, "target": target, "access": access, "length": length})))

    def remove_watchpoint(self, *, address: int | str, target: str) -> WatchpointList:
        """Remove a watchpoint. Returns the remaining watchpoints.

        Args:
            address: Data address.
            target: Target name or index.
        """
        return WatchpointList.from_dict(self._call("DELETE", "/api/v1/watchpoints", _args({"address": address, "target": target}), None))

    def flash_write(self, *, files: list[str], erase: bool | None = None, incremental: bool | None = None, verify: bool | None = None) -> CommandOutput:
        """Program image files into flash (`flash write_images`).

        Needs a preceding halting reset (`reset` with mode `halt`): the flash is protected
        otherwise. UCB data in the images is skipped. Only the sectors the images touch are
        erased. Blocks the debug server while it runs.

        Args:
            files: Image files (ELF, Intel HEX) on the debug server's machine; later files win where they overlap.
            erase: Erase programmed sectors first (needed unless they are erased).
            incremental: Skip sectors that already hold the image data.
            verify: Read back and compare after programming.
        """
        return CommandOutput.from_dict(self._call("POST", "/api/v1/flash/write", None, _args({"files": files, "erase": erase, "incremental": incremental, "verify": verify})))

    def flash_verify(self, *, file: str) -> CommandOutput:
        """Compare flash contents with an image file (`verify_image`).

        Args:
            file: Image file on the debug server's machine.
        """
        return CommandOutput.from_dict(self._call("POST", "/api/v1/flash/verify", None, _args({"file": file})))

    def tcl(self, *, command: str) -> CommandOutput:
        """Run a command of the OpenOCD-compatible Tcl console (like the telnet console).

        Args:
            command: Command line, as typed in the telnet console (e.g. `mdw 0x70000000 4`).
        """
        return CommandOutput.from_dict(self._call("POST", "/api/v1/tcl", None, _args({"command": command})))
