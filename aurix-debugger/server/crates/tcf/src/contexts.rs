//! TCF context IDs for the targets of the core.
//!
//! `S0.D0` is the chip (container), `S0.D0.C<coreid>` the cores - the
//! extension derives thread numbers from the `C<n>` part.

use aurix_core::arch::ArchInfo;
use aurix_core::target::{DebugReason, TargetState};
use aurix_core::{ops, ServerHandle};

pub const CONTAINER: &str = "S0.D0";

#[derive(Clone, Debug)]
pub struct CoreInfo {
    pub idx: usize,
    pub id: String,
    pub name: String,
    pub examined: bool,
    /// Listed as a thread (e.g. MCS channels only once they ran).
    pub visible: bool,
    /// False while the core has not been accessible for a while (e.g. target not powered).
    pub reachable: bool,
    pub state: TargetState,
    pub reason: DebugReason,
    pub arch: &'static ArchInfo,
}

impl CoreInfo {
    pub fn suspended(&self) -> bool {
        self.examined && self.state == TargetState::Halted
    }
}

/// TCF architecture name of a core (`Architecture` context property).
pub fn tcf_arch(arch: &ArchInfo) -> &'static str {
    match arch.name {
        "cortex_m" => "arm",
        "gtm_mcs" => "gtm_mcs",
        _ => "tricore",
    }
}

/// RunControl context of a core (resume modes: 0 resume, 1 step over, 2 step into, 3 step over
/// line, 4 step into line, 5 step out).
pub fn context_json(c: &CoreInfo) -> serde_json::Value {
    // An inaccessible core keeps running in the frontend (TCF has no such state): its name says it.
    let name = if c.reachable { c.name.clone() } else { format!("{} (not accessible)", c.name) };
    serde_json::json!({ "ID": c.id, "ParentID": CONTAINER, "ProcessID": CONTAINER, "Name": name,
            "IsContainer": false, "HasState": true, "CanSuspend": true, "CanResume": 0x3F,
            "CanCount": 0x3E, "CanTerminate": false, "RCGroup": CONTAINER, "SymbolsGroup": CONTAINER,
            "CPUGroup": CONTAINER, "WordSize": 4, "Architecture": tcf_arch(c.arch) })
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
                    id: core_id(t.core.coreid()),
                    name: t.name.clone(),
                    examined: t.core.examined(),
                    visible: t.core.visible(),
                    reachable: !t.unreachable,
                    state: t.core.state(),
                    reason: t.core.reason(),
                    arch: t.core.arch(),
                })
                .collect()
        })
    })
    .unwrap_or_default()
}

/// Context ID of the target with index `idx` (empty if there is none).
pub fn core_id_of(h: &ServerHandle, idx: usize) -> String {
    cores(h).into_iter().find(|c| c.idx == idx).map(|c| c.id).unwrap_or_default()
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
    h.call(move |cx| {
        let pc = cx.with(|c| c.targets.get(idx).map(|t| t.core.arch().pc))?;
        ops::read_reg(cx, idx, pc).ok()
    })
    .flatten()
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
