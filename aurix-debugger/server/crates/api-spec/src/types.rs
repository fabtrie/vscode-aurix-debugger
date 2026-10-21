//! Request and response types. Their `///` comments document the fields in all outputs.

use std::borrow::Cow;
use std::fmt;

use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

// --- Scalars ---------------------------------------------------------------------------------

/// A 32-bit address or value: a number, or a string in decimal or `0x` hex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addr(pub u32);

impl Serialize for Addr {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u32(self.0)
    }
}

impl<'de> Deserialize<'de> for Addr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Addr, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Addr;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a 32-bit number or a decimal / 0x hex string")
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Addr, E> {
                u32::try_from(v).map(Addr).map_err(|_| E::custom(format!("{v} does not fit 32 bits")))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Addr, E> {
                u32::try_from(v).map(Addr).map_err(|_| E::custom(format!("{v} does not fit 32 bits")))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Addr, E> {
                parse_u32(v).map(Addr).ok_or_else(|| E::custom(format!("invalid number '{v}'")))
            }
        }
        d.deserialize_any(V)
    }
}

impl JsonSchema for Addr {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "Addr".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "anyOf": [
                { "type": "integer", "minimum": 0, "maximum": 4294967295u32 },
                { "type": "string", "pattern": "^(0[xX][0-9a-fA-F]{1,8}|[0-9]{1,10})$" }
            ]
        })
    }
}

/// Parses decimal or `0x` hex.
pub fn parse_u32(s: &str) -> Option<u32> {
    let t = s.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u32::from_str_radix(h, 16).ok(),
        None => t.parse().ok(),
    }
}

/// Bytes as a hex string (two digits per byte, lowest address first).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hex(pub Vec<u8>);

impl Serialize for Hex {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(self.0.len() * 2);
        for b in &self.0 {
            out.push_str(&format!("{b:02x}"));
        }
        s.serialize_str(&out)
    }
}

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Hex, D::Error> {
        let s = String::deserialize(d)?;
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        let s = s.strip_prefix("0x").unwrap_or(&s);
        if s.len() % 2 != 0 {
            return Err(de::Error::custom("hex data needs two digits per byte"));
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| de::Error::custom(format!("invalid hex digits '{}'", &s[i..i + 2]))))
            .collect::<Result<Vec<u8>, _>>()
            .map(Hex)
    }
}

impl JsonSchema for Hex {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "Hex".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": "^([0-9a-fA-F]{2})*$" })
    }
}

// --- Targets ---------------------------------------------------------------------------------

/// No arguments.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

/// A target (core).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetRef {
    /// Target name (e.g. `tc37x.cpu0`) or index, as listed by `targets`.
    pub target: String,
}

/// Run state of a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Unknown,
    Running,
    Halted,
    /// Held in reset.
    Reset,
    DebugRunning,
}

/// A target (core) and its state.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TargetInfo {
    /// Index of the target (usable instead of the name).
    pub index: u32,
    /// Target name (e.g. `tc37x.cpu0`).
    pub name: String,
    /// Core architecture (`tricore`, `cortex_m`, `gtm_mcs`).
    pub arch: String,
    pub state: RunState,
    /// Why the target halted (`breakpoint`, `debug-request`, `single-step`, `reset`, ...),
    /// `not halted` while running.
    pub reason: String,
    /// The target has been examined (is accessible and under debug control).
    pub examined: bool,
    /// False while the target has not been accessible for a while (e.g. not powered).
    pub reachable: bool,
    /// Program counter, when halted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pc: Option<u32>,
}

/// The targets.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TargetList {
    pub targets: Vec<TargetInfo>,
}

/// Resume arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResumeRequest {
    /// Target name or index.
    pub target: String,
    /// Continue at this address instead of the current PC.
    #[serde(default)]
    pub address: Option<Addr>,
}

/// State to wait for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaitFor {
    Halted,
    Running,
}

/// Wait arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitRequest {
    /// Target name or index.
    pub target: String,
    /// State to wait for (default `halted`).
    #[serde(default)]
    pub state: Option<WaitFor>,
    /// Maximum wait in milliseconds (default 5000, at most 600000).
    #[serde(default)]
    pub timeout_ms: Option<u32>,
}

/// Reset mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResetMode {
    /// Reset and let the cores run.
    Run,
    /// Reset and halt the cores at their first instruction.
    Halt,
    /// Like `halt`, then run the `reset-init` handlers of the configuration.
    Init,
}

/// Reset arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResetRequest {
    /// Reset mode (default `halt`).
    #[serde(default)]
    pub mode: Option<ResetMode>,
}

// --- Registers -------------------------------------------------------------------------------

/// Register group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegisterGroup {
    /// Core registers (data, address, PC, PSW, ...); need a halted core.
    Core,
    /// Debug registers (DBGSR, trigger registers, ...); accessible while running.
    Debug,
}

/// Register list arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadRegistersRequest {
    /// Target name or index.
    pub target: String,
    /// Only this group (default `core`).
    #[serde(default)]
    pub group: Option<RegisterGroup>,
}

/// A register.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterRef {
    /// Target name or index.
    pub target: String,
    /// Register name (e.g. `pc`, `d4`, `a10`, `psw`; aliases like `sp` work).
    pub name: String,
}

/// Register write arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteRegisterRequest {
    /// Target name or index.
    pub target: String,
    /// Register name.
    pub name: String,
    /// New value.
    pub value: Addr,
}

/// A register value.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RegisterValue {
    /// Register name.
    pub name: String,
    pub value: u32,
    pub group: RegisterGroup,
    pub description: String,
}

/// Register values of a target.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RegisterList {
    /// Target name.
    pub target: String,
    pub registers: Vec<RegisterValue>,
}

// --- Memory ----------------------------------------------------------------------------------

/// Memory read arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadMemoryRequest {
    /// Start address.
    pub address: Addr,
    /// Number of bytes (at most 65536).
    pub length: u32,
    /// Read as this core sees memory (HSM-private memory, MCS program RAM); default: the
    /// system bus.
    #[serde(default)]
    pub target: Option<String>,
}

/// Memory contents.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MemoryData {
    /// Start address.
    pub address: u32,
    /// The bytes as hex string.
    pub data: Hex,
}

/// Memory write arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteMemoryRequest {
    /// Start address.
    pub address: Addr,
    /// The bytes as hex string (at most 65536 bytes).
    pub data: Hex,
    /// Write as this core sees memory (HSM-private memory, MCS program RAM); default: the
    /// system bus.
    #[serde(default)]
    pub target: Option<String>,
}

/// A written memory range.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MemoryWritten {
    /// Start address.
    pub address: u32,
    /// Number of bytes written.
    pub length: u32,
}

// --- Breakpoints -----------------------------------------------------------------------------

/// Where a breakpoint is: exactly one of `address`, `symbol` or `file` with `line`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddBreakpointRequest {
    /// Code address.
    #[serde(default)]
    pub address: Option<Addr>,
    /// Function name; needs the symbols loaded by a VS Code debug session.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Source file (with `line`); needs the symbols loaded by a VS Code debug session.
    #[serde(default)]
    pub file: Option<String>,
    /// Source line in `file`.
    #[serde(default)]
    pub line: Option<u32>,
    /// Targets (names or indices) to break on; default: all examined targets.
    #[serde(default)]
    pub targets: Option<Vec<String>>,
}

/// A breakpoint.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BreakpointRef {
    /// Breakpoint ID.
    pub id: String,
}

/// Who set a breakpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BreakpointOwner {
    /// This API (REST, MCP, Python): stays until removed.
    Api,
    /// A debugger client (VS Code): removed when it disconnects.
    Client,
}

/// Where a breakpoint is planted on one target.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BreakpointInstance {
    /// Target name.
    pub target: String,
    pub address: u32,
    /// Source file of the address, when the symbols are loaded (by a VS Code debug session).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Source line of the address in `file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Why it could not be planted (e.g. no free hardware breakpoint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A breakpoint and its state.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BreakpointInfo {
    /// Breakpoint ID.
    pub id: String,
    pub owner: BreakpointOwner,
    pub enabled: bool,
    /// Requested code address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<u32>,
    /// Requested function name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Requested source file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Source line (the actual line with code, for a file/line breakpoint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Where it is planted.
    pub instances: Vec<BreakpointInstance>,
    /// Number of hits reported to debugger clients.
    pub hits: u32,
    /// Why it is not planted (e.g. symbols not loaded yet: it is planted once they are).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The breakpoints of all clients.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BreakpointList {
    pub breakpoints: Vec<BreakpointInfo>,
}

// --- Watchpoints -----------------------------------------------------------------------------

/// Access that triggers a watchpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WatchAccess {
    Read,
    Write,
    /// Read or write.
    Access,
}

/// Watchpoint arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddWatchpointRequest {
    /// Target name or index.
    pub target: String,
    /// Data address.
    pub address: Addr,
    /// Watched bytes (default 4).
    #[serde(default)]
    pub length: Option<u32>,
    /// Triggering access (default `write`).
    #[serde(default)]
    pub access: Option<WatchAccess>,
}

/// A watchpoint by target and address.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchpointRef {
    /// Target name or index.
    pub target: String,
    /// Data address.
    pub address: Addr,
}

/// A watchpoint.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WatchpointInfo {
    pub id: u32,
    /// Target name.
    pub target: String,
    pub address: u32,
    pub length: u32,
    pub access: WatchAccess,
}

/// The watchpoints of all targets.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WatchpointList {
    pub watchpoints: Vec<WatchpointInfo>,
}

// --- Flash, Tcl ------------------------------------------------------------------------------

/// Flash programming arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlashWriteRequest {
    /// Image files (ELF, Intel HEX) on the debug server's machine; later files win where they
    /// overlap.
    pub files: Vec<String>,
    /// Erase programmed sectors first (needed unless they are erased).
    #[serde(default)]
    pub erase: bool,
    /// Skip sectors that already hold the image data.
    #[serde(default)]
    pub incremental: bool,
    /// Read back and compare after programming.
    #[serde(default)]
    pub verify: bool,
}

/// Flash verification arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlashVerifyRequest {
    /// Image file on the debug server's machine.
    pub file: String,
}

/// Tcl command arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TclRequest {
    /// Command line, as typed in the telnet console (e.g. `mdw 0x70000000 4`).
    pub command: String,
}

/// Output of a command.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct CommandOutput {
    /// Log output and result.
    pub output: String,
}

// --- Symbols ---------------------------------------------------------------------------------

/// Symbol files to load.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadSymbolsRequest {
    /// Files on the machine the debug server runs on. The first is the primary ELF; the others
    /// add symbols (bootmanager, FBL, ...).
    pub files: Vec<String>,
    /// Targets (names or indices) to load them for; default: all cores.
    #[serde(default)]
    pub targets: Option<Vec<String>>,
}

/// The symbol files of one target.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SymbolsForTarget {
    /// Target name.
    pub target: String,
    /// Loaded files, primary first.
    pub files: Vec<String>,
    /// Functions with debug information.
    pub functions: u32,
    /// Global and static variables that have a fixed address.
    pub globals: u32,
    /// Source files in the line table.
    pub source_files: u32,
}

/// The symbol files loaded per target.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SymbolsInfo {
    /// Only targets that have symbols.
    pub targets: Vec<SymbolsForTarget>,
    /// How long the load took.
    pub load_seconds: f64,
}

// --- Events ----------------------------------------------------------------------------------

/// An event of the stream `GET /api/v1/events` (server-sent events; the SSE event name is
/// `type`). Includes the actions of all clients, e.g. a halt from VS Code.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiEvent {
    /// A target halted.
    Halted {
        target: String,
        /// Halt reason (`breakpoint`, `debug-request`, `single-step`, ...).
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pc: Option<u32>,
    },
    /// A target runs.
    Resumed { target: String },
    /// A device reset was noticed.
    ResetDetected,
    /// A requested reset finished.
    ResetDone,
    /// A target became accessible.
    Examined { target: String },
    /// A target has not been accessible for a while (e.g. not powered).
    Unreachable { target: String },
    /// Memory written through this API.
    MemoryChanged {
        /// Core view used, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        address: u32,
        length: u32,
    },
    /// A register written through this API.
    RegisterChanged { target: String, name: String },
    /// Breakpoints added, changed or removed by any client.
    BreakpointsChanged { added: Vec<String>, changed: Vec<String>, removed: Vec<String> },
    /// A log line of the debug server.
    Log { text: String },
    /// The client did not keep up: this many events were dropped.
    Lagged { missed: u64 },
}

// --- AUTOSAR OS awareness (ORTI) -------------------------------------------------------------

/// Which ORTI file to load.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct LoadOrtiRequest {
    /// Path to the `.ORT` / `.orti` file the OS generator wrote beside the ELF.
    pub file: String,
    /// Target whose symbols resolve the attribute expressions, and whose memory view is read.
    /// Default: the first core that has symbols.
    #[serde(default)]
    pub target: Option<String>,
}

/// One attribute of an ORTI object type.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OsAttributeInfo {
    /// Name as the ORTI file declares it, e.g. `RUNNINGTASK`, `vs_HomePriority`.
    pub name: String,
    /// The description the generator gave it, suitable as a column header.
    pub description: String,
    /// `ctype` (a number), `enum` (a name from a fixed list) or `string` (literal text).
    pub kind: String,
    /// Declared `NAME[]`: it has one value per logical core. This is the ORTI 2.3 SMP extension.
    pub per_core: bool,
    /// The value can change while the target runs, so it is worth re-reading.
    pub live: bool,
}

/// One ORTI object type.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OsTypeInfo {
    /// `OS`, `TASK`, `STACK`, `vs_SPINLOCK`, ... Generators invent their own types, so this is
    /// whatever the file declared.
    pub name: String,
    /// The description the generator gave the collection, e.g. `Stacks`.
    pub description: String,
    /// Object instances of this type.
    pub count: u32,
    pub attributes: Vec<OsAttributeInfo>,
}

/// The loaded OS awareness description.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OsInfo {
    /// The ORTI file, or null when none is loaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The `IMPLEMENTATION` name, e.g. `Vector_ORTI`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implementation: Option<String>,
    /// ORTI semantics version. `2.3` is the unofficial SMP extension, where the OS attributes
    /// are arrays indexed by logical core.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Logical cores the file describes.
    pub cores: u32,
    /// The target whose symbols the attribute expressions were resolved against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub types: Vec<OsTypeInfo>,
    /// Attributes that could not be planned, with the reason. They are read again on demand, so
    /// entries here are usually attributes planned while the board was not reachable.
    pub errors: Vec<String>,
}

/// Which objects to read.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OsObjectsRequest {
    /// The ORTI object type, as `os` lists it: `TASK`, `STACK`, `vs_SPINLOCK`, ...
    pub kind: String,
    /// Only this object, by name.
    #[serde(default)]
    pub name: Option<String>,
    /// Skip attributes that cannot change, for a cheaper refresh. Default: false.
    #[serde(default)]
    pub live_only: Option<bool>,
}

/// One attribute value of one object.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OsAttributeValue {
    pub name: String,
    /// Logical core, for an attribute declared per core.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub core: Option<u32>,
    /// The value to display: an enumerator name, a number, or literal text.
    pub value: String,
    /// The raw integer behind `value`, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<i64>,
    /// The ORTI object this value points at, when the enumerator names one, e.g. the stack of a
    /// task or the owner of a spinlock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Why the value could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One object instance and its attribute values.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OsObject {
    pub name: String,
    pub attributes: Vec<OsAttributeValue>,
}

/// Objects of one ORTI type, read now.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OsObjectList {
    pub kind: String,
    pub objects: Vec<OsObject>,
    /// Bus accesses this call made. Constants and literal text cost none.
    pub reads: u32,
}

/// Which stacks to measure.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OsStacksRequest {
    /// Measure only these stacks, by name. Default: every stack the ORTI file describes.
    #[serde(default)]
    pub names: Option<Vec<String>>,
}

/// One stack's coverage.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OsStack {
    pub name: String,
    /// Lowest address of the region.
    pub base: u32,
    pub size: u32,
    /// `DOWN` or `UP`: which end keeps the fill pattern.
    pub direction: String,
    /// The word the OS fills the stack with at startup.
    pub pattern: u32,
    /// Bytes ever used (the high-water mark). Absent when it could not be measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<u32>,
    /// Why it could not be measured, e.g. a region the memory protection hides from the debugger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Stack coverage.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OsStackList {
    pub stacks: Vec<OsStack>,
    /// Bus accesses this call made. Measuring is the expensive part of OS awareness.
    pub reads: u32,
}

// --- Server ----------------------------------------------------------------------------------

/// A target as `info` reports it.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct InfoTarget {
    /// Target name (e.g. `tc37x.cpu0`).
    pub name: String,
    /// The target has been examined (is accessible and under debug control).
    pub examined: bool,
    /// False while the target has not been accessible for a while (e.g. not powered).
    pub reachable: bool,
}

/// A loaded symbol file and the targets it is loaded for.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct InfoSymbolFile {
    /// Path on the debug server's machine.
    pub path: String,
    /// Targets whose symbol set holds the file.
    pub targets: Vec<String>,
}

/// The debug server, its probe connection and what is loaded.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ServerInfo {
    /// Always `aurix-dbg`.
    pub server: String,
    /// Version of the server.
    pub version: String,
    /// Revision of this API. It is raised whenever an operation or a field is added or changed,
    /// so a client can check for what it needs instead of parsing `version`.
    pub api_version: u32,
    /// Process ID of the server.
    pub pid: u32,
    /// The adapter driver in use (`tas_client`, `sim`, `winidea`), once one is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// The adapter is connected to its probe.
    pub connected: bool,
    /// At least one target is examined.
    pub examined: bool,
    pub targets: Vec<InfoTarget>,
    /// The loaded symbol files (by `load_symbols` or a VS Code debug session).
    pub symbol_files: Vec<InfoSymbolFile>,
    /// Symbol files are being loaded right now; `symbol_files` lists what was loaded before.
    pub symbols_loading: bool,
    /// Milliseconds since the debug core last finished a job or a run-state poll. It stays below
    /// the polling period while the server is responsive and grows while one long operation
    /// (flashing, a reset, a Tcl script) holds the core; the other operations wait for it.
    pub core_lag_ms: u64,
}

// --- Variables -------------------------------------------------------------------------------

/// What a variable holds, from its type with typedefs and qualifiers stripped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VariableClass {
    /// Unsigned integer.
    Uint,
    /// Signed integer.
    Int,
    Float,
    /// `_Bool` / `bool`.
    Bool,
    /// Enumeration; its value is the enumerator name.
    Enum,
    /// Pointer; its value is the address it holds.
    Pointer,
    Array,
    Struct,
    Union,
    Function,
    /// No usable type information (`void`, a label without debug information of its type).
    Unknown,
}

/// A variable, or a member or element of one, as the symbols describe it.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct VariableInfo {
    /// Name to show: the variable, the member, or the index (`[3]`).
    pub name: String,
    /// C expression naming exactly this object, for `evaluate`, `write_variable`,
    /// `symbol_children` and the `expression` of a DAQ point. It is the plain name unless another
    /// global of the same name would be found first; then it is a symbol reference
    /// (`${E0/GV:12}`).
    pub expression: String,
    /// C type as declared, e.g. `uint32`, `struct motor_s`, `uint8[16]`.
    pub type_name: String,
    pub class: VariableClass,
    /// Size in bytes; for a bit field, of its storage unit.
    pub size: u32,
    /// Address, when it is fixed (always for a global).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<u32>,
    /// Bit field: position of its least significant bit in the storage unit at `address`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_offset: Option<u32>,
    /// Bit field: width in bits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_size: Option<u32>,
    /// `symbol_children` lists members or elements: a struct or union, or an array of known
    /// length.
    pub has_children: bool,
    /// A DAQ point decodes it to a typed value: an integer, float, bool, enum or pointer of at
    /// most 8 bytes.
    pub sampleable: bool,
    /// The compilation unit (source file) that defines a global; `list_globals` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// Which globals to list.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListGlobalsRequest {
    /// Glob pattern for the name, case-insensitive: `*` matches any run of characters, `?` one
    /// character. `*speed*` finds every name containing `speed`. Default: all globals.
    #[serde(default)]
    pub query: Option<String>,
    /// Skip this many matches (default 0).
    #[serde(default)]
    pub offset: Option<u32>,
    /// Return at most this many (default 100, at most 10000).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Symbols of this target (name or index); default: the first core that has symbols.
    #[serde(default)]
    pub target: Option<String>,
}

/// One page of the globals that match a query, sorted by name.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GlobalList {
    /// Matches in all, not only on this page.
    pub total: u32,
    /// Index of the first item among the matches.
    pub offset: u32,
    pub items: Vec<VariableInfo>,
}

/// Whose children to list.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymbolChildrenRequest {
    /// C expression of the parent, e.g. `g_motor`, or an `expression` that `list_globals` or
    /// `symbol_children` returned.
    pub expression: String,
    /// Skip this many children (default 0).
    #[serde(default)]
    pub offset: Option<u32>,
    /// Return at most this many (default 100, at most 10000); a large array is paged like
    /// `list_globals`.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Symbols of this target (name or index); default: the first core that has symbols.
    #[serde(default)]
    pub target: Option<String>,
}

/// The members of a struct or union, or the elements of an array. Anything else has none.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SymbolChildren {
    /// The parent expression.
    pub expression: String,
    /// The parent's type.
    pub type_name: String,
    /// Children in all (members, or array elements), not only on this page.
    pub total: u32,
    /// Index of the first item.
    pub offset: u32,
    pub items: Vec<VariableInfo>,
}

/// An expression to evaluate.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvaluateRequest {
    /// C expression: a global or static variable, a member or element of one (`g_motor.speed`,
    /// `g_buf[3]`, `p->x`), a cast or arithmetic. There is no stack frame, so no locals.
    pub expression: String,
    /// Use this target's symbols and read memory as it sees it (HSM-private memory, MCS program
    /// RAM); default: the first core that has symbols.
    #[serde(default)]
    pub target: Option<String>,
}

/// A value: a number, a bool, or a string (an enumerator name, a number, or hex bytes).
fn scalar_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({ "anyOf": [{ "type": "number" }, { "type": "boolean" }, { "type": "string" }] })
}

/// The value of an expression, read now.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct VariableValue {
    pub expression: String,
    /// C type, e.g. `uint32`, `enum state_e`.
    pub type_name: String,
    pub class: VariableClass,
    /// Size in bytes; for a bit field, of its storage unit.
    pub size: u32,
    /// Address of the value, when it is stored on the target (not for `g_x + 1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<u32>,
    /// Bit field: position of its least significant bit in the storage unit at `address`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_offset: Option<u32>,
    /// Bit field: width in bits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_size: Option<u32>,
    /// Decoded as a DAQ point decodes it: a number for an integer, float or pointer, `true` /
    /// `false` for a bool, the enumerator name for an enum (the number if it matches none),
    /// `nan` / `inf` / `-inf` for a float that is not finite, and the bytes as hex for anything
    /// else or larger than 8 bytes.
    #[schemars(schema_with = "scalar_schema")]
    pub value: serde_json::Value,
    /// The bytes as read, lowest address first: the whole storage unit for a bit field.
    pub raw_hex: Hex,
    /// `symbol_children` lists members or elements.
    pub has_children: bool,
    /// A DAQ point decodes it to a typed value.
    pub sampleable: bool,
}

/// A variable to write.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteVariableRequest {
    /// C expression of the variable, as for `evaluate`; it must name something stored on the
    /// target.
    pub expression: String,
    /// The new value: a number; `true` / `false`; for an enum, an enumerator name; for an
    /// integer, enum or pointer also a decimal or `0x` hex string, which keeps 64-bit values
    /// exact where a JSON number does not; for a float also `nan`, `inf` or `-inf`. A struct or
    /// array takes its bytes as a hex string of exactly its size.
    #[schemars(schema_with = "scalar_schema")]
    pub value: serde_json::Value,
    /// Use this target's symbols and write memory as it sees it; default: the first core that has
    /// symbols.
    #[serde(default)]
    pub target: Option<String>,
}
