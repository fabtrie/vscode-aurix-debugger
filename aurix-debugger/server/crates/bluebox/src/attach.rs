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
use crate::{hex, message, Error, Link, Model, Probe, Result};

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
    /// The same, described: service, reply lengths got and captured.
    pub other_desc: Vec<String>,
}

const TC37X: &str = include_str!("attach_tc37x.txt");
const TC37X_SYSTEM: &str = include_str!("attach_tc37x_system.txt");
const TC37X_IC5000: &str = include_str!("attach_tc37x_ic5000.txt");

impl Script {
    /// The reduced winIDEA 9.21.356 attach to a TC377 (see the module docs),
    /// captured with an active probe.
    pub fn tc37x() -> Script {
        Script::parse(TC37X).expect("built-in attach script")
    }

    /// The attach for a BlueBox without active probe (winIDEA probe "System"):
    /// winIDEA 9.21.356's own, exported from `no-ap-after-driver.jsonl` (probe
    /// iC5700 B, its kernel running, 4114 exchanges). It differs from
    /// [`Script::tc37x`] in the probe configuration (`//1///14/`, no component
    /// 0x1211), the node-4 setup, no messages to the active probe (`//7//1//`,
    /// `//7///3/`), and above all in node 5: 11 writes go to device 05 where the
    /// active-probe attach writes to 06, and none to devices 1f and 08, which
    /// only the active-probe attach writes to. An attach patched from
    /// [`Script::tc37x`] without that connected nothing.
    /// It connects after a kernel load too, and does not reset the target.
    pub fn tc37x_system() -> Script {
        Script::parse(TC37X_SYSTEM).expect("built-in attach script")
    }

    /// The attach for an iC5000 (probe "System"): winIDEA 9.21.356's own,
    /// exported from `ic5000.jsonl` (the iC5000, kernel loaded first, 430
    /// exchanges) without its debug FPGA upload (`//1///7/`, `//1///10/`,
    /// `//1///9/`), which [`Probe::connect_target`] sends from winIDEA's files.
    /// Its node 5 is set up with other services than the iC5700's
    /// (`//5///1/`, `//5///2/`, `//5///30/`, `//5///33/`; no `//5///19/` and no
    /// node scan), and sets the DAP clock through node 5 ([`CLOCK_SERVICE_IC5000`]).
    /// It is winIDEA's hot attach (`ic5000-warm.jsonl`, the
    /// target running): winIDEA's attaches that found the target unreachable
    /// reset it, up to 8 times, and are not used. Its 13 pauses are winIDEA's
    /// ([`Exchange::pause_ms`]); without the 10 ms after `//5///1/`, the
    /// probe's next read of its own setup answered differently.
    pub fn tc37x_ic5000() -> Script {
        Script::parse(TC37X_IC5000).expect("built-in attach script")
    }

    /// The built-in attach for `model`. The iC5700's continues with the
    /// setup's own after its node scan ([`Probe::connect_target`]).
    pub fn builtin(model: Model) -> Script {
        match model {
            Model::Ic5700 => Script::tc37x(),
            Model::Ic5000 => Script::tc37x_ic5000(),
        }
    }

    /// One exchange per line: `x <fetch header, toggle 0> <read caps> <reply lengths> [w<ms>] <payload>`,
    /// all hex but the comma-separated numbers. `w<ms>` is winIDEA's pause
    /// before the message ([`Exchange::pause_ms`]). The payload may instead be
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
            let (f, pause_ms) = match f.as_slice() {
                [x, fe, c, l, w, p] if *x == "x" && w.starts_with('w') => {
                    (vec![*x, *fe, *c, *l, *p], w[1..].parse::<u32>().map_err(|_| bad("pause"))?)
                }
                _ => (f, 0),
            };
            if f.len() != 5 || f[0] != "x" {
                return Err(bad("expected: x <fetch> <caps> <lens> [w<ms>] <payload>"));
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
            exchanges.push(Exchange { payload, fetch, reads, expect, pause_ms });
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
        s.push_str("# x <fetch header, toggle 0> <read caps> <captured reply lengths> [w<pause ms>] @<payload n>\n");
        for x in exchanges {
            let id = ids.iter().position(|p| *p == x.payload.as_slice()).unwrap();
            s.push_str(&format!(
                "x {} {} {} {}@{id}\n",
                hexs(&x.fetch.encode(false)),
                join(&mut x.reads.iter().copied()),
                join(&mut x.expect.iter().map(|e| e.len())),
                if x.pause_ms > 0 { format!("w{} ", x.pause_ms) } else { String::new() },
            ));
        }
        s
    }
}

/// Service that sets the DAP clock: body = frequency in Hz, u32 LE. The
/// captured attach sends it three times (and the reset step once), always with
/// 10,000,000: the workspace's `DAPClockkHz`.
pub const CLOCK_SERVICE: &str = "//7//1//";

/// The DAP clock is a 100 MHz base divided by an integer (winIDEA: "Clock
/// divider for 100MHz base clock", `SetDAPClockPrescaler`).
pub const DAP_BASE_HZ: u32 = 100_000_000;
/// Highest divided clock tested good on the TC377 board: 25 MHz. Asking for
/// 30 MHz gave failed bytes, then a corrupted link whose reads came back marked
/// good, then a chip reset.
pub const DAP_MIN_DIVIDER: u32 = 4;
/// Lowest tested clock: 1 MHz.
pub const DAP_MAX_DIVIDER: u32 = 100;

/// The clock the probe can run closest to `hz` without going above it, within
/// the tested range, and its divider. 30 MHz gives 25 MHz (100 / 4).
pub fn dap_clock(hz: u32) -> (u32, u32) {
    let div = DAP_BASE_HZ.div_ceil(hz.max(1)).clamp(DAP_MIN_DIVIDER, DAP_MAX_DIVIDER);
    (DAP_BASE_HZ / div, div)
}

impl Script {
    /// The same script with every clock message set to `hz`.
    pub fn with_clock(&self, hz: u32) -> Script {
        let mut s = self.clone();
        for x in &mut s.exchanges {
            if let Some((kind, CLOCK_SERVICE, body)) = message::decode(&x.payload) {
                if body.len() == 4 {
                    x.payload = message::encode(kind, CLOCK_SERVICE, &hz.to_le_bytes());
                }
            }
        }
        s
    }

    /// The frequencies its clock messages set.
    pub fn clocks(&self) -> Vec<u32> {
        self.exchanges
            .iter()
            .filter_map(|x| match message::decode(&x.payload) {
                Some((_, CLOCK_SERVICE, b)) if b.len() == 4 => Some(u32::from_le_bytes(b.try_into().unwrap())),
                _ => None,
            })
            .collect()
    }
}

/// Service that sets the DAP clock without an active probe: body `01 00`,
/// then the frequency in kHz, u32 LE. winIDEA sends it in both setups
/// (`01 00 10 27 00 00`, 10,000 kHz); only with the active probe does it also
/// send [`CLOCK_SERVICE`]. Answered like that one, status 1 at 0x1e.
pub const CLOCK_SERVICE_SYSTEM: &str = "//4///7/";

/// How the BlueBox reaches the target. winIDEA's probe setting: "Active Probe"
/// or "System" (the BlueBox's own debug connector).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setup {
    /// An active probe sits between BlueBox and target (the AURIX DAP one,
    /// type `0x1211`, `AuAURv1`): the setup [`Script::tc37x`] was captured with.
    ActiveProbe,
    /// The target is connected to the BlueBox directly ([`Script::tc37x_system`]).
    System,
}

impl Setup {
    /// The built-in TC37x attach for this setup.
    pub fn script(self) -> Script {
        match self {
            Setup::ActiveProbe => Script::tc37x(),
            Setup::System => Script::tc37x_system(),
        }
    }

    /// The setup from the reply to the node scan ([`node_scan`]): its node count
    /// at 0x1e is 2 with an active probe (the BlueBox and the probe), 1 without.
    /// winIDEA reads the active probe's descriptor only when it is 2.
    pub fn from_node_scan(r: &[u8]) -> Result<Setup> {
        if r.len() <= 0x1e || !message::is_reply(r) {
            return Err(Error::Protocol(format!("{NODE_SCAN}: {}", hex(r, 40))));
        }
        match r[0x1e] {
            1 => Ok(Setup::System),
            2 => Ok(Setup::ActiveProbe),
            n => Err(Error::Protocol(format!("{NODE_SCAN}: {n} nodes; only 1 (no active probe) or 2 (one) are known"))),
        }
    }
}

/// The node scan winIDEA starts node 5 with: `//5///22/ 00 00 00 00 c8 ...`.
pub const NODE_SCAN: &str = "//5///22/";

/// Where `script` scans the probe's nodes: its first `//5///22/` with body
/// `00 00 00 00 c8 ...`. The exchanges before it are the same in both setups.
pub fn node_scan(script: &Script) -> Option<usize> {
    script.exchanges.iter().position(|x| {
        matches!(message::decode(&x.payload), Some((_, NODE_SCAN, b)) if b.len() == 11 && b[0] == 0 && b[4] == 0xc8)
    })
}

impl<L: Link> Probe<L> {
    /// Sets the debug clock (Hz) through the active probe. Fails if the probe does not confirm it.
    pub fn set_clock(&mut self, hz: u32) -> Result<()> {
        self.clock_message(CLOCK_SERVICE, &hz.to_le_bytes(), hz)
    }

    /// Sets the debug clock (Hz, used in kHz) without an active probe.
    pub fn set_clock_system(&mut self, hz: u32) -> Result<()> {
        let mut body = vec![0x01, 0x00];
        body.extend_from_slice(&(hz / 1000).to_le_bytes());
        self.clock_message(CLOCK_SERVICE_SYSTEM, &body, hz)
    }

    /// Sets the debug clock the way this model and `setup` do it.
    pub fn set_clock_for(&mut self, setup: Setup, hz: u32) -> Result<()> {
        match (self.model(), setup) {
            (Model::Ic5000, _) => self.set_clock_ic5000(hz),
            (Model::Ic5700, Setup::ActiveProbe) => self.set_clock(hz),
            (Model::Ic5700, Setup::System) => self.set_clock_system(hz),
        }
    }

    /// Sets an iC5000's debug clock to the closest it runs at or below `hz`
    /// ([`dap_clock_ic5000`]).
    pub fn set_clock_ic5000(&mut self, hz: u32) -> Result<()> {
        let (_, div) = dap_clock_ic5000(hz);
        let x = Exchange {
            payload: message::encode(message::KIND, CLOCK_SERVICE_IC5000, &[0x8e, 0x00, 0x00, 0xb0, (div - 1) as u8, 0xff]),
            fetch: Fetch::new(0x0e, crate::frame::READ_WINDOW, 0x02),
            reads: vec![crate::frame::REPLY_CAPACITY],
            expect: Vec::new(),
            pause_ms: 0,
        };
        let r = self.exchange(&x)?.remove(0);
        // Captured: a bare reply, `08 00 00 00 09 01 00 ...`, nothing at 0x1e.
        if !message::is_reply(&r) {
            return Err(Error::Protocol(format!("{CLOCK_SERVICE_IC5000}: {}", hex(&r, 32))));
        }
        Ok(())
    }

    fn clock_message(&mut self, path: &str, body: &[u8], hz: u32) -> Result<()> {
        let x = Exchange {
            payload: message::encode(message::KIND, path, body),
            fetch: Fetch::new(0x0e, crate::frame::READ_WINDOW, 0x02),
            reads: vec![crate::frame::REPLY_CAPACITY],
            expect: Vec::new(),
            pause_ms: 0,
        };
        let r = self.exchange(&x)?.remove(0);
        // Confirmed in the captures: `08 00 00 00 0a 01 00 .. 00 01 00`, status 1 at 0x1e.
        if r.len() < 0x1f || !message::is_reply(&r) {
            return Err(Error::Protocol(format!("{path}: {}", hex(&r, 32))));
        }
        if r[0x1e] != 1 {
            return Err(Error::Protocol(format!("{path} {hz} Hz refused: {}", hex(&r[16..], 16))));
        }
        Ok(())
    }

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
                rep.other_desc.push(format!(
                    "{}: {:?} bytes, captured {:?}{}",
                    describe(&x.payload),
                    got.iter().map(|g| g.len()).collect::<Vec<_>>(),
                    x.expect.iter().map(|e| e.len()).collect::<Vec<_>>(),
                    got.first().map(|g| format!(", {}", hex(&g[0x10.min(g.len())..], 20))).unwrap_or_default()
                ));
            }
        }
        Ok(rep)
    }
}

/// Service that sets the iC5000's DAP clock: body `8e 00 00 b0 <n> ff`, the
/// clock being [`IC5000_BASE_HZ`] / (n + 1). Captured with the workspace's
/// `DAPClockkHz` changed (`ic5000-clock5m.jsonl`): 10,000 kHz gives `n` = 2,
/// 5,000 gives 5, 7,000 gives 3 (winIDEA rounds up, to 7.5 MHz), 40,000 gives
/// 0 (30 MHz). Every attach starts with `n` = 0x1d, the workspace's initial
/// 1 MHz (`ClockkHz` 1000). No status in the reply.
pub const CLOCK_SERVICE_IC5000: &str = "//5///2/";
/// The iC5000's DAP clock base.
pub const IC5000_BASE_HZ: u32 = 30_000_000;
/// Largest divider the driver uses: 2 MHz. At 1 MHz (the attach's initial
/// clock) 8 KiB reads from flash came back with failed bytes; from 2 to 30 MHz
/// 256 KiB read back identical (TC377, the iC5000).
pub const IC5000_MAX_DIVIDER: u32 = 15;

/// The clock an iC5000 can run closest to `hz` without going above it, between
/// 2 and 30 MHz, and its divider (`n` + 1). winIDEA rounds up instead.
pub fn dap_clock_ic5000(hz: u32) -> (u32, u32) {
    let div = IC5000_BASE_HZ.div_ceil(hz.max(1)).clamp(1, IC5000_MAX_DIVIDER);
    (IC5000_BASE_HZ / div, div)
}

/// The DAP clock `model` can run closest to `hz`, not above it, and its divider.
pub fn dap_clock_for(model: Model, hz: u32) -> (u32, u32) {
    match model {
        Model::Ic5700 => dap_clock(hz),
        Model::Ic5000 => dap_clock_ic5000(hz),
    }
}

/// Where an iC5000 attach uploads the debug FPGA: right after its
/// `//1///41/`, where winIDEA does ([`crate::fpga::ic5000`]).
pub fn fpga_load_point(script: &Script) -> Option<usize> {
    script
        .exchanges
        .iter()
        .position(|x| matches!(message::decode(&x.payload), Some((_, "//1///41/", _))))
        .map(|i| i + 1)
}

/// Whether `x` is part of an iC5000 FPGA upload (`//1///7/`, `//1///10/`, `//1///9/`).
fn is_fpga_upload(x: &Exchange) -> bool {
    use crate::fpga::ic5000::{BEGIN, CHUNK, END};
    matches!(message::decode(&x.payload), Some((_, p, _)) if p == BEGIN || p == CHUNK || p == END)
}

/// An attach split at the iC5700's debug FPGA status query (`//1///35/`): what
/// comes before it, if it has one, and what comes after.
pub fn split_at_fpga_status(script: &Script) -> (Option<Script>, Script) {
    let at = script
        .exchanges
        .iter()
        .position(|x| message::decode(&x.payload).is_some_and(|(_, path, _)| path == crate::fpga::STATUS));
    match at {
        Some(i) => (
            Some(Script { exchanges: script.exchanges[..i].to_vec() }),
            Script { exchanges: script.exchanges[i + 1..].to_vec() },
        ),
        None => (None, script.clone()),
    }
}

/// What [`Probe::connect_target`] did.
#[derive(Debug, Clone)]
pub struct Connected {
    /// With or without active probe.
    pub setup: Setup,
    /// The whole attach that ran (for a built-in iC5700 attach, the setup's).
    pub script: Script,
    pub sent: usize,
    /// Replies of another length than captured.
    pub other_len: usize,
    /// The first of them, described.
    pub other_desc: Vec<String>,
    /// Things worth a warning.
    pub notes: Vec<String>,
}

impl<L: Link> Probe<L> {
    /// The target connection after the bring-up (kernel and session), as
    /// winIDEA 9.21.356 makes it:
    ///
    /// - **iC5700:** the attach's first messages, then the debug FPGA status,
    ///   which must be 9.21.356's image ([`crate::fpga::WANTED_9_21_356`]);
    ///   then up to the node scan, whose node count tells the setup. With
    ///   `builtin`, the rest is that setup's built-in script.
    /// - **iC5000:** the attach up to its `//1///41/`, then the debug FPGA
    ///   upload of `fpga` ([`crate::fpga::ic5000::image_from`]), then the rest.
    ///   An upload inside `script` (a replayed trace) is left out. The setup is
    ///   [`Setup::System`]: the iC5000 attach scans no nodes.
    pub fn connect_target(&mut self, script: &Script, builtin: bool, fpga: Option<&[u8]>) -> Result<Connected> {
        let mut notes = Vec::new();
        let (sent_c, other_c) = (std::cell::Cell::new(0), std::cell::Cell::new(0));
        let desc = std::cell::RefCell::new(Vec::new());
        let run = |p: &mut Probe<L>, xs: &[Exchange]| -> Result<()> {
            let r = p.attach(&Script { exchanges: xs.to_vec() })?;
            let mut d = desc.borrow_mut();
            for (i, t) in r.other_len.iter().zip(r.other_desc) {
                if d.len() < 16 {
                    d.push(format!("{}: {t}", sent_c.get() + i));
                }
            }
            sent_c.set(sent_c.get() + r.sent);
            other_c.set(other_c.get() + r.other_len.len());
            Ok(())
        };
        match self.model() {
            Model::Ic5000 => {
                let xs: Vec<Exchange> = script.exchanges.iter().filter(|x| !is_fpga_upload(x)).cloned().collect();
                let s = Script { exchanges: xs };
                let at = fpga_load_point(&s).ok_or_else(|| {
                    Error::Fpga("the attach has no //1///41/, after which the iC5000's debug FPGA is loaded".into())
                })?;
                run(self, &s.exchanges[..at])?;
                let image = fpga.ok_or_else(|| Error::Fpga("the iC5000 needs its debug FPGA image on every attach".into()))?;
                self.load_fpga_ic5000(image)?;
                run(self, &s.exchanges[at..])?;
                Ok(Connected { setup: Setup::System, script: s, sent: sent_c.get(), other_len: other_c.get(), other_desc: desc.take(), notes })
            }
            Model::Ic5700 => {
                let (head, rest) = split_at_fpga_status(script);
                match head {
                    Some(head) => {
                        run(self, &head.exchanges)?;
                        let st = self.fpga_status()?;
                        let want = crate::fpga::WANTED_9_21_356;
                        if !st.holds(&want) {
                            return Err(Error::Fpga(format!(
                                "the probe holds another winIDEA version's debug FPGA (size 0x{:x}, MD5 {}; winIDEA \
                                 {v} needs 0x{:x}). Another winIDEA used this BlueBox last. Connect it once with winIDEA \
                                 {v}, which loads its FPGA (about 90 s), then connect again",
                                st.size,
                                st.md5_hex(),
                                want.size,
                                v = crate::kernel::WINIDEA_VERSION,
                            )));
                        }
                    }
                    None => notes.push("the attach asks for no debug FPGA status; the FPGA is not checked".into()),
                }
                // With or without active probe: winIDEA tells by the node count of
                // its node-5 scan, which comes after the same exchanges in both setups.
                let Some(at) = node_scan(&rest) else {
                    notes.push("the attach has no node scan: taken as with active probe".into());
                    run(self, &rest.exchanges)?;
                    return Ok(Connected {
                        setup: Setup::ActiveProbe,
                        script: script.clone(),
                        sent: sent_c.get(),
                        other_len: other_c.get(),
                        other_desc: desc.take(),
                        notes,
                    });
                };
                run(self, &rest.exchanges[..at])?;
                let reply = self.exchange(&rest.exchanges[at])?.remove(0);
                sent_c.set(sent_c.get() + 1);
                let setup = Setup::from_node_scan(&reply)?;
                let (full, after) = if builtin {
                    let full = setup.script();
                    let (_, r) = split_at_fpga_status(&full);
                    let at = node_scan(&r).expect("the built-in attaches scan the nodes");
                    let after = r.exchanges[at + 1..].to_vec();
                    (full, after)
                } else {
                    (script.clone(), rest.exchanges[at + 1..].to_vec())
                };
                run(self, &after)?;
                Ok(Connected { setup, script: full, sent: sent_c.get(), other_len: other_c.get(), other_desc: desc.take(), notes })
            }
        }
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
    fn the_clock_messages_can_be_set() {
        let s = Script::tc37x();
        assert_eq!(s.clocks(), vec![10_000_000; 3], "the captured attach runs at 10 MHz");
        let fast = s.with_clock(30_000_000);
        assert_eq!(fast.clocks(), vec![30_000_000; 3]);
        assert_eq!(fast.exchanges.len(), s.exchanges.len());
        let changed = s.exchanges.iter().zip(&fast.exchanges).filter(|(a, b)| a != b).count();
        assert_eq!(changed, 3, "only the clock messages change");
    }

    #[test]
    fn dap_clocks_round_down_to_a_divider_of_100_mhz() {
        assert_eq!(dap_clock(10_000_000), (10_000_000, 10));
        assert_eq!(dap_clock(30_000_000), (25_000_000, 4), "the cfg's 30 MHz");
        assert_eq!(dap_clock(20_000_000), (20_000_000, 5));
        assert_eq!(dap_clock(16_000_000), (14_285_714, 7));
        assert_eq!(dap_clock(50_000_000), (25_000_000, 4), "capped at the tested 25 MHz");
        assert_eq!(dap_clock(100_000), (1_000_000, 100), "not below the tested 1 MHz");
    }

    #[test]
    fn set_clock_checks_the_status() {
        let ok = "0f000000555555555555555555555555080000000a0100000000000000000100";
        let unhex = |s: &str| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>();
        let mut link = ScriptedLink::default();
        link.replies.push_back(unhex(ok));
        let mut refused = unhex(ok);
        refused[0x1e] = 0;
        link.replies.push_back(refused);
        let mut p = Probe::new(link, false);
        p.set_clock(20_000_000).unwrap();
        assert!(p.set_clock(99_000_000).is_err());
        let w = &p.link_mut().written[0].1;
        let m = message::encode(1, "//7//1//", &20_000_000u32.to_le_bytes());
        assert_eq!(&w[8..8 + m.len()], &m[..]);
    }

    #[test]
    fn the_clock_without_active_probe_goes_in_khz_to_node_4() {
        // The reply as iC5700 B sent it (no-active-probe.jsonl), stale filler and all.
        let ok = "0f000000eaebda77a0b3fcfff27f16bf080000000a0100000000000000000100";
        let unhex = |s: &str| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>();
        let mut link = ScriptedLink::default();
        link.replies.push_back(unhex(ok));
        let mut p = Probe::new(link, false);
        p.set_clock_for(Setup::System, 10_000_000).unwrap();
        let w = &p.link_mut().written[0].1;
        // As winIDEA sends it: `01 00 10 27 00 00`.
        let m = message::encode(1, CLOCK_SERVICE_SYSTEM, &[0x01, 0x00, 0x10, 0x27, 0x00, 0x00]);
        assert_eq!(&w[8..8 + m.len()], &m[..]);
    }

    #[test]
    fn the_attach_without_active_probe_is_winideas_for_that_setup() {
        let ap = Script::tc37x();
        let sys = Script::tc37x_system();
        // The driver sends the exchanges up to the node scan before it knows the
        // setup, so they must be the same in both.
        let at = node_scan(&ap).unwrap();
        assert_eq!(node_scan(&sys), Some(at));
        let payloads = |s: &Script| s.exchanges[..=at].iter().map(|x| x.payload.clone()).collect::<Vec<_>>();
        assert_eq!(payloads(&ap), payloads(&sys));
        let mut own_device = 0;
        for x in &sys.exchanges {
            let Some((_, p, b)) = message::decode(&x.payload) else { continue };
            assert!(!["//7//1//", "//7///3/", "//4/4//2/", "//4/4///", "//4/5///"].contains(&p), "an active probe message: {p}");
            match p {
                "//1///14/" => assert_eq!((b.len(), &b[..12]), (98, &[4, 0, 0, 0, 9, 0, 0, 0, 1, 0, 0x70, 0][..])),
                "//4///10/" => assert_eq!(&b[..8], &[0, 5, 4, 3, 0, 0, 0x70, 0]),
                "//4///3/" => assert_eq!(b, [1, 5]),
                // Node-5 writes: 11 to device 05 where the active-probe attach has 06,
                // none to 1f or 08, which only the active-probe attach writes to.
                "//5///19/" if b.len() == 40 && b[0] == 2 && b[25] == 0xfb => {
                    assert!(![0x06, 0x08, 0x1f].contains(&b[24]), "a write to an active-probe device: {:02x}", b[24]);
                    own_device += (b[24] == 0x05) as usize;
                }
                _ => {}
            }
        }
        assert_eq!(own_device, 11, "writes to device 05");
    }

    #[test]
    fn the_node_scan_tells_the_setup() {
        // Replies to `//5///22/ 00 00 00 00 c8 ...`: iC5700 A with its active probe
        // (probe-refresh.jsonl), iC5700 B without (no-active-probe.jsonl).
        let unhex = |s: &str| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>();
        let with = unhex("150000005555555555555555555555550800000010010000000000000000020000000000000000");
        let without = unhex("15000000eaebda77a0b3fcfff27f16bf0800000010010000000000000000010000000000000000");
        assert_eq!(Setup::from_node_scan(&with).unwrap(), Setup::ActiveProbe);
        assert_eq!(Setup::from_node_scan(&without).unwrap(), Setup::System);
        let mut three = with.clone();
        three[0x1e] = 3;
        assert!(Setup::from_node_scan(&three).is_err());
        assert!(Setup::from_node_scan(&with[..20]).is_err());
    }

    #[test]
    fn the_ic5000_attach_is_winideas_hot_attach_without_the_fpga_upload() {
        let s = Script::tc37x_ic5000();
        assert_eq!(s.exchanges.len(), 272);
        let paths: Vec<&str> = s.exchanges.iter().filter_map(|x| message::decode(&x.payload).map(|(_, p, _)| p)).collect();
        assert_eq!(paths.first(), Some(&"/////6/"), "starts after the kernel check");
        for p in ["//1///7/", "//1///10/", "//1///9/", "//7///7/", "//5///22/", "//5///19/", "//1///35/"] {
            assert!(!paths.contains(&p), "{p}: an FPGA upload, a reset or an iC5700 service");
        }
        // The upload goes after its //1///41/.
        let at = fpga_load_point(&s).unwrap();
        assert_eq!(message::decode(&s.exchanges[at - 1].payload).map(|(_, p, _)| p), Some("//1///41/"));
        // winIDEA's 10 ms after //5///1/ (node 5's setup), before it reads that setup back.
        let i = paths.iter().position(|p| *p == "//5///1/").unwrap();
        let next = s.exchanges.iter().filter(|x| message::decode(&x.payload).is_some()).nth(i + 1).unwrap();
        assert!(next.pause_ms >= 5, "pause before the read-back: {} ms", next.pause_ms);
        assert!(s.exchanges.iter().all(|x| !x.reads.is_empty()));
        assert_eq!(Script::builtin(Model::Ic5000), s);
        assert_eq!(Script::builtin(Model::Ic5700), Script::tc37x());
    }

    #[test]
    fn an_ic5000_without_its_fpga_image_stops_before_the_upload() {
        // Two exchanges up to //1///41/, then the upload would come.
        let x = |path: &str| Exchange {
            payload: message::encode(1, path, &[0]),
            fetch: Fetch::new(0x0e, 0xf000, 2),
            reads: vec![62480],
            expect: vec![vec![0; 30]],
            pause_ms: 0,
        };
        let s = Script { exchanges: vec![x("//1///13/"), x("//1///41/"), x("//1///10/"), x("//1///44/")] };
        let mut link = ScriptedLink { model: Model::Ic5000, ..Default::default() };
        link.replies.extend([vec![0; 30], vec![0; 30]]);
        let mut p = Probe::new(link, false);
        let e = p.connect_target(&s, true, None).unwrap_err();
        assert!(matches!(e, Error::Fpga(_)), "{e}");
        // Only the two before the upload went out (a message and its fetch each).
        assert_eq!(p.link_mut().written.len(), 4);
    }

    #[test]
    fn the_ic5000_clock_goes_to_node_5_as_a_divider_of_30_mhz() {
        assert_eq!(dap_clock_ic5000(10_000_000), (10_000_000, 3));
        assert_eq!(dap_clock_ic5000(5_000_000), (5_000_000, 6));
        assert_eq!(dap_clock_ic5000(7_000_000), (6_000_000, 5), "rounded down; winIDEA goes up to 7.5 MHz");
        assert_eq!(dap_clock_ic5000(40_000_000), (30_000_000, 1));
        assert_eq!(dap_clock_ic5000(1_000_000), (2_000_000, 15), "not below 2 MHz: 1 MHz lost bytes");
        assert_eq!(dap_clock_for(Model::Ic5700, 30_000_000), (25_000_000, 4));
        let mut link = ScriptedLink { model: Model::Ic5000, ..Default::default() };
        // The reply as captured.
        let mut r = vec![0u8; 30];
        r[0] = 0x0e;
        r[0x10] = 0x08;
        r[0x14] = 0x09;
        r[0x15] = 0x01;
        link.replies.push_back(r);
        let mut p = Probe::new(link, false);
        p.set_clock_for(Setup::System, 5_000_000).unwrap();
        // As winIDEA sends it for 5,000 kHz.
        let m = message::encode(1, CLOCK_SERVICE_IC5000, &[0x8e, 0x00, 0x00, 0xb0, 0x05, 0xff]);
        let w = &p.link_mut().written[0].1;
        assert_eq!(&w[8..8 + m.len()], &m[..]);
    }

    #[test]
    fn pauses_survive_render_and_parse() {
        let x = Exchange {
            payload: crate::message::encode(1, "//5///3/", &[3, 0, 1, 0]),
            fetch: Fetch::new(0x0e, 0xf000, 2),
            reads: vec![62480],
            expect: vec![vec![0; 32]],
            pause_ms: 10,
        };
        let text = Script::render(&[&x], "pause");
        assert!(text.contains(" w10 @0"), "{text}");
        assert_eq!(Script::parse(&text).unwrap().exchanges, vec![x]);
        assert!(Script::parse("x 0ef0000000000002 62480 32 wx @0").is_err());
    }

    #[test]
    fn render_and_parse_round_trip() {
        let x = Exchange {
            payload: crate::message::encode(1, "//1///12/", &[0]),
            fetch: Fetch::new(0x0e, 0xf000, 2),
            reads: vec![62480],
            expect: vec![vec![0; 32]],
            pause_ms: 0,
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
            pause_ms: 0,
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
