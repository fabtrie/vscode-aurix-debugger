//! TriCore / AURIX TC3xx architecture constants.

/// OCDS status and enable (SCU), TC3xx.
pub const OSTATE: u32 = 0xF000_0480;
pub const OEC: u32 = 0xF000_0478;
pub const OSTATE_OEN: u32 = 1;
pub const OSTATE_LOCKED: u32 = 1 << 16;

/// CBS trigger line control (OTGS TL1 force).
pub const CBS_TLC: u32 = 0xF000_0490;
/// CBS_TLS: trigger line state; bit 1 = TL1 active.
pub const CBS_TLS: u32 = 0xF000_0470;
pub const CBS_TLS_TL1: u32 = 1 << 1;
/// CBS_TLCHS: writing 2 clears a latched TL1 (capture-and-hold).
pub const CBS_TLCHS: u32 = 0xF000_049C;

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
/// TRxEVT.SUSP: suspend-out state loaded into DBGSR.SUSP when the trigger
/// fires (the old state goes to DBGSR.PREVSUSP; observed on the TC377).
pub const TREVT_SUSP: u32 = 1 << 5;
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
    TRICORE.reg_index(name)
}

/// Register set and register roles of a core architecture.
#[derive(Debug)]
pub struct ArchInfo {
    /// Target type name (`target create <name> <type>`).
    pub name: &'static str,
    pub regs: &'static [RegDef],
    pub pc: usize,
    pub sp: usize,
    /// Return address register.
    pub ra: usize,
    /// Alternative register names.
    pub aliases: &'static [(&'static str, &'static str)],
    pub description: &'static str,
}

impl ArchInfo {
    /// Register index by name, alias or number.
    pub fn reg_index(&self, name: &str) -> Option<usize> {
        let n = name.to_ascii_lowercase();
        let n = self.aliases.iter().find(|(a, _)| *a == n).map(|(_, r)| r.to_string()).unwrap_or(n);
        self.regs.iter().position(|r| r.name == n).or_else(|| n.parse::<usize>().ok().filter(|&i| i < self.regs.len()))
    }
}

pub static TRICORE: ArchInfo = ArchInfo {
    name: "tricore",
    regs: REGS,
    pc: REG_PC,
    sp: REG_A10,
    ra: REG_A11,
    aliases: &[("pcx", "pcxi"), ("sp", "a10"), ("ra", "a11"), ("fpc", "fcx")],
    description: "TriCore general purpose and system registers",
};

// --- Cortex-M3 (TC3xx HSM) ---------------------------------------------------------------

/// DCRSR selector of a Cortex-M core register. Byte fields of the combined
/// CONTROL/FAULTMASK/BASEPRI/PRIMASK register (selector 20) carry the byte
/// number plus one in the high byte.
const fn cm(name: &'static str, sel: u16, dwarf: Option<u16>, group: RegGroup, description: &'static str) -> RegDef {
    RegDef { name, offset: sel, gdb: None, dwarf, group, description }
}

/// Offset of a memory-mapped Cortex-M debug register: its address is `0xE000_0000 | offset`.
const fn cm_dbg(name: &'static str, offset: u16, description: &'static str) -> RegDef {
    RegDef { name, offset, gdb: None, dwarf: None, group: RegGroup::Debug, description }
}

/// Cortex-M register table (DWARF numbering: r0-r15 = 0-15).
pub static CM_REGS: &[RegDef] = &[
    cm("r0", 0, Some(0), RegGroup::General, "Register 0 (argument, return value)"),
    cm("r1", 1, Some(1), RegGroup::General, "Register 1 (argument, return value)"),
    cm("r2", 2, Some(2), RegGroup::General, "Register 2 (argument)"),
    cm("r3", 3, Some(3), RegGroup::General, "Register 3 (argument)"),
    cm("r4", 4, Some(4), RegGroup::General, "Register 4"),
    cm("r5", 5, Some(5), RegGroup::General, "Register 5"),
    cm("r6", 6, Some(6), RegGroup::General, "Register 6"),
    cm("r7", 7, Some(7), RegGroup::General, "Register 7"),
    cm("r8", 8, Some(8), RegGroup::General, "Register 8"),
    cm("r9", 9, Some(9), RegGroup::General, "Register 9"),
    cm("r10", 10, Some(10), RegGroup::General, "Register 10"),
    cm("r11", 11, Some(11), RegGroup::General, "Register 11"),
    cm("r12", 12, Some(12), RegGroup::General, "Register 12"),
    cm("sp", 13, Some(13), RegGroup::General, "Stack pointer (r13)"),
    cm("lr", 14, Some(14), RegGroup::General, "Link register (r14)"),
    cm("pc", 15, Some(15), RegGroup::General, "Program counter (r15)"),
    cm("xpsr", 16, None, RegGroup::System, "Program status register"),
    cm("msp", 17, None, RegGroup::System, "Main stack pointer"),
    cm("psp", 18, None, RegGroup::System, "Process stack pointer"),
    cm("primask", 20 | 1 << 8, None, RegGroup::System, "Priority mask"),
    cm("basepri", 20 | 2 << 8, None, RegGroup::System, "Base priority"),
    cm("faultmask", 20 | 3 << 8, None, RegGroup::System, "Fault mask"),
    cm("control", 20 | 4 << 8, None, RegGroup::System, "Control register"),
    cm_dbg("dhcsr", 0xEDF0, "Debug halting control and status"),
    cm_dbg("dfsr", 0xED30, "Debug fault status"),
    cm_dbg("demcr", 0xEDFC, "Debug exception and monitor control"),
];

pub const CM_REG_SP: usize = 13;
pub const CM_REG_LR: usize = 14;
pub const CM_REG_PC: usize = 15;

pub static CORTEX_M: ArchInfo = ArchInfo {
    name: "cortex_m",
    regs: CM_REGS,
    pc: CM_REG_PC,
    sp: CM_REG_SP,
    ra: CM_REG_LR,
    aliases: &[("r13", "sp"), ("r14", "lr"), ("r15", "pc"), ("cpsr", "xpsr")],
    description: "Cortex-M general purpose and system registers",
};

// --- GTM MCS channel ---------------------------------------------------------------------

/// Offset flag of an MCS register in the module block (channel 0) instead of the channel block.
pub const MCS_MODULE_REG: u16 = 0x8000;

const fn mcs(name: &'static str, offset: u16, group: RegGroup, description: &'static str) -> RegDef {
    RegDef { name, offset, gdb: None, dwarf: None, group, description }
}

/// MCS channel registers (TC3xx UM 28.17.11): offsets in the channel register block, or with
/// [`MCS_MODULE_REG`] in the module block.
pub static MCS_REGS: &[RegDef] = &[
    mcs("r0", 0x00, RegGroup::General, "General purpose register 0"),
    mcs("r1", 0x04, RegGroup::General, "General purpose register 1"),
    mcs("r2", 0x08, RegGroup::General, "General purpose register 2"),
    mcs("r3", 0x0C, RegGroup::General, "General purpose register 3"),
    mcs("r4", 0x10, RegGroup::General, "General purpose register 4 (upper multiplication result)"),
    mcs("r5", 0x14, RegGroup::General, "General purpose register 5 (MRDIO/MWRIO offset)"),
    mcs("r6", 0x18, RegGroup::General, "General purpose register 6 (indirect jump target, WURMX mask)"),
    mcs("r7", 0x1C, RegGroup::General, "General purpose register 7 (stack pointer)"),
    mcs("pc", 0x40, RegGroup::General, "Program counter"),
    mcs("ctrl", 0x20, RegGroup::System, "Channel control: EN, IRQ, ERR, CY, Z, V, N, CAT, CWT, SAT, SP_CNT"),
    mcs("acb", 0x24, RegGroup::System, "ARU control bits"),
    mcs("mhb", 0x3C, RegGroup::System, "Memory high byte"),
    mcs("irq_notify", 0x44, RegGroup::System, "Interrupt notification"),
    mcs("strg", 0x2C | MCS_MODULE_REG, RegGroup::Debug, "Trigger bits (read STRG, shared by the module)"),
    mcs("reg_prot", 0x60 | MCS_MODULE_REG, RegGroup::Debug, "Module write protection"),
    mcs("ctrl_stat", 0x64 | MCS_MODULE_REG, RegGroup::Debug, "Module control and status (scheduling, ERR_SRC_ID)"),
    mcs("err", 0x7C | MCS_MODULE_REG, RegGroup::Debug, "Module error register"),
];

pub const MCS_REG_R6: usize = 6;
pub const MCS_REG_R7: usize = 7;
pub const MCS_REG_PC: usize = 8;
pub const MCS_REG_CTRL: usize = 9;

pub static GTM_MCS: ArchInfo = ArchInfo {
    name: "gtm_mcs",
    regs: MCS_REGS,
    pc: MCS_REG_PC,
    sp: MCS_REG_R7,
    ra: MCS_REG_PC,
    aliases: &[("sp", "r7"), ("sta", "ctrl")],
    description: "GTM MCS channel and module registers",
};

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
