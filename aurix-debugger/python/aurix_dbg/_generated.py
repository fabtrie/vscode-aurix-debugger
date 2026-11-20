# Generated from the aurix-dbg API definition (server/crates/api-spec) by `cargo build`. Do not edit.
"""Generated part of the aurix-dbg client: response types and API methods.

Control and inspect AURIX targets through the aurix-dbg hub: open debug sessions on boards, then run control, call stacks, registers, memory, variables by name, peripheral registers by name, breakpoints, watchpoints, flash programming and the OpenOCD-compatible Tcl console.

A session is one board connection (one worker process). `list_sessions` shows the open ones; `open_session` starts one from a profile (`list_profiles`) or configuration scripts and can run the launch steps of a VS Code launch configuration: symbols, flash, reset, breakpoints, run to a function. The session operations take a `session` (ID or name); it can be left out while exactly one session is open.

A board without a profile needs cfg scripts: `reference` (topic `cfg`) explains how to write a board cfg and a profile, and serves the Tcl command reference and the aurix-dbg scripts to copy. It needs no session.

Every client shares a session with the others: actions taken here are reflected in a connected VS Code debug session (TCF) and the other way round, e.g. a breakpoint set here shows up there and a halt from VS Code is seen in `wait_state` and the event stream. A client that needs the target to itself takes the control lease (`acquire_control`).

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


BreakpointOwner = Literal["api", "client", "console"]
"""Who set a breakpoint."""

ErrorCode = Literal["bad_request", "not_found", "not_halted", "not_examined", "busy", "timeout", "memory", "adapter", "device_locked", "device_access", "resource", "failed"]
"""Error kind."""

ImageType = Literal["elf", "ihex", "srec", "bin"]
"""Image file type."""

LaunchPhase = Literal["idle", "starting", "ready", "failed"]
"""Where the launch steps of a session are."""

LaunchResetKeyword = Literal["none"]
"""`none`: do not reset."""

PeripheralNodeKind = Literal["peripheral", "cluster", "register", "field"]
"""What a peripheral path names."""

RegisterAccess = Literal["read_only", "read_write", "write_only"]
"""Access type of a register or field."""

RegisterGroup = Literal["core", "debug"]
"""Register group."""

ResetMode = Literal["run", "halt", "init"]
"""Reset mode."""

RunState = Literal["unknown", "running", "halted", "debug_running", "reset"]
"""Run state of a target."""

SessionState = Literal["starting", "running", "exited"]
"""The state of a session."""

VariableClass = Literal["float", "array", "struct", "union", "function", "uint", "int", "bool", "enum", "pointer", "unknown"]
"""What a variable holds, from its type with typedefs and qualifiers stripped."""

WaitFor = Literal["halted", "running"]
"""State to wait for."""

WatchAccess = Literal["read", "write", "access"]
"""Access that triggers a watchpoint."""


@dataclass
class AddressInfo:
    """What the symbols say about an address."""

    address: int
    location: SourceLocation | None = None
    """Function and line of code at the address."""
    variable: str | None = None
    """The global or static variable the address lies in."""
    variable_offset: int | None = None
    """Offset of the address into `variable`."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> AddressInfo:
        return cls(
            address=d.get("address"),
            location=_obj(SourceLocation, d.get("location")),
            variable=d.get("variable"),
            variable_offset=d.get("variable_offset"),
        )


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
    client: str | None = None
    """The API client that set it (`X-Aurix-Client`, e.g. `mcp:<id>`), for owner `api`."""
    condition: str | None = None
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
            client=d.get("client"),
            condition=d.get("condition"),
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
class ConnectSpec:
    """How a worker connects to its board: the OpenOCD-style command line."""

    adapter: str | None = None
    """Adapter driver (`tas_client`, `winidea`, `bluebox`, `sim`); default: what the scripts
    select."""
    commands: list[str] | None = None
    """Tcl commands (`-c`), run after the scripts."""
    config_files: list[str] | None = None
    """Configuration scripts (`-f`), searched in `search_dirs` and the bundled OpenOCD scripts."""
    probe: str | None = None
    """Probe serial number, to refuse a second session on the same probe."""
    search_dirs: list[str] | None = None
    """Script search directories (`-s`)."""
    ucb_write: bool | None = None
    """Allow writing UCBs in this session (`ucb write`, after a person's confirmation). Off by
    default; refused when an MCP client asks for it (a hub profile or the VS Code launch
    configuration `ucbWrite` can set it)."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ConnectSpec:
        return cls(
            adapter=d.get("adapter"),
            commands=d.get("commands"),
            config_files=d.get("config_files"),
            probe=d.get("probe"),
            search_dirs=d.get("search_dirs"),
            ucb_write=d.get("ucb_write"),
        )


@dataclass
class ControlInfo:
    """The control lease of a session."""

    expires_in_s: int | None = None
    """Seconds until it expires."""
    held_s: int | None = None
    """Seconds it has been held."""
    holder: str | None = None
    """The client holding it; none: nobody, every client may control the target."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ControlInfo:
        return cls(
            expires_in_s=d.get("expires_in_s"),
            held_s=d.get("held_s"),
            holder=d.get("holder"),
        )


@dataclass
class EndClientResult:
    """What ending a client removed."""

    released_control: bool
    """The client held the control lease, which is now free."""
    removed: list[str]
    """IDs of the breakpoints and watchpoints that were removed."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> EndClientResult:
        return cls(
            released_control=d.get("released_control"),
            removed=d.get("removed"),
        )


@dataclass
class FieldDescription:
    access: RegisterAccess
    """The field's, else its register's."""
    bit_offset: int
    bit_width: int
    name: str
    description: str | None = None
    modified_write: str | None = None
    """What writing does (`one_to_clear`, `one_to_set`, `one_to_toggle`, `zero_to_clear`, ...)."""
    read_action: str | None = None
    """Reading changes it (`clear`, `set`, `modify`, `modify_external`)."""
    values: list[FieldValueName] | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> FieldDescription:
        return cls(
            access=d.get("access"),
            bit_offset=d.get("bit_offset"),
            bit_width=d.get("bit_width"),
            name=d.get("name"),
            description=d.get("description"),
            modified_write=d.get("modified_write"),
            read_action=d.get("read_action"),
            values=_list(FieldValueName, d.get("values")),
        )


@dataclass
class FieldReading:
    name: str
    value: int
    value_name: str | None = None
    """The value's name, when the SVD names it."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> FieldReading:
        return cls(
            name=d.get("name"),
            value=d.get("value"),
            value_name=d.get("value_name"),
        )


@dataclass
class FieldValueName:
    """A named value of a field."""

    name: str
    value: int
    description: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> FieldValueName:
        return cls(
            name=d.get("name"),
            value=d.get("value"),
            description=d.get("description"),
        )


@dataclass
class GlobalList:
    """One page of the globals that match a query, sorted by name."""

    items: list[VariableInfo]
    offset: int
    """Index of the first item among the matches."""
    total: int
    """Matches in all, not only on this page."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> GlobalList:
        return cls(
            items=_list(VariableInfo, d.get("items")),
            offset=d.get("offset"),
            total=d.get("total"),
        )


@dataclass
class HubInfo:
    """What is serving: the hub."""

    api_version: int
    autostart: bool
    """The hub is registered to start at logon."""
    bind: str
    """The address it listens on."""
    exe: str
    """The hub executable; hub and workers come from the same directory."""
    log_file: str
    mcp_clients: int
    """MCP sessions of clients."""
    pid: int
    port: int
    server: str
    """Always `aurix-hub`."""
    sessions: int
    token_required: bool
    """Clients from other machines need the token."""
    update_pending: bool
    """A newer installation is waiting: the hub restarts from it once no session is open."""
    version: str

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> HubInfo:
        return cls(
            api_version=d.get("api_version"),
            autostart=d.get("autostart"),
            bind=d.get("bind"),
            exe=d.get("exe"),
            log_file=d.get("log_file"),
            mcp_clients=d.get("mcp_clients"),
            pid=d.get("pid"),
            port=d.get("port"),
            server=d.get("server"),
            sessions=d.get("sessions"),
            token_required=d.get("token_required"),
            update_pending=d.get("update_pending"),
            version=d.get("version"),
        )


@dataclass
class InfoSymbolFile:
    """A loaded symbol file and the targets it is loaded for."""

    path: str
    """Path on the debug server's machine."""
    targets: list[str]
    """Targets whose symbol set holds the file."""
    local_cores: list[int] | None = None
    """The TriCore cores (core IDs) the file is assigned to (`load_symbols` `path@N`, or
    `symbolFiles` in VS Code): whose core-local RAM its variables there are in. Left out when
    it is assigned to none."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> InfoSymbolFile:
        return cls(
            path=d.get("path"),
            targets=d.get("targets"),
            local_cores=d.get("local_cores"),
        )


@dataclass
class InfoTarget:
    """A target as `info` reports it."""

    examined: bool
    """The target has been examined (is accessible and under debug control)."""
    name: str
    """Target name (e.g. `tc37x.cpu0`)."""
    reachable: bool
    """False while the target has not been accessible for a while (e.g. not powered)."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> InfoTarget:
        return cls(
            examined=d.get("examined"),
            name=d.get("name"),
            reachable=d.get("reachable"),
        )


@dataclass
class LaunchBreakpoint:
    """A breakpoint set at launch, before the target runs."""

    location: str
    """A function, `file:line` or address (`0x...`)."""
    condition: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> LaunchBreakpoint:
        return cls(
            location=d.get("location"),
            condition=d.get("condition"),
        )


@dataclass
class LaunchFlash:
    """Images to flash at launch."""

    files: list[str | dict[str, Any]]
    """Image files, merged into one image (later files win where they overlap), as for
    `flash_write`."""
    incremental: bool | None = None
    """Skip sectors that already hold the image; default true."""
    verify: bool | None = None
    """Read the programmed sectors back and compare; default true."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> LaunchFlash:
        return cls(
            files=d.get("files"),
            incremental=d.get("incremental"),
            verify=d.get("verify"),
        )


@dataclass
class LaunchResetSpec:
    """A reset at launch."""

    halt: bool | None = None
    """Halt at the reset vector instead of running (VS Code's `resetSuspend`)."""
    type: str | None = None
    """Reset type; `reset` (through the debug adapter) is the only one."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> LaunchResetSpec:
        return cls(
            halt=d.get("halt"),
            type=d.get("type"),
        )


@dataclass
class LaunchStatus:
    """The state of a session's launch."""

    phase: LaunchPhase
    steps: list[LaunchStep]
    """The steps done so far."""
    error: str | None = None
    """Why the launch failed."""
    targets: list[TargetInfo] | None = None
    """The targets after the steps (when `ready` or `failed`)."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> LaunchStatus:
        return cls(
            phase=d.get("phase"),
            steps=_list(LaunchStep, d.get("steps")),
            error=d.get("error"),
            targets=_list(TargetInfo, d.get("targets")),
        )


@dataclass
class LaunchStep:
    """One launch step and how it went."""

    ms: int
    """Milliseconds it took."""
    ok: bool
    step: str
    """`connect`, `symbols`, `orti`, `flash`, `reset`, `breakpoints`, `run_to` or
    `stop_on_entry`."""
    detail: str | None = None
    """What it did (e.g. the flash output, the files loaded) or why it failed."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> LaunchStep:
        return cls(
            ms=d.get("ms"),
            ok=d.get("ok"),
            step=d.get("step"),
            detail=d.get("detail"),
        )


@dataclass
class LocalList:
    """The locals and parameters of a stack frame."""

    frame: int
    items: list[LocalVariable]
    """Outermost declaration first, as the VS Code Variables view lists them; a local an inner
    declaration of the same name shadows is left out."""
    target: str
    function: str | None = None
    """The frame's function, where the symbols name it."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> LocalList:
        return cls(
            frame=d.get("frame"),
            items=_list(LocalVariable, d.get("items")),
            target=d.get("target"),
            function=d.get("function"),
        )


@dataclass
class LocalVariable:
    """A local variable or parameter of a stack frame."""

    argument: bool
    """A parameter of the frame's function."""
    class_: VariableClass
    expression: str
    """C expression naming exactly this object, for `evaluate`, `write_variable`,
    `symbol_children` and the `expression` of a DAQ point. It is the plain name unless another
    global of the same name would be found first; then it is a symbol reference
    (`${E0/GV:12}`). A copy in one core's core-local RAM ends in its core, `CRASH_state@2`."""
    has_children: bool
    """`symbol_children` lists members or elements: a struct or union, or an array of known
    length; or, for a `per_core` group, the copies."""
    name: str
    """Name to show: the variable, the member, or the index (`[3]`)."""
    sampleable: bool
    """A DAQ point decodes it to a typed value: an integer, float, bool, enum or pointer of at
    most 8 bytes."""
    size: int
    """Size in bytes; for a bit field, of its storage unit."""
    type_name: str
    """C type as declared, e.g. `uint32`, `struct motor_s`, `uint8[16]`."""
    address: int | None = None
    """Address, when it is fixed (always for a global, except a `per_core` group). Always the
    system-bus address: a variable linked to the core-local alias (`0xD000_0000` DSPR,
    `0xC000_0000` PSPR) is reported in the DSPR/PSPR of its core, e.g. `0x60000100` for
    `0xD0000100` on core 1."""
    bit_offset: int | None = None
    """Bit field: position of its least significant bit in the storage unit at `address`."""
    bit_size: int | None = None
    """Bit field: width in bits."""
    error: str | None = None
    """Why there is no value: e.g. its location is not available at the frame's PC (optimized
    out), or its memory cannot be read."""
    per_core: bool | None = None
    """A group rather than one variable: it is in core-local RAM, which every TriCore core has
    its own of, and its symbol file is not assigned to exactly one core (`load_symbols`
    `path@N`, or `symbolFiles` in VS Code). Its `symbol_children` are the copies, one per core
    (`CRASH_state@0`, `CRASH_state@1`, ...), each with its own address. A group has no
    `address`, is not `sampleable`, and `evaluate` refuses its bare name. Left out when false."""
    unit: str | None = None
    """The compilation unit (source file) that defines a global; `list_globals` only."""
    value: float | bool | str | None = None
    """The value, decoded as `evaluate` decodes it, when `values` was asked for and it could be
    read."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> LocalVariable:
        return cls(
            argument=d.get("argument"),
            class_=d.get("class"),
            expression=d.get("expression"),
            has_children=d.get("has_children"),
            name=d.get("name"),
            sampleable=d.get("sampleable"),
            size=d.get("size"),
            type_name=d.get("type_name"),
            address=d.get("address"),
            bit_offset=d.get("bit_offset"),
            bit_size=d.get("bit_size"),
            error=d.get("error"),
            per_core=d.get("per_core"),
            unit=d.get("unit"),
            value=d.get("value"),
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
class OsAttributeInfo:
    """One attribute of an ORTI object type."""

    description: str
    """The description the generator gave it, suitable as a column header."""
    kind: str
    """`ctype` (a number), `enum` (a name from a fixed list) or `string` (literal text)."""
    live: bool
    """The value can change while the target runs, so it is worth re-reading."""
    name: str
    """Name as the ORTI file declares it, e.g. `RUNNINGTASK`, `vs_HomePriority`."""
    per_core: bool
    """Declared `NAME[]`: it has one value per logical core. This is the ORTI 2.3 SMP extension."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> OsAttributeInfo:
        return cls(
            description=d.get("description"),
            kind=d.get("kind"),
            live=d.get("live"),
            name=d.get("name"),
            per_core=d.get("per_core"),
        )


@dataclass
class OsAttributeValue:
    """One attribute value of one object."""

    name: str
    value: str
    """The value to display: an enumerator name, a number, or literal text."""
    core: int | None = None
    """Logical core, for an attribute declared per core."""
    error: str | None = None
    """Why the value could not be read."""
    link: str | None = None
    """The ORTI object this value points at, when the enumerator names one, e.g. the stack of a
    task or the owner of a spinlock."""
    number: int | None = None
    """The raw integer behind `value`, when there is one."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> OsAttributeValue:
        return cls(
            name=d.get("name"),
            value=d.get("value"),
            core=d.get("core"),
            error=d.get("error"),
            link=d.get("link"),
            number=d.get("number"),
        )


@dataclass
class OsInfo:
    """The loaded OS awareness description."""

    cores: int
    """Logical cores the file describes."""
    errors: list[str]
    """Attributes that could not be planned, with the reason. They are read again on demand, so
    entries here are usually attributes planned while the board was not reachable."""
    types: list[OsTypeInfo]
    file: str | None = None
    """The ORTI file, or null when none is loaded."""
    implementation: str | None = None
    """The `IMPLEMENTATION` name, e.g. `Vector_ORTI`."""
    target: str | None = None
    """The target whose symbols the attribute expressions were resolved against."""
    version: str | None = None
    """ORTI semantics version. `2.3` is the unofficial SMP extension, where the OS attributes
    are arrays indexed by logical core."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> OsInfo:
        return cls(
            cores=d.get("cores"),
            errors=d.get("errors"),
            types=_list(OsTypeInfo, d.get("types")),
            file=d.get("file"),
            implementation=d.get("implementation"),
            target=d.get("target"),
            version=d.get("version"),
        )


@dataclass
class OsObject:
    """One object instance and its attribute values."""

    attributes: list[OsAttributeValue]
    name: str

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> OsObject:
        return cls(
            attributes=_list(OsAttributeValue, d.get("attributes")),
            name=d.get("name"),
        )


@dataclass
class OsObjectList:
    """Objects of one ORTI type, read now."""

    kind: str
    objects: list[OsObject]
    reads: int
    """Bus accesses this call made. Constants and literal text cost none."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> OsObjectList:
        return cls(
            kind=d.get("kind"),
            objects=_list(OsObject, d.get("objects")),
            reads=d.get("reads"),
        )


@dataclass
class OsStack:
    """One stack's coverage."""

    base: int
    """Lowest address of the region."""
    direction: str
    """`DOWN` or `UP`: which end keeps the fill pattern."""
    name: str
    pattern: int
    """The word the OS fills the stack with at startup."""
    size: int
    error: str | None = None
    """Why it could not be measured, e.g. a region the memory protection hides from the debugger."""
    free: int | None = None
    percent: int | None = None
    used: int | None = None
    """Bytes ever used (the high-water mark). Absent when it could not be measured."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> OsStack:
        return cls(
            base=d.get("base"),
            direction=d.get("direction"),
            name=d.get("name"),
            pattern=d.get("pattern"),
            size=d.get("size"),
            error=d.get("error"),
            free=d.get("free"),
            percent=d.get("percent"),
            used=d.get("used"),
        )


@dataclass
class OsStackList:
    """Stack coverage."""

    reads: int
    """Bus accesses this call made. Measuring is the expensive part of OS awareness."""
    stacks: list[OsStack]

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> OsStackList:
        return cls(
            reads=d.get("reads"),
            stacks=_list(OsStack, d.get("stacks")),
        )


@dataclass
class OsTypeInfo:
    """One ORTI object type."""

    attributes: list[OsAttributeInfo]
    count: int
    """Object instances of this type."""
    description: str
    """The description the generator gave the collection, e.g. `Stacks`."""
    name: str
    """`OS`, `TASK`, `STACK`, `vs_SPINLOCK`, ... Generators invent their own types, so this is
    whatever the file declared."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> OsTypeInfo:
        return cls(
            attributes=_list(OsAttributeInfo, d.get("attributes")),
            count=d.get("count"),
            description=d.get("description"),
            name=d.get("name"),
        )


@dataclass
class PeripheralDescription:
    address: int
    """Base address of a peripheral or cluster, address of a register."""
    kind: PeripheralNodeKind
    offset: int
    path: str
    registers: list[RegisterDescription]
    """In address order; a field's path gives its register with that field only."""
    total: int
    """Registers under the path."""
    description: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> PeripheralDescription:
        return cls(
            address=d.get("address"),
            kind=d.get("kind"),
            offset=d.get("offset"),
            path=d.get("path"),
            registers=_list(RegisterDescription, d.get("registers")),
            total=d.get("total"),
            description=d.get("description"),
        )


@dataclass
class PeripheralList:
    device: str
    """The bundled device, or the file of your own."""
    items: list[PeripheralSummary]
    offset: int
    total: int
    """All matches."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> PeripheralList:
        return cls(
            device=d.get("device"),
            items=_list(PeripheralSummary, d.get("items")),
            offset=d.get("offset"),
            total=d.get("total"),
        )


@dataclass
class PeripheralSummary:
    base_address: int
    length: int
    """Bytes its address blocks span."""
    name: str
    registers: int
    """Registers, those of its clusters included."""
    description: str | None = None
    group: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> PeripheralSummary:
        return cls(
            base_address=d.get("base_address"),
            length=d.get("length"),
            name=d.get("name"),
            registers=d.get("registers"),
            description=d.get("description"),
            group=d.get("group"),
        )


@dataclass
class PeripheralValues:
    offset: int
    path: str
    registers: list[RegisterReading]
    total: int

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> PeripheralValues:
        return cls(
            offset=d.get("offset"),
            path=d.get("path"),
            registers=_list(RegisterReading, d.get("registers")),
            total=d.get("total"),
        )


@dataclass
class PeripheralWritten:
    address: int
    path: str
    written: int
    """The register value written."""
    note: str | None = None
    """Something to know: the value did not stick (an ENDINIT-protected register, read-only
    bits), or the register could not be read first."""
    readback: RegisterReading | None = None
    """The register as read back (absent for one that cannot be read without a side effect)."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> PeripheralWritten:
        return cls(
            address=d.get("address"),
            path=d.get("path"),
            written=d.get("written"),
            note=d.get("note"),
            readback=_obj(RegisterReading, d.get("readback")),
        )


@dataclass
class PlacedSymbolFile:
    """A symbol file limited to some cores."""

    path: str
    cores: list[str] | None = None
    """Cores (target names, core IDs, `*` and `?` as wildcards, e.g. `tc37x.cpu0`, `cpu0`,
    `mcs0.*`) the file applies to; default: every core of its architecture."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> PlacedSymbolFile:
        return cls(
            path=d.get("path"),
            cores=d.get("cores"),
        )


@dataclass
class ProfileInfo:
    """A session setup stored on the hub machine."""

    connect: ConnectSpec
    file: str
    """The profile file."""
    name: str
    error: str | None = None
    """Why the file could not be read."""
    launch: LaunchRequest | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ProfileInfo:
        return cls(
            connect=_obj(ConnectSpec, d.get("connect")),
            file=d.get("file"),
            name=d.get("name"),
            error=d.get("error"),
            launch=_obj(LaunchRequest, d.get("launch")),
        )


@dataclass
class ProfileList:
    """The profiles of the hub machine."""

    dir: str
    """The directory they are read from."""
    profiles: list[ProfileInfo]

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ProfileList:
        return cls(
            dir=d.get("dir"),
            profiles=_list(ProfileInfo, d.get("profiles")),
        )


@dataclass
class Reference:
    """A part of the reference."""

    text: str
    """The text (markdown, or the cfg script itself)."""
    topic: str
    """The topic asked for."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> Reference:
        return cls(
            text=d.get("text"),
            topic=d.get("topic"),
        )


@dataclass
class RegisterDescription:
    access: RegisterAccess
    address: int
    fields: list[FieldDescription]
    path: str
    reset_value: int
    size: int
    """Bits."""
    description: str | None = None
    modified_write: str | None = None
    read_action: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> RegisterDescription:
        return cls(
            access=d.get("access"),
            address=d.get("address"),
            fields=_list(FieldDescription, d.get("fields")),
            path=d.get("path"),
            reset_value=d.get("reset_value"),
            size=d.get("size"),
            description=d.get("description"),
            modified_write=d.get("modified_write"),
            read_action=d.get("read_action"),
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
class RegisterReading:
    address: int
    path: str
    size: int
    """Bits."""
    error: str | None = None
    """The read failed: e.g. the register is protected or its module is not clocked."""
    fields: list[FieldReading] | None = None
    skipped: str | None = None
    """Why it was not read: write-only, or reading has a side effect (`force` reads it)."""
    value: int | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> RegisterReading:
        return cls(
            address=d.get("address"),
            path=d.get("path"),
            size=d.get("size"),
            error=d.get("error"),
            fields=_list(FieldReading, d.get("fields")),
            skipped=d.get("skipped"),
            value=d.get("value"),
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
class ReverseData:
    """What the reverse debugging service answers: a status, the history or a restore report."""

    data: Any

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ReverseData:
        return cls(
            data=d.get("data"),
        )


@dataclass
class ReverseRegion:
    """A RAM range for reverse debugging."""

    private: bool | None = None
    """Written only by this core (its stacks, context save area): restored by difference."""
    size: int | None = None
    """Length in bytes."""
    start: int | str | None = None
    """Start address."""
    symbol: str | None = None
    """A global of the ELF (for `exclude`): its symbol's address and size."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ReverseRegion:
        return cls(
            private=d.get("private"),
            size=d.get("size"),
            start=d.get("start"),
            symbol=d.get("symbol"),
        )


@dataclass
class ServerInfo:
    """The debug server, its probe connection and what is loaded."""

    api_version: int
    """Revision of this API. It is raised whenever an operation or a field is added or changed,
    so a client can check for what it needs instead of parsing `version`."""
    connected: bool
    """The adapter is connected to its probe."""
    core_lag_ms: int
    """Milliseconds since the debug core last finished a job or a run-state poll. It stays below
    the polling period while the server is responsive and grows while one long operation
    (flashing, a reset, a Tcl script) holds the core; the other operations wait for it."""
    examined: bool
    """At least one target is examined."""
    launch: LaunchPhase
    """Where the launch steps are (`idle` before the first `launch`)."""
    pid: int
    """Process ID of the server."""
    server: str
    """Always `aurix-dbg`."""
    symbol_files: list[InfoSymbolFile]
    """The loaded symbol files (by `load_symbols` or a VS Code debug session)."""
    symbols_loading: bool
    """Symbol files are being loaded right now; `symbol_files` lists what was loaded before."""
    targets: list[InfoTarget]
    version: str
    """Version of the server."""
    adapter: str | None = None
    """The adapter driver in use (`tas_client`, `sim`, `winidea`), once one is configured."""
    control: str | None = None
    """The client holding the control lease, if any."""
    log_file: str | None = None
    """The worker's log file, when it writes one."""
    session: str | None = None
    """The hub's session ID of this worker, when a hub started it."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ServerInfo:
        return cls(
            api_version=d.get("api_version"),
            connected=d.get("connected"),
            core_lag_ms=d.get("core_lag_ms"),
            examined=d.get("examined"),
            launch=d.get("launch"),
            pid=d.get("pid"),
            server=d.get("server"),
            symbol_files=_list(InfoSymbolFile, d.get("symbol_files")),
            symbols_loading=d.get("symbols_loading"),
            targets=_list(InfoTarget, d.get("targets")),
            version=d.get("version"),
            adapter=d.get("adapter"),
            control=d.get("control"),
            log_file=d.get("log_file"),
            session=d.get("session"),
        )


@dataclass
class SessionInfo:
    """A session: one board connection held by one worker process."""

    age_s: int
    """Seconds since it was opened."""
    base_url: str
    """Base URL of the session's operations on the hub."""
    launch: LaunchStatus
    """The launch steps."""
    log_file: str
    """The worker's log file."""
    name: str
    session: str
    """Session ID."""
    state: SessionState
    adapter: str | None = None
    control: str | None = None
    """The client holding the control lease."""
    error: str | None = None
    """Why the worker ended, when `exited`."""
    opened_by: str | None = None
    """The client that opened it."""
    owner_pid: int | None = None
    """The process the session belongs to (`owner_pid` of `open_session`): it closes when that
    process ends."""
    pid: int | None = None
    """Process ID of the worker."""
    probe: str | None = None
    profile: str | None = None
    targets: list[str] | None = None
    """Targets of the board."""
    tcf_port: int | None = None
    """TCF port of the worker on the hub machine's loopback (VS Code connects there, or through
    `/api/v1/sessions/{session}/tcf` from another machine)."""
    telnet_port: int | None = None
    """Port of the worker's Tcl console on the hub machine's loopback."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> SessionInfo:
        return cls(
            age_s=d.get("age_s"),
            base_url=d.get("base_url"),
            launch=_obj(LaunchStatus, d.get("launch")),
            log_file=d.get("log_file"),
            name=d.get("name"),
            session=d.get("session"),
            state=d.get("state"),
            adapter=d.get("adapter"),
            control=d.get("control"),
            error=d.get("error"),
            opened_by=d.get("opened_by"),
            owner_pid=d.get("owner_pid"),
            pid=d.get("pid"),
            probe=d.get("probe"),
            profile=d.get("profile"),
            targets=d.get("targets"),
            tcf_port=d.get("tcf_port"),
            telnet_port=d.get("telnet_port"),
        )


@dataclass
class SessionList:
    """The open sessions."""

    sessions: list[SessionInfo]

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> SessionList:
        return cls(
            sessions=_list(SessionInfo, d.get("sessions")),
        )


@dataclass
class SessionLog:
    """The end of a session's log, or the part from an `offset`."""

    file: str
    next: int
    """Byte offset after `text`: the `offset` that continues the log."""
    session: str
    text: str

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> SessionLog:
        return cls(
            file=d.get("file"),
            next=d.get("next"),
            session=d.get("session"),
            text=d.get("text"),
        )


@dataclass
class SourceLocation:
    """A code address in the source: its function and line, as far as the symbols describe it."""

    column: int | None = None
    file: str | None = None
    """Source file (as the compiler recorded its path)."""
    function: str | None = None
    """The function the address is in."""
    function_offset: int | None = None
    """Offset of the address from the start of the function."""
    line: int | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> SourceLocation:
        return cls(
            column=d.get("column"),
            file=d.get("file"),
            function=d.get("function"),
            function_offset=d.get("function_offset"),
            line=d.get("line"),
        )


@dataclass
class StackFrame:
    """A frame of a call stack."""

    index: int
    """0 for the top (innermost) frame, the one the core is halted in."""
    pc: int
    """Where the frame executes: the PC of the top frame, the return address of the others."""
    location: SourceLocation | None = None
    """Function and line. For a caller frame the call is looked up: its `pc` is the return
    address, which may already lie on the next line or past the calling function."""
    return_address: int | None = None
    """Where the frame's function returns to (TriCore A11, Cortex-M LR), where it is known."""
    sp: int | None = None
    """Stack pointer of the frame (TriCore A10, Cortex-M SP), where it is known."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> StackFrame:
        return cls(
            index=d.get("index"),
            pc=d.get("pc"),
            location=_obj(SourceLocation, d.get("location")),
            return_address=d.get("return_address"),
            sp=d.get("sp"),
        )


@dataclass
class StackTrace:
    """The call stack of a halted target."""

    frames: list[StackFrame]
    """Top (innermost) frame first."""
    replay: bool
    """The frames are those of an instruction trace replay (VS Code shows the core at a trace
    row), not of the live core."""
    target: str
    truncated: bool
    """More frames exist than were returned (`max_frames`, or the limit of 64)."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> StackTrace:
        return cls(
            frames=_list(StackFrame, d.get("frames")),
            replay=d.get("replay"),
            target=d.get("target"),
            truncated=d.get("truncated"),
        )


@dataclass
class SvdInfo:
    """The SVD description of the session."""

    available: list[str]
    """Devices the bundled SVD files describe."""
    loaded: bool
    """A description is loaded. Without one, the first peripheral operation loads the bundled
    one of `chip_device`."""
    peripherals: int
    registers: int
    chip: str | None = None
    """The OpenOCD chip name of the target configuration (`tc37x`)."""
    chip_device: str | None = None
    """The bundled device that chip name stands for, if there is one."""
    device: str | None = None
    """The bundled device it describes (absent for a file of your own)."""
    extra_files: list[str] | None = None
    """Files merged into it."""
    file: str | None = None
    load_seconds: float | None = None
    """Seconds the conversion took."""
    source: str | None = None
    """How it was chosen: `load_svd`, or `chip` (from the target configuration's chip name)."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> SvdInfo:
        return cls(
            available=d.get("available"),
            loaded=d.get("loaded"),
            peripherals=d.get("peripherals"),
            registers=d.get("registers"),
            chip=d.get("chip"),
            chip_device=d.get("chip_device"),
            device=d.get("device"),
            extra_files=d.get("extra_files"),
            file=d.get("file"),
            load_seconds=d.get("load_seconds"),
            source=d.get("source"),
        )


@dataclass
class SymbolChildren:
    """The members of a struct or union, or the elements of an array. Anything else has none."""

    expression: str
    """The parent expression."""
    items: list[VariableInfo]
    offset: int
    """Index of the first item."""
    total: int
    """Children in all (members, or array elements), not only on this page."""
    type_name: str
    """The parent's type."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> SymbolChildren:
        return cls(
            expression=d.get("expression"),
            items=_list(VariableInfo, d.get("items")),
            offset=d.get("offset"),
            total=d.get("total"),
            type_name=d.get("type_name"),
        )


@dataclass
class SymbolsForTarget:
    """The symbol files of one target."""

    files: list[str]
    """Loaded files, primary first."""
    functions: int
    """Functions with debug information."""
    globals: int
    """Global and static variables that have a fixed address."""
    source_files: int
    """Source files in the line table."""
    target: str
    """Target name."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> SymbolsForTarget:
        return cls(
            files=d.get("files"),
            functions=d.get("functions"),
            globals=d.get("globals"),
            source_files=d.get("source_files"),
            target=d.get("target"),
        )


@dataclass
class SymbolsInfo:
    """The symbol files loaded per target."""

    load_seconds: float
    """How long the load took."""
    targets: list[SymbolsForTarget]
    """Only targets that have symbols."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> SymbolsInfo:
        return cls(
            load_seconds=d.get("load_seconds"),
            targets=_list(SymbolsForTarget, d.get("targets")),
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
    location: SourceLocation | None = None
    """Where the PC is in the source, when halted and the loaded symbols describe it. Left out
    while symbols are loading."""
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
            location=_obj(SourceLocation, d.get("location")),
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
class TraceData:
    """Trace data as the trace service answers: a plan, a status, a recording or a decoded trace."""

    data: Any

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> TraceData:
        return cls(
            data=d.get("data"),
        )


@dataclass
class TraceRange:
    """A trace range: a function (by name, its whole code) or an address range."""

    function: str | None = None
    len: int | None = None
    start: Any | None = None
    """Start address (number or `0x` string)."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> TraceRange:
        return cls(
            function=d.get("function"),
            len=d.get("len"),
            start=d.get("start"),
        )


@dataclass
class UcbChange:
    """One staged UCB change."""

    field: str
    """`<UCB>.<word>.<field>` (e.g. `BMHD1.BMI_BMHDID.HWCFG`) or `<UCB>.<word>` for a whole word."""
    value: str
    """A number, `yes`/`no`, an option label (e.g. `Internal flash`) or a sector list
    (`S0-S3, S10`, `none`)."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> UcbChange:
        return cls(
            field=d.get("field"),
            value=d.get("value"),
        )


@dataclass
class UcbData:
    """UCB data as the `ucb` Tcl command answers with `-json`: a readout (device, status, UCBs with
    their words and fields), a review (changes, problems, risk, token) or an export result."""

    data: Any

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> UcbData:
        return cls(
            data=d.get("data"),
        )


@dataclass
class VariableInfo:
    """A variable, or a member or element of one, as the symbols describe it."""

    class_: VariableClass
    expression: str
    """C expression naming exactly this object, for `evaluate`, `write_variable`,
    `symbol_children` and the `expression` of a DAQ point. It is the plain name unless another
    global of the same name would be found first; then it is a symbol reference
    (`${E0/GV:12}`). A copy in one core's core-local RAM ends in its core, `CRASH_state@2`."""
    has_children: bool
    """`symbol_children` lists members or elements: a struct or union, or an array of known
    length; or, for a `per_core` group, the copies."""
    name: str
    """Name to show: the variable, the member, or the index (`[3]`)."""
    sampleable: bool
    """A DAQ point decodes it to a typed value: an integer, float, bool, enum or pointer of at
    most 8 bytes."""
    size: int
    """Size in bytes; for a bit field, of its storage unit."""
    type_name: str
    """C type as declared, e.g. `uint32`, `struct motor_s`, `uint8[16]`."""
    address: int | None = None
    """Address, when it is fixed (always for a global, except a `per_core` group). Always the
    system-bus address: a variable linked to the core-local alias (`0xD000_0000` DSPR,
    `0xC000_0000` PSPR) is reported in the DSPR/PSPR of its core, e.g. `0x60000100` for
    `0xD0000100` on core 1."""
    bit_offset: int | None = None
    """Bit field: position of its least significant bit in the storage unit at `address`."""
    bit_size: int | None = None
    """Bit field: width in bits."""
    per_core: bool | None = None
    """A group rather than one variable: it is in core-local RAM, which every TriCore core has
    its own of, and its symbol file is not assigned to exactly one core (`load_symbols`
    `path@N`, or `symbolFiles` in VS Code). Its `symbol_children` are the copies, one per core
    (`CRASH_state@0`, `CRASH_state@1`, ...), each with its own address. A group has no
    `address`, is not `sampleable`, and `evaluate` refuses its bare name. Left out when false."""
    unit: str | None = None
    """The compilation unit (source file) that defines a global; `list_globals` only."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> VariableInfo:
        return cls(
            class_=d.get("class"),
            expression=d.get("expression"),
            has_children=d.get("has_children"),
            name=d.get("name"),
            sampleable=d.get("sampleable"),
            size=d.get("size"),
            type_name=d.get("type_name"),
            address=d.get("address"),
            bit_offset=d.get("bit_offset"),
            bit_size=d.get("bit_size"),
            per_core=d.get("per_core"),
            unit=d.get("unit"),
        )


@dataclass
class VariableValue:
    """The value of an expression, read now."""

    class_: VariableClass
    expression: str
    has_children: bool
    """`symbol_children` lists members or elements."""
    raw_hex: str
    """The bytes as read, lowest address first: the whole storage unit for a bit field."""
    sampleable: bool
    """A DAQ point decodes it to a typed value."""
    size: int
    """Size in bytes; for a bit field, of its storage unit."""
    type_name: str
    """C type, e.g. `uint32`, `enum state_e`."""
    value: float | bool | str
    """Decoded as a DAQ point decodes it: a number for an integer, float or pointer, `true` /
    `false` for a bool, the enumerator name for an enum (the number if it matches none),
    `nan` / `inf` / `-inf` for a float that is not finite, and the bytes as hex for anything
    else or larger than 8 bytes."""
    address: int | None = None
    """Address of the value, when it is stored on the target (not for `g_x + 1`). Always the
    system-bus address: core-local RAM is reported in its core's DSPR/PSPR."""
    bit_offset: int | None = None
    """Bit field: position of its least significant bit in the storage unit at `address`."""
    bit_size: int | None = None
    """Bit field: width in bits."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> VariableValue:
        return cls(
            class_=d.get("class"),
            expression=d.get("expression"),
            has_children=d.get("has_children"),
            raw_hex=d.get("raw_hex"),
            sampleable=d.get("sampleable"),
            size=d.get("size"),
            type_name=d.get("type_name"),
            value=d.get("value"),
            address=d.get("address"),
            bit_offset=d.get("bit_offset"),
            bit_size=d.get("bit_size"),
        )


@dataclass
class WatchpointInfo:
    """A watchpoint (data breakpoint)."""

    access: WatchAccess
    enabled: bool
    hits: int
    """Number of hits reported to debugger clients (not counting those whose condition was
    false)."""
    id: str
    """Watchpoint ID."""
    instances: list[WatchpointInstance]
    """Where it is planted."""
    owner: BreakpointOwner
    address: int | None = None
    """Requested data address."""
    client: str | None = None
    """The API client that set it (`X-Aurix-Client`, e.g. `mcp:<id>`), for owner `api`."""
    condition: str | None = None
    error: str | None = None
    """Why it is not planted (e.g. symbols not loaded yet: it is planted once they are)."""
    expression: str | None = None
    """Requested expression."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> WatchpointInfo:
        return cls(
            access=d.get("access"),
            enabled=d.get("enabled"),
            hits=d.get("hits"),
            id=d.get("id"),
            instances=_list(WatchpointInstance, d.get("instances")),
            owner=d.get("owner"),
            address=d.get("address"),
            client=d.get("client"),
            condition=d.get("condition"),
            error=d.get("error"),
            expression=d.get("expression"),
        )


@dataclass
class WatchpointInstance:
    """Where a watchpoint is planted on one target."""

    address: int
    """Address as this target accesses it (core-local RAM: the local address on its own core)."""
    length: int
    target: str
    """Target name."""
    error: str | None = None
    """Why it could not be planted (e.g. no free hardware event)."""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> WatchpointInstance:
        return cls(
            address=d.get("address"),
            length=d.get("length"),
            target=d.get("target"),
            error=d.get("error"),
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
    """The API operations; a subclass provides the transport (`_call`) and the session the
    session operations go to (`_session_path`)."""

    def _call(self, method: str, path: str, query: dict[str, Any] | None, body: dict[str, Any] | None) -> Any:
        raise NotImplementedError

    def _session_path(self) -> str:
        raise NotImplementedError

    def hub_info(self) -> HubInfo:
        """What is serving: the hub, its version, address, sessions and whether an update waits.

        Always answers at once.
        """
        return HubInfo.from_dict(self._call("GET", "/api/v1/hub", None, None))

    def list_sessions(self) -> SessionList:
        """List the open sessions: one per board, each with its targets, ports, launch state and
        control lease holder.
        """
        return SessionList.from_dict(self._call("GET", "/api/v1/sessions", None, None))

    def open_session(self, *, acquire_control: bool | None = None, close_on_error: bool | None = None, connect: ConnectSpec | None = None, launch: LaunchRequest | None = None, name: str | None = None, owner_pid: int | None = None, profile: str | None = None, wait_s: int | None = None) -> SessionInfo:
        """Open a session: start a worker for a board, connect, and run the launch steps.

        Give a `profile` (see `list_profiles`), or `connect` with the configuration scripts, or
        both: the arguments override the profile. Without `launch` (and without a `[launch]` part
        in the profile) the session only connects. Opening a session connects to the hardware and
        may reset the target.

        Returns once the steps are done or after `wait_s` seconds with phase `starting`;
        `wait_session` waits for the rest. A failed step leaves the session open to inspect,
        unless `close_on_error`.

        Args:
            acquire_control: Take the control lease for the calling client before the first step.
            close_on_error: Close the session when a step fails, instead of leaving it open to inspect.
            connect: How to connect; overrides the profile's.
            launch: The launch steps; each field set here overrides the profile's.
            name: Session name; default: the profile name or `session-<n>`.
            owner_pid: Process ID, on the hub machine, of the program the session belongs to (e.g. the IDE that opened it): the hub closes the session when that process ends, also when it crashed or was killed before it could close the session itself.
            profile: Profile on the hub machine (`list_profiles`): connection and launch defaults that the other arguments override.
            wait_s: Seconds to wait for the worker and the launch steps; default 60. Longer starts return with phase `starting`, `wait_session` waits for the rest.
        """
        return SessionInfo.from_dict(self._call("POST", "/api/v1/sessions", None, _args({"acquire_control": acquire_control, "close_on_error": close_on_error, "connect": connect, "launch": launch, "name": name, "owner_pid": owner_pid, "profile": profile, "wait_s": wait_s})))

    def close_session(self, session: str) -> SessionList:
        """Close a session: the worker disconnects from the board and ends. Connected VS Code
        sessions see the disconnect.

        Args:
            session: Session ID or name.
        """
        return SessionList.from_dict(self._call("DELETE", f"/api/v1/sessions/{quote(str(session), safe='')}", None, None))

    def session_log(self, session: str, *, lines: int | None = None, offset: int | None = None) -> SessionLog:
        """The end of a session's log (the worker's console output), e.g. to see why `init` failed.

        Args:
            session: Session ID or name.
            lines: Lines from the end; default 200.
            offset: Read from this byte offset instead, whole lines up to 1 MiB: to follow the log, pass the `next` of the previous answer. A log shorter than the offset is read from its start.
        """
        return SessionLog.from_dict(self._call("GET", f"/api/v1/sessions/{quote(str(session), safe='')}/log", _args({"lines": lines, "offset": offset}), None))

    def list_profiles(self) -> ProfileList:
        """List the session profiles stored on the hub machine.
        """
        return ProfileList.from_dict(self._call("GET", "/api/v1/profiles", None, None))

    def reference(self, *, topic: str | None = None) -> Reference:
        """How to write the cfg scripts and profiles a session opens with, the Tcl command
        reference, and the aurix-dbg cfg scripts to copy next to a board cfg. Needs no session.

        Read topic `cfg` before writing a board cfg or a profile. `commands` lists every Tcl
        command (cfg scripts, the `tcl` operation); a command name (`reset_config`, `suspend add`)
        gives its full description, any other word searches the descriptions. A file name
        (`tc3xx_ocds.cfg`, `fs26_sbc.cfg`, `tc375_lite_example.cfg`, `tc37x_fs26_example.cfg`)
        gives the script's text. Without a topic: the list of topics.

        Args:
            topic: `cfg` (how to write a board cfg and a profile), `commands` (all Tcl commands), a command name (`reset_config`, `suspend add`), a word to search the command descriptions for, or a file name (`tc3xx_ocds.cfg`). Empty: the list of topics.
        """
        return Reference.from_dict(self._call("GET", "/api/v1/reference", _args({"topic": topic}), None))

    def info(self) -> ServerInfo:
        """What is serving: server version and API revision, process, probe connection, targets and
        loaded symbol files.

        Always answers at once, whatever the state - no adapter, no examined target, no symbols, or
        a core busy flashing: it reads what the core publishes and never queues a job on it. That
        makes it the liveness probe; `core_lag_ms` says whether other operations would wait.
        """
        return ServerInfo.from_dict(self._call("GET", self._session_path() + "/info", None, None))

    def targets(self) -> TargetList:
        """List the targets (cores) with their run state.
        """
        return TargetList.from_dict(self._call("GET", self._session_path() + "/targets", None, None))

    def state(self, target: str) -> TargetInfo:
        """Get the run state of a target (and its PC when halted).

        Args:
            target: Target name (e.g. `tc37x.cpu0`) or index, as listed by `targets`.
        """
        return TargetInfo.from_dict(self._call("GET", self._session_path() + f"/targets/{quote(str(target), safe='')}", None, None))

    def halt(self, target: str) -> TargetInfo:
        """Halt a target. Returns once it is halted (up to one second).

        Does nothing if it is already halted. A source-level step from VS Code running on it is
        cancelled, like the VS Code pause button does.

        Args:
            target: Target name (e.g. `tc37x.cpu0`) or index, as listed by `targets`.
        """
        return TargetInfo.from_dict(self._call("POST", self._session_path() + f"/targets/{quote(str(target), safe='')}/halt", None, None))

    def resume(self, target: str, *, address: int | str | None = None) -> TargetInfo:
        """Resume a halted target, optionally at another address.

        Fails with `busy` while VS Code does a source-level step on it.

        Args:
            target: Target name or index.
            address: Continue at this address instead of the current PC.
        """
        return TargetInfo.from_dict(self._call("POST", self._session_path() + f"/targets/{quote(str(target), safe='')}/resume", None, _args({"address": address})))

    def step(self, target: str) -> TargetInfo:
        """Execute one instruction on a halted target.

        Fails with `busy` while VS Code does a source-level step on it.

        Args:
            target: Target name (e.g. `tc37x.cpu0`) or index, as listed by `targets`.
        """
        return TargetInfo.from_dict(self._call("POST", self._session_path() + f"/targets/{quote(str(target), safe='')}/step", None, None))

    def wait_state(self, target: str, *, state: WaitFor | None = None, timeout_ms: int | None = None) -> TargetInfo:
        """Wait until a target is halted (or running), e.g. after `resume` until a breakpoint hits.

        Fails with `timeout` if the state is not reached in time. Other clients are not blocked
        while waiting.

        Args:
            target: Target name or index.
            state: State to wait for (default `halted`).
            timeout_ms: Maximum wait in milliseconds (default 5000, at most 600000).
        """
        return TargetInfo.from_dict(self._call("GET", self._session_path() + f"/targets/{quote(str(target), safe='')}/wait", _args({"state": state, "timeout_ms": timeout_ms}), None))

    def reset(self, *, mode: ResetMode | None = None) -> TargetList:
        """Reset the device through the debug adapter and run the reset handlers of the
        configuration (OpenOCD `reset run|halt|init`).

        Flash programming needs a preceding halting reset. Fails with `busy` while VS Code does
        a source-level step.

        Args:
            mode: Reset mode (default `halt`).
        """
        return TargetList.from_dict(self._call("POST", self._session_path() + "/reset", None, _args({"mode": mode})))

    def stack_trace(self, target: str, *, max_frames: int | None = None) -> StackTrace:
        """The call stack of a halted target: each frame's PC, stack pointer, return address,
        function and source line, top frame first.

        The same frames VS Code shows: TriCore frames come from the CSA chain, Cortex-M (HSM) ones
        from the function prologues; a GTM MCS channel has at most two frames, the SCR one. While
        VS Code shows the core at a trace row (instruction trace replay), the frames are the
        replay's. Functions and lines need loaded symbols (`load_symbols`). Fails with
        `not_halted` while the target runs and with `busy` during a source-level step of VS Code.

        Args:
            target: Target name or index.
            max_frames: Return at most this many frames, from the top (default: all, up to 64).
        """
        return StackTrace.from_dict(self._call("GET", self._session_path() + f"/targets/{quote(str(target), safe='')}/stack", _args({"max_frames": max_frames}), None))

    def read_registers(self, target: str, *, group: RegisterGroup | None = None) -> RegisterList:
        """Read the registers of a target (the core registers need a halted target).

        Args:
            target: Target name or index.
            group: Only this group (default `core`).
        """
        return RegisterList.from_dict(self._call("GET", self._session_path() + f"/targets/{quote(str(target), safe='')}/registers", _args({"group": group}), None))

    def read_register(self, target: str, name: str) -> RegisterValue:
        """Read one register.

        Args:
            target: Target name or index.
            name: Register name (e.g. `pc`, `d4`, `a10`, `psw`; aliases like `sp` work).
        """
        return RegisterValue.from_dict(self._call("GET", self._session_path() + f"/targets/{quote(str(target), safe='')}/registers/{quote(str(name), safe='')}", None, None))

    def write_register(self, target: str, name: str, *, value: int | str) -> RegisterValue:
        """Write one register. Core registers need a halted target.

        Connected debugger clients (VS Code) are told to refresh their views.

        Args:
            target: Target name or index.
            name: Register name.
            value: New value.
        """
        return RegisterValue.from_dict(self._call("PUT", self._session_path() + f"/targets/{quote(str(target), safe='')}/registers/{quote(str(name), safe='')}", None, _args({"value": value})))

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
        return MemoryData.from_dict(self._call("GET", self._session_path() + "/memory", _args({"address": address, "length": length, "target": target}), None))

    def write_memory(self, *, address: int | str, data: str, target: str | None = None) -> MemoryWritten:
        """Write memory (at most 65536 bytes per call). PFLASH is written with `flash_write`.

        Connected debugger clients (VS Code) are told to refresh their views.

        Args:
            address: Start address.
            data: The bytes as hex string (at most 65536 bytes).
            target: Write as this core sees memory (HSM-private memory, MCS program RAM); default: the system bus.
        """
        return MemoryWritten.from_dict(self._call("PUT", self._session_path() + "/memory", None, _args({"address": address, "data": data, "target": target})))

    def list_breakpoints(self) -> BreakpointList:
        """List the breakpoints of all clients (this API and VS Code).
        """
        return BreakpointList.from_dict(self._call("GET", self._session_path() + "/breakpoints", None, None))

    def add_breakpoint(self, *, address: int | str | None = None, condition: str | None = None, file: str | None = None, line: int | None = None, symbol: str | None = None, targets: list[str] | None = None) -> BreakpointInfo:
        """Set a hardware breakpoint at an address, function or source line.

        The breakpoint is shared with the connected debugger clients: VS Code reports its hits.
        A function or source line breakpoint needs the symbols a VS Code debug session loaded;
        before that it is pending (see `error`) and is planted when they are loaded.

        Args:
            address: Code address.
            condition: C expression evaluated on the halted core (in its top stack frame): a hit only halts when it is not zero, e.g. `count > 100`. Otherwise the core is resumed at once.
            file: Source file (with `line`); needs loaded symbols.
            line: Source line in `file`.
            symbol: Function name; needs loaded symbols (`load_symbols`, `launch` or a VS Code debug session).
            targets: Targets (names or indices) to break on; default: all examined targets.
        """
        return BreakpointInfo.from_dict(self._call("POST", self._session_path() + "/breakpoints", None, _args({"address": address, "condition": condition, "file": file, "line": line, "symbol": symbol, "targets": targets})))

    def remove_breakpoint(self, id: str) -> BreakpointList:
        """Remove a breakpoint (also one set in VS Code). Returns the remaining breakpoints.

        Args:
            id: Breakpoint ID.
        """
        return BreakpointList.from_dict(self._call("DELETE", self._session_path() + f"/breakpoints/{quote(str(id), safe='')}", None, None))

    def list_watchpoints(self) -> WatchpointList:
        """List the watchpoints (data breakpoints) of all clients: the API's, VS Code's and those of
        the Tcl console.
        """
        return WatchpointList.from_dict(self._call("GET", self._session_path() + "/watchpoints", None, None))

    def add_watchpoint(self, *, access: WatchAccess | None = None, address: int | str | None = None, condition: str | None = None, expression: str | None = None, length: int | None = None, target: str | None = None) -> WatchpointInfo:
        """Set a watchpoint on an address or a variable (`expression`): the target halts after an
        access to it, with reason `watchpoint` and the PC at the instruction after the access.

        A watchpoint wider than one byte takes two of the 8 hardware events of a core (shared with
        breakpoints) and catches every access that starts inside it; an access that starts below
        it and reaches into it is not caught. It shows up in VS Code's Breakpoints view.

        Args:
            access: Triggering access (default `write`).
            address: Data address.
            condition: C expression evaluated on the halted core (in its top stack frame): a hit only halts when it is not zero, e.g. `g_motor.speed > 100`. Otherwise the core is resumed at once, which still costs a halt on every access.
            expression: C expression of the watched object instead of an address, as `evaluate` takes it (`g_motor.speed`, `g_buf[3]`, `CRASH_state@2`). Its address and size come from the symbols, and are looked up again when other symbols are loaded. A variable in core-local RAM is watched at its local address on its own core and at its global address on the other cores.
            length: Watched bytes; default: the size of the expression's type, or 4 for an address.
            target: Target name or index; default: every TriCore core.
        """
        return WatchpointInfo.from_dict(self._call("POST", self._session_path() + "/watchpoints", None, _args({"access": access, "address": address, "condition": condition, "expression": expression, "length": length, "target": target})))

    def remove_watchpoint(self, *, address: int | str | None = None, id: str | None = None, target: str | None = None) -> WatchpointList:
        """Remove a watchpoint (also one set in VS Code or the Tcl console). Returns the remaining
        watchpoints.

        Args:
            address: Data address (with `target`): removes the watchpoint planted there, on all its targets.
            id: Watchpoint ID.
            target: Target name or index (with `address`).
        """
        return WatchpointList.from_dict(self._call("DELETE", self._session_path() + "/watchpoints", _args({"address": address, "id": id, "target": target}), None))

    def flash_write(self, *, files: list[str | dict[str, Any]], erase: bool | None = None, incremental: bool | None = None, verify: bool | None = None) -> CommandOutput:
        """Program image files into flash (`flash write_images`).

        Needs a preceding halting reset (`reset` with mode `halt`): the flash is protected
        otherwise. Data linked to the non-cached PFLASH addresses (0xA...) is programmed at the
        cached ones (0x8...). UCB data and data outside the flash banks (e.g. linked to RAM) in the
        images are skipped with a warning. Only the sectors the images touch are erased. Blocks the debug
        server while it runs.

        Args:
            files: Image files (ELF, Intel HEX, S-records, binary) on the debug server's machine; later files win where they overlap. An entry is a path, or an object with `path`, `offset` and `type` (`elf`, `ihex`, `srec`, `bin`).
            erase: Erase programmed sectors first (needed unless they are erased).
            incremental: Skip sectors that already hold the image data.
            verify: Read back and compare after programming.
        """
        return CommandOutput.from_dict(self._call("POST", self._session_path() + "/flash/write", None, _args({"files": files, "erase": erase, "incremental": incremental, "verify": verify})))

    def flash_verify(self, *, file: str) -> CommandOutput:
        """Compare flash contents with an image file (`verify_image`).

        Args:
            file: Image file on the debug server's machine.
        """
        return CommandOutput.from_dict(self._call("POST", self._session_path() + "/flash/verify", None, _args({"file": file})))

    def ucb_read(self, *, cached: bool | None = None, ucb: str | None = None) -> UcbData:
        """Read the TC3xx UCBs (user configuration blocks) from the device and decode them.

        Each UCB comes with its confirmation state (ORIG, COPY, installed), its risk class, and its
        words and fields: value, text, default (delivery state; Infineon data: the chip's first
        readout), whether it differs from the default or between ORIG and COPY, staged changes,
        and for protection words the value the DMU installed (`pending_reset` when they differ).
        Passwords and HSM keys are masked; words the DMU protects are reported unreadable.

        Args:
            cached: Use the last readout instead of reading the device again.
            ucb: One UCB by name (`BMHD0`, `DBG`, `OTP3`, ...); default: all.
        """
        return UcbData.from_dict(self._call("GET", self._session_path() + "/ucb", _args({"cached": cached, "ucb": ucb}), None))

    def ucb_stage(self, *, changes: list[UcbChange] | None = None, confirm: list[str] | None = None, discard: bool | None = None) -> UcbData:
        """Stage UCB changes by field (or discard them). Nothing is written to the device.

        The staged changes are shared with the other clients: the VS Code UCB panel shows them, and
        a person writes them from there after the review. Returns the staged UCBs.

        Args:
            changes: Changes to stage, in order.
            confirm: UCBs to stage the confirmation UNLOCKED -> CONFIRMED of.
            discard: Discard all staged changes first.
        """
        return UcbData.from_dict(self._call("POST", self._session_path() + "/ucb/stage", None, _args({"changes": changes, "confirm": confirm, "discard": discard})))

    def ucb_review(self, *, files: list[str | dict[str, Any]] | None = None) -> UcbData:
        """Review the staged UCB changes, or the UCB records of image files: the changes by field,
        the problems the checks found (blocking ones first), the risk (`reversible`, `password`,
        `permanent`) and the confirmation the write needs.

        Writing is not part of this API: UCBs are written only after a person confirmed the
        review, in the VS Code UCB panel or the Tcl console (`ucb write`), and only in a session
        started with UCB writing enabled.

        Args:
            files: Review the UCB records of these image files instead of the staged changes (e.g. a post-build HEX). An entry is a path, or an object with `path`, `offset` and `type`.
        """
        return UcbData.from_dict(self._call("POST", self._session_path() + "/ucb/review", None, _args({"files": files})))

    def ucb_compare(self, *, files: list[str | dict[str, Any]] | None = None) -> UcbData:
        """Compare the device's UCBs with the UCB records of image files (e.g. a post-build HEX), field
        by field: per UCB whether the files hold it (ORIG, COPY, both, none), how many fields
        differ, and per field the device and the file value (passwords only as set/zero). Read-only
        UCBs are reported but cannot be applied.

        Args:
            files: Image files with UCB records (e.g. a post-build HEX) on the debug server's machine; empty: compare again with the files of the last comparison.
        """
        return UcbData.from_dict(self._call("POST", self._session_path() + "/ucb/compare", None, _args({"files": files})))

    def trace_arm(self, *, core: int | None = None, kind: str | None = None, ranges: list[TraceRange] | None = None, stop: str | None = None, timestamps: str | None = None) -> TraceData:
        """Arm a miniMCDS trace of one core (TC37x production device; CPU0 so far): the program flow
        (optionally only while the PC is inside a function or address range) or every write into an
        address range. Stop: `halt` (the last 8 KB up to the core's halt; the debugger stops it),
        `full` (the first 8 KB), `manual` (the last 8 KB before `trace_stop`). The trace RAM holds
        about 80 µs of unfiltered program flow. Shared with traced DAQ points: fails while they
        use it.

        Args:
            core: Observed core; only 0 (CPU0) so far.
            kind: `program` (default) or `data`.
            ranges: Program: at most one range the PC must be inside; data: exactly one range.
            stop: `halt` (default for program), `full`, `manual` (default for data), or a function name to stop at.
            timestamps: `tsu` (default: time stamps with messages) or `ticks`.
        """
        return TraceData.from_dict(self._call("POST", self._session_path() + "/trace/arm", None, _args({"core": core, "kind": kind, "ranges": ranges, "stop": stop, "timestamps": timestamps})))

    def trace_auto(self, *, config: TraceConfigRequest | None = None) -> TraceData:
        """Arm a trace automatically whenever the traced core runs (a run, not a step) and read the
        recording when it halts, so every halt comes with the trace up to it (and its
        instruction trace and replay). `config` null ends it; the answer is the current setting.

        Args:
            config: The configuration to arm on every run (stop `halt` is the useful one); null: off.
        """
        return TraceData.from_dict(self._call("POST", self._session_path() + "/trace/auto", None, _args({"config": config})))

    def trace_check(self, *, core: int | None = None, kind: str | None = None, ranges: list[TraceRange] | None = None, stop: str | None = None, timestamps: str | None = None) -> TraceData:
        """Check a trace configuration without arming: the resources it takes and the window to
        expect, or why it cannot be armed.

        Args:
            core: Observed core; only 0 (CPU0) so far.
            kind: `program` (default) or `data`.
            ranges: Program: at most one range the PC must be inside; data: exactly one range.
            stop: `halt` (default for program), `full`, `manual` (default for data), or a function name to stop at.
            timestamps: `tsu` (default: time stamps with messages) or `ticks`.
        """
        return TraceData.from_dict(self._call("POST", self._session_path() + "/trace/check", None, _args({"core": core, "kind": kind, "ranges": ranges, "stop": stop, "timestamps": timestamps})))

    def trace_status(self) -> TraceData:
        """Whether a trace is armed, by whom, how full the trace RAM is and whether it finished (a
        trace stopped on halt finishes when the core halts), and the last recording.
        """
        return TraceData.from_dict(self._call("GET", self._session_path() + "/trace", None, None))

    def trace_stop(self) -> TraceData:
        """Stop the armed trace (if it still runs) and read the trace RAM.
        """
        return TraceData.from_dict(self._call("POST", self._session_path() + "/trace/stop", None, None))

    def trace_read(self, *, max: int | None = None) -> TraceData:
        """The last recording, decoded: program flow as instructions with function, file and line
        (`Columns` names the fields of each `Steps` row; gaps are objects), or data trace writes
        with address, value and the variable hit. `max`: the newest rows only.

        Args:
            max: The newest rows only (default 20000).
        """
        return TraceData.from_dict(self._call("GET", self._session_path() + "/trace/read", _args({"max": max}), None))

    def trace_insns(self, *, count: int | None = None, from_: int | None = None, scope: str | None = None) -> TraceData:
        """The last program recording as instructions, a page at a time: each row (`I`) with its time
        (`T`, ns from the first), kind (`K`: `i` instruction, `e` interrupt entry, `g` gap), address
        (`A`), mnemonic and operands (`M`, `O`), function and file (`F`, `Fl`: indices into
        `Functions`, `Files`), line (`L`), call depth (`D`), branch taken (`B`), the registers it
        wrote (`W`: `[name, value or null, "exact" | "mem" | null, known bits]`) and its memory
        accesses (`X`). Registers are rebuilt from the state at the halt: `exact`, `mem` (taken from
        memory at the halt) or unknown (null). `Replay` summarises how much could be rebuilt.

        Args:
            count: Rows (default 500, at most 5000).
            from_: First row (default 0).
            scope: With reverse debugging on: `history` (default) numbers the rows of every action of the history one after the other (`Act`: checkpoint index, `R`: row for `reverse_goto`), `action` only the last action's recording.
        """
        return TraceData.from_dict(self._call("GET", self._session_path() + "/trace/insns", _args({"count": count, "from": from_, "scope": scope}), None))

    def trace_regs(self, *, at: int | None = None, before: bool | None = None, scope: str | None = None) -> TraceData:
        """All registers after one row of the instruction trace, each with how sure it is and where
        it came from (`Why`), and the call stack at that point.

        Args:
            at: The row (default: the last, where the core halted).
            before: The registers before the row instead of after it.
            scope: With reverse debugging on: `history` (default) numbers the rows of every action of the history one after the other (`Act`: checkpoint index, `R`: row for `reverse_goto`), `action` only the last action's recording.
        """
        return TraceData.from_dict(self._call("GET", self._session_path() + "/trace/regs", _args({"at": at, "before": before, "scope": scope}), None))

    def trace_find(self, *, query: str, back: bool | None = None, from_: int | None = None, scope: str | None = None) -> TraceData:
        """The next row of the instruction trace matching `query`: an address (`0x80001234`), a
        register value (`d15=0xC`), a register write (`a4 changes`), or text in the instruction,
        function or file name. `from`: start after this row; `back`: search backwards.

        Args:
            query: 
            back: 
            from_: 
            scope: With reverse debugging on: `history` (default) numbers the rows of every action of the history one after the other (`Act`: checkpoint index, `R`: row for `reverse_goto`), `action` only the last action's recording.
        """
        return TraceData.from_dict(self._call("GET", self._session_path() + "/trace/find", _args({"query": query, "back": back, "from": from_, "scope": scope}), None))

    def reverse_enable(self, *, csa_symbols: list[list[str]] | None = None, exclude: list[ReverseRegion] | None = None, history: int | None = None, max_hits: int | None = None, regions: list[ReverseRegion] | None = None, stack_symbols: list[list[str]] | None = None, strict: bool | None = None, target: str | None = None, trace: bool | None = None, verify: bool | None = None) -> ReverseData:
        """Switch reverse debugging on for a TriCore core: before every action (run, step, source
        step, register or memory write) the core's registers and the RAM regions are saved, and
        `reverse_back` / `reverse_continue` write an earlier state back into the halted core, which
        then really runs from there. Regions: the ELF's writable sections, the core's stacks and
        context save area, plus `regions`, without `exclude`. While on it owns the miniMCDS (each
        action is recorded; manual traces are refused). Not rewound: peripherals, timers, other
        cores. The answer has the regions and what one checkpoint costs (`CheckpointMs`).

        Args:
            csa_symbols: Linker symbol pairs bounding the context save area.
            exclude: RAM left out (large buffers).
            history: Checkpoints kept (default 50; 0: all since the last reset).
            max_hits: Most breakpoint hits a run again may count (default 200).
            regions: RAM saved in addition to the ELF's writable sections.
            stack_symbols: Linker symbol pairs bounding the stacks (`[[low, high], ...]`; defaults for iLLD/TASKING).
            strict: Refuse a Step Back that cannot restore everything.
            target: The core (default: CPU0).
            trace: Record every action with the miniMCDS (default true; CPU0 only).
            verify: Check of the faster checkpoints: read the core's stacks and context save area in full at every checkpoint anyway and log where they differ from what the decoded writes predict.
        """
        return ReverseData.from_dict(self._call("POST", self._session_path() + "/reverse/enable", None, _args({"csa_symbols": csa_symbols, "exclude": exclude, "history": history, "max_hits": max_hits, "regions": regions, "stack_symbols": stack_symbols, "strict": strict, "target": target, "trace": trace, "verify": verify})))

    def reverse_disable(self) -> ReverseData:
        """Switch reverse debugging off (the history is dropped, the trace released).
        """
        return ReverseData.from_dict(self._call("POST", self._session_path() + "/reverse/disable", None, None))

    def reverse_status(self) -> ReverseData:
        """Whether reverse debugging is on, for which core, how many checkpoints and the regions.
        """
        return ReverseData.from_dict(self._call("GET", self._session_path() + "/reverse", None, None))

    def reverse_history(self) -> ReverseData:
        """The checkpoints, oldest first: the action that followed each (`Action`), where it started
        (`Pc`, `At`) and ended (`EndPc`), whether it was recorded.
        """
        return ReverseData.from_dict(self._call("GET", self._session_path() + "/reverse/history", None, None))

    def reverse_back(self, *, granularity: str | None = None) -> ReverseData:
        """Step Back on the hardware: `line` (default) to the start of the previous source line,
        stepping backwards over calls; `instruction`; `action` (before the whole last action). A
        point inside an action is reached by restoring its checkpoint and running there again;
        `Diverged` reports a run again that took another path (it stops on a real state). The
        report lists what was restored and what not (`NotUndone`: peripheral registers).

        Args:
            granularity: `line` (default), `instruction` or `action`.
        """
        return ReverseData.from_dict(self._call("POST", self._session_path() + "/reverse/back", None, _args({"granularity": granularity})))

    def reverse_continue(self) -> ReverseData:
        """Reverse Continue on the hardware: back to the last breakpoint or data breakpoint hit in the
        history, or to its start.
        """
        return ReverseData.from_dict(self._call("POST", self._session_path() + "/reverse/continue", None, None))

    def reverse_goto(self, *, index: int, row: int | None = None) -> ReverseData:
        """Go to checkpoint `index` of the history (and to row `row` of the action's recording).

        Args:
            index: Checkpoint index in the history.
            row: Row of that action's recording (default 0: the checkpoint itself).
        """
        return ReverseData.from_dict(self._call("POST", self._session_path() + "/reverse/goto", None, _args({"index": index, "row": row})))

    def ucb_apply(self, *, target: str) -> UcbData:
        """Stage values from the compared files: everything, one UCB, one field or the password.
        Nothing is written; the result is the updated comparison (`taken` per field).

        Args:
            target: `all`, a UCB (`DFLASH`), a word or field (`BMHD1.STAD`, `DFLASH.PROCONRAM.RAMIN`) or a password (`DBG.PW`).
        """
        return UcbData.from_dict(self._call("POST", self._session_path() + "/ucb/apply", None, _args({"target": target})))

    def ucb_export(self, *, file: str, changed: bool | None = None, classes: list[str] | None = None, staged: bool | None = None, states: list[str] | None = None, ucbs: list[str] | None = None, which: str | None = None) -> UcbData:
        """Export a selection of UCBs (whole 512-byte UCBs) to an Intel HEX file: by name, state
        (e.g. only `unlocked`), group, or only those changed from the delivery state.

        Args:
            file: Output file on the debug server's machine.
            changed: Only UCBs that differ from the delivery state (Infineon data: from the chip's first readout).
            classes: Only these groups: `boot`, `protection`, `hsm`, `otp`, `infineon`.
            staged: Export the staged content instead of the device's.
            states: Only UCBs in one of these states: `unlocked` (unconfirmed), `confirmed`, `errored`.
            ucbs: UCBs by name; default: all (filtered by the other fields).
            which: `orig`, `copy` or `both` (default) of the ORIG/COPY pairs.
        """
        return UcbData.from_dict(self._call("POST", self._session_path() + "/ucb/export", None, _args({"file": file, "changed": changed, "classes": classes, "staged": staged, "states": states, "ucbs": ucbs, "which": which})))

    def load_symbols(self, *, files: list[str], targets: list[str] | None = None) -> SymbolsInfo:
        """Load symbol files (ELF) for the targets, as a VS Code debug session does.

        Makes `add_breakpoint` by `symbol` or `file` with `line` and the `expression` points of a
        DAQ session work without VS Code. Breakpoints that were pending are planted.

        There is one symbol set per core, and the last writer wins: a VS Code debug session
        started afterwards replaces what was loaded here, and the other way round. `path@N`
        assigns a file to the cores that run it, for its variables in core-local RAM.

        Args:
            files: Files on the machine the debug server runs on. The first is the primary ELF; the others add symbols (bootmanager, FBL, ...).  A file may end in `@N` or `@N,M` (TriCore core IDs), as in VS Code's `symbolFiles`: the cores that run it, whose core-local RAM (`0xD000_0000` / `0xC000_0000` alias) holds its variables linked there. Assigned to exactly one core, such a variable reads by its plain name; otherwise it needs its core, `CRASH_state@2`. A file without the suffix is assigned to no core, also when VS Code had assigned it.
            targets: Targets (names or indices) to load them for; default: all cores.
        """
        return SymbolsInfo.from_dict(self._call("POST", self._session_path() + "/symbols", None, _args({"files": files, "targets": targets})))

    def symbols(self) -> SymbolsInfo:
        """The symbol files loaded per target (by this API or by a VS Code debug session).
        """
        return SymbolsInfo.from_dict(self._call("GET", self._session_path() + "/symbols", None, None))

    def lookup_address(self, *, address: int | str, return_address: bool | None = None, target: str | None = None) -> AddressInfo:
        """What the loaded symbols say about an address: the function, offset and source line of
        code there, and the global or static variable data there lies in.

        Reads no target memory. Addresses are as linked: a variable in core-local RAM linked at
        its `0xD000_0000` alias is found by that address, not by a core's global one. Fails with
        `not_found` when no symbols are loaded.

        Args:
            address: Code or data address.
            return_address: The address is a return address (e.g. a saved A11 or LR): look up the call before it.
            target: Use this target's symbols (default: the first core that has symbols).
        """
        return AddressInfo.from_dict(self._call("GET", self._session_path() + "/symbols/address", _args({"address": address, "return_address": return_address, "target": target}), None))

    def load_svd(self, *, device: str | None = None, file: str | None = None) -> SvdInfo:
        """Load the SVD description the peripheral operations use: a bundled device's, or a file.

        Without it, the first peripheral operation loads the bundled description of the chip the
        target configuration names (`tc37x` -> `TC37x`, with the HSM and SCR merged). VS Code
        loads the launch configuration's `device` when a debug session starts. Converting takes
        about half a second (TC3xx). Fails with `not_found` for a device no bundled file describes
        (the message lists them) or a missing file, and with `bad_request` for a file that is not
        an SVD description.

        Args:
            device: A device the bundled SVD files describe (`TC37x`, `TC39x`, ...; see `svd` `available`), with its extra files merged (the TC3xx HSM and SCR). Case and `_` do not matter.
            file: An SVD file of your own instead (`.svd` or `.svd.gz`).
        """
        return SvdInfo.from_dict(self._call("POST", self._session_path() + "/svd", None, _args({"device": device, "file": file})))

    def svd(self) -> SvdInfo:
        """The SVD description of the session: device, files, size, and the devices available.

        Loads nothing: before the first peripheral operation it says which device the chip name
        stands for.
        """
        return SvdInfo.from_dict(self._call("GET", self._session_path() + "/svd", None, None))

    def list_peripherals(self, *, limit: int | None = None, offset: int | None = None, query: str | None = None) -> PeripheralList:
        """List the peripherals of the SVD description, filtered by a name pattern.

        Reads nothing from the target. `describe_peripheral` gives a peripheral's registers and
        fields, `read_peripheral` their values.

        Args:
            limit: At most this many (default 200).
            offset: 
            query: Case-insensitive glob over the name and the group (`*`, `?`), e.g. `STM*`, `GTM`.
        """
        return PeripheralList.from_dict(self._call("GET", self._session_path() + "/peripherals", _args({"limit": limit, "offset": offset, "query": query}), None))

    def describe_peripheral(self, path: str, *, limit: int | None = None, offset: int | None = None) -> PeripheralDescription:
        """The registers of a peripheral, cluster or register path, with their fields, access, reset
        values and named field values, as the SVD description has them.

        Reads nothing from the target. Large peripherals (GTM: thousands of registers) are paged
        with `offset` and `limit`. Fails with `not_found` for a path the description does not have.

        Args:
            path: `PERIPHERAL`, `PERIPHERAL.REGISTER`, `PERIPHERAL.CLUSTER.REGISTER` or `....REGISTER.FIELD`, case-insensitive: `STM0`, `STM0.TIM0`, `GTM.TOM0.CH1.CTRL`, `SCU.CCUCON0.CLKSEL`. Arrays are named as the SVD names their copies, e.g. `STM0.CMP[0]`.
            limit: At most this many registers (default 200).
            offset: Skip this many registers (default 0).
        """
        return PeripheralDescription.from_dict(self._call("GET", self._session_path() + f"/peripherals/{quote(str(path), safe='')}", _args({"limit": limit, "offset": offset}), None))

    def read_peripheral(self, path: str, *, force: bool | None = None, limit: int | None = None, offset: int | None = None) -> PeripheralValues:
        """Read the registers of a peripheral, cluster, register or field path, decoded into their
        fields and named values.

        Only the registers the description declares are read, each at its own address and width:
        gaps between them are never read (reserved addresses can raise bus errors and safety
        alarms). Registers whose reading has a side effect and write-only registers are skipped
        and say so, unless `force`. A side effect is an SVD `readAction` (clear on read), or a
        debug view next to the register: Infineon's descriptions declare no `readAction`, but give
        FIFO and result registers a twin that reads without popping or clearing (`ASCLIN0.RXDATA`
        and `ASCLIN0.RXDATAD`, `EVADC.G0RES[0]` and `EVADC.G0RESD[0]`). A register that cannot
        be read (protected, a module not clocked) has an `error`; the others are still read.
        HSM peripherals and the SCR's SFRs are read through their core, as `read_memory` does.
        Works while the cores run.

        Args:
            path: As for `describe_peripheral`.
            force: Also read registers whose reading has a side effect (SVD `readAction`: clear on read, a FIFO pop, ...). They are skipped by default.
            limit: At most this many registers (default 200).
            offset: 
        """
        return PeripheralValues.from_dict(self._call("GET", self._session_path() + f"/peripherals/{quote(str(path), safe='')}/value", _args({"force": force, "limit": limit, "offset": offset}), None))

    def write_peripheral(self, path: str, *, value: float | bool | str) -> PeripheralWritten:
        """Write a register, or a field (read-modify-write of its register), and read it back.

        A field takes a number or the name of one of its values. When a field is written, the
        register's other write-to-clear/set/toggle fields (SVD `modifiedWriteValues`) are written
        with the value that leaves them unchanged, so they are not cleared by accident. Read-only
        registers and fields are refused (`bad_request`). A value that does not stick is reported
        in `note`: registers protected by ENDINIT or Safety ENDINIT ignore writes without the
        unlock sequence, which this operation does not do. Connected debugger clients (VS Code)
        are told to refresh their views.

        Args:
            path: A register or field (see `describe_peripheral`).
            value: A number, a decimal or `0x` string, or for a field the name of one of its values.
        """
        return PeripheralWritten.from_dict(self._call("PUT", self._session_path() + f"/peripherals/{quote(str(path), safe='')}/value", None, _args({"value": value})))

    def list_globals(self, *, limit: int | None = None, offset: int | None = None, query: str | None = None, target: str | None = None) -> GlobalList:
        """List the global and static variables of the loaded symbols, filtered by a name pattern, one
        page at a time.

        The search runs on the server and stays fast for an ELF with 10^5 globals. Each item says
        whether a DAQ point can sample it and gives the expression to sample it by. A variable in
        core-local RAM whose file is not assigned to exactly one core is a `per_core` group: its
        children are the copies of the cores, `CRASH_state@0`, `CRASH_state@1`, ... Fails with
        `not_found` when no symbols are loaded (`load_symbols`).

        Args:
            limit: Return at most this many (default 100, at most 10000).
            offset: Skip this many matches (default 0).
            query: Glob pattern for the name, case-insensitive: `*` matches any run of characters, `?` one character. `*speed*` finds every name containing `speed`. Default: all globals.
            target: Symbols of this target (name or index); default: the first core that has symbols.
        """
        return GlobalList.from_dict(self._call("GET", self._session_path() + "/globals", _args({"limit": limit, "offset": offset, "query": query, "target": target}), None))

    def symbol_children(self, *, expression: str, frame: int | None = None, limit: int | None = None, offset: int | None = None, target: str | None = None) -> SymbolChildren:
        """List the members of a struct or union, or the elements of an array, that an expression
        stands for, each with the expression that names it.

        Reads no memory unless the expression itself needs to (`p->s`, `buf[i]`). Pointers are not
        followed: evaluate `*p` or `p->x` instead. Arrays are paged with `offset` and `limit`. The
        children of a `per_core` group are its copies, one per core, each named with its `@N`.

        Args:
            expression: C expression of the parent, e.g. `g_motor`, or an `expression` that `list_globals` or `symbol_children` returned. It may end in `@N` as for `evaluate`; the children's expressions then end in it too (`g_s@2` -> `g_s.a@2`). A `per_core` group's children are its copies, one per core.
            frame: Evaluate in this stack frame of the halted `target` (0: the top frame, as `stack_trace` numbers them): its locals and parameters first, then the globals, as the VS Code Watch view does with the frame selected. Needs `target`, and no `@N`.
            limit: Return at most this many (default 100, at most 10000); a large array is paged like `list_globals`.
            offset: Skip this many children (default 0).
            target: Symbols of this target (name or index); default: the first core that has symbols, or the core of an `@N`.
        """
        return SymbolChildren.from_dict(self._call("GET", self._session_path() + "/expressions/children", _args({"expression": expression, "frame": frame, "limit": limit, "offset": offset, "target": target}), None))

    def evaluate(self, *, expression: str, frame: int | None = None, target: str | None = None) -> VariableValue:
        """Evaluate a C expression and read its value, decoded as a DAQ point decodes it.

        Without `frame` it works while the cores run: there is no stack frame, so names are
        globals and statics, as in a VS Code live watch. Core-local RAM is the copy of the core an
        `@N` names, or of the one core the symbol file is assigned to; `address` is the system-bus
        address of that copy. With `frame` (a halted `target`) names are the frame's locals and
        parameters first, as in the VS Code Watch view; a value in a register has no `address`.

        Fails with `not_found` for an unknown name, an `@N` that names no TriCore core, a frame
        the call stack does not have, or when no symbols are loaded; with `bad_request` for a
        variable in core-local RAM that needs an `@N` (the message lists the choices) or an `@N`
        that contradicts `target`; with `not_halted` / `busy` for a `frame` of a running or
        stepping target; and with `memory` when the target memory cannot be read.

        Args:
            expression: C expression: a global or static variable, a member or element of one (`g_motor.speed`, `g_buf[3]`, `p->x`), a cast or arithmetic. Locals and parameters with `frame` only.  A trailing `@N` names the TriCore core (core ID) whose core-local RAM the expression means, as in a VS Code live watch: `CRASH_state@2`, `${E0/GV:12}@1`; `@hsm` evaluates for the HSM. It is needed for a variable in core-local RAM whose symbol file is not assigned to exactly one core, and allowed (selecting that core's symbols) anywhere else.
            frame: Evaluate in this stack frame of the halted `target` (0: the top frame, as `stack_trace` numbers them): its locals and parameters first, then the globals, as the VS Code Watch view does with the frame selected. Needs `target`, and no `@N`.
            target: Use this target's symbols and read memory as it sees it (HSM-private memory, MCS program RAM); default: the first core that has symbols, or the core of an `@N`. It does not choose the core-local RAM, `@N` does; naming another core than the `@N` is a `bad_request`.
        """
        return VariableValue.from_dict(self._call("GET", self._session_path() + "/expressions", _args({"expression": expression, "frame": frame, "target": target}), None))

    def write_variable(self, *, expression: str, value: float | bool | str, frame: int | None = None, target: str | None = None) -> VariableValue:
        """Write a variable by its expression and read it back.

        Works while the cores run, like `write_memory`; a bit field is merged into its storage
        unit. With `frame` it writes a local or parameter; one in a register only in the top frame
        (a caller's registers are saved copies). Connected debugger clients (VS Code) are told to
        refresh their views. Core-local RAM and the errors are as for `evaluate`.

        Args:
            expression: C expression of the variable, as for `evaluate` (including `@N`); it must name something stored on the target.
            value: The new value: a number; `true` / `false`; for an enum, an enumerator name; for an integer, enum or pointer also a decimal or `0x` hex string, which keeps 64-bit values exact where a JSON number does not; for a float also `nan`, `inf` or `-inf`. A struct or array takes its bytes as a hex string of exactly its size.
            frame: Write a local or parameter of this stack frame (see `evaluate`). One that lives in a register can only be written in the top frame (0).
            target: Use this target's symbols and write memory as it sees it; default: the first core that has symbols, or the core of an `@N`. As for `evaluate`, `@N` chooses the core-local RAM.
        """
        return VariableValue.from_dict(self._call("PUT", self._session_path() + "/expressions", None, _args({"expression": expression, "value": value, "frame": frame, "target": target})))

    def list_locals(self, target: str, *, frame: int | None = None, values: bool | None = None) -> LocalList:
        """The locals and parameters of a stack frame of a halted target, with their values.

        The variables the VS Code Variables view shows for the frame. Each item's `expression`
        goes to `evaluate`, `write_variable` and `symbol_children` with the same `frame`. A local
        whose location the debug information does not give at the frame's PC (optimized out) is
        listed with an `error` instead of a value. Fails with `not_halted` / `busy` as
        `stack_trace` does and with `not_found` for a frame the call stack does not have; without
        symbols for the frame's code the list is empty.

        Args:
            target: Target name or index.
            frame: Stack frame (default 0, the top frame), as `stack_trace` numbers them.
            values: Read each local's value too (default true). Aggregates over 256 bytes are left to `evaluate`.
        """
        return LocalList.from_dict(self._call("GET", self._session_path() + f"/targets/{quote(str(target), safe='')}/locals", _args({"frame": frame, "values": values}), None))

    def load_orti(self, *, file: str, target: str | None = None) -> OsInfo:
        """Load an ORTI file: the AUTOSAR/OSEK OS awareness description the OS generator emits.

        Every attribute is parsed as a C expression and resolved against the loaded symbols, so
        `load_symbols` (or a VS Code debug session) has to have run first. Attributes that cannot
        be resolved are reported in `errors` and retried when they are first read, so loading
        before the board is powered is harmless.

        Args:
            file: Path to the `.ORT` / `.orti` file the OS generator wrote beside the ELF.
            target: Target whose symbols resolve the attribute expressions, and whose memory view is read. Default: the first core that has symbols.
        """
        return OsInfo.from_dict(self._call("POST", self._session_path() + "/os/orti", None, _args({"file": file, "target": target})))

    def os(self) -> OsInfo:
        """The loaded ORTI file: its object types, their attributes and how many cores it describes.

        Object types are whatever the file declared. A MICROSAR file adds `vs_ISR`, `vs_SPINLOCK`,
        `vs_EVENT` and `vs_OS_CONFIG` to the standard OSEK ones.
        """
        return OsInfo.from_dict(self._call("GET", self._session_path() + "/os", None, None))

    def os_objects(self, kind: str, *, live_only: bool | None = None, name: str | None = None) -> OsObjectList:
        """Read the objects of one ORTI type with their current attribute values.

        Values come from the target as it is now. Attributes that are constants (a stack address,
        a `sizeof`) or literal text (an alarm action) cost no bus access.

        Args:
            kind: The ORTI object type, as `os` lists it: `TASK`, `STACK`, `vs_SPINLOCK`, ...
            live_only: Skip attributes that cannot change, for a cheaper refresh. Default: false.
            name: Only this object, by name.
        """
        return OsObjectList.from_dict(self._call("GET", self._session_path() + f"/os/objects/{quote(str(kind), safe='')}", _args({"live_only": live_only, "name": name}), None))

    def os_stacks(self, *, names: list[str] | None = None) -> OsStackList:
        """Measure stack coverage: how much of each OS stack has ever been used.

        The OS fills every stack with a pattern at startup, so the high-water mark is the far end
        of the region that still holds it. Finding it means reading the untouched part of every
        stack, so this is on demand and never part of a refresh - prefer `names` to narrow it.

        A stack the debugger cannot read (a region the memory protection hides) is reported with
        its `error` rather than as unused.

        Args:
            names: Measure only these stacks, by name. Default: every stack the ORTI file describes.
        """
        return OsStackList.from_dict(self._call("GET", self._session_path() + "/os/stacks", _args({"names": names}), None))

    def tcl(self, *, command: str) -> CommandOutput:
        """Run a command of the OpenOCD-compatible Tcl console (like the telnet console).

        Args:
            command: Command line, as typed in the telnet console (e.g. `mdw 0x70000000 4`).
        """
        return CommandOutput.from_dict(self._call("POST", self._session_path() + "/tcl", None, _args({"command": command})))

    def launch(self, *, breakpoints: list[LaunchBreakpoint] | None = None, flash: LaunchFlash | None = None, orti: str | None = None, reset: LaunchResetKeyword | LaunchResetSpec | None = None, run_to: str | None = None, stop_on_entry: bool | None = None, symbols: list[str | PlacedSymbolFile] | None = None, timeout_s: int | None = None, wait_s: int | None = None) -> LaunchStatus:
        """Run the launch steps on this session: what a VS Code launch configuration does.

        In this order, each only when asked for: load the symbols, load the ORTI file, program
        the flash (after a halting reset, as the flash needs it), reset, set the breakpoints, run
        to `run_to`, halt all cores (`stop_on_entry`). The steps stop at the first failure, which
        leaves the session open to inspect.

        Returns once the steps are done or after `wait_s` seconds with phase `starting`;
        `wait_session` waits for the rest. The session keeps the arguments for `relaunch`.

        Args:
            breakpoints: Breakpoints, set before the target runs and owned by the calling client.
            flash: Images to program (VS Code `flashFiles` with `enableFlashing`).
            orti: ORTI file (VS Code `ortiFile`), loaded after the symbols.
            reset: Reset after the flash step; default: a running reset when flashing, none otherwise.
            run_to: Run to this location (a function, `file:line` or address) with a temporary breakpoint: the target is reset halted, runs and the call waits until it halts there.
            stop_on_entry: Halt all cores after the steps (VS Code `stopOnEntry`).
            symbols: Symbol files (VS Code `symbolFiles`). Each core gets the files of its architecture (TriCore ELF files the TriCore cores, an ARM ELF the HSM, `.asm` the MCS channels, `.cdb` the SCR) unless `cores` says otherwise.
            timeout_s: Seconds `run_to` waits for the breakpoint; default 30.
            wait_s: Seconds the call waits for the steps; default 60. When they take longer it returns with phase `starting`; `wait_session` waits for the rest.
        """
        return LaunchStatus.from_dict(self._call("POST", self._session_path() + "/launch", None, _args({"breakpoints": breakpoints, "flash": flash, "orti": orti, "reset": reset, "run_to": run_to, "stop_on_entry": stop_on_entry, "symbols": symbols, "timeout_s": timeout_s, "wait_s": wait_s})))

    def relaunch(self, *, skip_flash: bool | None = None, wait_s: int | None = None) -> LaunchStatus:
        """Run the last launch again, e.g. after a rebuild: flash (incremental), reset, breakpoints,
        `run_to`.

        Args:
            skip_flash: Leave the flash step out (only reset, breakpoints, `run_to`).
            wait_s: Seconds the call waits; see `launch`.
        """
        return LaunchStatus.from_dict(self._call("POST", self._session_path() + "/relaunch", None, _args({"skip_flash": skip_flash, "wait_s": wait_s})))

    def wait_session(self, *, timeout_s: int | None = None) -> LaunchStatus:
        """Wait until the launch steps are done and return how they went.

        Args:
            timeout_s: Seconds to wait at most; default 60, 0 returns the status at once.
        """
        return LaunchStatus.from_dict(self._call("GET", self._session_path() + "/launch", _args({"timeout_s": timeout_s}), None))

    def acquire_control(self, *, ttl_s: int | None = None) -> ControlInfo:
        """Take the control lease of the session for the calling client.

        While another client holds it, run control (halt, resume, step, reset), register, memory
        and variable writes, flashing, launches and Tcl commands fail with `busy`; reading never
        waits for it. Nobody holds it unless a client takes it. It expires after `ttl_s` unless
        taken again, and is released when its client ends.

        Args:
            ttl_s: Seconds until the lease expires unless it is taken again; default 600. Taking it again as its holder renews it.
        """
        return ControlInfo.from_dict(self._call("POST", self._session_path() + "/control", None, _args({"ttl_s": ttl_s})))

    def release_control(self) -> ControlInfo:
        """Release the control lease (only its holder can).
        """
        return ControlInfo.from_dict(self._call("DELETE", self._session_path() + "/control", None, None))

    def end_client(self, *, client: str) -> EndClientResult:
        """End an API client: remove its breakpoints and watchpoints and release its lease.

        The hub calls it when an MCP session ends; a script can call it for itself when done.

        Args:
            client: Client name (e.g. `mcp:3f2a...` for an MCP session of the hub, or a Python client's ID).
        """
        return EndClientResult.from_dict(self._call("POST", self._session_path() + "/clients/end", None, _args({"client": client})))
