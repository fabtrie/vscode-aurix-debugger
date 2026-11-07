//! The target connection: what follows the link bring-up.
//!
//! Its messages are not decoded one by one: an attach is a script of what
//! winIDEA 9.21.417 sent after the kernel, sent again with this session's
//! toggles. Played back byte for byte instead, an attach reset the chip every
//! time. Built in: [`Script::tc37x_ic5000`], and for an iC5700
//! [`Script::tc37x`] (with active probe) and [`Script::tc37x_system`]
//! (without). Another target or setup needs a capture (`bluebox attach_replay`,
//! or a script exported with `examples/probe.rs --script <trace> --export`).
//!
//! What was learned from 9.21.356's iC5700 attaches:
//!
//! - On a probe that earlier sessions had set up, 53 messages were enough.
//!   After a power cycle that reduced script failed and left the probe hung:
//!   the node-5 setup and winIDEA's work after the connect were needed.
//! - Whether an active probe sits between BlueBox and target is told by the
//!   node count of the node-5 scan ([`Setup::from_node_scan`]); with and
//!   without one, the attaches differ in node 4, node 5 and the clock service.
//!
//! - winIDEA's attach writes `STMx_OCS` (its "suspend the STMs with the
//!   cores"). On an application that has locked STM0/1 against the debugger
//!   (`STMx_ACCEN0`), the bus refuses those writes and the SMU raises ALM7[20]
//!   (measured on a TC37x board). An application that
//!   started without a debugger keeps that alarm's reaction and the FS26's FCCU
//!   monitoring, so the attach reset the board. [`Probe::attach`] leaves these
//!   writes out ([`is_stm_ocs_write`]); the cfg scripts write `STMx_OCS` where
//!   `STMx_ACCEN0` lets them.
//!
//! The tables (`//2///26/`, 7378 bytes; `//3/<core>/6/`, 5699 bytes each;
//! `/////6/`, 4107 bytes) describe the SoC and the debug setup of the winIDEA
//! workspace, so a script fits a TC37x with those settings. Another
//! derivative needs its own capture.

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
    /// Exchanges left out: writes to `STMx_OCS` ([`is_stm_ocs_write`]).
    pub skipped: usize,
    /// Exchanges whose replies differed in length from the captured ones.
    pub other_len: Vec<usize>,
    /// The same, described: service, reply lengths got and captured.
    pub other_desc: Vec<String>,
}

const TC37X: &str = include_str!("attach_tc37x.txt");
const TC37X_SYSTEM: &str = include_str!("attach_tc37x_system.txt");
const TC37X_IC5000: &str = include_str!("attach_tc37x_ic5000.txt");

impl Script {
    /// The attach for an iC5000 (probe "System"): winIDEA 9.21.417's own,
    /// exported from `ic5000-417-warm.jsonl` (the iC5000, its kernel and the
    /// target running, 261 exchanges) without its kernel check and its debug
    /// FPGA upload (`//1///7/`, `//1///10/`, `//1///9/`), which
    /// [`Probe::connect_target`] sends from winIDEA's files.
    /// Its node 5 is set up with other services than the iC5700's
    /// (`//5///1/`, `//5///2/`, `//5///30/`, `//5///33/`; no `//5///19/` and no
    /// node scan), and sets the DAP clock through node 5 ([`CLOCK_SERVICE_IC5000`]).
    /// It is winIDEA's hot attach: winIDEA's attaches that found the target
    /// unreachable reset it, up to 8 times (9.21.356), and are not used. Its pauses are winIDEA's
    /// ([`Exchange::pause_ms`]); without the 10 ms after `//5///1/`, the
    /// probe's next read of its own setup answered differently.
    pub fn tc37x_ic5000() -> Script {
        Script::parse(TC37X_IC5000).expect("built-in attach script")
    }

    /// The attach for an iC5700 with an active probe (the AURIX DAP one, probe
    /// "Active Probe"): winIDEA 9.21.417's hot attach, exported from
    /// `ap-417-warm.jsonl` (iC5700 B with the active probe, kernel, FPGA image
    /// and target running; 3705 exchanges) without the kernel check. Up to and
    /// with its node scan it is the same as [`Script::tc37x_system`]; then it
    /// configures the probe and sets the clock with [`CLOCK_SERVICE`] (Hz) next
    /// to [`CLOCK_SERVICE_SYSTEM`], 10 MHz each, three times.
    pub fn tc37x() -> Script {
        Script::parse(TC37X).expect("built-in attach script")
    }

    /// The attach for an iC5700 without active probe (probe "System"): winIDEA
    /// 9.21.417's hot attach, exported from `no-ap-417-warm.jsonl` (iC5700 B,
    /// its kernel, its FPGA image and the target running; 3532 exchanges)
    /// without the kernel check. It asks for the FPGA status (`//1///37/`) after
    /// its first three messages, scans the nodes (`//5///22/`), sets the clock
    /// with [`CLOCK_SERVICE_SYSTEM`] (10,000 kHz) and does not reset the target.
    pub fn tc37x_system() -> Script {
        Script::parse(TC37X_SYSTEM).expect("built-in attach script")
    }

    /// The built-in attach for `model`. For the iC5700 the one without active
    /// probe; [`Probe::connect_target`] continues with [`Script::tc37x`] after the
    /// node scan if it finds one.
    pub fn builtin(model: Model) -> Option<Script> {
        match model {
            Model::Ic5700 => Some(Script::tc37x_system()),
            Model::Ic5000 => Some(Script::tc37x_ic5000()),
        }
    }

    /// The setup the script was captured with: an active probe's attach sets
    /// the clock with [`CLOCK_SERVICE`], one without never does.
    pub fn captured_setup(&self) -> Setup {
        let ap = self.exchanges.iter().any(|x| matches!(message::decode(&x.payload), Some((_, CLOCK_SERVICE, _))));
        if ap { Setup::ActiveProbe } else { Setup::System }
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

/// Service that sets the iC5700's DAP clock with an active probe: body =
/// frequency in Hz, u32 LE. The attach sends it three times, always with
/// 10,000,000: the workspace's `DAPClockkHz`. The same in 9.21.356 and
/// 9.21.417 (`ap-417-warm.jsonl`).
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
/// send [`CLOCK_SERVICE`]. Answered like that one, status 1 at 0x1e. The same
/// in 9.21.356 and 9.21.417 (`no-ap-417-warm.jsonl`).
pub const CLOCK_SERVICE_SYSTEM: &str = "//4///7/";

/// How the BlueBox reaches the target. winIDEA's probe setting: "Active Probe"
/// or "System" (the BlueBox's own debug connector).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setup {
    /// An active probe sits between BlueBox and target (the AURIX DAP one,
    /// type `0x1211`, `AuAURv1`).
    ActiveProbe,
    /// The target is connected to the BlueBox directly.
    System,
}

impl Setup {
    /// The built-in iC5700 attach for this setup.
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
            if is_stm_ocs_write(x) {
                rep.skipped += 1;
                continue;
            }
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

/// `STM0_OCS` .. `STM5_OCS`: written by winIDEA's attach, refused (and an SMU
/// alarm) once the application has locked the STM against the debugger.
const STM_OCS: [u64; 6] = [0xF000_10E8, 0xF000_11E8, 0xF000_12E8, 0xF000_13E8, 0xF000_14E8, 0xF000_15E8];

/// Whether `x` is a memory-service write ([`crate::memory`]: `//2///14/`, or
/// 9.21.356's `//2///16/`; direction at body offset 0x68, address u64 at 0x0a)
/// to an `STMx_OCS`.
pub fn is_stm_ocs_write(x: &Exchange) -> bool {
    let Some((_, path, body)) = message::decode(&x.payload) else { return false };
    if (path != "//2///14/" && path != "//2///16/") || body.len() < 0x7f || body[0x68] != 1 {
        return false;
    }
    let addr = u64::from_le_bytes(body[0x0a..0x12].try_into().unwrap());
    STM_OCS.contains(&addr)
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

/// The node-1 service after which winIDEA uploads the iC5000's debug FPGA
/// (9.21.417; 9.21.356: `//1///41/`).
pub const FPGA_LOAD_AFTER: &str = "//1///43/";

/// Where an iC5000 attach uploads the debug FPGA: right after its
/// [`FPGA_LOAD_AFTER`], where winIDEA does ([`crate::fpga::ic5000`]).
pub fn fpga_load_point(script: &Script) -> Option<usize> {
    script
        .exchanges
        .iter()
        .position(|x| matches!(message::decode(&x.payload), Some((_, p, _)) if p == FPGA_LOAD_AFTER))
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
    /// The whole attach that ran (an iC5000's without its FPGA upload).
    pub script: Script,
    pub sent: usize,
    /// Replies of another length than captured.
    pub other_len: usize,
    /// The first of them, described.
    pub other_desc: Vec<String>,
    /// Things worth a warning.
    pub notes: Vec<String>,
    /// `STMx_OCS` writes of the script left out ([`is_stm_ocs_write`]).
    pub skipped: usize,
}

impl<L: Link> Probe<L> {
    /// The target connection after the bring-up (kernel and session), as
    /// winIDEA 9.21.417 makes it:
    ///
    /// - **iC5700:** the attach's first messages, then the debug FPGA status,
    ///   which must be 9.21.417's image ([`crate::fpga::WANTED_9_21_417`]);
    ///   then the rest, whose node scan tells the setup. A script captured in
    ///   the other setup ([`Script::captured_setup`]) continues with that
    ///   setup's built-in script ([`Setup::script`]) if the two are the same up
    ///   to the scan, as the built-in ones are; otherwise it stops there, since
    ///   its node-5 writes would go to the other setup's devices.
    /// - **iC5000:** the attach up to its [`FPGA_LOAD_AFTER`], then the debug FPGA
    ///   upload of `fpga` ([`crate::fpga::ic5000::image_from`]), then the rest.
    ///   An upload inside `script` (a replayed trace) is left out. The setup is
    ///   [`Setup::System`]: the iC5000 attach scans no nodes.
    pub fn connect_target(&mut self, script: &Script, fpga: Option<&[u8]>) -> Result<Connected> {
        let mut notes = Vec::new();
        let (sent_c, other_c) = (std::cell::Cell::new(0), std::cell::Cell::new(0));
        let skipped_c = std::cell::Cell::new(0);
        let desc = std::cell::RefCell::new(Vec::new());
        let run = |p: &mut Probe<L>, xs: &[Exchange]| -> Result<()> {
            let r = p.attach(&Script { exchanges: xs.to_vec() })?;
            skipped_c.set(skipped_c.get() + r.skipped);
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
                    Error::Fpga(format!("the attach has no {FPGA_LOAD_AFTER}, after which the iC5000's debug FPGA is loaded"))
                })?;
                run(self, &s.exchanges[..at])?;
                let image = fpga.ok_or_else(|| Error::Fpga("the iC5000 needs its debug FPGA image on every attach".into()))?;
                self.load_fpga_ic5000(image)?;
                run(self, &s.exchanges[at..])?;
                Ok(Connected { setup: Setup::System, script: s, sent: sent_c.get(), other_len: other_c.get(), other_desc: desc.take(), notes, skipped: skipped_c.get() })
            }
            Model::Ic5700 => {
                let (head, rest) = split_at_fpga_status(script);
                match head {
                    Some(head) => {
                        run(self, &head.exchanges)?;
                        let st = self.fpga_status()?;
                        let want = crate::fpga::WANTED_9_21_417;
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
                        skipped: skipped_c.get(),
                    });
                };
                run(self, &rest.exchanges[..at])?;
                let reply = self.exchange(&rest.exchanges[at])?.remove(0);
                sent_c.set(sent_c.get() + 1);
                let setup = Setup::from_node_scan(&reply)?;
                let mut full = script.clone();
                let mut after = rest.exchanges[at + 1..].to_vec();
                if setup != script.captured_setup() {
                    // The setup's built-in attach, if this one is the same up to the scan.
                    let other = setup.script();
                    let (_, other_rest) = split_at_fpga_status(&other);
                    let same_head = node_scan(&other_rest).is_some_and(|o| {
                        let head = |s: &Script, n: usize| s.exchanges[..n].iter().map(|x| x.payload.clone()).collect::<Vec<_>>();
                        o == at && head(&other_rest, o + 1) == head(&rest, at + 1)
                    });
                    if !same_head {
                        let with = |s: Setup| if s == Setup::ActiveProbe { "with" } else { "without" };
                        return Err(Error::Protocol(format!(
                            "the BlueBox is connected {} active probe, but the attach was captured {} one; \
                             give one captured in this setup ('bluebox attach_replay <trace.jsonl>')",
                            with(setup),
                            with(script.captured_setup()),
                        )));
                    }
                    after = other_rest.exchanges[at + 1..].to_vec();
                    full = other;
                }
                run(self, &after)?;
                Ok(Connected { setup, script: full, sent: sent_c.get(), other_len: other_c.get(), other_desc: desc.take(), notes, skipped: skipped_c.get() })
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
    fn the_clock_messages_can_be_set() {
        let x = |path: &str, body: &[u8]| Exchange {
            payload: message::encode(1, path, body),
            fetch: Fetch::new(0x0e, 0xf000, 2),
            reads: vec![62480],
            expect: vec![vec![0; 32]],
            pause_ms: 0,
        };
        let ten = 10_000_000u32.to_le_bytes();
        let s = Script { exchanges: vec![x(CLOCK_SERVICE, &ten), x("//1///12/", &[0]), x(CLOCK_SERVICE, &ten)] };
        assert_eq!(s.clocks(), vec![10_000_000; 2]);
        let fast = s.with_clock(30_000_000);
        assert_eq!(fast.clocks(), vec![30_000_000; 2]);
        assert_eq!(fast.exchanges[1], s.exchanges[1], "only the clock messages change");
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
        assert_eq!(s.exchanges.len(), 261);
        let paths: Vec<&str> = s.exchanges.iter().filter_map(|x| message::decode(&x.payload).map(|(_, p, _)| p)).collect();
        assert_eq!(paths.first(), Some(&"/////6/"), "starts after the kernel check");
        for p in ["//1///7/", "//1///10/", "//1///9/", "//7///7/", "//5///22/", "//5///19/", "//1///37/"] {
            assert!(!paths.contains(&p), "{p}: an FPGA upload, a reset or an iC5700 service");
        }
        // The upload goes after its //1///43/.
        let at = fpga_load_point(&s).unwrap();
        assert_eq!(message::decode(&s.exchanges[at - 1].payload).map(|(_, p, _)| p), Some(FPGA_LOAD_AFTER));
        // 9.21.417's numbering: the memory service and the SoC table.
        assert!(paths.contains(&"//2///14/") && paths.contains(&"//2///26/") && !paths.contains(&"//2///16/"));
        // winIDEA's 10 ms after //5///1/ (node 5's setup), before it reads that setup back.
        let i = paths.iter().position(|p| *p == "//5///1/").unwrap();
        let next = s.exchanges.iter().filter(|x| message::decode(&x.payload).is_some()).nth(i + 1).unwrap();
        assert!(next.pause_ms >= 5, "pause before the read-back: {} ms", next.pause_ms);
        assert!(s.exchanges.iter().all(|x| !x.reads.is_empty()));
        // winIDEA's leftover breakpoint (0x80120ae0) only cleared, never armed (byte 8).
        let bps: Vec<&[u8]> = s
            .exchanges
            .iter()
            .filter_map(|x| match message::decode(&x.payload) {
                Some((_, crate::replay::BREAKPOINT_SERVICE, b)) => Some(b),
                _ => None,
            })
            .collect();
        assert!(!bps.is_empty() && bps.iter().all(|b| b.len() == 13 && b[8] == 0), "an armed breakpoint in the attach");
        assert_eq!(Script::builtin(Model::Ic5000), Some(s));
    }

    #[test]
    fn the_ic5700_attach_without_active_probe_is_winideas_hot_attach() {
        let s = Script::tc37x_system();
        assert_eq!(s.exchanges.len(), 3532);
        assert_eq!(s.captured_setup(), Setup::System);
        let paths: Vec<&str> = s.exchanges.iter().filter_map(|x| message::decode(&x.payload).map(|(_, p, _)| p)).collect();
        assert_eq!(&paths[..4], ["/////6/", "//1///12/", "//1///13/", crate::fpga::STATUS], "the FPGA status after three messages");
        assert!(node_scan(&s).is_some(), "the node scan");
        for p in ["//7///7/", CLOCK_SERVICE, "//1///10/", "//2///16/"] {
            assert!(!paths.contains(&p), "{p}: a reset, an active-probe clock, an iC5000 upload or 9.21.356's memory service");
        }
        let clocks: Vec<&[u8]> = s
            .exchanges
            .iter()
            .filter_map(|x| match message::decode(&x.payload) {
                Some((_, CLOCK_SERVICE_SYSTEM, b)) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(clocks, vec![&[0x01, 0x00, 0x10, 0x27, 0x00, 0x00][..]; 3], "10,000 kHz, three times");
        for x in &s.exchanges {
            if let Some((_, crate::replay::BREAKPOINT_SERVICE, b)) = message::decode(&x.payload) {
                assert_eq!(b[8], 0, "an armed breakpoint in the attach");
            }
        }
        assert_eq!(Script::builtin(Model::Ic5700), Some(s));
    }

    #[test]
    fn the_ic5700_attach_with_active_probe_shares_the_head_up_to_the_scan() {
        let ap = Script::tc37x();
        let sys = Script::tc37x_system();
        assert_eq!(ap.exchanges.len(), 3705);
        assert_eq!(ap.captured_setup(), Setup::ActiveProbe);
        let at = node_scan(&ap).unwrap();
        assert_eq!(node_scan(&sys), Some(at));
        let payloads = |s: &Script| s.exchanges[..=at].iter().map(|x| x.payload.clone()).collect::<Vec<_>>();
        assert_eq!(payloads(&ap), payloads(&sys), "the driver sends these before it knows the setup");
        let clocks: Vec<u32> = ap.clocks();
        assert_eq!(clocks, vec![10_000_000; 3]);
        for x in &ap.exchanges {
            match message::decode(&x.payload) {
                Some((_, "//7///7/", _)) => panic!("a reset in the attach"),
                Some((_, crate::replay::BREAKPOINT_SERVICE, b)) => assert_eq!(b[8], 0, "an armed breakpoint"),
                _ => {}
            }
        }
        assert_eq!(Setup::ActiveProbe.script(), ap);
        assert_eq!(Setup::System.script(), sys);
    }

    #[test]
    fn an_attach_of_the_other_setup_stops_after_the_node_scan() {
        // A script without active-probe messages, on a BlueBox whose scan finds two nodes.
        let x = |path: &str, body: &[u8]| Exchange {
            payload: message::encode(1, path, body),
            fetch: Fetch::new(0x0e, 0xf000, 2),
            reads: vec![62480],
            expect: vec![vec![0; 32]],
            pause_ms: 0,
        };
        let scan = [0, 0, 0, 0, 0xc8, 0, 0, 0, 0, 0, 0];
        let s = Script { exchanges: vec![x("//1///12/", &[0]), x(NODE_SCAN, &scan), x("//5///19/", &[2])] };
        let hexs = "150000005555555555555555555555550800000010010000000000000000020000000000000000";
        let two_nodes: Vec<u8> = (0..hexs.len()).step_by(2).map(|i| u8::from_str_radix(&hexs[i..i + 2], 16).unwrap()).collect();
        let mut link = ScriptedLink::default();
        link.replies.extend([vec![0; 32], two_nodes]);
        let mut p = Probe::new(link, false);
        let e = p.connect_target(&s, None).unwrap_err().to_string();
        assert!(e.contains("with active probe") && e.contains("attach_replay"), "{e}");
        // (A built-in head would have continued with the setup's built-in script.)
        assert_eq!(p.link_mut().written.len(), 4, "nothing after the scan");
    }

    #[test]
    fn an_ic5000_without_its_fpga_image_stops_before_the_upload() {
        // Two exchanges up to //1///43/, then the upload would come.
        let x = |path: &str| Exchange {
            payload: message::encode(1, path, &[0]),
            fetch: Fetch::new(0x0e, 0xf000, 2),
            reads: vec![62480],
            expect: vec![vec![0; 30]],
            pause_ms: 0,
        };
        let s = Script { exchanges: vec![x("//1///13/"), x("//1///43/"), x("//1///10/"), x("//1///46/")] };
        let mut link = ScriptedLink { model: Model::Ic5000, ..Default::default() };
        link.replies.extend([vec![0; 30], vec![0; 30]]);
        let mut p = Probe::new(link, false);
        let e = p.connect_target(&s, None).unwrap_err();
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

    /// winIDEA's attaches write STM0-2_OCS once each (refused by an application that locked the
    /// STMs, an SMU alarm); nothing else in them is taken for such a write.
    #[test]
    fn builtin_attaches_write_stm_ocs_three_times() {
        for (name, s) in [("tc37x_system", Script::tc37x_system()), ("tc37x", Script::tc37x()), ("tc37x_ic5000", Script::tc37x_ic5000())] {
            let n = s.exchanges.iter().filter(|x| is_stm_ocs_write(x)).count();
            assert_eq!(n, 3, "{name}");
        }
    }

    #[test]
    fn attach_leaves_out_stm_ocs_writes() {
        let s = Script::tc37x_system();
        let i = s.exchanges.iter().position(is_stm_ocs_write).unwrap();
        let other = s.exchanges[i - 1].clone();
        let script = Script { exchanges: vec![s.exchanges[i].clone(), other] };
        let mut link = ScriptedLink::default();
        link.replies.extend([vec![0; 76]]);
        let mut p = Probe::new(link, true);
        let rep = p.attach(&script).unwrap();
        assert_eq!((rep.sent, rep.skipped), (1, 1));
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
