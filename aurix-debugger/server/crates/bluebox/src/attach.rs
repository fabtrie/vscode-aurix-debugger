//! The target connection: what follows the link bring-up.
//!
//! Its messages are not decoded one by one. [`Script::tc37x`] holds what
//! winIDEA 9.21.356 sent after the kernel when attaching to a TC377 right
//! after a power cycle of the probe (`powercycle.jsonl`): 3582 exchanges,
//! from `/////6/` to its last reply inside the attach step. Sent with this
//! session's toggles, it connected both after a power cycle and with a
//! running kernel, and the target was **not** reset (its uptime kept
//! counting). Played back byte for byte instead, an attach reset the chip
//! every time.
//!
//! What was learned while reducing it:
//!
//! - On a probe that earlier sessions had set up, 53 messages were enough:
//!   no node-5 polls or writes, no 917 KB test-pattern reads (`//1///42/`), no
//!   repeated checks, and nothing after the first `A` command.
//! - After a power cycle that reduced script failed and left the probe hung.
//!   The power-cycle attach adds node-5 setup (`//5///22/` with body `02`), and
//!   winIDEA's work after `A` (`//7///8/`, `//2///38/`, per-core `//3/.../6/`)
//!   is needed to finish the connection. Reducing further needs one power
//!   cycle per try.
//!
//! The tables (`//2///30/`, 7337 bytes; `//3/<core>/9/`, 5663 bytes each;
//! `/////6/`, 4107 bytes) describe the SoC and the debug setup of the winIDEA
//! workspace, so this script fits a TC37x with those settings. Another
//! derivative needs its own capture (`examples/probe.rs --script <trace>
//! --export`).

use crate::probe::{Exchange, Fetch};
use crate::{hex, Error, Link, Probe, Result};

/// A list of exchanges to send after the link bring-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub exchanges: Vec<Exchange>,
}

/// What [`Probe::attach`] saw.
#[derive(Debug, Clone, Default)]
pub struct AttachReport {
    pub sent: usize,
    /// Exchanges whose replies differed in length from the captured ones.
    pub other_len: Vec<usize>,
}

const TC37X: &str = include_str!("attach_tc37x.txt");

impl Script {
    /// The reduced winIDEA 9.21.356 attach to a TC377 (see the module docs).
    pub fn tc37x() -> Script {
        Script::parse(TC37X).expect("built-in attach script")
    }

    /// One exchange per line: `x <fetch header, toggle 0> <read caps> <reply lengths> <payload>`,
    /// all hex but the comma-separated numbers. The payload may instead be
    /// `@<n>`, referring to an earlier line `p <n> <payload>`. `#` starts a comment line.
    pub fn parse(text: &str) -> Result<Script> {
        let mut exchanges = Vec::new();
        let mut payloads: std::collections::HashMap<usize, Vec<u8>> = std::collections::HashMap::new();
        for (no, line) in text.lines().enumerate() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let bad = |why: &str| Error::Trace(format!("attach script line {}: {why}", no + 1));
            let f: Vec<&str> = t.split_whitespace().collect();
            if f.len() == 3 && f[0] == "p" {
                let id = f[1].parse().map_err(|_| bad("payload id"))?;
                payloads.insert(id, unhex(f[2]).ok_or_else(|| bad("payload"))?);
                continue;
            }
            if f.len() != 5 || f[0] != "x" {
                return Err(bad("expected: x <fetch> <caps> <lens> <payload>"));
            }
            let fetch = Fetch::from_header(&unhex(f[1]).ok_or_else(|| bad("fetch"))?).ok_or_else(|| bad("fetch"))?;
            let nums = |s: &str| s.split(',').map(|n| n.parse::<usize>()).collect::<std::result::Result<Vec<_>, _>>();
            let reads = nums(f[2]).map_err(|_| bad("read caps"))?;
            let lens = nums(f[3]).map_err(|_| bad("reply lengths"))?;
            let payload = match f[4].strip_prefix('@') {
                Some(id) => payloads
                    .get(&id.parse().map_err(|_| bad("payload id"))?)
                    .cloned()
                    .ok_or_else(|| bad("unknown payload id"))?,
                None => unhex(f[4]).ok_or_else(|| bad("payload"))?,
            };
            if reads.len() != lens.len() {
                return Err(bad("as many reply lengths as reads"));
            }
            // Only the lengths are kept; contents are compared by length.
            let expect = lens.into_iter().map(|n| vec![0u8; n]).collect();
            exchanges.push(Exchange { payload, fetch, reads, expect });
        }
        Ok(Script { exchanges })
    }

    /// The text form of `exchanges`, with `note` as a header comment. Each
    /// distinct payload is written once (`p`) and referred to (`@n`).
    pub fn render(exchanges: &[&Exchange], note: &str) -> String {
        let hexs = |b: &[u8]| b.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let join = |v: &mut dyn Iterator<Item = usize>| v.map(|n| n.to_string()).collect::<Vec<_>>().join(",");
        let mut s = String::new();
        for l in note.lines() {
            s.push_str(&format!("# {l}\n"));
        }
        let mut ids: Vec<&[u8]> = Vec::new();
        for x in exchanges {
            if !ids.contains(&x.payload.as_slice()) {
                ids.push(&x.payload);
            }
        }
        s.push_str("# p <n> <payload>: each distinct payload once\n");
        for (i, p) in ids.iter().enumerate() {
            s.push_str(&format!("p {i} {}\n", hexs(p)));
        }
        s.push_str("# x <fetch header, toggle 0> <read caps> <captured reply lengths> @<payload n>\n");
        for x in exchanges {
            let id = ids.iter().position(|p| *p == x.payload.as_slice()).unwrap();
            s.push_str(&format!(
                "x {} {} {} @{id}\n",
                hexs(&x.fetch.encode(false)),
                join(&mut x.reads.iter().copied()),
                join(&mut x.expect.iter().map(|e| e.len())),
            ));
        }
        s
    }
}

impl<L: Link> Probe<L> {
    /// Sends `script` right after [`Probe::handshake`] (and a kernel load, if
    /// one was needed), in the same session.
    pub fn attach(&mut self, script: &Script) -> Result<AttachReport> {
        let mut rep = AttachReport::default();
        for (i, x) in script.exchanges.iter().enumerate() {
            let got = self.exchange(x).map_err(|e| match e {
                Error::Usb { op, code } => Error::Protocol(format!(
                    "attach exchange {i} ({}): {op} failed, Win32 error {code}",
                    describe(&x.payload)
                )),
                other => other,
            })?;
            rep.sent += 1;
            if got.iter().map(|g| g.len()).ne(x.expect.iter().map(|e| e.len())) {
                rep.other_len.push(i);
            }
        }
        Ok(rep)
    }
}

fn describe(p: &[u8]) -> String {
    match crate::message::decode(p) {
        Some((_, path, body)) => format!("{path}, {} bytes", body.len()),
        None => hex(p, 8),
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

    #[test]
    fn the_built_in_script_parses_and_has_the_reduced_attach() {
        let s = Script::tc37x();
        assert_eq!(s.exchanges.len(), 3582);
        let paths: Vec<String> = s
            .exchanges
            .iter()
            .filter_map(|x| crate::message::decode(&x.payload).map(|(_, p, _)| p.to_string()))
            .collect();
        assert_eq!(paths.first().map(String::as_str), Some("/////6/"), "starts after the session message");
        assert!(paths.iter().any(|p| p == "//2///30/"), "the SoC table");
        assert!(paths.iter().any(|p| p == "//5///22/"), "node 5 is set up after a power cycle");
        assert!(s.exchanges.iter().all(|x| !x.reads.is_empty()), "every exchange has its reply");
        // Breakpoints only cleared (byte 8 of the //3///10/ body), never armed.
        for x in &s.exchanges {
            if let Some((_, "//3///10/", body)) = crate::message::decode(&x.payload) {
                assert_eq!(body[8], 0, "an armed breakpoint in the attach");
            }
        }
    }

    #[test]
    fn render_and_parse_round_trip() {
        let x = Exchange {
            payload: crate::message::encode(1, "//1///12/", &[0]),
            fetch: Fetch::new(0x0e, 0xf000, 2),
            reads: vec![62480],
            expect: vec![vec![0; 32]],
        };
        let text = Script::render(&[&x, &x], "a note\nsecond line");
        assert!(text.starts_with("# a note\n# second line\n"));
        assert_eq!(text.matches("\np ").count(), 1, "the payload once");
        assert_eq!(Script::parse(&text).unwrap().exchanges, vec![x.clone(), x]);
    }

    #[test]
    fn attach_sends_each_exchange_with_live_toggles() {
        let x = Exchange {
            payload: crate::message::encode(1, "//1///12/", &[0]),
            fetch: Fetch::new(0x0e, 0xf000, 2),
            reads: vec![62480],
            expect: vec![vec![0; 4]],
        };
        let s = Script { exchanges: vec![x.clone(), x] };
        let mut link = ScriptedLink::default();
        link.replies.extend([vec![1; 4], vec![1; 5]]);
        let mut p = Probe::new(link, true);
        let rep = p.attach(&s).unwrap();
        assert_eq!((rep.sent, rep.other_len.clone()), (2, vec![1]));
        let w = &p.link_mut().written;
        // message (toggle 1), fetch (0), message (0), fetch (1)
        assert_eq!(w[0].1[0], 0xea);
        assert_eq!(w[1].1[0], 0x0e);
        assert_eq!(w[2].1[0], 0x6a);
        assert_eq!(w[3].1[0], 0x8e);
    }
}
