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
