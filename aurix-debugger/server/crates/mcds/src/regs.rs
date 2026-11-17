//! miniMCDS registers of the TC37x production device, as seen from the debugger.
//!
//! Learned from winIDEA's DAP traffic on a TC377 (recorded with a logic analyzer): every
//! address and value here was written by winIDEA or read back from the chip. Registers whose
//! meaning is unknown are written exactly as winIDEA writes them ([`CONSTANTS`]).

/// Base of the miniMCDS SFRs.
pub const BASE: u32 = 0xFB71_8000;
/// Module clock control. Reads 0x3 (clock off) after a reset; 0 switches the clock on. Every
/// other MCDS register refuses access while the clock is off.
pub const CLC: u32 = BASE;
/// winIDEA ORs 0x8000 into it at the first session (reads 0x00A0 before).
pub const CFG10: u32 = BASE + 0x010;
/// Cerberus register winIDEA sets to 0x300 at the first session.
pub const CBS_47C: u32 = 0xF000_047C;
pub const CBS_47C_VALUE: u32 = 0x300;

/// Write pointer into the trace RAM (byte offset, 32-byte granularity; 0x1FE0 = full).
pub const FIFONOW: u32 = BASE + 0x200;
/// Ring bounds: 0x204 bottom, 0x208 = 0x1FFF for a ring (0 = linear), 0x20C end (0x1FFF).
pub const FIFO_BOTTOM: u32 = BASE + 0x204;
pub const FIFO_WRAP: u32 = BASE + 0x208;
pub const FIFO_END: u32 = BASE + 0x20C;
/// Control and status.
pub const CTRL: u32 = BASE + 0x210;
/// Overflow counter (public notes; not verified on hardware yet).
pub const FIFOOVRCNT: u32 = BASE + 0x21C;

/// Written before the configuration (reset of the recorder).
pub const CTRL_CONFIG: u32 = 0x8000;
/// Start with tick time stamps (winIDEA's tick sessions).
pub const CTRL_START_TICK: u32 = 0x6003;
/// Start with TSU time stamps (winIDEA's TSU session).
pub const CTRL_START_TSU: u32 = 0x6002;
/// Stop after the recorder finished by itself (written after [`STOP_PULSE`]).
pub const CTRL_STOP_DONE: u32 = 0xA003;
/// Stop by the host while recording.
pub const CTRL_STOP_HOST: u32 = 0x8003;
/// Status bit: the recorder finished (trace_done fired or the buffer is full).
pub const CTRL_DONE: u32 = 0x2000;

/// MCX event registers: EVTn at `EVT + 4 * n`.
pub const EVT: u32 = BASE + 0x800;
pub const EVT_COUNT: usize = 24;
/// MCX action registers: action i at `ACT + 4 * i`.
pub const ACT: u32 = BASE + 0x880;
pub const ACT_COUNT: usize = 41;
/// Action slots winIDEA pulses with 0x60 (then 0) when it stops a recording.
pub const STOP_PULSE: [u32; 3] = [ACT, ACT + 4, ACT + 0x84];

/// DTU address comparators: X at +0, Y - X at +4. Comparator 1 at +0x10.
pub const DTU_CMP: [u32; 2] = [0xFB71_A400, 0xFB71_A410];
/// PTU address comparators: X at +0, Y - X at +4. Comparator 1 at +0x10.
pub const PTU_CMP: [u32; 2] = [0xFB71_B000, 0xFB71_B010];
/// Y - X of an unused comparator.
pub const DTU_CMP_UNUSED: u32 = 0xFFFF_FFFF;
pub const PTU_CMP_UNUSED: u32 = 0xFFFF_FFFE;

/// The trace RAM as seen from the debugger.
pub const TRAM: u32 = 0x9800_0000;
pub const TRAM_SIZE: u32 = 0x2000;
pub const PARAGRAPH: u32 = 0x400;
pub const PARAGRAPHS: usize = (TRAM_SIZE / PARAGRAPH) as usize;

/// Duration of one TSU unit or tick at TSUPRSCL 1: 1/150 MHz, in picoseconds.
pub const TIME_UNIT_PS: u64 = 6_667;

/// Registers winIDEA writes with the same value in every session; meaning unknown.
pub const CONSTANTS: &[(u32, u32)] = &[
    (0xFB71_A480, 0), (0xFB71_A4A0, 0), (0xFB71_A490, 0xFFFF_FFFF), (0xFB71_A4B0, 0xFFFF_FFFF),
    (0xFB71_A488, 0), (0xFB71_A4A8, 0), (0xFB71_A49C, 0), (0xFB71_A4BC, 0),
    (0xFB71_A00C, 0),
    (0xFB71_A500, 0), (0xFB71_A518, 0), (0xFB71_A508, 0x3001), (0xFB71_A520, 0x3001),
    (0xFB71_A504, 0), (0xFB71_A51C, 0),
    (BASE + 0x020, 0x0800_8000), (BASE + 0x014, 0x81),
];
/// Written as 0 by winIDEA (0xFB718A00..A74), meaning unknown.
pub const ZERO_BLOCK_A00: core::ops::Range<u32> = 0xFB71_8A00..0xFB71_8A80;
/// Written by winIDEA after the ring bounds.
pub const REG_404: (u32, u32) = (BASE + 0x404, 1);

/// MCX actions with a known slot (index into the action registers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    TsuRelEn,
    TsuRelSync,
    TickEnable,
    TraceDone,
    DtuWdat,
    DtuWadr,
    PtuEnable,
}

impl Action {
    pub const fn index(self) -> usize {
        match self {
            Action::TsuRelEn => 0,
            Action::TsuRelSync => 1,
            Action::TickEnable => 21,
            Action::TraceDone => 22,
            Action::DtuWdat => 27,
            Action::DtuWadr => 28,
            Action::PtuEnable => 31,
        }
    }
    pub const fn addr(self) -> u32 {
        ACT + 4 * self.index() as u32
    }
}

/// When an action is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Never,
    Always,
    /// While the event is true (level).
    While(u8),
    /// When the event becomes true (edge).
    On(u8),
}

impl When {
    /// Action register value: event in bits 0..4, Always 0x20, Active 0x40, level State 0x80.
    pub const fn value(self) -> u32 {
        match self {
            When::Never => 0,
            When::Always => 0x60,
            When::While(e) => 0xC0 | (e as u32 & 0x1F),
            When::On(e) => 0x40 | (e as u32 & 0x1F),
        }
    }
}

/// MCX trigger inputs used by the configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Input {
    PtuTrig0,
    PtuTrig1,
    DtuEaTrig0,
    DtuEaTrig1,
    /// A qualifier of the data comparator: alone it is always true; together with
    /// `dtu_ea_trig_0` it drops most read accesses (measured: 12 instead of 29 messages per write
    /// on a range the application reads often). Its configuration is unknown (reset value).
    DtuAccTrig0,
    DcuHalt,
    /// The core is halted or suspended, whatever the cause (its own breakpoint, a debugger halt,
    /// another core's halt over trigger line 1). Measured on a TC377; `dcu_halt` misses the
    /// trigger-line halts.
    DcuSus,
    BreakIn,
    TsuTcTrig,
}

impl Input {
    pub const fn name(self) -> &'static str {
        match self {
            Input::PtuTrig0 => "ptu_trig_0",
            Input::PtuTrig1 => "ptu_trig_1",
            Input::DtuEaTrig0 => "dtu_ea_trig_0",
            Input::DtuEaTrig1 => "dtu_ea_trig_1",
            Input::DtuAccTrig0 => "dtu_acc_trig_0",
            Input::DcuHalt => "dcu_halt",
            Input::DcuSus => "dcu_sus",
            Input::BreakIn => "break_in",
            Input::TsuTcTrig => "tsu_tc_trig",
        }
    }
}

/// Inputs of each MCX event, by block (from winIDEA's event dialogs; `None` = no input, which
/// counts as true). Checked on hardware: EVT0/3 ptu_trig_0, EVT7/2 dtu_ea_trig_0, EVT4/5
/// dcu_halt, EVT6/4 core_exevt, EVT6/7 empty.
pub const EVENT_INPUTS: [[Option<&str>; 16]; 16] = {
    const fn e(n: &'static str) -> Option<&'static str> {
        Some(n)
    }
    const N: Option<&str> = None;
    [
        [e("core_crevt"), e("core_trig0"), e("core_trig1"), e("ptu_trig_0"), e("dcu_idle"), e("dcu_isr"), e("break_in"), e("fifo_trg_0"), e("fifo_trg_1"), e("tsu_tc_trig"), e("mcx_trig_0"), e("mcx_trig_1"), e("cnt_trig_0"), e("cnt_trig_1"), e("cnt_trig_2"), e("cnt_trig_3")],
        [e("core_crevt"), e("core_trig1"), e("core_trig2"), e("ptu_trig_1"), e("dcu_idle"), e("dcu_halt"), e("break_in"), e("fifo_trg_0"), e("fifo_trg_1"), e("tsu_tc_trig"), e("mcx_trig_0"), e("mcx_trig_1"), e("cnt_trig_0"), e("cnt_trig_1"), e("cnt_trig_2"), e("cnt_trig_3")],
        [e("core_trig2"), e("core_trig3"), e("core_tr0evt"), e("core_tr1evt"), e("core_exevt"), e("core_swevt"), e("ptu_trig_0"), e("dcu_sus"), e("break_in"), e("fifo_trg_0"), e("tsu_tc_trig"), e("mcx_trig_0"), e("cnt_trig_0"), e("cnt_trig_1"), e("cnt_trig_2"), e("cnt_trig_3")],
        [e("core_crevt"), e("core_trig3"), e("core_trig4"), e("core_tr0evt"), e("core_tr1evt"), e("core_swevt"), e("ptu_trig_1"), e("dcu_sus"), e("break_in"), e("fifo_trg_0"), e("tsu_tc_trig"), e("mcx_trig_0"), e("cnt_trig_0"), e("cnt_trig_1"), e("cnt_trig_2"), e("cnt_trig_3")],
        [e("core_crevt"), e("core_trig4"), e("core_trig5"), e("core_tr0evt"), e("core_tr1evt"), e("dcu_halt"), e("dcu_isr"), e("break_in"), e("fifo_trg_0"), e("fifo_trg_1"), e("mcx_trig_2"), e("mcx_trig_3"), e("cnt_trig_4"), e("cnt_trig_5"), e("cnt_trig_6"), e("cnt_trig_7")],
        [e("core_trig6"), e("core_trig7"), e("core_tr0evt"), e("core_tr1evt"), e("core_exevt"), e("core_swevt"), e("ptu_trig_0"), e("dcu_idle"), e("dcu_halt"), e("dcu_isr"), e("mcx_trig_2"), e("mcx_trig_3"), e("cnt_trig_4"), e("cnt_trig_5"), e("cnt_trig_6"), e("cnt_trig_7")],
        [e("core_trig0"), e("core_trig1"), e("core_trig2"), e("core_trig3"), e("core_exevt"), e("break_in"), e("fifo_trg_0"), N, N, N, e("mcx_trig_2"), e("mcx_trig_3"), e("cnt_trig_4"), e("cnt_trig_5"), e("cnt_trig_6"), e("cnt_trig_7")],
        [e("ptu_trig_0"), e("ptu_trig_1"), e("dtu_ea_trig_0"), e("dtu_ea_trig_1"), e("dtu_dat_trig_0"), e("dtu_acc_trig_1"), e("break_in"), e("fifo_trg_0"), e("fifo_trg_1"), N, N, N, e("cnt_trig_4"), e("cnt_trig_5"), e("cnt_trig_6"), e("cnt_trig_7")],
        [e("ptu_trig_0"), e("ptu_trig_1"), e("dtu_ea_trig_0"), e("dtu_ea_trig_1"), e("dtu_dat_trig_0"), e("dtu_dat_trig_1"), e("dtu_acc_trig_0"), e("dtu_acc_trig_1"), e("break_in"), e("fifo_trg_0"), e("mcx_trig_2"), e("mcx_trig_3"), e("cnt_trig_2"), e("cnt_trig_3"), e("cnt_trig_4"), e("cnt_trig_5")],
        [e("ptu_trig_0"), e("ptu_trig_1"), e("dtu_ea_trig_0"), e("dtu_ea_trig_1"), e("dtu_dat_trig_0"), e("dtu_acc_trig_0"), e("fifo_trg_1"), N, N, N, e("mcx_trig_2"), e("mcx_trig_3"), e("cnt_trig_2"), e("cnt_trig_3"), e("cnt_trig_4"), e("cnt_trig_5")],
        [e("ptu_trig_0"), e("ptu_trig_1"), e("dtu_ea_trig_0"), e("dtu_ea_trig_1"), e("dtu_dat_trig_0"), e("dtu_dat_trig_1"), e("dtu_acc_trig_0"), e("dtu_acc_trig_1"), e("break_in"), e("fifo_trg_1"), N, e("mcx_trig_2"), e("cnt_trig_2"), e("cnt_trig_3"), e("cnt_trig_4"), e("cnt_trig_5")],
        [e("ptu_trig_0"), e("ptu_trig_1"), e("dtu_ea_trig_0"), e("dtu_ea_trig_1"), e("dtu_dat_trig_1"), e("dtu_acc_trig_1"), e("break_in"), e("fifo_trg_0"), e("fifo_trg_1"), N, N, N, e("cnt_trig_2"), e("cnt_trig_3"), e("cnt_trig_4"), e("cnt_trig_5")],
        [e("ptu_trig_0"), e("ptu_trig_1"), e("dtu_ea_trig_0"), e("dtu_ea_trig_1"), e("dtu_dat_trig_0"), e("dtu_dat_trig_1"), e("dtu_acc_trig_0"), e("break_in"), e("fifo_trg_1"), N, N, N, e("mcx_trig_2"), e("mcx_trig_3"), e("cnt_trig_0"), e("cnt_trig_1")],
        [e("ptu_trig_0"), e("ptu_trig_1"), e("dtu_ea_trig_0"), e("dtu_ea_trig_1"), e("dtu_dat_trig_0"), e("dtu_dat_trig_1"), e("dtu_acc_trig_0"), e("dtu_acc_trig_1"), e("break_in"), e("fifo_trg_0"), e("fifo_trg_1"), N, e("mcx_trig_2"), e("mcx_trig_3"), e("cnt_trig_0"), e("cnt_trig_1")],
        [e("break_in"), e("fifo_trg_0"), e("fifo_trg_1"), e("tsu_tc_trig"), e("mcx_trig_0"), e("mcx_trig_1"), e("mcx_trig_2"), e("mcx_trig_3"), e("cnt_trig_0"), e("cnt_trig_1"), e("cnt_trig_2"), e("cnt_trig_3"), e("cnt_trig_4"), e("cnt_trig_5"), e("cnt_trig_6"), e("cnt_trig_7")],
        [e("break_in"), e("fifo_trg_0"), e("fifo_trg_1"), e("tsu_tc_trig"), e("mcx_trig_0"), e("mcx_trig_1"), e("mcx_trig_2"), e("mcx_trig_3"), e("cnt_trig_0"), e("cnt_trig_1"), e("cnt_trig_2"), e("cnt_trig_3"), e("cnt_trig_4"), e("cnt_trig_5"), e("cnt_trig_6"), e("cnt_trig_7")],
    ]
};

/// Blocks of `event` that carry `input`.
pub fn block_of(event: u8, input: Input) -> Option<u8> {
    EVENT_INPUTS.get(event as usize)?.iter().position(|n| *n == Some(input.name())).map(|b| b as u8)
}

/// EVTn register value with the given blocks selected: block k clears bit 2k.
pub fn event_value(blocks: &[u8]) -> u32 {
    blocks.iter().fold(0xFFFF_FFFFu32, |v, &k| v & !(1u32 << (2 * k as u32)))
}

/// EVTn register value for a combination of inputs, all of which must hold (the blocks of an
/// event are ANDed): `(block, true)` requires the input, `(block, false)` its absence. Each block
/// has two bits: clearing bit 2k selects "input true", clearing bit 2k+1 "input false" (measured).
pub fn event_value_of(terms: &[(u8, bool)]) -> u32 {
    terms.iter().fold(0xFFFF_FFFFu32, |v, &(k, on)| v & !(1u32 << (2 * k as u32 + if on { 0 } else { 1 })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_event_values() {
        assert_eq!(block_of(0, Input::PtuTrig0), Some(3));
        assert_eq!(event_value(&[3]), 0xFFFF_FFBF);
        assert_eq!(block_of(7, Input::DtuEaTrig0), Some(2));
        assert_eq!(event_value(&[2]), 0xFFFF_FFEF);
        assert_eq!(block_of(4, Input::DcuHalt), Some(5));
        assert_eq!(event_value(&[5]), 0xFFFF_FBFF);
        assert_eq!(block_of(14, Input::BreakIn), Some(0));
        assert_eq!(event_value(&[0]), 0xFFFF_FFFE);
        assert_eq!(block_of(0, Input::TsuTcTrig), Some(9));
        assert_eq!(event_value(&[9]), 0xFFFB_FFFF);
        // Measured: EVT2 0xFFFFBFFF fires when CPU0 halts, 0xFFFF7FFF while it runs.
        assert_eq!(block_of(2, Input::DcuSus), Some(7));
        assert_eq!(event_value_of(&[(7, true)]), 0xFFFF_BFFF);
        assert_eq!(event_value_of(&[(7, false)]), 0xFFFF_7FFF);
        assert_eq!(event_value_of(&[(10, true), (7, false)]), 0xFFEF_7FFF);
    }

    #[test]
    fn action_values() {
        assert_eq!(When::Always.value(), 0x60);
        assert_eq!(When::While(7).value(), 0xC7);
        assert_eq!(When::On(14).value(), 0x4E);
        assert_eq!(When::On(4).value(), 0x44);
        assert_eq!(When::While(0).value(), 0xC0);
        assert_eq!(Action::TraceDone.addr(), 0xFB71_88D8);
        assert_eq!(Action::DtuWdat.addr(), 0xFB71_88EC);
        assert_eq!(Action::DtuWadr.addr(), 0xFB71_88F0);
        assert_eq!(Action::PtuEnable.addr(), 0xFB71_88FC);
        assert_eq!(STOP_PULSE[2], 0xFB71_8904);
    }
}
