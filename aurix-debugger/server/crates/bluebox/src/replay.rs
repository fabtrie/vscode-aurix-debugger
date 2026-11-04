//! Plays back a captured winIDEA attach.
//!
//! The probe's bring-up (target connection, core discovery, OCDS setup) is not
//! decoded yet. Until it is, a session starts by sending the probe what winIDEA
//! sent during `CSessionCtrl.begin_attach`, transfer for transfer, from a
//! trace recorded with Frida capture tools (not in this repository). Replies are read but
//! not interpreted. They are compared with the captured ones, so a probe
//! that reacts differently shows up in the [`ReplayReport`].
//!
//! The captured attach writes the OCDS registers at 0xF0000494 and the STM
//! suspend controls (`STMx_OCS`) with winIDEA's values, as winIDEA does on every
//! attach. It assumes the probe already holds its firmware. A probe that was
//! power-cycled first needs one attach from winIDEA, or a trace of a
//! first attach (23 MB, mostly firmware).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::frame::{Header, CHANNEL_SERVICE, KIND_INLINE, KIND_SPLIT, PIPE_OUT};
use crate::{Error, Link, Result};

const WRITE_PIPE: &str = "0x3508021";
const READ_PIPE: &str = "0x350401e";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transfer {
    Write { pipe: u8, data: Vec<u8> },
    Read { pipe: u8, cap: usize, expect: Vec<u8> },
}

#[derive(Debug, Clone, Default)]
pub struct Replay {
    pub transfers: Vec<Transfer>,
}

#[derive(Debug, Clone)]
pub struct Diff {
    pub index: usize,
    /// The last service message sent before this read.
    pub request: Vec<u8>,
    pub expected: Vec<u8>,
    pub got: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct ReplayReport {
    pub writes: usize,
    pub reads: usize,
    /// Replies identical to the captured ones.
    pub same: usize,
    /// Replies of the captured length, different content.
    pub same_len: usize,
    /// Replies of another length; the first few kept in [`Self::diffs`].
    pub other_len: usize,
    pub diffs: Vec<Diff>,
}

impl Replay {
    /// The probe transfers between the markers `begin <step>` and `end <step>`
    /// of a JSON-lines trace written by `capture.py`.
    pub fn from_trace(path: &Path, step: &str) -> Result<Replay> {
        let f = std::fs::File::open(path).map_err(|e| Error::Trace(format!("{}: {e}", path.display())))?;
        let (begin, end) = (format!("begin {step}"), format!("end {step}"));
        let mut active: Option<i64> = None; // pid the markers came from
        let mut seen = false;
        // The pipe a transfer used is in the IOCTL's input; its bytes are in the
        // completion, logged later and possibly after other calls on the thread.
        let mut pending: HashMap<(i64, String), (u8, usize)> = HashMap::new();
        let mut transfers = Vec::new();
        for (no, line) in BufReader::new(f).lines().enumerate() {
            let line = line.map_err(|e| Error::Trace(e.to_string()))?;
            // Cheap filter: the trace is mostly lines this does not need.
            if !(line.contains("\"mark\"") || line.contains(WRITE_PIPE) || line.contains(READ_PIPE)) {
                continue;
            }
            let r: serde_json::Value =
                serde_json::from_str(&line).map_err(|e| Error::Trace(format!("line {}: {e}", no + 1)))?;
            let ev = r["ev"].as_str().unwrap_or("");
            let pid = r["pid"].as_i64().unwrap_or(-1);
            if ev == "mark" {
                let label = r["label"].as_str().unwrap_or("");
                if label == begin && !seen {
                    active = Some(pid);
                    seen = true;
                } else if label == end && active == Some(pid) {
                    active = None;
                }
                continue;
            }
            if active != Some(pid) {
                continue;
            }
            let code = r["code"].as_str().unwrap_or("").to_string();
            if code != WRITE_PIPE && code != READ_PIPE {
                continue;
            }
            let tid = r["tid"].as_i64().unwrap_or(-1);
            let bytes = unhex(r["hex"].as_str().unwrap_or(""))
                .ok_or_else(|| Error::Trace(format!("line {}: bad hex", no + 1)))?;
            match ev {
                "ioctl" => {
                    // 2-byte selector: 00 <pipe id>.
                    let pipe = *bytes.get(1).ok_or_else(|| Error::Trace(format!("line {}: no pipe", no + 1)))?;
                    let cap = r["cap"].as_u64().unwrap_or(0) as usize;
                    pending.insert((tid, code), (pipe, cap));
                }
                "ioctl_out" => {
                    let (pipe, cap) = pending.remove(&(tid, code.clone())).ok_or_else(|| {
                        Error::Trace(format!("line {}: completion without its call", no + 1))
                    })?;
                    transfers.push(if code == WRITE_PIPE {
                        Transfer::Write { pipe, data: bytes }
                    } else {
                        Transfer::Read { pipe, cap, expect: bytes }
                    });
                }
                _ => {}
            }
        }
        if !seen {
            return Err(Error::Trace(format!("no marker '{begin}' in {}", path.display())));
        }
        if transfers.is_empty() {
            return Err(Error::Trace(format!("no probe transfers inside '{step}'")));
        }
        Ok(Replay { transfers })
    }

    /// Turns every breakpoint the attach would arm into a clear, and returns
    /// how many were armed.
    ///
    /// winIDEA arms the breakpoints of its session during the attach: the
    /// captured attach sets `//3///10/` (body `00 00 00 00 <addr u32 LE> 01 00 80 00 00`)
    /// for 0x80120ae0, left over from an earlier session. Played back as is,
    /// it halted cpu0 there, and cpu1 with it over the break bus. The enable byte
    /// is patched to 00 rather than the message dropped, because the frames
    /// around it carry toggles and lengths that must stay as captured.
    pub fn disarm_breakpoints(&mut self) -> usize {
        const PREFIX: &[u8] = b"\x23\x00\x00\x00\x00\x01\x09//3///10/\x00\x00\x00\x0d";
        let mut n = 0;
        for t in &mut self.transfers {
            let Transfer::Write { data, .. } = t else { continue };
            let mut from = 0;
            while let Some(p) = data[from..].windows(PREFIX.len()).position(|w| w == PREFIX) {
                let enable = from + p + PREFIX.len() + 8;
                if data.get(enable) == Some(&1) {
                    data[enable] = 0;
                    n += 1;
                }
                from += p + PREFIX.len();
            }
        }
        n
    }

    /// Whether the trace loads the probe's kernel itself (low-level `W` commands,
    /// `03 21 57`): an attach captured right after a power cycle. Such a trace
    /// must not be played onto a probe whose kernel runs; use one captured with
    /// the kernel running, after [`crate::lowlevel::bring_up`].
    pub fn loads_kernel(&self) -> bool {
        self.transfers.iter().any(|t| match t {
            Transfer::Write { data, .. } => data.starts_with(&[0x03, 0x21, 0x57]) || data.get(8..11) == Some(&[0x03, 0x21, 0x57][..]),
            _ => false,
        })
    }

    pub fn writes(&self) -> usize {
        self.transfers.iter().filter(|t| matches!(t, Transfer::Write { .. })).count()
    }

    pub fn bytes_out(&self) -> usize {
        self.transfers.iter().map(|t| if let Transfer::Write { data, .. } = t { data.len() } else { 0 }).sum()
    }

    /// The toggle the probe expects on the next service message once this
    /// replay has run: the opposite of the last one's. `None` if it sent none.
    pub fn toggle_after(&self) -> Option<bool> {
        self.transfers.iter().rev().find_map(|t| match t {
            Transfer::Write { pipe: PIPE_OUT, data } => Header::decode(data)
                .filter(|h| h.channel == CHANNEL_SERVICE && (h.kind == KIND_INLINE || h.kind == KIND_SPLIT))
                .map(|h| !h.toggle),
            _ => None,
        })
    }

    /// Sends every transfer in order. `progress` gets the index of each
    /// transfer before it runs.
    ///
    /// Call [`Self::disarm_breakpoints`] first unless the captured
    /// breakpoints are wanted.
    pub fn play<L: Link + ?Sized>(&self, link: &mut L, mut progress: impl FnMut(usize)) -> Result<ReplayReport> {
        let mut rep = ReplayReport::default();
        let mut last_msg: &[u8] = &[];
        for (i, t) in self.transfers.iter().enumerate() {
            progress(i);
            match t {
                Transfer::Write { pipe, data } => {
                    match Header::decode(data) {
                        Some(h) if *pipe == PIPE_OUT && h.kind == KIND_INLINE => last_msg = &data[8..],
                        _ if *pipe != PIPE_OUT => last_msg = data,
                        _ => {}
                    }
                    link.write(*pipe, data).map_err(|e| Error::Trace(format!("transfer {i} (write): {e}")))?;
                    rep.writes += 1;
                }
                Transfer::Read { pipe, cap, expect } => {
                    let got = link.read(*pipe, *cap).map_err(|e| Error::Trace(format!("transfer {i} (read): {e}")))?;
                    rep.reads += 1;
                    if &got == expect {
                        rep.same += 1;
                    } else if got.len() == expect.len() {
                        rep.same_len += 1;
                    } else {
                        rep.other_len += 1;
                        if rep.diffs.len() < 8 {
                            rep.diffs.push(Diff { index: i, request: last_msg.to_vec(), expected: expect.clone(), got });
                        }
                    }
                }
            }
        }
        Ok(rep)
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::ScriptedLink;

    const TRACE: &str = r#"{"ev":"mark","pid":7,"label":"begin connect: begin_attach","info":null}
{"ev":"ioctl","pid":7,"tid":1,"code":"0x3508021","n":2,"cap":12,"hex":"0008"}
{"ev":"ioctl","pid":7,"tid":2,"code":"0x350401e","n":2,"cap":260,"hex":"0082"}
{"ev":"ioctl_out","pid":7,"tid":1,"code":"0x3508021","n":12,"hex":"6a0003000000001403214900"}
{"ev":"ioctl_out","pid":7,"tid":2,"code":"0x350401e","n":4,"hex":"08aaaa00"}
{"ev":"ioctl","pid":7,"tid":1,"code":"0x3508021","n":2,"cap":8,"hex":"0008"}
{"ev":"ioctl_out","pid":7,"tid":1,"code":"0x3508021","n":8,"hex":"e300ca0000000014"}
{"ev":"ioctl","pid":7,"tid":1,"code":"0x3508021","n":2,"cap":204,"hex":"0006"}
{"ev":"ioctl_out","pid":7,"tid":1,"code":"0x3508021","n":4,"hex":"23000000"}
{"ev":"mark","pid":7,"label":"end connect: begin_attach","info":"ATTACHED"}
{"ev":"ioctl","pid":7,"tid":1,"code":"0x3508021","n":2,"cap":8,"hex":"0008"}
{"ev":"ioctl_out","pid":7,"tid":1,"code":"0x3508021","n":8,"hex":"0e00000000000014"}
"#;

    fn trace() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("bluebox-replay-test-{}.jsonl", std::process::id()));
        std::fs::write(&p, TRACE).unwrap();
        p
    }

    #[test]
    fn transfers_between_the_markers_with_their_pipes() {
        let p = trace();
        let r = Replay::from_trace(&p, "connect: begin_attach").unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(r.transfers.len(), 4);
        assert_eq!(r.transfers[0], Transfer::Write { pipe: 0x08, data: vec![0x6a, 0, 3, 0, 0, 0, 0, 0x14, 3, 0x21, 0x49, 0] });
        assert_eq!(r.transfers[1], Transfer::Read { pipe: 0x82, cap: 260, expect: vec![0x08, 0xaa, 0xaa, 0] });
        assert_eq!(r.transfers[3], Transfer::Write { pipe: 0x06, data: vec![0x23, 0, 0, 0] });
        // The last service message carried toggle 1, so the next one carries 0.
        assert_eq!(r.toggle_after(), Some(false));
    }

    #[test]
    fn playing_reports_replies_that_differ() {
        let p = trace();
        let r = Replay::from_trace(&p, "connect: begin_attach").unwrap();
        std::fs::remove_file(&p).ok();
        let mut link = ScriptedLink::default();
        link.replies.push_back(vec![0x08, 0xbb]);
        let rep = r.play(&mut link, |_| {}).unwrap();
        assert_eq!((rep.writes, rep.reads, rep.other_len), (3, 1, 1));
        assert_eq!(link.written.len(), 3);
        assert_eq!(link.written[2].0, 0x06);
    }

    #[test]
    fn armed_breakpoints_are_turned_into_clears() {
        let msg = crate::message::encode(1, "//3///10/", &[0, 0, 0, 0, 0xe0, 0x0a, 0x12, 0x80, 1, 0, 0x80, 0, 0]);
        let frames = crate::frame::message_transfers(&msg, true, 0x14);
        assert_eq!(frames.len(), 1, "a breakpoint message travels inline");
        let mut r = Replay { transfers: vec![Transfer::Write { pipe: 0x08, data: frames[0].1.clone() }] };
        assert_eq!(r.disarm_breakpoints(), 1);
        let Transfer::Write { data, .. } = &r.transfers[0] else { unreachable!() };
        let (_, path, body) = crate::message::decode(&data[8..]).unwrap();
        assert_eq!(path, "//3///10/");
        assert_eq!(body, &[0, 0, 0, 0, 0xe0, 0x0a, 0x12, 0x80, 0, 0, 0x80, 0, 0]);
        // Header (toggle, length) untouched; a second pass finds nothing armed.
        assert_eq!(&data[..8], &frames[0].1[..8]);
        assert_eq!(r.disarm_breakpoints(), 0);
    }

    #[test]
    fn a_missing_step_is_an_error() {
        let p = trace();
        let e = Replay::from_trace(&p, "nope").unwrap_err();
        std::fs::remove_file(&p).ok();
        assert!(matches!(e, Error::Trace(_)));
    }
}
