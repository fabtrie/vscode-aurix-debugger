//! TriCore register names, as winIDEA spells them.
//!
//! On the TAS path a register is a CSFR offset inside the core's debug window,
//! which is the wrong currency here: winIDEA owns that window and answers by
//! *name* instead. So each index in [`crate::arch::TRICORE`] needs the name
//! winIDEA knows it by.
//!
//! Uppercasing our own name is what winIDEA uses for the registers it names at
//! all. Checked on a BlueBox against winIDEA 9.21.356 and a TC377: every GPR
//! (`D0`..`D15`, `A0`..`A15`), `PCXI`, `PSW` and `PC` are found this way -- the
//! 35 the call-stack view reads. The other system and debug CSFRs (`ICR`,
//! `FCX`, `BTV`, `DBGSR`, ...) are not core registers in winIDEA under any name
//! it accepted; the adapter reads those through the core's CSFR window instead,
//! and learns which ones from winIDEA's own [`is_unknown_register`] answer
//! rather than from a list here.

use crate::arch::TRICORE;

/// Registers whose winIDEA name is not simply ours uppercased.
///
/// Empty: none turned up on the hardware this was checked against. A wrong
/// entry would be indistinguishable from a missing register, so nothing is
/// added here without a winIDEA that confirms it.
const OVERRIDES: &[(&str, &str)] = &[];

/// The winIDEA name for register `idx` of [`crate::arch::TRICORE`].
pub fn name(idx: usize) -> Option<String> {
    let def = TRICORE.regs.get(idx)?;
    Some(winidea_name(def.name))
}

fn winidea_name(ours: &str) -> String {
    match OVERRIDES.iter().find(|(o, _)| *o == ours) {
        Some((_, theirs)) => (*theirs).to_string(),
        None => ours.to_ascii_uppercase(),
    }
}

/// Whether a `read_register` failure means winIDEA has no register by that
/// name -- as opposed to one it has but cannot read right now ("can not be
/// accessed in the current CPU mode"), which must stay an error.
pub fn is_unknown_register(message: &str) -> bool {
    message.contains("specified register does not exist")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_register_has_a_name() {
        // A `None` here would mean a register the Registers view can display
        // but nothing can read.
        for i in 0..TRICORE.regs.len() {
            assert!(name(i).is_some(), "register {i} has no winIDEA name");
        }
    }

    #[test]
    fn names_are_uppercased_by_default() {
        let idx = TRICORE.reg_index("d0").unwrap();
        assert_eq!(name(idx).unwrap(), "D0");
        let idx = TRICORE.reg_index("a10").unwrap();
        assert_eq!(name(idx).unwrap(), "A10");
        let idx = TRICORE.reg_index("pc").unwrap();
        assert_eq!(name(idx).unwrap(), "PC");
    }

    #[test]
    fn aliases_resolve_to_the_same_register() {
        // `sp` is an alias for a10, so both must reach one name -- otherwise a
        // frontend asking for the stack pointer and one asking for a10 would
        // read different things.
        let by_alias = TRICORE.reg_index("sp").unwrap();
        let by_name = TRICORE.reg_index("a10").unwrap();
        assert_eq!(by_alias, by_name);
        assert_eq!(name(by_alias).unwrap(), "A10");
    }

    /// Both messages are verbatim from winIDEA 9.21.356.
    #[test]
    fn unknown_is_told_apart_from_unreadable() {
        assert!(is_unknown_register(
            "application error: F. ReadRegister() failed. The specified register does not exist: ICR.   [CDataController.cpp, 261]"
        ));
        assert!(!is_unknown_register(
            "application error: F. ReadRegister() failed. The register can not be accessed in the current CPU mode.  [CDataController.cpp, 255]"
        ));
    }
}
