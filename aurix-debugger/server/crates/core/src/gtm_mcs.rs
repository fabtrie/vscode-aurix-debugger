//! GTM MCS channel target: one channel of a Multi Channel Sequencer module (TC3xx UM 28.17).
//!
//! The MCS has no debug hardware. All its registers and its RAM are readable and writable
//! through the host bus while it runs (28.25.19), so run control is built from the channel
//! enable bit and the program RAM:
//!
//! - Halt clears `CHx_CTRL.EN`. The channel stops at the end of the current instruction; an
//!   instruction waiting for an event (WURM, ARU or bus access) finishes its wait first. The
//!   cancel bits (CAT/CWT) are never used, since cancelling a wait changes the program flow.
//! - A breakpoint replaces the instruction with a jump to itself: a channel reaching it loops
//!   there and is reported as halted at the breakpoint. Unlike an invalid opcode this raises no
//!   error interrupt visible to the application.
//! - A single step places such self-jumps on all possible successors of the instruction, enables
//!   the channel until it arrives at one of them and removes them again.
//!
//! All eight channels of a module execute from the same RAM, so breakpoints and step traps live
//! in [`McsRam`], shared by the channel targets of a module.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use isa::mcs;
use tas::Batch;

use crate::adapter::{check, Io};
use crate::arch::*;
use crate::error::{other, Error, Result};
use crate::target::{DebugReason, PollOps, RunCfg, RunMode, TargetState, Transition};

/// GTM base address of the TC3xx.
pub const GTM_BASE: u32 = 0xF010_0000;
/// GTM_CLC: bit 1 (DISS) set while the GTM is disabled.
pub const GTM_CLC: u32 = GTM_BASE + 0x9_FD00;
/// Register block of MCS module `i` (channel `x` at `+ x * 0x80`).
pub const fn module_regs(i: u32) -> u32 {
    GTM_BASE + 0xF_0000 + i * 0x1000
}
/// Program RAM of MCS module `i`, seen from the host.
pub const fn module_ram(i: u32) -> u32 {
    GTM_BASE + 0x3_8000 + i * 0x8000
}
/// Cluster configuration module of cluster `i` (the cluster of MCS module `i`).
pub const fn ccm_base(i: u32) -> u32 {
    GTM_BASE + 0xE_2000 + i * 0x200
}
/// CCMi_ARPz_CTRL: address range protector `z` of the MCS RAM (z = 0..9).
pub const fn ccm_arp_ctrl(i: u32, z: u32) -> u32 {
    ccm_base(i) + z * 8
}
/// CCMi_PROT: CLS_PROT (bit 0) locks the cluster configuration, including the ARPs.
pub const fn ccm_prot(i: u32) -> u32 {
    ccm_base(i) + 0x1FC
}
pub const NUM_ARP: u32 = 10;
pub const ARP_WPROT_AEI: u32 = 1 << 31;
pub const ARP_DIS_PROT: u32 = 1 << 24;

/// MCS RAM range `[start, end)` of an ARP control value (UM 28.11.4.6: 2^SIZE words from ADDR).
pub fn arp_range(ctrl: u32) -> (u32, u32) {
    let size = (ctrl >> 16) & 0xF;
    let len = 4u32 << size;
    let start = (ctrl & 0xFFFF) & !(len - 1);
    (start, start + len)
}

/// MCS RAM per module unless configured with `-ramsize`: 3072 words (TC37x). The size differs between
/// derivatives.
pub const DEFAULT_RAM_SIZE: u32 = 3072 * 4;
/// Largest MCS RAM: the module RAMs are 32 KB apart.
pub const MAX_RAM_SIZE: u32 = 0x8000;
/// Most MCS modules of a TC3xx GTM (one per cluster).
pub const MAX_MODULES: u32 = 10;
pub const CHANNELS: u32 = 8;

const CTRL: u32 = 0x20;
const PC: u32 = 0x40;
pub const CTRL_EN: u32 = 1 << 0;
pub const CTRL_ERR: u32 = 1 << 2;
const DISABLE_TIMEOUT: Duration = Duration::from_millis(200);
const STEP_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Planted {
    /// Instruction replaced by the self-jump.
    orig: u32,
    /// Channel targets of the module holding the breakpoint.
    refs: u32,
}

/// Program RAM state shared by the channels of one MCS module.
#[derive(Debug)]
pub struct McsRam {
    /// RAM size in bytes (`-ramsize`).
    pub size: u32,
    bps: BTreeMap<u32, Planted>,
    /// Temporary step traps: address -> (instruction replaced, channel stepping).
    traps: BTreeMap<u32, (u32, u32)>,
}

impl Default for McsRam {
    fn default() -> Self {
        McsRam { size: DEFAULT_RAM_SIZE, bps: BTreeMap::new(), traps: BTreeMap::new() }
    }
}

pub struct McsChannel {
    pub coreid: u32,
    /// Register block of the module (`-dbgbase`), default [`module_regs`].
    pub dbgbase: Option<u32>,
    pub ocmts: Option<String>,
    pub state: TargetState,
    pub reason: DebugReason,
    pub examined: bool,
    pub run: RunCfg,
    pub module: u32,
    pub channel: u32,
    /// RAM base in the host address space (`-rambase`), default [`module_ram`].
    pub rambase: Option<u32>,
    pub ram: Arc<Mutex<McsRam>>,
    /// Breakpoint address per slot of this target.
    slots: Vec<Option<u32>>,
    pub ctrl: u32,
    pub pc: u32,
    /// The channel ran or left its reset PC since the last reset (shown to the frontends).
    pub used: bool,
    /// A halt was requested and the channel is not disabled yet.
    stop_req: bool,
    cache: Vec<Option<u32>>,
    /// Breakpoints whose self-jump was overwritten (the application loaded the program again).
    replant: bool,
}

impl Default for McsChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl McsChannel {
    pub fn new() -> Self {
        McsChannel {
            coreid: 0,
            dbgbase: None,
            ocmts: None,
            state: TargetState::Unknown,
            reason: DebugReason::Undefined,
            examined: false,
            run: RunCfg::default(),
            module: 0,
            channel: 0,
            rambase: None,
            ram: Arc::new(Mutex::new(McsRam::default())),
            slots: Vec::new(),
            ctrl: 0,
            pc: 0,
            used: false,
            stop_req: false,
            cache: vec![None; MCS_REGS.len()],
            replant: false,
        }
    }

    fn regs_base(&self) -> u32 {
        self.dbgbase.unwrap_or(module_regs(self.module))
    }

    fn chan(&self, off: u32) -> u32 {
        self.regs_base() + self.channel * 0x80 + off
    }

    fn ram_base(&self) -> u32 {
        self.rambase.unwrap_or(module_ram(self.module))
    }

    fn ram_addr(&self, addr: u32) -> Result<u32> {
        if addr & 3 != 0 || addr >= self.ram_size() {
            return Err(other(format!("0x{addr:x} is no MCS instruction address (0..0x{:x}, word aligned)", self.ram_size() - 4)));
        }
        Ok(self.ram_base() + addr)
    }

    fn reset_pc(&self) -> u32 {
        4 * self.channel
    }

    /// MCS RAM size of the module in bytes.
    pub fn ram_size(&self) -> u32 {
        self.shared().size
    }

    /// Sets the MCS RAM size (`-ramsize`) for all channels of the module.
    pub fn set_ram_size(&self, size: u32) -> Result<()> {
        if size == 0 || size % 4 != 0 || size > MAX_RAM_SIZE {
            return Err(Error::Usage(format!("invalid MCS RAM size 0x{size:x} (a multiple of 4 up to 0x{MAX_RAM_SIZE:x})")));
        }
        self.shared().size = size;
        Ok(())
    }

    /// Checks `-module` and `-channel` against the TC3xx GTM.
    pub fn check_config(&self) -> Result<()> {
        if self.module >= MAX_MODULES {
            return Err(Error::Usage(format!("MCS module {} does not exist (0..{})", self.module, MAX_MODULES - 1)));
        }
        if self.channel >= CHANNELS {
            return Err(Error::Usage(format!("MCS channel {} does not exist (0..{})", self.channel, CHANNELS - 1)));
        }
        Ok(())
    }

    fn shared(&self) -> std::sync::MutexGuard<'_, McsRam> {
        self.ram.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether frontends show this channel: it ran since the reset or has breakpoints.
    pub fn visible(&self) -> bool {
        self.used || self.slots.iter().any(Option::is_some)
    }

    /// MCS program addresses (the RAM) are accessed through the module RAM.
    pub fn owns_address(&self, addr: u32) -> bool {
        addr < self.ram_size()
    }

    fn read_word(&self, io: &mut Io, addr: u32) -> Result<u32> {
        io.read32(self.ram_addr(addr)?)
    }

    fn write_word(&self, io: &mut Io, addr: u32, value: u32) -> Result<()> {
        self.write_ram(io, &[(addr, value)])
    }

    /// Writes MCS RAM words (MCS addresses). Code is usually write protected against CPU access
    /// by an address range protector of the cluster (CCMi_ARPz_CTRL.WPROT_AEI, locked by
    /// CCMi_PROT.CLS_PROT): the protectors covering the words are opened for the writes and
    /// restored in the same transaction.
    fn write_ram(&self, io: &mut Io, writes: &[(u32, u32)]) -> Result<()> {
        if writes.is_empty() {
            return Ok(());
        }
        let ram: Vec<(u32, u32)> = writes.iter().map(|&(a, v)| self.ram_addr(a).map(|h| (h, v))).collect::<Result<_>>()?;
        let m = self.module;
        let mut addrs: Vec<u32> = (0..NUM_ARP).map(|z| ccm_arp_ctrl(m, z)).collect();
        addrs.push(ccm_prot(m));
        let v = io.read32_many(&addrs)?;
        let prot = v[NUM_ARP as usize];
        let covers = |ctrl: u32| writes.iter().any(|&(a, _)| (arp_range(ctrl).0..arp_range(ctrl).1).contains(&a));
        // An ARP with DIS_PROT and WPROT_AEI explicitly allows CPU writes.
        let blocking: Vec<(u32, u32)> = (0..NUM_ARP as usize)
            .filter(|&z| v[z] & ARP_WPROT_AEI != 0 && v[z] & ARP_DIS_PROT == 0 && covers(v[z]))
            .map(|z| (addrs[z], v[z]))
            .collect();
        if blocking.is_empty() {
            return io.write32_seq(&ram);
        }
        let mut open = Vec::new();
        if prot & 1 != 0 {
            open.push((ccm_prot(m), prot & !1));
        }
        open.extend(blocking.iter().map(|&(a, c)| (a, c & !ARP_WPROT_AEI)));
        let mut restore: Vec<(u32, u32)> = blocking.clone();
        if prot & 1 != 0 {
            restore.push((ccm_prot(m), prot));
        }
        let seq: Vec<(u32, u32)> = open.iter().chain(&ram).chain(&restore).copied().collect();
        let r = io.write32_seq(&seq);
        if r.is_err() {
            // A failed write may skip the rest of the transaction: restore the protection again.
            if let Err(e) = io.write32_seq(&restore) {
                crate::log_warn!("MCS{m} RAM write protection not restored: {e}");
            }
        }
        r
    }

    /// Reads MCS RAM (word access; addresses are MCS program addresses).
    pub fn read_memory(&mut self, io: &mut Io, addr: u32, len: usize) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let start = addr & !3;
        let end = addr.checked_add(len as u32 - 1).ok_or_else(|| other("address range overflow"))? | 3;
        if end >= self.ram_size() {
            return Err(Error::Memory { addr: addr as u64, write: false, detail: "outside the MCS RAM".into() });
        }
        let words: Vec<u32> = (start..=end).step_by(4).map(|a| self.ram_base() + a).collect();
        let vals = io.read32_many(&words)?;
        // Breakpoints and step traps show the instruction they replace.
        let ram = self.shared();
        let bytes: Vec<u8> = vals
            .iter()
            .enumerate()
            .flat_map(|(i, v)| {
                let a = start + 4 * i as u32;
                let v = ram.bps.get(&a).map(|b| b.orig).or_else(|| ram.traps.get(&a).map(|t| t.0)).unwrap_or(*v);
                v.to_le_bytes()
            })
            .collect();
        let off = (addr - start) as usize;
        Ok(bytes[off..off + len].to_vec())
    }

    /// Writes MCS RAM (partial words merged). A write over a breakpoint changes the instruction
    /// the breakpoint restores.
    pub fn write_memory(&mut self, io: &mut Io, addr: u32, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let start = addr & !3;
        let off = (addr - start) as usize;
        let total = (off + data.len() + 3) & !3;
        let mut bytes = self.read_memory(io, start, total)?;
        bytes[off..off + data.len()].copy_from_slice(data);
        let mut writes = Vec::new();
        {
            let mut ram = self.shared();
            for (i, w) in bytes.chunks(4).enumerate() {
                let a = start + 4 * i as u32;
                let v = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
                if let Some(b) = ram.bps.get_mut(&a) {
                    b.orig = v;
                } else if let Some(t) = ram.traps.get_mut(&a) {
                    t.0 = v;
                } else {
                    writes.push((a, v));
                }
            }
        }
        self.write_ram(io, &writes)
    }

    // --- Target -------------------------------------------------------------------------------

    pub fn invalidate(&mut self) {
        self.cache.iter_mut().for_each(|c| *c = None);
    }

    /// Checks that the GTM is enabled and the channel registers are accessible.
    pub fn examine(&mut self, io: &mut Io) -> Result<()> {
        self.check_config()?;
        let clc = io.read32(GTM_CLC)?;
        if clc & 2 != 0 {
            return Err(other("GTM is disabled (GTM_CLC.DISS)"));
        }
        let v = io.read32_many(&[self.chan(CTRL), self.chan(PC)])?;
        self.poll_apply(&v);
        self.invalidate();
        self.examined = true;
        Ok(())
    }

    pub fn poll_queue(&self, b: &mut Batch) -> PollOps {
        let start = b.len();
        let mut reads = vec![b.read32(self.chan(CTRL) as u64).0, b.read32(self.chan(PC) as u64).0];
        // Breakpoint words: the application may load the program again, overwriting them.
        for a in self.slots.iter().flatten() {
            reads.push(b.read32((self.ram_base() + a) as u64).0);
        }
        PollOps { all: (start..b.len()).collect(), reads }
    }

    pub fn poll(&mut self, io: &mut Io) -> Result<Transition> {
        let mut b = Batch::new();
        let ops = self.poll_queue(&mut b);
        let r = io.batch(&b)?;
        for &h in &ops.all {
            check(&r[h], 0, false)?;
        }
        let vals: Vec<u32> = ops.reads.iter().map(|&h| r[h].u32()).collect();
        Ok(self.poll_apply(&vals))
    }

    /// Updates the run state from CHx_CTRL, CHx_PC and the breakpoint words.
    pub fn poll_apply(&mut self, vals: &[u32]) -> Transition {
        self.ctrl = vals[0];
        self.pc = vals[1] & 0xFFFC;
        let enabled = self.ctrl & CTRL_EN != 0;
        if enabled || self.pc != self.reset_pc() {
            self.used = true;
        }
        let (at_bp, at_trap) = {
            let ram = self.shared();
            (ram.bps.contains_key(&self.pc), ram.traps.get(&self.pc).is_some_and(|t| t.1 == self.channel))
        };
        let planted: Vec<u32> = self.slots.iter().flatten().copied().collect();
        if planted.iter().zip(&vals[2..]).any(|(a, w)| *w != mcs::self_jump(*a)) {
            self.replant = true;
        }
        let halted = !enabled || at_bp || at_trap;
        let prev = self.state;
        if halted {
            self.state = TargetState::Halted;
            if prev != TargetState::Halted {
                self.invalidate();
                self.reason = if at_trap {
                    DebugReason::SingleStep
                } else if at_bp {
                    DebugReason::Breakpoint
                } else {
                    DebugReason::DbgRq
                };
                self.stop_req = false;
                return Transition::Halted { debug: false, boot: false };
            }
        } else {
            self.state = TargetState::Running;
            if prev == TargetState::Halted {
                self.reason = DebugReason::NotHalted;
                self.invalidate();
                return Transition::Running;
            }
        }
        Transition::None
    }

    /// Follow-up of a poll that needs target access (see [`Self::wants_fixup`]).
    pub fn wants_fixup(&self) -> bool {
        self.replant
    }

    /// Plants breakpoints again whose self-jump was overwritten: the overwriting word is the
    /// instruction the breakpoint now replaces.
    pub fn fixup(&mut self, io: &mut Io) -> Result<()> {
        self.replant = false;
        let addrs: Vec<u32> = self.slots.iter().flatten().copied().collect();
        for a in addrs {
            let w = self.read_word(io, a)?;
            if w != mcs::self_jump(a) {
                if let Some(b) = self.shared().bps.get_mut(&a) {
                    b.orig = w;
                }
                self.write_word(io, a, mcs::self_jump(a))?;
                crate::log_info!("MCS{} breakpoint at 0x{a:x} planted again (program RAM was reloaded)", self.module);
            }
        }
        Ok(())
    }

    pub fn hit_trigger(&self) -> Option<usize> {
        if self.reason != DebugReason::Breakpoint {
            return None;
        }
        self.slots.iter().position(|s| *s == Some(self.pc))
    }

    pub fn halt(&mut self, io: &mut Io) -> Result<()> {
        if self.state == TargetState::Halted {
            return Ok(());
        }
        io.write32(self.chan(CTRL), 0)?;
        self.stop_req = true;
        self.reason = DebugReason::DbgRq;
        Ok(())
    }

    fn read_ctrl_pc(&self, io: &mut Io) -> Result<(u32, u32)> {
        let v = io.read32_many(&[self.chan(CTRL), self.chan(PC)])?;
        Ok((v[0], v[1] & 0xFFFC))
    }

    /// Clears EN and waits until the channel is disabled. Returns the PC, or None if the channel
    /// is still busy (waiting in an instruction).
    fn disable(&mut self, io: &mut Io) -> Result<Option<u32>> {
        io.write32(self.chan(CTRL), 0)?;
        let t0 = Instant::now();
        loop {
            let (ctrl, pc) = self.read_ctrl_pc(io)?;
            if ctrl & CTRL_EN == 0 {
                self.ctrl = ctrl;
                self.pc = pc;
                return Ok(Some(pc));
            }
            if t0.elapsed() > DISABLE_TIMEOUT {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Stops a channel held at a breakpoint or step trap and removes this channel's traps, so
    /// it is disabled at its PC.
    fn settle(&mut self, io: &mut Io) -> Result<()> {
        if self.state != TargetState::Halted {
            return Err(Error::NotHalted(String::new()));
        }
        if self.ctrl & CTRL_EN != 0 && self.disable(io)?.is_none() {
            return Err(other(format!("MCS{} channel {} does not stop (EN stays set)", self.module, self.channel)));
        }
        self.remove_traps(io)
    }

    fn remove_traps(&mut self, io: &mut Io) -> Result<()> {
        let mine: Vec<(u32, u32)> = {
            let mut ram = self.shared();
            let mine: Vec<(u32, u32)> = ram.traps.iter().filter(|(_, t)| t.1 == self.channel).map(|(a, t)| (*a, t.0)).collect();
            for (a, _) in &mine {
                ram.traps.remove(a);
            }
            mine
        };
        self.write_ram(io, &mine)
    }

    /// Executes the instruction at the PC of the disabled channel. Returns false when it did not
    /// complete in time (it waits for an event): the channel then stops after it on its own.
    fn step_inner(&mut self, io: &mut Io, over: bool) -> Result<bool> {
        let (_, pc) = self.read_ctrl_pc(io)?;
        let planted = self.shared().bps.get(&pc).map(|b| b.orig);
        let word = match planted {
            Some(orig) => orig,
            None => self.read_word(io, pc)?,
        };
        let op = mcs::decode(word).ok_or_else(|| other(format!("invalid MCS instruction 0x{word:08x} at 0x{pc:x}")))?;
        let reg = |io: &mut Io, off: u32| io.read32(self.chan(off));
        let r6 = reg(io, 0x18)?;
        let ret = if op.flow == mcs::Flow::Ret {
            let sp = reg(io, 0x1C)? & 0x7FFC;
            Some(self.read_word(io, sp)?)
        } else {
            None
        };
        let succ = mcs::successors(word, pc, r6, ret, over);
        if succ.is_empty() {
            return Err(other(format!("cannot determine the instruction after 0x{pc:x} ({})", op.name)));
        }
        // Traps on the successors (breakpoints already are self-jumps), the instruction restored.
        let size = self.ram_size();
        let mut traps = Vec::new();
        for &s in succ.iter().filter(|&&s| s != pc && s < size) {
            let known = {
                let ram = self.shared();
                ram.bps.contains_key(&s) || ram.traps.contains_key(&s)
            };
            if !known {
                traps.push((s, self.read_word(io, s)?));
            }
        }
        {
            let mut ram = self.shared();
            for &(a, orig) in &traps {
                ram.traps.insert(a, (orig, self.channel));
            }
        }
        let mut writes: Vec<(u32, u32)> = traps.iter().map(|&(a, _)| (a, mcs::self_jump(a))).collect();
        if planted.is_some() {
            writes.push((pc, word));
        }
        if let Err(e) = self.write_ram(io, &writes).and_then(|_| io.write32(self.chan(CTRL), CTRL_EN)) {
            let _ = self.remove_traps(io);
            return Err(e);
        }

        let t0 = Instant::now();
        let mut done = false;
        while t0.elapsed() < STEP_TIMEOUT {
            let (_, now) = self.read_ctrl_pc(io)?;
            // A jump to itself ends at the same PC: give it a moment instead.
            if (now != pc && succ.contains(&now)) || (succ == [pc] && t0.elapsed() > Duration::from_millis(10)) {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let stopped = self.disable(io)?;
        self.remove_traps(io)?;
        if planted.is_some() {
            self.write_word(io, pc, mcs::self_jump(pc))?;
        }
        self.invalidate();
        Ok(done && stopped.is_some())
    }

    pub fn resume(&mut self, io: &mut Io, addr: Option<u32>, _mode: RunMode) -> Result<()> {
        if self.state == TargetState::Running {
            return Ok(());
        }
        self.settle(io)?;
        if let Some(a) = addr {
            self.write_reg(io, MCS_REG_PC, a)?;
        }
        let pc = self.read_ctrl_pc(io)?.1;
        if self.shared().bps.contains_key(&pc) && !self.step_inner(io, false)? {
            // Still in the instruction under the breakpoint: it continues on its own.
            self.state = TargetState::Running;
            self.reason = DebugReason::NotHalted;
            return Ok(());
        }
        io.write32(self.chan(CTRL), CTRL_EN)?;
        self.invalidate();
        self.state = TargetState::Running;
        self.reason = DebugReason::NotHalted;
        Ok(())
    }

    /// Executes one instruction (a call is stepped over for [`RunMode::StepOver`]).
    pub fn step(&mut self, io: &mut Io, addr: Option<u32>, mode: RunMode) -> Result<bool> {
        self.settle(io)?;
        if let Some(a) = addr {
            self.write_reg(io, MCS_REG_PC, a)?;
        }
        let done = self.step_inner(io, mode == RunMode::StepOver)?;
        if done {
            let (ctrl, pc) = self.read_ctrl_pc(io)?;
            self.ctrl = ctrl;
            self.pc = pc;
            self.state = TargetState::Halted;
            self.reason = DebugReason::SingleStep;
        } else {
            // The channel stops after the instruction (EN is cleared): a poll reports it.
            self.state = TargetState::Running;
            self.reason = DebugReason::NotHalted;
            self.stop_req = true;
        }
        Ok(done)
    }

    pub fn irq_masked(&self) -> bool {
        false
    }

    pub fn unmask_irq(&mut self, _io: &mut Io) -> Result<()> {
        Ok(())
    }

    // --- Registers ----------------------------------------------------------------------------

    fn reg_addr(&self, def: &RegDef) -> u32 {
        if def.offset & MCS_MODULE_REG != 0 {
            self.regs_base() + (def.offset & !MCS_MODULE_REG) as u32
        } else {
            self.chan(def.offset as u32)
        }
    }

    pub fn read_reg(&mut self, io: &mut Io, idx: usize) -> Result<u32> {
        Ok(self.read_regs(io, &[idx])?[0])
    }

    /// Registers are readable while the channel runs; values are cached only while halted.
    pub fn read_regs(&mut self, io: &mut Io, idxs: &[usize]) -> Result<Vec<u32>> {
        if let Some(i) = idxs.iter().find(|&&i| i >= MCS_REGS.len()) {
            return Err(other(format!("invalid register {i}")));
        }
        if self.state != TargetState::Halted {
            return self.read_uncached(io, idxs);
        }
        let missing: Vec<usize> = idxs.iter().copied().filter(|&i| self.cache[i].is_none()).collect();
        if !missing.is_empty() {
            let vals = self.read_uncached(io, &missing)?;
            for (&i, v) in missing.iter().zip(vals) {
                self.cache[i] = Some(v);
            }
        }
        Ok(idxs.iter().map(|&i| self.cache[i].unwrap_or(0)).collect())
    }

    fn read_uncached(&self, io: &mut Io, idxs: &[usize]) -> Result<Vec<u32>> {
        let addrs: Vec<u32> = idxs.iter().map(|&i| self.reg_addr(&MCS_REGS[i])).collect();
        let vals = io.read32_many(&addrs)?;
        Ok(idxs.iter().zip(vals).map(|(&i, v)| if i == MCS_REG_PC { v & 0xFFFC } else { v }).collect())
    }

    /// Channel registers (R0..R7, PC) can only be written while the channel is disabled.
    pub fn write_reg(&mut self, io: &mut Io, idx: usize, value: u32) -> Result<()> {
        let def = *MCS_REGS.get(idx).ok_or_else(|| other(format!("invalid register {idx}")))?;
        if def.group == RegGroup::General {
            self.settle(io)?;
        }
        let value = if idx == MCS_REG_PC { value & 0xFFFC } else { value };
        io.write32(self.reg_addr(&def), value)?;
        self.invalidate();
        if idx == MCS_REG_PC {
            self.pc = value;
        }
        Ok(())
    }

    // --- Breakpoints --------------------------------------------------------------------------

    pub fn add_breakpoint(&mut self, io: &mut Io, addr: u32, len: u32) -> Result<u8> {
        if len > 4 {
            return Err(Error::Resource("Breakpoint ranges are not supported on MCS channels".into()));
        }
        self.ram_addr(addr)?;
        if self.slots.contains(&Some(addr)) {
            return Err(Error::Resource(format!("Breakpoint at 0x{addr:x} exists")));
        }
        let planted = self.shared().bps.get(&addr).copied();
        match planted {
            Some(_) => {
                if let Some(b) = self.shared().bps.get_mut(&addr) {
                    b.refs += 1;
                }
            }
            None => {
                let orig = self.read_word(io, addr)?;
                self.write_word(io, addr, mcs::self_jump(addr))?;
                self.shared().bps.insert(addr, Planted { orig, refs: 1 });
            }
        }
        let slot = match self.slots.iter().position(Option::is_none) {
            Some(s) => s,
            None => {
                self.slots.push(None);
                self.slots.len() - 1
            }
        };
        if slot > u8::MAX as usize {
            return Err(Error::Resource("Too many breakpoints".into()));
        }
        self.slots[slot] = Some(addr);
        Ok(slot as u8)
    }

    /// Breakpoints of the module RAM (all channels), for `gtm_mcs triggers`.
    pub fn describe_breakpoints(&self) -> Vec<String> {
        let ram = self.shared();
        let mut out: Vec<String> = ram
            .bps
            .iter()
            .map(|(a, b)| {
                let mine = if self.slots.contains(&Some(*a)) { " (this channel)" } else { "" };
                format!("0x{a:04x}: {} channel(s), replaces {}{mine}", b.refs, mcs::format(b.orig, &|_| None))
            })
            .collect();
        out.extend(ram.traps.iter().map(|(a, t)| format!("0x{a:04x}: step trap of channel {}", t.1)));
        if out.is_empty() {
            out.push("no breakpoints".into());
        }
        out
    }

    pub fn add_watchpoint(&mut self, _io: &mut Io, _addr: u32, _len: u32, _access: u32) -> Result<u8> {
        Err(Error::Resource("MCS channels have no watchpoints".into()))
    }

    pub fn remove_trigger(&mut self, io: &mut Io, slot: u8) -> Result<()> {
        let Some(addr) = self.slots.get_mut(slot as usize).and_then(Option::take) else {
            return Ok(());
        };
        let restore = {
            let mut ram = self.shared();
            match ram.bps.get_mut(&addr) {
                Some(b) if b.refs > 1 => {
                    b.refs -= 1;
                    None
                }
                Some(b) => {
                    let orig = b.orig;
                    ram.bps.remove(&addr);
                    Some(orig)
                }
                None => None,
            }
        };
        match restore {
            Some(orig) => self.write_word(io, addr, orig),
            None => Ok(()),
        }
    }

    /// Plants all breakpoints of this channel again (e.g. after the RAM was cleared).
    pub fn restore_triggers(&mut self, io: &mut Io) -> Result<()> {
        if self.slots.iter().any(Option::is_some) {
            self.replant = true;
            self.fixup(io)?;
        }
        Ok(())
    }

    pub fn assert_reset(&mut self) {
        self.invalidate();
        self.state = TargetState::Reset;
        self.reason = DebugReason::Reset;
        self.used = false;
        self.stop_req = false;
        self.shared().traps.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(module_regs(0), 0xF01F_0000);
        assert_eq!(module_regs(1), 0xF01F_1000);
        assert_eq!(module_ram(0), 0xF013_8000);
        assert_eq!(module_ram(2), 0xF014_8000);
        let mut c = McsChannel::new();
        c.module = 1;
        c.channel = 3;
        assert_eq!(c.chan(PC), 0xF01F_1000 + 3 * 0x80 + 0x40);
        assert_eq!(c.reg_addr(&MCS_REGS[GTM_MCS.reg_index("ctrl_stat").unwrap()]), 0xF01F_1064);
        assert_eq!(c.ram_addr(0x200).unwrap(), 0xF014_0200);
        assert!(c.ram_addr(0x201).is_err());
    }

    #[test]
    fn ram_size_and_config() {
        let mut c = McsChannel::new();
        assert_eq!(c.ram_size(), DEFAULT_RAM_SIZE);
        assert!(c.ram_addr(DEFAULT_RAM_SIZE - 4).is_ok() && c.ram_addr(DEFAULT_RAM_SIZE).is_err());
        c.set_ram_size(0x2000).unwrap();
        assert!(c.owns_address(0x1FFC) && !c.owns_address(0x2000));
        assert!(c.ram_addr(0x2000).is_err());
        assert!(c.set_ram_size(0x2002).is_err() && c.set_ram_size(0).is_err() && c.set_ram_size(MAX_RAM_SIZE + 4).is_err());
        c.set_ram_size(MAX_RAM_SIZE).unwrap();
        c.module = MAX_MODULES - 1;
        c.channel = CHANNELS - 1;
        assert!(c.check_config().is_ok());
        c.module = MAX_MODULES;
        assert!(c.check_config().is_err());
        c.module = 0;
        c.channel = CHANNELS;
        assert!(c.check_config().is_err());
    }

    #[test]
    fn run_states() {
        let mut c = McsChannel::new();
        c.channel = 2;
        c.state = TargetState::Running;
        // Reset state: disabled at the reset PC.
        assert_eq!(c.poll_apply(&[0, 8]), Transition::Halted { debug: false, boot: false });
        assert!(!c.used);
        assert_eq!(c.poll_apply(&[CTRL_EN, 0x200]), Transition::Running);
        assert!(c.used);
        c.shared().bps.insert(0x300, Planted { orig: 0, refs: 1 });
        c.slots.push(Some(0x300));
        assert_eq!(c.poll_apply(&[CTRL_EN, 0x300, mcs::self_jump(0x300)]), Transition::Halted { debug: false, boot: false });
        assert_eq!(c.reason, DebugReason::Breakpoint);
        assert_eq!(c.hit_trigger(), Some(0));
        assert!(!c.wants_fixup());
        c.poll_apply(&[CTRL_EN, 0x300, 0x1000_0000]);
        assert!(c.wants_fixup());
    }
}
