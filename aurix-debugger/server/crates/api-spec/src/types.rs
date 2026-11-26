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
    /// Where the PC is in the source, when halted and the loaded symbols describe it. Left out
    /// while symbols are loading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<SourceLocation>,
}

/// A code address in the source: its function and line, as far as the symbols describe it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SourceLocation {
    /// The function the address is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// Offset of the address from the start of the function.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_offset: Option<u32>,
    /// Source file (as the compiler recorded its path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

/// Call stack arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StackTraceRequest {
    /// Target name or index.
    pub target: String,
    /// Return at most this many frames, from the top (default: all, up to 64).
    #[serde(default)]
    pub max_frames: Option<u32>,
}

/// A frame of a call stack.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StackFrame {
    /// 0 for the top (innermost) frame, the one the core is halted in.
    pub index: u32,
    /// Where the frame executes: the PC of the top frame, the return address of the others.
    pub pc: u32,
    /// Stack pointer of the frame (TriCore A10, Cortex-M SP), where it is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sp: Option<u32>,
    /// Where the frame's function returns to (TriCore A11, Cortex-M LR), where it is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_address: Option<u32>,
    /// Function and line. For a caller frame the call is looked up: its `pc` is the return
    /// address, which may already lie on the next line or past the calling function.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<SourceLocation>,
}

/// The call stack of a halted target.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StackTrace {
    pub target: String,
    /// Top (innermost) frame first.
    pub frames: Vec<StackFrame>,
    /// More frames exist than were returned (`max_frames`, or the limit of 64).
    pub truncated: bool,
    /// The frames are those of an instruction trace replay (VS Code shows the core at a trace
    /// row), not of the live core.
    pub replay: bool,
}

/// Address lookup arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LookupAddressRequest {
    /// Code or data address.
    pub address: Addr,
    /// Use this target's symbols (default: the first core that has symbols).
    #[serde(default)]
    pub target: Option<String>,
    /// The address is a return address (e.g. a saved A11 or LR): look up the call before it.
    #[serde(default)]
    pub return_address: bool,
}

/// What the symbols say about an address.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AddressInfo {
    pub address: u32,
    /// Function and line of code at the address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<SourceLocation>,
    /// The global or static variable the address lies in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variable: Option<String>,
    /// Offset of the address into `variable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variable_offset: Option<u32>,
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
    /// Function name; needs loaded symbols (`load_symbols`, `launch` or a VS Code debug session).
    #[serde(default)]
    pub symbol: Option<String>,
    /// Source file (with `line`); needs loaded symbols.
    #[serde(default)]
    pub file: Option<String>,
    /// Source line in `file`.
    #[serde(default)]
    pub line: Option<u32>,
    /// Targets (names or indices) to break on; default: all examined targets.
    #[serde(default)]
    pub targets: Option<Vec<String>>,
    /// C expression evaluated on the halted core (in its top stack frame): a hit only halts
    /// when it is not zero, e.g. `count > 100`. Otherwise the core is resumed at once.
    #[serde(default)]
    pub condition: Option<String>,
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
    /// The Tcl console (`wp`): only on the core, not shown in VS Code.
    Console,
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
    /// The API client that set it (`X-Aurix-Client`, e.g. `mcp:<id>`), for owner `api`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
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

/// Watchpoint arguments: an `address` or an `expression`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddWatchpointRequest {
    /// Target name or index; default: every TriCore core.
    #[serde(default)]
    pub target: Option<String>,
    /// Data address.
    #[serde(default)]
    pub address: Option<Addr>,
    /// C expression of the watched object instead of an address, as `evaluate` takes it
    /// (`g_motor.speed`, `g_buf[3]`, `CRASH_state@2`). Its address and size come from the
    /// symbols, and are looked up again when other symbols are loaded. A variable in core-local
    /// RAM is watched at its local address on its own core and at its global address on the
    /// other cores.
    #[serde(default)]
    pub expression: Option<String>,
    /// Watched bytes; default: the size of the expression's type, or 4 for an address.
    #[serde(default)]
    pub length: Option<u32>,
    /// Triggering access (default `write`).
    #[serde(default)]
    pub access: Option<WatchAccess>,
    /// C expression evaluated on the halted core (in its top stack frame): a hit only halts
    /// when it is not zero, e.g. `g_motor.speed > 100`. Otherwise the core is resumed at once,
    /// which still costs a halt on every access.
    #[serde(default)]
    pub condition: Option<String>,
}

/// A watchpoint by `id`, or by `target` and `address`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchpointRef {
    /// Watchpoint ID.
    #[serde(default)]
    pub id: Option<String>,
    /// Target name or index (with `address`).
    #[serde(default)]
    pub target: Option<String>,
    /// Data address (with `target`): removes the watchpoint planted there, on all its targets.
    #[serde(default)]
    pub address: Option<Addr>,
}

/// Where a watchpoint is planted on one target.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WatchpointInstance {
    /// Target name.
    pub target: String,
    /// Address as this target accesses it (core-local RAM: the local address on its own core).
    pub address: u32,
    pub length: u32,
    /// Why it could not be planted (e.g. no free hardware event).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A watchpoint (data breakpoint).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WatchpointInfo {
    /// Watchpoint ID.
    pub id: String,
    pub owner: BreakpointOwner,
    /// The API client that set it (`X-Aurix-Client`, e.g. `mcp:<id>`), for owner `api`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    pub enabled: bool,
    /// Requested data address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<u32>,
    /// Requested expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    pub access: WatchAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Where it is planted.
    pub instances: Vec<WatchpointInstance>,
    /// Number of hits reported to debugger clients (not counting those whose condition was
    /// false).
    pub hits: u32,
    /// Why it is not planted (e.g. symbols not loaded yet: it is planted once they are).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The watchpoints of all targets.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WatchpointList {
    pub watchpoints: Vec<WatchpointInfo>,
}

// --- Flash, Tcl ------------------------------------------------------------------------------

/// Image file type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImageType {
    Elf,
    /// Intel HEX.
    Ihex,
    /// Motorola S-records.
    Srec,
    /// Raw binary.
    Bin,
}

impl ImageType {
    /// The type name of the `flash write_images` command.
    pub fn tcl_name(self) -> &'static str {
        match self {
            ImageType::Elf => "elf",
            ImageType::Ihex => "ihex",
            ImageType::Srec => "s19",
            ImageType::Bin => "bin",
        }
    }
}

/// An image file to program: a path, or a path with an offset and its type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum FlashFile {
    Path(String),
    Placed(PlacedFlashFile),
}

/// An image file with an offset and its type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(inline)]
pub struct PlacedFlashFile {
    /// Path on the debug server's machine.
    pub path: String,
    /// Added to all addresses of the file: the load address of a binary file, which is
    /// placed at 0 otherwise.
    #[serde(default)]
    pub offset: Option<Addr>,
    /// File type; detected from the content and extension by default.
    #[serde(default, rename = "type")]
    pub kind: Option<ImageType>,
}

/// Flash programming arguments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlashWriteRequest {
    /// Image files (ELF, Intel HEX, S-records, binary) on the debug server's machine; later
    /// files win where they overlap. An entry is a path, or an object with `path`, `offset` and
    /// `type` (`elf`, `ihex`, `srec`, `bin`).
    pub files: Vec<FlashFile>,
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

// --- UCBs ------------------------------------------------------------------------------------

/// Reading the UCBs.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UcbReadRequest {
    /// One UCB by name (`BMHD0`, `DBG`, `OTP3`, ...); default: all.
    #[serde(default)]
    pub ucb: Option<String>,
    /// Use the last readout instead of reading the device again.
    #[serde(default)]
    pub cached: bool,
}

/// One staged UCB change.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UcbChange {
    /// `<UCB>.<word>.<field>` (e.g. `BMHD1.BMI_BMHDID.HWCFG`) or `<UCB>.<word>` for a whole word.
    pub field: String,
    /// A number, `yes`/`no`, an option label (e.g. `Internal flash`) or a sector list
    /// (`S0-S3, S10`, `none`).
    pub value: String,
}

/// Staging UCB changes.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UcbStageRequest {
    /// Discard all staged changes first.
    #[serde(default)]
    pub discard: bool,
    /// Changes to stage, in order.
    #[serde(default)]
    pub changes: Vec<UcbChange>,
    /// UCBs to stage the confirmation UNLOCKED -> CONFIRMED of.
    #[serde(default)]
    pub confirm: Vec<String>,
}

/// Reviewing UCB changes.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UcbReviewRequest {
    /// Review the UCB records of these image files instead of the staged changes (e.g. a
    /// post-build HEX). An entry is a path, or an object with `path`, `offset` and `type`.
    #[serde(default)]
    pub files: Vec<FlashFile>,
}

/// Comparing the UCBs with image files.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UcbCompareRequest {
    /// Image files with UCB records (e.g. a post-build HEX) on the debug server's machine; empty:
    /// compare again with the files of the last comparison.
    #[serde(default)]
    pub files: Vec<FlashFile>,
}

/// Staging UCB values from the compared image files.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UcbApplyRequest {
    /// `all`, a UCB (`DFLASH`), a word or field (`BMHD1.STAD`, `DFLASH.PROCONRAM.RAMIN`) or a
    /// password (`DBG.PW`).
    pub target: String,
}

/// Exporting UCBs to Intel HEX.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UcbExportRequest {
    /// Output file on the debug server's machine.
    pub file: String,
    /// UCBs by name; default: all (filtered by the other fields).
    #[serde(default)]
    pub ucbs: Vec<String>,
    /// Only UCBs in one of these states: `unlocked` (unconfirmed), `confirmed`, `errored`.
    #[serde(default)]
    pub states: Vec<String>,
    /// Only these groups: `boot`, `protection`, `hsm`, `otp`, `infineon`.
    #[serde(default)]
    pub classes: Vec<String>,
    /// Only UCBs that differ from the delivery state (Infineon data: from the chip's first
    /// readout).
    #[serde(default)]
    pub changed: bool,
    /// `orig`, `copy` or `both` (default) of the ORIG/COPY pairs.
    #[serde(default)]
    pub which: Option<String>,
    /// Export the staged content instead of the device's.
    #[serde(default)]
    pub staged: bool,
}

/// UCB data as the `ucb` Tcl command answers with `-json`: a readout (device, status, UCBs with
/// their words and fields), a review (changes, problems, risk, token) or an export result.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct UcbData {
    pub data: serde_json::Value,
}

// --- Trace -----------------------------------------------------------------------------------

/// A trace range: a function (by name, its whole code) or an address range.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceRange {
    #[serde(default)]
    pub function: Option<String>,
    /// Start address (number or `0x` string).
    #[serde(default)]
    pub start: Option<serde_json::Value>,
    #[serde(default)]
    pub len: Option<u32>,
}

/// A trace configuration.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceConfigRequest {
    /// `program` (default) or `data`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Observed core: 0..2 (CPU0..CPU2, default 0). The data trace sees only this core's writes.
    #[serde(default)]
    pub core: Option<u32>,
    /// Program: at most one range the PC must be inside; data: exactly one range.
    #[serde(default)]
    pub ranges: Vec<TraceRange>,
    /// `halt` (default for program), `full`, `manual` (default for data), or a function name to
    /// stop at.
    #[serde(default)]
    pub stop: Option<String>,
    /// `tsu` (default: time stamps with messages) or `ticks`.
    #[serde(default)]
    pub timestamps: Option<String>,
    /// Program trace: record which task or ISR runs besides, from the loaded ORTI file's
    /// `RUNNINGTASK` / `RUNNINGISR2` of the core (their writes, with the data trace); read with
    /// `trace_calls`. Default: off.
    #[serde(default)]
    pub context: bool,
}

/// Automatic arming on run.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceAutoRequest {
    /// The configuration to arm on every run (stop `halt` is the useful one); null: off.
    #[serde(default)]
    pub config: Option<TraceConfigRequest>,
    /// Record steps too: a source step (step over, into, out) as one recording, an instruction
    /// step on its own, each joined to the earlier recordings. Default: off.
    #[serde(default)]
    pub steps: Option<bool>,
}

/// Reading a recording.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceReadRequest {
    /// The newest rows only (default 20000).
    #[serde(default)]
    pub max: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceInsnsRequest {
    /// First row (default 0).
    #[serde(default)]
    pub from: Option<u32>,
    /// Rows (default 500, at most 5000).
    #[serde(default)]
    pub count: Option<u32>,
    /// With reverse debugging on: `history` (default) numbers the rows of every action of the
    /// history one after the other (`Act`: checkpoint index, `R`: row for `reverse_goto`),
    /// `action` only the last action's recording.
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceRegsRequest {
    /// The row (default: the last, where the core halted).
    #[serde(default)]
    pub at: Option<u32>,
    /// The registers before the row instead of after it.
    #[serde(default)]
    pub before: bool,
    /// With reverse debugging on: `history` (default) numbers the rows of every action of the
    /// history one after the other (`Act`: checkpoint index, `R`: row for `reverse_goto`),
    /// `action` only the last action's recording.
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceFindRequest {
    pub query: String,
    #[serde(default)]
    pub from: Option<u32>,
    #[serde(default)]
    pub back: bool,
    /// With reverse debugging on: `history` (default) numbers the rows of every action of the
    /// history one after the other (`Act`: checkpoint index, `R`: row for `reverse_goto`),
    /// `action` only the last action's recording.
    #[serde(default)]
    pub scope: Option<String>,
}

/// A RAM range for reverse debugging.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReverseRegion {
    /// Start address.
    #[serde(default)]
    pub start: Option<Addr>,
    /// Length in bytes.
    #[serde(default)]
    pub size: Option<u32>,
    /// A global of the ELF (for `exclude`): its symbol's address and size.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Written only by this core (its stacks, context save area): restored by difference.
    #[serde(default)]
    pub private: Option<bool>,
}

/// Switching reverse debugging on.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReverseEnableRequest {
    /// The core (default: CPU0).
    #[serde(default)]
    pub target: Option<String>,
    /// Checkpoints kept (default 50; 0: all since the last reset).
    #[serde(default)]
    pub history: Option<u32>,
    /// Record every action with the miniMCDS (default true; CPU0..CPU2).
    #[serde(default)]
    pub trace: Option<bool>,
    /// RAM saved in addition to the ELF's writable sections.
    #[serde(default)]
    pub regions: Vec<ReverseRegion>,
    /// RAM left out (large buffers).
    #[serde(default)]
    pub exclude: Vec<ReverseRegion>,
    /// Linker symbol pairs bounding the stacks (`[[low, high], ...]`; defaults for iLLD/TASKING).
    #[serde(default)]
    pub stack_symbols: Vec<[String; 2]>,
    /// Linker symbol pairs bounding the context save area.
    #[serde(default)]
    pub csa_symbols: Vec<[String; 2]>,
    /// Refuse a Step Back that cannot restore everything.
    #[serde(default)]
    pub strict: Option<bool>,
    /// Most breakpoint hits a run again may count (default 200).
    #[serde(default)]
    pub max_hits: Option<u32>,
    /// Check of the faster checkpoints: read the core's stacks and context save area in full at
    /// every checkpoint anyway and log where they differ from what the decoded writes predict.
    #[serde(default)]
    pub verify: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReverseBackRequest {
    /// `line` (default), `instruction` or `action`.
    #[serde(default)]
    pub granularity: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReverseGotoRequest {
    /// Checkpoint index in the history.
    pub index: u32,
    /// Row of that action's recording (default 0: the checkpoint itself).
    #[serde(default)]
    pub row: Option<u32>,
}

/// What the reverse debugging service answers: a status, the history or a restore report.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReverseData {
    pub data: serde_json::Value,
}

/// Trace data as the trace service answers: a plan, a status, a recording or a decoded trace.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TraceData {
    pub data: serde_json::Value,
}

// --- Symbols ---------------------------------------------------------------------------------

/// Symbol files to load.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadSymbolsRequest {
    /// Files on the machine the debug server runs on. The first is the primary ELF; the others
    /// add symbols (bootmanager, FBL, ...).
    ///
    /// A file may end in `@N` or `@N,M` (TriCore core IDs), as in VS Code's `symbolFiles`: the
    /// cores that run it, whose core-local RAM (`0xD000_0000` / `0xC000_0000` alias) holds its
    /// variables linked there. Assigned to exactly one core, such a variable reads by its plain
    /// name; otherwise it needs its core, `CRASH_state@2`. A file without the suffix is assigned
    /// to no core, also when VS Code had assigned it.
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
    /// What the probe reports about the target connection changed. Without `state` the probe
    /// cannot tell (TAS without a failure yet, winIDEA, the simulator).
    Health {
        /// `ok`, `unpowered`, `reset`, `noDebugAccess`, `noDevice` or `probeLost`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// The target supply the probe measured, when it reports the target unpowered.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vref_mv: Option<u32>,
    },
    /// The debug probe has been gone for longer than `probe_lost_timeout`; the debug server's
    /// frontends end their sessions.
    ProbeLost { why: String },
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
    /// The TriCore cores (core IDs) the file is assigned to (`load_symbols` `path@N`, or
    /// `symbolFiles` in VS Code): whose core-local RAM its variables there are in. Left out when
    /// it is assigned to none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_cores: Vec<u32>,
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
    /// The hub's session ID of this worker, when a hub started it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The worker's log file, when it writes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_file: Option<String>,
    /// Where the launch steps are (`idle` before the first `launch`).
    pub launch: LaunchPhase,
    /// The client holding the control lease, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<String>,
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
    /// (`${E0/GV:12}`). A copy in one core's core-local RAM ends in its core, `CRASH_state@2`.
    pub expression: String,
    /// C type as declared, e.g. `uint32`, `struct motor_s`, `uint8[16]`.
    pub type_name: String,
    pub class: VariableClass,
    /// Size in bytes; for a bit field, of its storage unit.
    pub size: u32,
    /// Address, when it is fixed (always for a global, except a `per_core` group). Always the
    /// system-bus address: a variable linked to the core-local alias (`0xD000_0000` DSPR,
    /// `0xC000_0000` PSPR) is reported in the DSPR/PSPR of its core, e.g. `0x60000100` for
    /// `0xD0000100` on core 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<u32>,
    /// Bit field: position of its least significant bit in the storage unit at `address`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_offset: Option<u32>,
    /// Bit field: width in bits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_size: Option<u32>,
    /// `symbol_children` lists members or elements: a struct or union, or an array of known
    /// length; or, for a `per_core` group, the copies.
    pub has_children: bool,
    /// A DAQ point decodes it to a typed value: an integer, float, bool, enum or pointer of at
    /// most 8 bytes.
    pub sampleable: bool,
    /// The compilation unit (source file) that defines a global; `list_globals` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// A group rather than one variable: it is in core-local RAM, which every TriCore core has
    /// its own of, and its symbol file is not assigned to exactly one core (`load_symbols`
    /// `path@N`, or `symbolFiles` in VS Code). Its `symbol_children` are the copies, one per core
    /// (`CRASH_state@0`, `CRASH_state@1`, ...), each with its own address. A group has no
    /// `address`, is not `sampleable`, and `evaluate` refuses its bare name. Left out when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub per_core: bool,
    /// The `expression` of the variable this is a member or element of, exactly as that one's
    /// own `expression` reads (`g_s.a@2` -> `g_s@2`, `${E0/GV:12}.x` -> `${E0/GV:12}`), so a
    /// client can nest items by it. Set for members and elements in `list_globals` (`search`
    /// and paths in `query`) and `symbol_children` results (there the request's `expression`);
    /// left out for globals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Not a match itself: an ancestor of a match on the same `list_globals` `search` page,
    /// there so that the matches can be shown as a tree. Context items come before the first
    /// match below them, once per page, and count toward neither `limit` nor `total`. Left out
    /// when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub context: bool,
}

/// Which globals to list.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListGlobalsRequest {
    /// Glob pattern for the name, case-insensitive: `*` matches any run of characters, `?` one
    /// character. `*speed*` finds every name containing `speed`. Default: all globals.
    ///
    /// With a `.` or `[`, members and elements are found too: the part before the last one is a
    /// C expression (no wildcards), the last segment a glob for a member name or an element
    /// index. Leading `*` belong to the last segment, so `*g_motor.spe*` finds the members of
    /// `g_motor` whose names contain `spe`; `g_buf[1*` finds the elements whose index starts
    /// with 1, and a closing `]` makes the index exact. A wildcard in the parent
    /// (`*.speed`) is not searched; `search` is the way to find members by name alone.
    #[serde(default)]
    pub query: Option<String>,
    /// Search-box text instead of a glob: finds globals and the members of struct globals, best
    /// matches first. A name matches when it contains the text (case-insensitive), when `*` and
    /// `?` make it match, or when the text is a run of beginnings of its words (`dcs` for
    /// `DriveCounterStamp`); then come names whose source file contains the text, or whose
    /// address starts with a `0x...` text. `DriveCounterStamp` finds
    /// `Drive_Counter_0.DriveCounterStamp`. A text with `.` or `[` also finds array elements
    /// and other children the search does not hold, by evaluating the part before the last one
    /// (`g_buf[3]`, `${E0/GV:12}.x`, `g_s.a@2`). Members carry their whole path as `name`. An
    /// empty text lists the globals by name, then the members. Not together with `query`.
    ///
    /// Each member and element names its parent's expression in `parent`; ancestors of a match
    /// that are not matches on the page come along as `context` items, so the page can be shown
    /// as a tree: `DriveCounterStamp` gives `Drive_Counter_0` (context) and
    /// `Drive_Counter_0.DriveCounterStamp` (parent `Drive_Counter_0`).
    #[serde(default)]
    pub search: Option<String>,
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

/// One page of the globals that match a query, sorted by name; then, for a path query, the
/// matching members or elements in declaration or index order. For a `search`, the best matches
/// first.
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
    /// `symbol_children` returned. It may end in `@N` as for `evaluate`; the children's
    /// expressions then end in it too (`g_s@2` -> `g_s.a@2`). A `per_core` group's children are
    /// its copies, one per core.
    pub expression: String,
    /// Skip this many children (default 0).
    #[serde(default)]
    pub offset: Option<u32>,
    /// Return at most this many (default 100, at most 10000); a large array is paged like
    /// `list_globals`.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Symbols of this target (name or index); default: the first core that has symbols, or the
    /// core of an `@N`.
    #[serde(default)]
    pub target: Option<String>,
    /// Evaluate in this stack frame of the halted `target` (0: the top frame, as `stack_trace`
    /// numbers them): its locals and parameters first, then the globals, as the VS Code Watch
    /// view does with the frame selected. Needs `target`, and no `@N`.
    #[serde(default)]
    pub frame: Option<u32>,
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
    /// `g_buf[3]`, `p->x`), a cast or arithmetic. Locals and parameters with `frame` only.
    ///
    /// A trailing `@N` names the TriCore core (core ID) whose core-local RAM the expression
    /// means, as in a VS Code live watch: `CRASH_state@2`, `${E0/GV:12}@1`; `@hsm` evaluates for
    /// the HSM. It is needed for a variable in core-local RAM whose symbol file is not assigned
    /// to exactly one core, and allowed (selecting that core's symbols) anywhere else.
    pub expression: String,
    /// Use this target's symbols and read memory as it sees it (HSM-private memory, MCS program
    /// RAM); default: the first core that has symbols, or the core of an `@N`. It does not choose
    /// the core-local RAM, `@N` does; naming another core than the `@N` is a `bad_request`.
    #[serde(default)]
    pub target: Option<String>,
    /// Evaluate in this stack frame of the halted `target` (0: the top frame, as `stack_trace`
    /// numbers them): its locals and parameters first, then the globals, as the VS Code Watch
    /// view does with the frame selected. Needs `target`, and no `@N`.
    #[serde(default)]
    pub frame: Option<u32>,
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
    /// Address of the value, when it is stored on the target (not for `g_x + 1`). Always the
    /// system-bus address: core-local RAM is reported in its core's DSPR/PSPR.
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
    /// C expression of the variable, as for `evaluate` (including `@N`); it must name something
    /// stored on the target.
    pub expression: String,
    /// The new value: a number; `true` / `false`; for an enum, an enumerator name; for an
    /// integer, enum or pointer also a decimal or `0x` hex string, which keeps 64-bit values
    /// exact where a JSON number does not; for a float also `nan`, `inf` or `-inf`. A struct or
    /// array takes its bytes as a hex string of exactly its size.
    #[schemars(schema_with = "scalar_schema")]
    pub value: serde_json::Value,
    /// Use this target's symbols and write memory as it sees it; default: the first core that has
    /// symbols, or the core of an `@N`. As for `evaluate`, `@N` chooses the core-local RAM.
    #[serde(default)]
    pub target: Option<String>,
    /// Write a local or parameter of this stack frame (see `evaluate`). One that lives in a
    /// register can only be written in the top frame (0).
    #[serde(default)]
    pub frame: Option<u32>,
}

/// Which frame's locals to list.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListLocalsRequest {
    /// Target name or index.
    pub target: String,
    /// Stack frame (default 0, the top frame), as `stack_trace` numbers them.
    #[serde(default)]
    pub frame: Option<u32>,
    /// Read each local's value too (default true). Aggregates over 256 bytes are left to
    /// `evaluate`.
    #[serde(default)]
    pub values: Option<bool>,
}

/// A local variable or parameter of a stack frame.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct LocalVariable {
    /// As for a global; `expression` is evaluated with the same `frame`.
    #[serde(flatten)]
    pub variable: VariableInfo,
    /// A parameter of the frame's function.
    pub argument: bool,
    /// The value, decoded as `evaluate` decodes it, when `values` was asked for and it could be
    /// read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_scalar_schema")]
    pub value: Option<serde_json::Value>,
    /// Why there is no value: e.g. its location is not available at the frame's PC (optimized
    /// out), or its memory cannot be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn optional_scalar_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({ "anyOf": [{ "type": "number" }, { "type": "boolean" }, { "type": "string" }, { "type": "null" }] })
}

/// The locals and parameters of a stack frame.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct LocalList {
    pub target: String,
    pub frame: u32,
    /// The frame's function, where the symbols name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// Outermost declaration first, as the VS Code Variables view lists them; a local an inner
    /// declaration of the same name shadows is left out.
    pub items: Vec<LocalVariable>,
}

// --- Clients and the control lease ----------------------------------------------------------

/// An API client, by the name it sends as `X-Aurix-Client`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClientRef {
    /// Client name (e.g. `mcp:3f2a...` for an MCP session of the hub, or a Python client's ID).
    pub client: String,
}

/// What ending a client removed.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct EndClientResult {
    /// IDs of the breakpoints and watchpoints that were removed.
    pub removed: Vec<String>,
    /// The client held the control lease, which is now free.
    pub released_control: bool,
}

/// Control lease arguments.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    /// Seconds until the lease expires unless it is taken again; default 600. Taking it again
    /// as its holder renews it.
    #[serde(default)]
    pub ttl_s: Option<u32>,
}

/// The control lease of a session.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ControlInfo {
    /// The client holding it; none: nobody, every client may control the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    /// Seconds it has been held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_s: Option<u64>,
    /// Seconds until it expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_s: Option<u64>,
}

// --- Launch ------------------------------------------------------------------------------------

/// A symbol file of a launch.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum LaunchSymbolFile {
    /// Path on the debug server's machine (a `file:` ID of an uploaded file works as well). A
    /// trailing `@N` or `@N,M` names the TriCore cores that run it (see `load_symbols`).
    Path(String),
    /// A path limited to some cores.
    Placed(PlacedSymbolFile),
}

/// A symbol file limited to some cores.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlacedSymbolFile {
    pub path: String,
    /// Cores (target names, core IDs, `*` and `?` as wildcards, e.g. `tc37x.cpu0`, `cpu0`,
    /// `mcs0.*`) the file applies to; default: every core of its architecture.
    #[serde(default)]
    pub cores: Option<Vec<String>>,
}

impl LaunchSymbolFile {
    pub fn path(&self) -> &str {
        match self {
            LaunchSymbolFile::Path(p) => p,
            LaunchSymbolFile::Placed(p) => &p.path,
        }
    }
    pub fn cores(&self) -> Option<&[String]> {
        match self {
            LaunchSymbolFile::Path(_) => None,
            LaunchSymbolFile::Placed(p) => p.cores.as_deref(),
        }
    }
}

/// Images to flash at launch.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchFlash {
    /// Image files, merged into one image (later files win where they overlap), as for
    /// `flash_write`.
    pub files: Vec<FlashFile>,
    /// Read the programmed sectors back and compare; default true.
    #[serde(default)]
    pub verify: Option<bool>,
    /// Skip sectors that already hold the image; default true.
    #[serde(default)]
    pub incremental: Option<bool>,
}

/// How to reset at launch.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum LaunchReset {
    /// `none`: no reset.
    Keyword(LaunchResetKeyword),
    /// A reset.
    Reset(LaunchResetSpec),
}

/// `none`: do not reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LaunchResetKeyword {
    None,
}

/// A reset at launch.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchResetSpec {
    /// Reset type; `reset` (through the debug adapter) is the only one.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Halt at the reset vector instead of running (VS Code's `resetSuspend`).
    #[serde(default)]
    pub halt: bool,
}

/// A breakpoint set at launch, before the target runs.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchBreakpoint {
    /// A function, `file:line` or address (`0x...`).
    pub location: String,
    #[serde(default)]
    pub condition: Option<String>,
}

/// The launch steps of a session: what a VS Code launch configuration does.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    /// Symbol files (VS Code `symbolFiles`). Each core gets the files of its architecture
    /// (TriCore ELF files the TriCore cores, an ARM ELF the HSM, `.asm` the MCS channels, `.cdb`
    /// the SCR) unless `cores` says otherwise.
    #[serde(default)]
    pub symbols: Option<Vec<LaunchSymbolFile>>,
    /// ORTI file (VS Code `ortiFile`), loaded after the symbols.
    #[serde(default)]
    pub orti: Option<String>,
    /// Images to program (VS Code `flashFiles` with `enableFlashing`).
    #[serde(default)]
    pub flash: Option<LaunchFlash>,
    /// Reset after the flash step; default: a running reset when flashing, none otherwise.
    #[serde(default)]
    pub reset: Option<LaunchReset>,
    /// Breakpoints, set before the target runs and owned by the calling client.
    #[serde(default)]
    pub breakpoints: Option<Vec<LaunchBreakpoint>>,
    /// Run to this location (a function, `file:line` or address) with a temporary breakpoint:
    /// the target is reset halted, runs and the call waits until it halts there.
    #[serde(default)]
    pub run_to: Option<String>,
    /// Halt all cores after the steps (VS Code `stopOnEntry`).
    #[serde(default)]
    pub stop_on_entry: bool,
    /// Seconds `run_to` waits for the breakpoint; default 30.
    #[serde(default)]
    pub timeout_s: Option<u32>,
    /// Seconds the call waits for the steps; default 60. When they take longer it returns with
    /// phase `starting`; `wait_session` waits for the rest.
    #[serde(default)]
    pub wait_s: Option<u32>,
}

/// Running the last launch again.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelaunchRequest {
    /// Leave the flash step out (only reset, breakpoints, `run_to`).
    #[serde(default)]
    pub skip_flash: bool,
    /// Seconds the call waits; see `launch`.
    #[serde(default)]
    pub wait_s: Option<u32>,
}

/// Waiting for the launch steps.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitSessionRequest {
    /// Seconds to wait at most; default 60, 0 returns the status at once.
    #[serde(default)]
    pub timeout_s: Option<u32>,
}

/// Where the launch steps of a session are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LaunchPhase {
    /// No launch has run.
    Idle,
    /// The steps are running.
    Starting,
    /// All steps succeeded.
    Ready,
    /// A step failed (see `error`).
    Failed,
}

/// One launch step and how it went.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct LaunchStep {
    /// `connect`, `symbols`, `orti`, `flash`, `reset`, `breakpoints`, `run_to` or
    /// `stop_on_entry`.
    pub step: String,
    pub ok: bool,
    /// Milliseconds it took.
    pub ms: u64,
    /// What it did (e.g. the flash output, the files loaded) or why it failed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// The state of a session's launch.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct LaunchStatus {
    pub phase: LaunchPhase,
    /// The steps done so far.
    pub steps: Vec<LaunchStep>,
    /// The targets after the steps (when `ready` or `failed`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<TargetInfo>,
    /// Why the launch failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl LaunchStatus {
    pub fn idle() -> LaunchStatus {
        LaunchStatus { phase: LaunchPhase::Idle, steps: Vec::new(), targets: Vec::new(), error: None }
    }
}

// --- Hub ---------------------------------------------------------------------------------------

/// How a worker connects to its board: the OpenOCD-style command line.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConnectSpec {
    /// Configuration scripts (`-f`), searched in `search_dirs` and the bundled OpenOCD scripts.
    #[serde(default)]
    pub config_files: Vec<String>,
    /// Tcl commands (`-c`), run after the scripts.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Script search directories (`-s`).
    #[serde(default)]
    pub search_dirs: Vec<String>,
    /// Adapter driver (`tas_client`, `winidea`, `bluebox`, `sim`); default: what the scripts
    /// select.
    #[serde(default)]
    pub adapter: Option<String>,
    /// Probe serial number, to refuse a second session on the same probe.
    #[serde(default)]
    pub probe: Option<String>,
    /// Allow writing UCBs in this session (`ucb write`, after a person's confirmation). Off by
    /// default; refused when an MCP client asks for it (a hub profile or the VS Code launch
    /// configuration `ucbWrite` can set it).
    #[serde(default)]
    pub ucb_write: bool,
}

/// Opening a session.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenSessionRequest {
    /// Profile on the hub machine (`list_profiles`): connection and launch defaults that the
    /// other arguments override.
    #[serde(default)]
    pub profile: Option<String>,
    /// How to connect; overrides the profile's.
    #[serde(default)]
    pub connect: Option<ConnectSpec>,
    /// Session name; default: the profile name or `session-<n>`.
    #[serde(default)]
    pub name: Option<String>,
    /// The launch steps; each field set here overrides the profile's.
    #[serde(default)]
    pub launch: Option<LaunchRequest>,
    /// Take the control lease for the calling client before the first step.
    #[serde(default)]
    pub acquire_control: bool,
    /// Close the session when a step fails, instead of leaving it open to inspect.
    #[serde(default)]
    pub close_on_error: bool,
    /// Process ID, on the hub machine, of the program the session belongs to (e.g. the IDE
    /// that opened it): the hub closes the session when that process ends, also when it crashed
    /// or was killed before it could close the session itself.
    #[serde(default)]
    pub owner_pid: Option<u32>,
    /// Seconds to wait for the worker and the launch steps; default 60. Longer starts return
    /// with phase `starting`, `wait_session` waits for the rest.
    #[serde(default)]
    pub wait_s: Option<u32>,
}

/// A session by ID or name.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionRef {
    /// Session ID or name.
    pub session: String,
}

/// Reading a session's log.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionLogRequest {
    /// Session ID or name.
    pub session: String,
    /// Lines from the end; default 200.
    #[serde(default)]
    pub lines: Option<u32>,
    /// Read from this byte offset instead, whole lines up to 1 MiB: to follow the log, pass the
    /// `next` of the previous answer. A log shorter than the offset is read from its start.
    #[serde(default)]
    pub offset: Option<u64>,
}

/// The end of a session's log, or the part from an `offset`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionLog {
    pub session: String,
    pub file: String,
    pub text: String,
    /// Byte offset after `text`: the `offset` that continues the log.
    pub next: u64,
}

/// The state of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// The worker is starting (running `init`).
    Starting,
    /// The worker is connected; see `launch` for the launch steps.
    Running,
    /// The worker exited or stopped answering.
    Exited,
}

/// A session: one board connection held by one worker process.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    /// Session ID.
    pub session: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub state: SessionState,
    /// Process ID of the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The client that opened it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_by: Option<String>,
    /// The process the session belongs to (`owner_pid` of `open_session`): it closes when that
    /// process ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
    /// Targets of the board.
    #[serde(default)]
    pub targets: Vec<String>,
    /// TCF port of the worker on the hub machine's loopback (VS Code connects there, or through
    /// `/api/v1/sessions/{session}/tcf` from another machine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcf_port: Option<u16>,
    /// Port of the worker's Tcl console on the hub machine's loopback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telnet_port: Option<u16>,
    /// Base URL of the session's operations on the hub.
    pub base_url: String,
    /// The client holding the control lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<String>,
    /// The launch steps.
    pub launch: LaunchStatus,
    /// The worker's log file.
    pub log_file: String,
    /// Seconds since it was opened.
    pub age_s: u64,
    /// Why the worker ended, when `exited`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The open sessions.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionList {
    pub sessions: Vec<SessionInfo>,
}

/// A session setup stored on the hub machine.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProfileInfo {
    pub name: String,
    /// The profile file.
    pub file: String,
    pub connect: ConnectSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchRequest>,
    /// Why the file could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Asking for a part of the reference (`reference`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReferenceRequest {
    /// `cfg` (how to write a board cfg and a profile), `commands` (all Tcl commands), a command
    /// name (`reset_config`, `suspend add`), a word to search the command descriptions for, or a
    /// file name (`tc3xx_ocds.cfg`). Empty: the list of topics.
    #[serde(default)]
    pub topic: Option<String>,
}

/// A part of the reference.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Reference {
    /// The topic asked for.
    pub topic: String,
    /// The text (markdown, or the cfg script itself).
    pub text: String,
}

/// The profiles of the hub machine.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProfileList {
    /// The directory they are read from.
    pub dir: String,
    pub profiles: Vec<ProfileInfo>,
}

/// What is serving: the hub.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct HubInfo {
    /// Always `aurix-hub`.
    pub server: String,
    pub version: String,
    pub api_version: u32,
    pub pid: u32,
    /// The hub executable; hub and workers come from the same directory.
    pub exe: String,
    pub port: u16,
    /// The address it listens on.
    pub bind: String,
    /// Clients from other machines need the token.
    pub token_required: bool,
    pub sessions: u32,
    /// MCP sessions of clients.
    pub mcp_clients: u32,
    /// A newer installation is waiting: the hub restarts from it once no session is open.
    pub update_pending: bool,
    /// The hub is registered to start at logon.
    pub autostart: bool,
    pub log_file: String,
}

/// Events of the hub (`/api/v1/events`); a session's own events are at
/// `/api/v1/sessions/{session}/events`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubEvent {
    /// A session was opened.
    SessionOpened {
        session: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opened_by: Option<String>,
    },
    /// A session's worker is connected.
    SessionRunning { session: String, name: String },
    /// A session's launch steps finished.
    SessionLaunched {
        session: String,
        name: String,
        phase: LaunchPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// A session was closed or its worker ended.
    SessionClosed {
        session: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The client did not keep up: this many events were dropped.
    Lagged { missed: u64 },
}

/// A worker registering with the hub (not an API operation: loopback only).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkerRegistration {
    pub session: String,
    pub pid: u32,
    /// The worker executable, compared with the hub's own directory: a worker of another
    /// installation is refused.
    pub exe: String,
    pub version: String,
    pub api_port: u16,
    #[serde(default)]
    pub tcf_port: Option<u16>,
    #[serde(default)]
    pub telnet_port: Option<u16>,
    #[serde(default)]
    pub adapter: Option<String>,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub log_file: Option<String>,
}

/// The hub's answer to a registration.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RegistrationReply {
    pub ok: bool,
    /// Process ID of the hub.
    pub hub_pid: u32,
    /// ID of this run of the hub, as `/health` reports it: a change means the hub was started
    /// again and the worker registers again.
    pub instance: String,
    /// Why it was refused; the worker shuts down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// --- Peripherals (SVD) -----------------------------------------------------------------------

/// Which SVD description to use.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadSvdRequest {
    /// A device the bundled SVD files describe (`TC37x`, `TC39x`, ...; see `svd` `available`),
    /// with its extra files merged (the TC3xx HSM and SCR). Case and `_` do not matter.
    #[serde(default)]
    pub device: Option<String>,
    /// An SVD file of your own instead (`.svd` or `.svd.gz`).
    #[serde(default)]
    pub file: Option<String>,
}

/// The SVD description of the session.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SvdInfo {
    /// A description is loaded. Without one, the first peripheral operation loads the bundled
    /// one of `chip_device`.
    pub loaded: bool,
    /// The bundled device it describes (absent for a file of your own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Files merged into it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_files: Vec<String>,
    /// How it was chosen: `load_svd`, or `chip` (from the target configuration's chip name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub peripherals: u32,
    pub registers: u32,
    /// Seconds the conversion took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_seconds: Option<f64>,
    /// The OpenOCD chip name of the target configuration (`tc37x`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chip: Option<String>,
    /// The bundled device that chip name stands for, if there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chip_device: Option<String>,
    /// Devices the bundled SVD files describe.
    pub available: Vec<String>,
}

/// Which peripherals to list.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListPeripheralsRequest {
    /// Case-insensitive glob over the name and the group (`*`, `?`), e.g. `STM*`, `GTM`.
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub offset: Option<u32>,
    /// At most this many (default 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PeripheralSummary {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub base_address: u32,
    /// Bytes its address blocks span.
    pub length: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Registers, those of its clusters included.
    pub registers: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PeripheralList {
    /// The bundled device, or the file of your own.
    pub device: String,
    /// All matches.
    pub total: u32,
    pub offset: u32,
    pub items: Vec<PeripheralSummary>,
}

/// What a peripheral path names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeripheralNodeKind {
    Peripheral,
    Cluster,
    Register,
    Field,
}

/// Access type of a register or field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegisterAccess {
    ReadOnly,
    ReadWrite,
    WriteOnly,
}

/// A path's registers to describe, a page at a time.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DescribePeripheralRequest {
    /// `PERIPHERAL`, `PERIPHERAL.REGISTER`, `PERIPHERAL.CLUSTER.REGISTER` or `....REGISTER.FIELD`,
    /// case-insensitive: `STM0`, `STM0.TIM0`, `GTM.TOM0.CH1.CTRL`, `SCU.CCUCON0.CLKSEL`. Arrays
    /// are named as the SVD names their copies, e.g. `STM0.CMP[0]`.
    pub path: String,
    /// Skip this many registers (default 0).
    #[serde(default)]
    pub offset: Option<u32>,
    /// At most this many registers (default 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

/// A named value of a field.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FieldValueName {
    pub value: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FieldDescription {
    pub name: String,
    pub bit_offset: u32,
    pub bit_width: u32,
    /// The field's, else its register's.
    pub access: RegisterAccess,
    /// Reading changes it (`clear`, `set`, `modify`, `modify_external`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_action: Option<String>,
    /// What writing does (`one_to_clear`, `one_to_set`, `one_to_toggle`, `zero_to_clear`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_write: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<FieldValueName>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RegisterDescription {
    pub path: String,
    pub address: u32,
    /// Bits.
    pub size: u32,
    pub access: RegisterAccess,
    pub reset_value: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_write: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub fields: Vec<FieldDescription>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PeripheralDescription {
    pub path: String,
    pub kind: PeripheralNodeKind,
    /// Base address of a peripheral or cluster, address of a register.
    pub address: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Registers under the path.
    pub total: u32,
    pub offset: u32,
    /// In address order; a field's path gives its register with that field only.
    pub registers: Vec<RegisterDescription>,
}

/// Registers to read.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadPeripheralRequest {
    /// As for `describe_peripheral`.
    pub path: String,
    /// Also read registers whose reading has a side effect (SVD `readAction`: clear on read,
    /// a FIFO pop, ...). They are skipped by default.
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub offset: Option<u32>,
    /// At most this many registers (default 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FieldReading {
    pub name: String,
    pub value: u64,
    /// The value's name, when the SVD names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RegisterReading {
    pub path: String,
    pub address: u32,
    /// Bits.
    pub size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<u64>,
    /// Why it was not read: write-only, or reading has a side effect (`force` reads it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// The read failed: e.g. the register is protected or its module is not clocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldReading>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PeripheralValues {
    pub path: String,
    pub total: u32,
    pub offset: u32,
    pub registers: Vec<RegisterReading>,
}

/// A register or field to write.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WritePeripheralRequest {
    /// A register or field (see `describe_peripheral`).
    pub path: String,
    /// A number, a decimal or `0x` string, or for a field the name of one of its values.
    #[schemars(schema_with = "scalar_schema")]
    pub value: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PeripheralWritten {
    pub path: String,
    pub address: u32,
    /// The register value written.
    pub written: u64,
    /// The register as read back (absent for one that cannot be read without a side effect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readback: Option<RegisterReading>,
    /// Something to know: the value did not stick (an ENDINIT-protected register, read-only
    /// bits), or the register could not be read first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
