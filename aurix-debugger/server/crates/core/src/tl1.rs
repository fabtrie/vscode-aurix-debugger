//! Multi-core run control through trigger line TL1 (`ocds sync tl1`): all cores of the chip halt,
//! resume and step together.
//!
//! Every BRKOUT (CPUs, HSM) drives TL1, capture-and-hold keeps it active, and TL1 suspends all CPUs,
//! the HSM, DMA, HSSL0 and every module whose OCS.SUS is set, in the same cycle. A pause pulses TL1
//! itself (`tl1_halt`). A resume releases the other cores' debug halt while the held TL1 still
//! suspends them, resumes this core, then drops the hold: all start together. A step does the same
//! for one instruction (`tl1_step`), or with `ocds step single` keeps the other cores halted.
//!
//! The routing is cleared by every power-on class reset: it is written again by the restore after a
//! reset ([`crate::restore`]) and at the attach. Off where the probe owns OCDS (winIDEA).

use crate::adapter::Io;
use crate::arch::*;
use crate::ctx::{Ctx, Cx};
use crate::target::{Core, DebugReason, TargetState};

pub const CBS_TL1ST: u32 = 0xF000_0494;
pub const CBS_TLCHE: u32 = 0xF000_0498;
pub const CBS_TRHSM: u32 = 0xF000_0438;
pub const fn cbs_trc(coreid: u32) -> u32 {
    0xF000_0420 + 4 * coreid
}
/// CBS_TRCx / CBS_TRHSM: the BRKOUT drives TL1.
const TR_TL1: u32 = 0x100;
/// CBS_TLC: force TL1 (TLSP1 = 3).
const TLC_FORCE_TL1: u32 = 0x30;
/// CBS_TLCHE / CBS_TLCHS: capture-and-hold of TL1 on, its hold cleared.
const TL1_HOLD: u32 = 0x2;

/// CBS_TL1ST bits (TC3xx: C0-C5, HSS0, DMA, HSM).
const TL1ST_CPUS: u32 = 0x3F;
const TL1ST_HSSL0: u32 = 1 << 28;
const TL1ST_DMA: u32 = 1 << 29;
const TL1ST_HSM: u32 = 1 << 31;

/// What TL1 suspends (`ocds tl1_members`); besides these, every module whose OCS.SUS is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Members {
    pub cpus: bool,
    pub hsm: bool,
    pub dma: bool,
    pub hssl: bool,
}

impl Members {
    pub const ALL: Members = Members { cpus: true, hsm: true, dma: true, hssl: true };

    pub fn parse(list: &str) -> Result<Members, String> {
        let mut m = Members { cpus: false, hsm: false, dma: false, hssl: false };
        for w in list.split_whitespace() {
            match w {
                "cpus" => m.cpus = true,
                "hsm" => m.hsm = true,
                "dma" => m.dma = true,
                "hssl" => m.hssl = true,
                _ => return Err(format!("{w}: not a TL1 member (cpus, hsm, dma, hssl)")),
            }
        }
        Ok(m)
    }

    pub fn names(&self) -> String {
        let mut v = Vec::new();
        for (on, n) in [(self.cpus, "cpus"), (self.hsm, "hsm"), (self.dma, "dma"), (self.hssl, "hssl")] {
            if on {
                v.push(n);
            }
        }
        v.join(" ")
    }

    fn tl1st(&self, hsm: bool) -> u32 {
        (if self.cpus { TL1ST_CPUS } else { 0 })
            | (if self.hssl { TL1ST_HSSL0 } else { 0 })
            | (if self.dma { TL1ST_DMA } else { 0 })
            | (if self.hsm && hsm { TL1ST_HSM } else { 0 })
    }
}

/// `ocds ...` settings, kept from before `init`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// `ocds sync tl1|none`.
    pub sync: bool,
    pub members: Members,
    /// `ocds step_timers frozen`: the cores' suspend-out (DBGSR.SUSP) stays set while halted and
    /// stepping, so the STMs do not advance during a step.
    pub frozen: bool,
    /// `ocds step all|single`: a step runs all cores for the instruction, or only this one.
    pub step_all: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { sync: false, members: Members::ALL, frozen: false, step_all: true }
    }
}

/// The TL1 set-up of the session, as built from the targets (shared with the device watch).
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub on: bool,
    /// The TriCore cores: target index and DBGSR address.
    pub cores: Vec<(usize, u32)>,
    /// The HSM target, when it is a TL1 member.
    pub hsm: Option<usize>,
    /// The routing writes (cleared by a power-on class reset).
    pub config: Vec<(u32, u32)>,
    pub frozen: bool,
    /// Suspend-out set on all cores for this halt (`frozen`).
    pub susp_set: bool,
}

impl Plan {
    fn build(c: &Ctx, s: &Settings, own_ocds: bool) -> Plan {
        let mut cores = Vec::new();
        let mut ids = Vec::new();
        let mut hsm = None;
        for (i, e) in c.targets.iter().enumerate() {
            match &e.core {
                Core::Tricore(t) if t.dbgbase.is_some() => {
                    cores.push((i, t.csfr(DBGSR)));
                    ids.push(t.coreid);
                }
                Core::CortexM(_) if s.members.hsm => hsm = hsm.or(Some(i)),
                _ => {}
            }
        }
        let on = s.sync && own_ocds && !cores.is_empty();
        let mut config = Vec::new();
        if on {
            // Drop a TL1 force and a hold left over (it would keep booting cores suspended), no
            // suspend-out, the suspend targets, every BRKOUT on TL1, capture-and-hold.
            config.push((CBS_TLC, 0));
            config.push((CBS_TLCHS, TL1_HOLD));
            config.extend(cores.iter().map(|&(_, d)| (d, 0)));
            config.push((CBS_TL1ST, s.members.tl1st(hsm.is_some())));
            config.extend(ids.iter().map(|&id| (cbs_trc(id), TR_TL1)));
            if hsm.is_some() {
                config.push((CBS_TRHSM, TR_TL1));
            }
            config.push((CBS_TLCHE, TL1_HOLD));
        }
        Plan { on, cores, hsm: if on { hsm } else { None }, config, frozen: s.frozen, susp_set: false }
    }

    /// Whether target `t` halts and runs with the others.
    pub fn member(&self, t: usize) -> bool {
        self.on && (self.cores.iter().any(|&(i, _)| i == t) || self.hsm == Some(t))
    }
}

/// Builds the plan from the settings and the targets, and gives the cores their part of it
/// (`tl1_halt`, `tl1_step`, the other cores to release in a step). At `init` and when a setting
/// changes afterwards.
pub fn apply(c: &mut Ctx) {
    let s = c.tl1;
    // Not where the probe does run control itself and owns OCDS (winIDEA).
    let own_ocds = c.adapter.as_deref_mut().is_some_and(|a| a.run_control().is_none()) && c.attach_mode != crate::attach::Mode::Passive;
    let plan = Plan::build(c, &s, own_ocds);
    let was_on = c.restore.tl1.borrow().on;
    let peers: Vec<u32> = plan.cores.iter().map(|&(_, d)| d).collect();
    for (i, e) in c.targets.iter_mut().enumerate() {
        match &mut e.core {
            Core::Tricore(t) if plan.member(i) => {
                t.run.tl1_halt = true;
                t.run.tl1_step = s.step_all;
                let own = t.csfr(DBGSR);
                t.tl1_peers = peers.iter().copied().filter(|&d| d != own).collect();
                t.tl1_freeze = s.frozen;
            }
            Core::Tricore(t) => {
                t.tl1_peers.clear();
                t.tl1_freeze = false;
                // Switched off: what `sync tl1` turned on goes off with it.
                if was_on {
                    t.run.tl1_halt = false;
                    t.run.tl1_step = false;
                }
            }
            Core::CortexM(m) if plan.member(i) => m.run.tl1_halt = true,
            Core::CortexM(m) if was_on => m.run.tl1_halt = false,
            _ => {}
        }
    }
    *c.restore.tl1.borrow_mut() = plan;
}

/// One TL1 pulse, latched by capture-and-hold: all members stop in the same cycle.
pub fn force_writes() -> [(u32, u32); 2] {
    [(CBS_TLC, TLC_FORCE_TL1), (CBS_TLC, 0)]
}

/// The routing, after a reset or at the attach (errors ignored: right after a reset parts may not
/// answer yet, and the next restore writes it again).
pub fn write_config(io: &mut Io, plan: &Plan) {
    if plan.on && !plan.config.is_empty() {
        let _ = io.write32_seq_packed(&plan.config);
    }
}

/// Before a member resumes: the other cores' debug halt released (the held TL1 keeps them
/// suspended), a halted HSM resumed (it keeps BRKOUT active and would latch TL1 again).
pub fn before_resume(cx: &Cx, t: usize) {
    let plan = cx.with(|c| c.restore.tl1.borrow().clone());
    if !plan.member(t) {
        return;
    }
    let writes: Vec<(u32, u32)> = plan.cores.iter().filter(|&&(i, _)| i != t).map(|&(_, d)| (d, DBGSR_RESUME)).collect();
    cx.with(|c| {
        let Ok(mut io) = c.io() else { return };
        if let Err(e) = io.write32_seq_packed(&writes) {
            crate::log_warn!("TL1: releasing the other cores failed: {e}");
        }
    });
    if let Some(h) = plan.hsm.filter(|&h| h != t) {
        cx.with(|c| {
            let Ok((mut io, e)) = c.target_io(h) else { return };
            if let Core::CortexM(m) = &mut e.core {
                // Not examined (HSM debugging off): nothing it could do there either.
                if m.examined {
                    let _ = m.release_if_halted(&mut io);
                }
            }
        });
    }
}

/// The other TriCore cores of `t`'s TL1 group in a debug halt now: a resume of `t` releases them
/// ([`before_resume`]). Not a core in boot halt: the release does not start it.
pub fn halted_peers(cx: &Cx, t: usize) -> Vec<usize> {
    cx.with(|c| {
        let plan = c.restore.tl1.borrow();
        if !plan.member(t) {
            return Vec::new();
        }
        plan.cores
            .iter()
            .map(|&(i, _)| i)
            .filter(|&i| i != t && c.targets.get(i).is_some_and(|e| e.core.state() == TargetState::Halted && e.core.reason() != DebugReason::BootHalt))
            .collect()
    })
}

/// After a member resumed: suspend-out off on all cores, then the TL1 hold dropped, in one
/// transaction: all cores and suspended modules start in the same cycle.
pub fn after_resume(cx: &Cx, t: usize) {
    let plan = cx.with(|c| c.restore.tl1.borrow().clone());
    if !plan.member(t) {
        return;
    }
    let mut writes: Vec<(u32, u32)> = plan.cores.iter().map(|&(_, d)| (d, 0)).collect();
    writes.push((CBS_TLCHS, TL1_HOLD));
    cx.with(|c| {
        c.restore.tl1.borrow_mut().susp_set = false;
        let Ok(mut io) = c.io() else { return };
        if let Err(e) = io.write32_seq_packed(&writes) {
            crate::log_warn!("TL1: dropping the hold failed: {e}");
        }
    });
}

/// A member halted: with `ocds step_timers frozen`, suspend-out on all cores (once per halt).
pub fn on_halted(cx: &Cx, t: usize) {
    let writes = cx.with(|c| {
        let mut p = c.restore.tl1.borrow_mut();
        if !p.member(t) || !p.frozen || p.susp_set {
            return None;
        }
        p.susp_set = true;
        Some(p.cores.iter().map(|&(_, d)| (d, DBGSR_SUSP)).collect::<Vec<_>>())
    });
    if let Some(w) = writes {
        cx.with(|c| {
            if let Ok(mut io) = c.io() {
                let _ = io.write32_seq_packed(&w);
            }
        });
    }
}

/// At the attach: the routing, and if cpu0 is found halted (an earlier session ended halted), all
/// cores halted together again, as the halt that left them was (part of the application running on
/// alone resets some boards).
pub fn on_attach(cx: &Cx) {
    let plan = cx.with(|c| c.restore.tl1.borrow().clone());
    if !plan.on {
        return;
    }
    let cpu0_halted = cx.with(|c| plan.cores.first().is_some_and(|&(i, _)| c.targets[i].core.examined() && c.targets[i].core.state() == TargetState::Halted));
    cx.with(|c| {
        let Ok(mut io) = c.io() else { return };
        write_config(&mut io, &plan);
        if cpu0_halted {
            let _ = io.write32_seq_packed(&force_writes());
        }
    });
}

/// `tl1_step`: TL1 latched before a step releases the other cores, so that they stay suspended
/// until the step starts.
pub fn ensure_hold(io: &mut Io) -> crate::error::Result<()> {
    if io.read32(CBS_TLS)? & CBS_TLS_TL1 == 0 {
        io.write32_seq(&force_writes())?;
    }
    Ok(())
}

// --- Command -----------------------------------------------------------------------------------

const USAGE: &str = "usage: ocds sync ?tl1|none? | ocds tl1_members ?{cpus hsm dma hssl}? | ocds step ?all|single? | ocds step_timers ?run|frozen? | ocds tl1_force";

/// `ocds ...`: the TL1 settings (before or after `init`), and `ocds tl1_force`.
pub fn cmd_ocds(cx: &Cx, a: &[&str]) -> Result<String, String> {
    let mut s = cx.with(|c| c.tl1);
    let out = match (a.get(1).copied(), a.get(2).copied()) {
        (Some("sync"), v) => {
            if let Some(v) = v {
                s.sync = match v {
                    "tl1" => true,
                    "none" => false,
                    _ => return Err(USAGE.into()),
                };
            }
            (if s.sync { "tl1" } else { "none" }).to_string()
        }
        (Some("tl1_members"), v) => {
            if let Some(v) = v {
                s.members = Members::parse(v)?;
            }
            s.members.names()
        }
        (Some("step"), v) => {
            if let Some(v) = v {
                s.step_all = match v {
                    "all" => true,
                    "single" => false,
                    _ => return Err(USAGE.into()),
                };
            }
            (if s.step_all { "all" } else { "single" }).to_string()
        }
        (Some("step_timers"), v) => {
            if let Some(v) = v {
                s.frozen = match v {
                    "frozen" => true,
                    "run" => false,
                    _ => return Err(USAGE.into()),
                };
            }
            (if s.frozen { "frozen" } else { "run" }).to_string()
        }
        (Some("tl1_force"), None) => {
            cx.with(|c| -> Result<(), String> {
                let mut io = c.io().map_err(|e| e.to_string())?;
                io.write32_seq(&force_writes()).map_err(|e| e.to_string())
            })?;
            return Ok(String::new());
        }
        _ => return Err(USAGE.into()),
    };
    cx.with(|c| {
        c.tl1 = s;
        if c.initialized {
            apply(c);
        }
    });
    Ok(out)
}
