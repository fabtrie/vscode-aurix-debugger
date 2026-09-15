//! TCF context IDs for the targets of the core.
//!
//! `S0.D0` is the chip (container), `S0.D0.C<coreid>` the cores - the
//! extension derives thread numbers from the `C<n>` part.

use aurix_core::arch::REG_PC;
use aurix_core::target::{DebugReason, TargetState};
use aurix_core::{ops, ServerHandle};

pub const CONTAINER: &str = "S0.D0";

#[derive(Clone, Debug)]
pub struct CoreInfo {
    pub idx: usize,
    pub id: String,
    pub name: String,
    pub examined: bool,
    pub state: TargetState,
    pub reason: DebugReason,
}

impl CoreInfo {
    pub fn suspended(&self) -> bool {
        self.examined && self.state == TargetState::Halted
    }
}

pub fn core_id(coreid: u32) -> String {
    format!("{CONTAINER}.C{coreid}")
}

pub fn cores(h: &ServerHandle) -> Vec<CoreInfo> {
    h.call(|cx| {
        cx.with(|c| {
            c.targets
                .iter()
                .enumerate()
                .map(|(i, t)| CoreInfo {
                    idx: i,
                    id: core_id(t.core.coreid),
                    name: t.name.clone(),
                    examined: t.core.examined,
                    state: t.core.state,
                    reason: t.core.reason,
                })
                .collect()
        })
    })
    .unwrap_or_default()
}

pub fn chip_name(cores: &[CoreInfo]) -> String {
    cores.first().and_then(|c| c.name.split('.').next()).unwrap_or("target").to_string()
}

/// The core context an ID belongs to (`S0.D0.C1.R.pc` -> `S0.D0.C1`).
pub fn owning_core(id: &str) -> Option<String> {
    let rest = id.strip_prefix(CONTAINER)?.strip_prefix(".C")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    Some(format!("{CONTAINER}.C{digits}"))
}

pub fn find_core(h: &ServerHandle, id: &str) -> Option<CoreInfo> {
    let core = owning_core(id)?;
    cores(h).into_iter().find(|c| c.id == core)
}

pub fn read_pc(h: &ServerHandle, idx: usize) -> Option<u32> {
    h.call(move |cx| ops::read_reg(cx, idx, REG_PC).ok()).flatten()
}

/// TCF suspend reason string for a core halt reason.
pub fn reason_text(r: DebugReason) -> &'static str {
    match r {
        DebugReason::Breakpoint => "Breakpoint",
        DebugReason::Watchpoint => "Watchpoint",
        DebugReason::SingleStep => "Step",
        DebugReason::Reset => "Reset",
        _ => "Suspended",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owning() {
        assert_eq!(owning_core("S0.D0.C1").as_deref(), Some("S0.D0.C1"));
        assert_eq!(owning_core("S0.D0.C12.R.pc").as_deref(), Some("S0.D0.C12"));
        assert_eq!(owning_core("S0.D0"), None);
    }
}
