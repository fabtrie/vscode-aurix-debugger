//! TriCore / AURIX TC3xx architecture constants.

/// OCDS status and enable (SCU), TC3xx.
pub const OSTATE: u32 = 0xF000_0480;
pub const OEC: u32 = 0xF000_0478;
pub const OSTATE_OEN: u32 = 1;
pub const OSTATE_LOCKED: u32 = 1 << 16;

/// CBS trigger line control (OTGS TL1 force).
pub const CBS_TLC: u32 = 0xF000_0490;

// Core special function register offsets inside the CSFR window
// (address = dbgbase + 0x10000 + offset).
pub const PCXI: u16 = 0xFE00;
pub const PSW: u16 = 0xFE04;
pub const PC: u16 = 0xFE08;
pub const SYSCON: u16 = 0xFE14;
pub const CPU_ID: u16 = 0xFE18;
pub const BIV: u16 = 0xFE20;
pub const BTV: u16 = 0xFE24;
pub const ISP: u16 = 0xFE28;
pub const ICR: u16 = 0xFE2C;
pub const PPRS: u16 = 0xFE34;
pub const FCX: u16 = 0xFE38;
pub const LCX: u16 = 0xFE3C;
pub const COMPAT: u16 = 0x9400;
pub const BOOTCON: u16 = 0xFE60;
pub const TCCON: u16 = 0xFE6C;
pub const DBGSR: u16 = 0xFD00;
pub const EXEVT: u16 = 0xFD08;
pub const CREVT: u16 = 0xFD0C;
pub const SWEVT: u16 = 0xFD10;
pub const DBGACT: u16 = 0xFD14;
pub const TRIG_ACC: u16 = 0xFD30;
pub const DMS: u16 = 0xFD40;
pub const DCX: u16 = 0xFD44;
pub const DBGTCR: u16 = 0xFD48;

pub const fn trxevt(n: usize) -> u16 {
    0xF000 + (n as u16) * 8
}
pub const fn trxadr(n: usize) -> u16 {
    0xF004 + (n as u16) * 8
}

pub const NUM_TRIGGERS: usize = 8;

pub const DBGSR_DE: u32 = 1 << 0;
pub const DBGSR_HALTED: u32 = 1 << 1;
pub const DBGSR_HALT_WRITE: u32 = 1 << 2;
pub const DBGSR_HALT_REQ: u32 = 0x6;
pub const DBGSR_RESUME: u32 = 0x4;
/// DBGSR.SUSP: the core's suspend-out signal (suspends e.g. the STMs with OCS.SUS set).
/// Written directly by every DBGSR write, so resume writes must carry it over.
pub const DBGSR_SUSP: u32 = 1 << 4;
pub const SYSCON_BHALT: u32 = 1 << 24;

/// SWEVT value making the DEBUG instruction halt the core (TC1.6.x).
pub const SWEVT_HALT_TC16: u32 = 0xA;
pub const SWEVT_HALT_TC18: u32 = 0x9;

/// DBGSR.EVTSRC values.
pub const EVTSRC_EXTERNAL: u32 = 0;
pub const EVTSRC_CORE_REGISTER: u32 = 1;
pub const EVTSRC_SOFTWARE: u32 = 2;
pub const EVTSRC_TRIGGER0: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegGroup {
    General,
    System,
    Debug,
}

#[derive(Clone, Copy, Debug)]
pub struct RegDef {
    pub name: &'static str,
    pub offset: u16,
    /// Register number in tricore-elf-gdb's built-in numbering.
    pub gdb: Option<u8>,
    /// DWARF register number (GHS and GCC: D0-D15 = 0-15, A0-A15 = 16-31).
    pub dwarf: Option<u16>,
    pub group: RegGroup,
    pub description: &'static str,
}

const fn gp(name: &'static str, offset: u16, n: u8, description: &'static str) -> RegDef {
    RegDef { name, offset, gdb: Some(n), dwarf: Some(n as u16), group: RegGroup::General, description }
}
const fn sys(name: &'static str, offset: u16, gdb: Option<u8>, description: &'static str) -> RegDef {
    RegDef { name, offset, gdb, dwarf: None, group: RegGroup::System, description }
}
const fn dbg(name: &'static str, offset: u16, description: &'static str) -> RegDef {
    RegDef { name, offset, gdb: None, dwarf: None, group: RegGroup::Debug, description }
}

/// Register table. Indices 0..=41 follow tricore-elf-gdb's numbering.
pub static REGS: &[RegDef] = &[
    gp("d0", 0xFF00, 0, "Data register 0 (return value)"),
    gp("d1", 0xFF04, 1, "Data register 1"),
    gp("d2", 0xFF08, 2, "Data register 2 (return value)"),
    gp("d3", 0xFF0C, 3, "Data register 3"),
    gp("d4", 0xFF10, 4, "Data register 4 (argument)"),
    gp("d5", 0xFF14, 5, "Data register 5 (argument)"),
    gp("d6", 0xFF18, 6, "Data register 6 (argument)"),
    gp("d7", 0xFF1C, 7, "Data register 7 (argument)"),
    gp("d8", 0xFF20, 8, "Data register 8"),
    gp("d9", 0xFF24, 9, "Data register 9"),
    gp("d10", 0xFF28, 10, "Data register 10"),
    gp("d11", 0xFF2C, 11, "Data register 11"),
    gp("d12", 0xFF30, 12, "Data register 12"),
    gp("d13", 0xFF34, 13, "Data register 13"),
    gp("d14", 0xFF38, 14, "Data register 14"),
    gp("d15", 0xFF3C, 15, "Data register 15 (implicit data)"),
    gp("a0", 0xFF80, 16, "Address register 0 (global, small data)"),
    gp("a1", 0xFF84, 17, "Address register 1 (global, small data)"),
    gp("a2", 0xFF88, 18, "Address register 2"),
    gp("a3", 0xFF8C, 19, "Address register 3"),
    gp("a4", 0xFF90, 20, "Address register 4 (argument, return value)"),
    gp("a5", 0xFF94, 21, "Address register 5 (argument)"),
    gp("a6", 0xFF98, 22, "Address register 6 (argument)"),
    gp("a7", 0xFF9C, 23, "Address register 7 (argument)"),
    gp("a8", 0xFFA0, 24, "Address register 8 (global)"),
    gp("a9", 0xFFA4, 25, "Address register 9 (global)"),
    gp("a10", 0xFFA8, 26, "Address register 10 (stack pointer)"),
    gp("a11", 0xFFAC, 27, "Address register 11 (return address)"),
    gp("a12", 0xFFB0, 28, "Address register 12"),
    gp("a13", 0xFFB4, 29, "Address register 13"),
    gp("a14", 0xFFB8, 30, "Address register 14"),
    gp("a15", 0xFFBC, 31, "Address register 15 (implicit base)"),
    sys("lcx", LCX, Some(32), "Free CSA list limit pointer"),
    sys("fcx", FCX, Some(33), "Free CSA list head pointer"),
    sys("pcxi", PCXI, Some(34), "Previous context information"),
    sys("psw", PSW, Some(35), "Program status word"),
    sys("pc", PC, Some(36), "Program counter"),
    sys("icr", ICR, Some(37), "Interrupt control register"),
    sys("isp", ISP, Some(38), "Interrupt stack pointer"),
    sys("btv", BTV, Some(39), "Base of trap vector table"),
    sys("biv", BIV, Some(40), "Base of interrupt vector table"),
    sys("syscon", SYSCON, Some(41), "System configuration"),
    // PPRS (0xFE34), BOOTCON (0xFE60) and DBGACT (0xFD14) are not readable on
    // TC1.6.2P (TC37x, verified on hardware) and are therefore not listed.
    sys("cpu_id", CPU_ID, None, "CPU identification"),
    sys("compat", COMPAT, None, "Compatibility control"),
    dbg("dbgsr", DBGSR, "Debug status register"),
    dbg("exevt", EXEVT, "External event register"),
    dbg("crevt", CREVT, "Core register access event"),
    dbg("swevt", SWEVT, "Software debug event"),
    dbg("trig_acc", TRIG_ACC, "Trigger address"),
    dbg("dms", DMS, "Debug monitor start address"),
    dbg("dcx", DCX, "Debug context save area pointer"),
    dbg("dbgtcr", DBGTCR, "Debug trap control"),
];

pub const REG_BTV: usize = 39;
pub const REG_ICR: usize = 37;
pub const REG_PC: usize = 36;
pub const REG_PSW: usize = 35;
pub const REG_PCXI: usize = 34;

/// ICR.IE: global interrupt enable.
pub const ICR_IE: u32 = 1 << 15;
/// PCXI.PIE: IE saved with a context (restored by RFE; RET leaves ICR.IE alone,
/// verified on the TC377).
pub const PCXI_PIE: u32 = 1 << 21;
/// Trap vector table size: 8 trap classes of 32 bytes from BTV.
pub const TRAP_TABLE_SIZE: u32 = 0x100;
pub const REG_A10: usize = 26;
pub const REG_A11: usize = 27;

pub fn reg_index(name: &str) -> Option<usize> {
    let n = name.to_ascii_lowercase();
    let n = match n.as_str() {
        "pcx" => "pcxi",
        "sp" => "a10",
        "ra" => "a11",
        "fpc" => "fcx",
        other => other,
    };
    REGS.iter().position(|r| r.name == n).or_else(|| n.parse::<usize>().ok().filter(|&i| i < REGS.len()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    V1_6,
    V1_6_2,
    V1_8,
}

impl Version {
    pub fn name(self) -> &'static str {
        match self {
            Version::V1_6 => "1.6P/E",
            Version::V1_6_2 => "1.6.2P",
            Version::V1_8 => "1.8",
        }
    }
}

/// Decodes CPU_ID into the TriCore core version.
pub fn decode_cpu_id(id: u32) -> Option<Version> {
    match (id & 0xFF_FF00, id & 0xFF) {
        (0xC0_C000, 0x31 | 0x32) => Some(Version::V1_8),
        (0xC0_C000, 0x21) => Some(Version::V1_6_2),
        (0xC0_C000, 0x11 | 0x12) => Some(Version::V1_6),
        (0xB7_C000, _) => Some(Version::V1_6),
        _ => None,
    }
}
