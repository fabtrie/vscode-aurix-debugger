//! The probe's own view of the target connection: target supply, reset latch
//! and debug access, read without touching the target's memory.
//!
//! winIDEA 9.21.417 sends this about 500 times a second on its poll thread: the
//! system batch status read (`//1///35/`, kernel type `SBBK_BB_BatchStatusRead`)
//! asking for the SoC part only (`What` = 7) and no cores. It takes about
//! 1.15 ms, the same as one small memory read. The field layout comes from the
//! kernel's reflection tables (`NSystemBatchStatusRead::SOut`,
//! `NBatchStatusRead::SOut`): little-endian, no padding, a vector is a `u32`
//! count followed by its elements.
//!
//! Measured on a TC377 board with an FS26 on the reset line:
//!
//! * `Vref_mV` is the target's I/O supply (about 4990 mV). A supply dip shows as
//!   `VrefOK` 0 and a short reply without the debug subsystem entry.
//! * `SRSTActive` is a latch: after any reset it stays set until the probe's
//!   connect messages run again (the attach's connect part), also long after the
//!   target came out of reset. It means "reset since the last connect", not
//!   "held in reset".
//! * `CommWorks` drops as soon as the debug connection is lost (a reset, a dip).
//! * The other single-core and multi-core status reads of the same service read
//!   the target and cost about 1 to 2.5 ms per core; they are not used.

use crate::probe::{Exchange, Fetch};
use crate::frame::REPLY_CAPACITY;
use crate::{hex, message, Error, Link, Probe, Result};

/// The system batch status read.
pub const SERVICE: &str = "//1///35/";

/// One SoC (index 0) with `What` = 7, no core items, `SoCItem` zero, one debug
/// subsystem, not a repeated read: what winIDEA's poll thread sends.
const REQUEST: [u8; 29] = [
    0x01, 0, 0, 0, // SoCs: 1
    0, 0, 0, 0, // SoCIndex
    0x07, 0, 0, 0, // What
    0, 0, 0, 0, // CoreItems: 0
    0, 0, 0, 0, // SoCItem.What
    0, 0, 0, 0, // SoCItem.HPOperation
    0x01, 0, 0, 0, // NumOfDebugSubSystems
    0,    // IsRepeatedReadStatus
];

/// Reply bytes before the first field: the 16-byte frame and the service reply header.
const BODY: usize = 16 + 14;

/// What the probe reports about the target connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TargetHealth {
    /// Target I/O supply as the probe measures it.
    pub vref_mv: u32,
    /// The supply is inside the probe's limits.
    pub vref_ok: bool,
    /// A reset happened since the probe's last connect (a latch, see the module docs).
    pub srst_active: bool,
    /// The OCDS `OSTATE` register (0 when there is no debug subsystem entry).
    pub ostate: u32,
    pub has_access: bool,
    /// The debug connection works.
    pub comm_works: bool,
    /// The probe has just restored the debug connection.
    pub comm_recovery: bool,
    /// Reported by the probe; what sets it on a TriCore is not known yet.
    pub app_reset: bool,
}

/// Little-endian reader with bounds checks.
struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}

impl Rd<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let s = self.b.get(self.i..self.i + n).ok_or_else(|| Error::Protocol(format!("{SERVICE}: reply ends at {}", self.i)))?;
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
}

/// Parses the reply to [`REQUEST`].
pub fn parse(r: &[u8]) -> Result<TargetHealth> {
    if !message::is_reply(r) {
        return Err(Error::Protocol(format!("{SERVICE}: {}", hex(r, 32))));
    }
    // The length word at 0 counts the bytes after the frame header.
    let len = r.get(..4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    let end = (16 + len).min(r.len());
    let mut d = Rd { b: r.get(..end).unwrap_or(r), i: BODY };
    let socs = d.u32()?;
    if socs != 1 {
        return Err(Error::Protocol(format!("{SERVICE}: {socs} SoCs in the reply")));
    }
    d.skip(4)?; // SoCIndex
    d.skip(4)?; // What
    d.skip(8)?; // BatchStatusReadTime
    let mut h = TargetHealth { vref_mv: d.u32()?, vref_ok: d.u8()? != 0, srst_active: d.u8()? != 0, ..Default::default() };
    d.skip(1)?; // StatusReadSkipped
    // Without target supply the probe sends no debug subsystem entry.
    if d.u32()? > 0 {
        d.skip(1)?; // IsDebuggable
        if d.u32()? > 0 {
            // NBatchStatusRead::SOut::SDebugSubSystemData::SAurix, in declaration order.
            h.ostate = d.u32()?;
            h.has_access = d.u8()? != 0;
            d.skip(4)?; // OCDFaults
            d.skip(1)?; // OpBufferFull
            d.skip(1)?; // OpPercentage
            d.skip(8)?; // AllOps
            d.skip(1)?; // ReadSuccess
            d.skip(1)?; // DXCPLEngineActive
            h.app_reset = d.u8()? != 0;
            h.comm_works = d.u8()? != 0;
            h.comm_recovery = d.u8()? != 0;
        }
    }
    Ok(h)
}

impl<L: Link> Probe<L> {
    /// The probe's view of the target connection ([`SERVICE`], about 1.15 ms).
    /// Reads nothing on the target.
    pub fn target_health(&mut self) -> Result<TargetHealth> {
        let x = Exchange {
            payload: message::encode(message::KIND, SERVICE, &REQUEST),
            fetch: Fetch::new(0x0e, crate::frame::READ_WINDOW, 0x02),
            reads: vec![REPLY_CAPACITY],
            expect: Vec::new(),
            pause_ms: 0,
        };
        let r = self.exchange(&x)?.remove(0);
        parse(&r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::ScriptedLink;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// Captured on the TC377 board (2026-11-08): target running.
    const RUNNING: &str = "6e000000eaebda77a1b3fcfff27f17bf0800000069010000000000000000010000000000000007000000000000000000000080130000010000010000000101000000010000000100000000000000000000000000000100000100000000000000";
    /// After an FS26 supply dip: supply back, reset latched, debug connection lost.
    const AFTER_RESET: &str = "6e000000eaebda77a1b3fcfff27f17bf08000000690100000000000000000100000000000000070000000000000000000000600d0000010100010000000001000000010000000100000000000000000000000000000100000000000000000000";
    /// During the dip: no supply, no debug subsystem entry.
    const NO_SUPPLY: &str = "35000000eaebda77a1b3fcfff27f17bf0800000030010000000000000000010000000000000007000000000000000000000061020000000000000000000000000000000000000100";
    /// Right after the connect messages ran again.
    const RECOVERED: &str = "6e000000eaebda77a1b3fcfff27f17bf080000006901000000000000000001000000000000000700000000000000000000007c130000010000010000000101000000010000000100000000000000000000000000000100000101000000000000";

    #[test]
    fn a_running_target() {
        let h = parse(&unhex(RUNNING)).unwrap();
        assert_eq!(
            h,
            TargetHealth { vref_mv: 4992, vref_ok: true, srst_active: false, ostate: 1, has_access: true, comm_works: true, comm_recovery: false, app_reset: false }
        );
    }

    #[test]
    fn a_latched_reset_with_the_connection_lost() {
        let h = parse(&unhex(AFTER_RESET)).unwrap();
        assert_eq!((h.vref_mv, h.vref_ok, h.srst_active, h.comm_works), (3424, true, true, false));
    }

    #[test]
    fn no_supply_has_no_debug_subsystem_entry() {
        let h = parse(&unhex(NO_SUPPLY)).unwrap();
        assert_eq!(h, TargetHealth { vref_mv: 609, ..Default::default() });
    }

    #[test]
    fn the_connect_messages_report_the_recovery() {
        let h = parse(&unhex(RECOVERED)).unwrap();
        assert!(h.comm_works && h.comm_recovery && !h.srst_active);
    }

    #[test]
    fn a_short_or_refused_reply_is_an_error() {
        let full = unhex(RUNNING);
        let mut cut = full[..60].to_vec();
        let n = (cut.len() - 16) as u32;
        cut[..4].copy_from_slice(&n.to_le_bytes());
        assert!(matches!(parse(&cut), Err(Error::Protocol(_))));
        assert!(matches!(parse(&full[..16]), Err(Error::Protocol(_))));
    }

    #[test]
    fn target_health_sends_winidea_s_poll_query() {
        let mut link = ScriptedLink::default();
        link.replies.push_back(unhex(RUNNING));
        let mut p = Probe::new(link, false);
        assert!(p.target_health().unwrap().comm_works);
        let w = &p.link_mut().written[0].1;
        let msg = message::encode(message::KIND, SERVICE, &REQUEST);
        assert!(w.windows(msg.len()).any(|s| s == msg.as_slice()), "{}", hex(w, 80));
    }
}
