//! Target reset, as winIDEA's `CExecutionController.reset()` does it (found
//! with 9.21.356; 9.21.417 sends the same with the reconnect renumbered).
//!
//! winIDEA's reset step sends 151 messages. Sent one at a time on a TC377 with
//! the core running (`reset.jsonl`, `examples/probe.rs --then`), three were
//! enough to reset the chip (STM0 uptime back to about 0 ms) and leave cpu0
//! halted at its start address:
//!
//! 1. `//2///7/` body `01`: prepares the reset. Alone it changes nothing.
//! 2. `//7///7/` body eight zero bytes: the reset. Without the next message
//!    the debug connection stays broken (every read fails).
//! 3. `//2///34/` (9.21.356: `//2///38/`) body `01 00 00 00`: reconnects. Its reply carries cpu0's
//!    `DBGSR` and PC (`0x1013`, `0x800a8000` after the reset). Sent on its own,
//!    it reports the running core's state without resetting anything.
//!
//! On an iC5000 the reconnect must wait until the target has come out of
//! reset: sent 10 ms after the reset, it found cpu0 halted, and the connection
//! dropped about 30 ms later for good. winIDEA waits 110 ms there
//! (`ic5000.jsonl`), and so does [`Probe::reset_halt`] ([`RECONNECT_WAIT_IC5000`]);
//! the target then stayed reachable.
//!
//! The reset clears the OCDS trigger setup. winIDEA's `resetAndRun()` writes it
//! again (CBS_TLCHE, TL1ST, TRCx, TRHSM, TLC, OCNTRL, STMx_OCS) before it runs;
//! `tc3xx_ocds.cfg` does the same in its reset hooks. Running after the reset is
//! left to the caller: write `DBGSR` to resume (see the `bluebox` driver).

use crate::frame::REPLY_CAPACITY;
use crate::probe::{Exchange, Fetch};
use crate::{hex, message, Error, Link, Model, Probe, Result};

/// The reconnect after the reset, which also reports cpu0's state (winIDEA 9.21.417).
pub const RECONNECT: &str = "//2///34/";

/// How long an iC5000 waits between the reset and the reconnect, as winIDEA does.
pub const RECONNECT_WAIT_IC5000: std::time::Duration = std::time::Duration::from_millis(110);

/// cpu0's state as [`RECONNECT`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreState {
    /// cpu0 `DBGSR` (bit 1 set: halted).
    pub dbgsr: u32,
    pub pc: u32,
}

impl CoreState {
    pub fn halted(&self) -> bool {
        self.dbgsr & 0x2 != 0
    }
}

pub(crate) fn exchange(path: &str, body: &[u8]) -> Exchange {
    Exchange {
        payload: message::encode(message::KIND, path, body),
        fetch: Fetch::new(0x0e, crate::frame::READ_WINDOW, 0x02),
        reads: vec![REPLY_CAPACITY],
        expect: Vec::new(),
        pause_ms: 0,
    }
}

pub(crate) fn service_reply(path: &str, r: &[u8]) -> Result<()> {
    if r.len() < 0x1e || !message::is_reply(r) {
        return Err(Error::Protocol(format!("{path}: {}", hex(r, 32))));
    }
    Ok(())
}

impl<L: Link> Probe<L> {
    /// cpu0's `DBGSR` and PC (`//2///34/ 01`). Changes nothing on the target.
    pub fn core_state(&mut self) -> Result<CoreState> {
        let r = self.exchange(&exchange(RECONNECT, &[0x01, 0, 0, 0]))?.remove(0);
        service_reply(RECONNECT, &r)?;
        let word = |o: usize| r.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
        match (word(0x1e), word(0x22)) {
            (Some(dbgsr), Some(pc)) => Ok(CoreState { dbgsr, pc }),
            _ => Err(Error::Protocol(format!("{RECONNECT} reply too short: {}", hex(&r, 48)))),
        }
    }

    /// Resets the target and leaves cpu0 halted at its start address. Returns
    /// cpu0's state after the reset.
    pub fn reset_halt(&mut self) -> Result<CoreState> {
        self.reset_without_reconnect()?;
        if self.model() == Model::Ic5000 {
            std::thread::sleep(RECONNECT_WAIT_IC5000);
        }
        self.core_state()
    }

    /// The reset without the reconnect that follows it: the chip resets and
    /// the probe's target connection stays broken, as after a reset the
    /// target did by itself. A test aid for recovery.
    pub fn reset_without_reconnect(&mut self) -> Result<()> {
        for (path, body) in [("//2///7/", &[0x01][..]), ("//7///7/", &[0u8; 8][..])] {
            let r = self.exchange(&exchange(path, body))?.remove(0);
            service_reply(path, &r)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::ScriptedLink;

    /// The replies captured in winIDEA's reset step (`reset.jsonl`).
    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn reset_halt_sends_winidea_s_three_messages_and_parses_the_state() {
        let mut link = ScriptedLink::default();
        link.replies.extend([
            unhex("12000000555555555555555555555555080000000d010000000000000000000000000101"),
            unhex("0f000000555555555555555555555555080000000a0100000000000000000100"),
            unhex("1a00000055555555555555555555555508000000150100000000000000001310000000800a80000000000000"),
        ]);
        let mut p = Probe::new(link, false);
        let st = p.reset_halt().unwrap();
        assert_eq!(st, CoreState { dbgsr: 0x1013, pc: 0x800a_8000 });
        assert!(st.halted());
        // As captured: message frames and fetches, toggles alternating.
        let w: Vec<String> = p.link_mut().written.iter().map(|(_, d)| hex(d, 64)).collect();
        assert_eq!(w[0], "6a 00 14 00 00 00 00 14 23 00 00 00 00 01 08 2f 2f 32 2f 2f 2f 37 2f 00 00 00 01 01");
        assert_eq!(w[1], "8e f0 00 00 00 00 00 02");
        assert!(w[2].starts_with("ea 00 1b 00 00 00 00 14 23 00 00 00 00 01 08 2f 2f 37 2f 2f 2f 37 2f 00 00 00 08 00 00"));
        assert!(w[4].starts_with("6a 00 18 00 00 00 00 14 23 00 00 00 00 01 09 2f 2f 32 2f 2f 2f 33 34 2f 00 00 00 04 01"));
    }

    #[test]
    fn a_short_state_reply_is_an_error() {
        let mut link = ScriptedLink::default();
        link.replies.push_back(unhex("0f000000555555555555555555555555080000000a0100000000000000000100"));
        let mut p = Probe::new(link, false);
        assert!(matches!(p.core_state(), Err(Error::Protocol(_))));
    }
}
