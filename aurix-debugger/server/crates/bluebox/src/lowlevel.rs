//! Link bring-up: hello frames, low-level commands, kernel load, session.
//!
//! The order is what winIDEA 9.21.417 does on every attach (`tests/data/attach_*.txt`):
//!
//! 1. **Hello**: four frames on channel 0 (`42`, `85`, `c9`, `02`). They reset the
//!    toggle: the next service message carries 0 (its fetch frame 1).
//! 2. **Handshake**: low-level commands `03 21 <letter> <args>`, each sent as a
//!    service-channel frame. Its answer is fetched with a channel-2 read
//!    (`04 <len BE16> 00 00 00 00 02`) and comes back as `08` + answer. The
//!    answer to `I` says whether a kernel runs: last byte `2f` yes, `00` no.
//! 3. **Kernel** (only when none runs): `L`, then one `W <addr BE32> <len BE16>
//!    <data>` per [`Kernel::chunks`], then `K <entry BE32>` to start it.
//! 4. **Session**: `//1///29/` with a 22-byte value, kind 0x11. A running kernel
//!    returns it through `//1///28/`.
//!
//! The `I` state does not tell whose kernel runs: 9.21.356 found `64` with
//! 9.21.417's kernel, and 9.21.417 `2f` with 9.21.356's, as with its own. So a
//! probe with a kernel is asked ([`Probe::kernel_state`]), and another
//! version's kernel is stopped and replaced, as winIDEA does.
//!
//! The values in `G` and in the session message are winIDEA 9.21.417's: identical in
//! every session captured, version-specific, meaning not decoded. 9.21.356's
//! node-1 services were numbered one or two lower (session `//1///28/`, check
//! `//1///25/`), its host id was `a6 64`.
//!
//! An iC5000 runs the same steps (`tests/data/attach_ic5000_*.txt`) with
//! frames padded to 2 bytes, answers rounded to an even length
//! ([`crate::Model::answer_capacity`]), its own kernel file and session value,
//! and `I` states `ef` or `eb` without a kernel.

use crate::frame::{PIPE_IN, PIPE_OUT};
use crate::kernel::{Kernel, ENTRY};
use crate::{hex, message, Error, Link, Probe, Result};

/// Channel of the reads that fetch low-level answers.
pub const CHANNEL_LOW: u8 = 0x02;
/// Frame type of such a read; its length field is the answer's length.
pub const KIND_LOW_READ: u8 = 0x04;

/// Kind byte of root and node-1 control messages (`//////`, `//1///29/`).
pub const KIND_CONTROL: u8 = 0x11;

/// Host id in `G`, and (byte-swapped) the start of the session value. winIDEA 9.21.417.
pub const HOST_ID: [u8; 2] = [0xa6, 0xa1];

/// The session value winIDEA 9.21.417 sends to a freshly loaded iC5700 kernel
/// (`first.jsonl`, the 9.21.417 session after a power cycle).
pub const SESSION_9_21_417: [u8; 22] = [
    0xa1, 0xa6, 0xc4, 0xa2, 0x1b, 0x6e, 0x82, 0x08, 0x5d, 0x3d, 0xa2, 0x12, 0xb6, 0x2e, 0x37, 0x82, 0xd6, 0xb8,
    0x00, 0x00, 0x00, 0x00,
];

/// The session value winIDEA 9.21.417 sends to a freshly loaded iC5000 kernel
/// (`ic5000-417.jsonl`, `ic5000-417-foreign.jsonl`; its later attach read the same back).
pub const SESSION_IC5000_9_21_417: [u8; 22] = [
    0xa1, 0xa6, 0x77, 0x4b, 0x4f, 0xc4, 0x1c, 0x34, 0x27, 0x74, 0x6e, 0x3a, 0xa4, 0x73, 0xfa, 0xee, 0xcf, 0x69,
    0x00, 0x00, 0x00, 0x00,
];

/// The session value winIDEA 9.21.417 gives `model`'s kernel.
pub fn session_value(model: crate::Model) -> [u8; 22] {
    match model {
        crate::Model::Ic5700 => SESSION_9_21_417,
        crate::Model::Ic5000 => SESSION_IC5000_9_21_417,
    }
}

/// Node-1 service that takes the session value, in 9.21.417's kernel.
pub const SESSION_SERVICE: &str = "//1///29/";

/// The hello frames, with the reply length each expects (if any).
const HELLO: [([u8; 8], Option<usize>); 4] = [
    ([0x42, 0x00, 0x02, 0, 0, 0, 0, 0x00], None),
    ([0x85, 0x00, 0x00, 0, 0, 0, 0, 0x00], Some(8)),
    ([0xc9, 0x00, 0x00, 0, 0, 0, 0, 0x00], Some(8)),
    ([0x02, 0x00, 0x02, 0, 0, 0, 0, 0x00], None),
];

/// `I` state after a power cycle: no kernel runs.
pub const STATE_NO_KERNEL: u8 = 0x00;
/// `I` state of a boot loader waiting for a kernel: a handshake ran (it ends with `L`, which
/// starts a kernel load) and no kernel followed, e.g. a connect that stopped after the
/// handshake because it had no kernel file. It stays until a kernel is loaded or the probe is
/// power cycled. Loading works from here, as from [`STATE_NO_KERNEL`] (iC5700 B, 2026-11-08); a
/// kernel restart (`////// 01`) sent here got no answer and the probe answered nothing more
/// until a power cycle.
pub const STATE_LOAD_MODE: u8 = 0x01;
/// `I` states of an iC5000 without a kernel have the two top bits set: `ef`
/// in winIDEA's cold attach, `eb` right after a power cycle. With its kernel
/// it reports `2f`, as the iC5700 does.
pub const NO_KERNEL_IC5000_MASK: u8 = 0xc0;
/// `I` state with a kernel (ours, or none right after a `BYE`).
pub const STATE_KERNEL: u8 = 0x2f;
/// `I` state 9.21.356 found with 9.21.417's kernel running.
pub const STATE_FOREIGN_KERNEL: u8 = 0x64;

/// What the handshake learned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// A kernel runs (answer to `I` ends in `2f`).
    pub kernel_running: bool,
    /// Answer to `I`.
    pub ident: Vec<u8>,
    /// Answers to the four probe-info reads (`E R2)` / `E R2+`, 257 bytes each,
    /// starting `aa aa 00 00 5c` and a board id such as `0000R000001`).
    pub info: Vec<Vec<u8>>,
    /// Answer to `G` (258 bytes).
    pub config: Vec<u8>,
}

impl Handshake {
    /// None of the known states (00 or 01 no kernel, 2f a kernel, 64 another
    /// version's kernel): the probe may still be booting, or its kernel is
    /// stuck (40 was seen). [`open_ready`] waits; if the state stays,
    /// stop the kernel with [`Probe::restart_kernel`] and load it again.
    pub fn needs_restart(&self) -> bool {
        !self.no_kernel() && ![STATE_KERNEL, STATE_FOREIGN_KERNEL].contains(&self.state())
    }

    /// No kernel runs: `I` state 00, 01 ([`STATE_LOAD_MODE`], a boot loader left
    /// waiting for one), or one with [`NO_KERNEL_IC5000_MASK`] set (an iC5000; the
    /// iC5700 never reported such states).
    pub fn no_kernel(&self) -> bool {
        let s = self.state();
        s == STATE_NO_KERNEL || s == STATE_LOAD_MODE || s & NO_KERNEL_IC5000_MASK == NO_KERNEL_IC5000_MASK
    }

    /// The `I` answer's state byte.
    pub fn state(&self) -> u8 {
        self.ident.last().copied().unwrap_or(0)
    }

    /// The printable board ids in the probe-info answers.
    pub fn board_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for a in &self.info {
            let t: String = a.iter().skip(5).take_while(|b| b.is_ascii_graphic()).map(|b| *b as char).collect();
            if t.len() >= 4 && !ids.contains(&t) {
                ids.push(t);
            }
        }
        ids
    }
}

/// Sends the hello frames on a freshly opened link.
pub fn hello<L: Link + ?Sized>(link: &mut L) -> Result<()> {
    for (frame, reply) in HELLO {
        link.write(PIPE_OUT, &frame)?;
        if let Some(n) = reply {
            let r = link.read(PIPE_IN, n)?;
            if r.len() != n {
                return Err(Error::Protocol(format!("hello: {} byte reply to {:02x}: {}", r.len(), frame[0], hex(&r, 16))));
            }
        }
    }
    Ok(())
}

/// A two-byte answer `<code> <second>`: the code, and the second byte when
/// it came (an iC5000 leaves it out).
fn acked(a: &[u8], code: u8, second: u8) -> bool {
    a.first() == Some(&code) && a.get(1).is_none_or(|b| *b == second)
}

fn cmd(letter: u8, args: &[u8]) -> Vec<u8> {
    let mut c = vec![0x03, 0x21, letter];
    c.extend_from_slice(args);
    c
}

const INFO_ARGS: [&[u8]; 8] = [
    &[0x52, 0x32, 0x29, 0x00, 0x00, 0x80],
    &[0x52, 0x32, 0x29, 0x01, 0x00, 0x80],
    &[0x52, 0x32, 0x29, 0x00, 0x00, 0x80],
    &[0x52, 0x32, 0x29, 0x01, 0x00, 0x80],
    &[0x52, 0x32, 0x2b, 0x00, 0x00, 0x80],
    &[0x52, 0x32, 0x2b, 0x01, 0x00, 0x80],
    &[0x52, 0x32, 0x2b, 0x00, 0x00, 0x80],
    &[0x52, 0x32, 0x2b, 0x01, 0x00, 0x80],
];

impl<L: Link> Probe<L> {
    /// Opens a probe session on a freshly opened link: hello, then the
    /// toggles winIDEA's first frames use.
    pub fn start(mut link: L) -> Result<Probe<L>> {
        link.drain();
        hello(&mut link)?;
        Ok(Probe::new(link, false))
    }

    /// Reads the answer, `n` bytes, to the last low-level command or control
    /// message. From an iC5000 an even-length answer comes without its last
    /// byte ([`crate::Model::answer_capacity`]), so callers check only the
    /// bytes they get beyond the first.
    pub fn low_read(&mut self, n: usize) -> Result<Vec<u8>> {
        self.fetch(crate::probe::Fetch::new(KIND_LOW_READ, n as u16, CHANNEL_LOW))?;
        let cap = self.model().answer_capacity(n);
        let r = self.link_mut().read(PIPE_IN, cap)?;
        if r.len() < cap.min(n + 1) || r.len() < 2 || r[0] != 0x08 {
            return Err(Error::Protocol(format!("answer of {n} bytes: {}", hex(&r, 24))));
        }
        Ok(r[1..r.len().min(1 + n)].to_vec())
    }

    /// Sends a low-level command (`03 21 <letter> <args>`) and reads its `answer`-byte answer.
    pub fn command(&mut self, letter: u8, args: &[u8], answer: usize) -> Result<Vec<u8>> {
        self.send(&cmd(letter, args))?;
        self.low_read(answer)
    }

    /// The handshake winIDEA runs after hello.
    pub fn handshake(&mut self) -> Result<Handshake> {
        let ident = self.command(b'I', &[], 7)?;
        // Last byte, a state rather than a flag: 00 when no kernel runs (after a
        // power cycle, once the probe has booted); 01 when a handshake left the
        // boot loader waiting for a kernel ([`STATE_LOAD_MODE`]); 2f with a kernel
        // that takes the attach. 40 was seen too; attaching in it timed out and
        // left the probe hung until a power cycle. Only 2f counts as a usable
        // kernel; see [`open_ready`] for the others.
        let kernel_running = match ident.last() {
            Some(0x2f) => true,
            Some(_) => false,
            None => return Err(Error::Protocol(format!("answer to I: {}", hex(&ident, 8)))),
        };
        self.command(b'V', &[], 5)?;
        self.command(b'S', &[b'N'], 5)?;
        let mut info = Vec::new();
        for a in INFO_ARGS {
            info.push(self.command(b'E', a, 0x101)?);
        }
        self.command(b'V', &[], 5)?;
        self.command(b'C', &[0x01, 0, 0, 0, 0x03, 0, 0, 0, 0x04], 2)?;
        self.command(b'V', &[], 5)?;
        self.command(b'V', &[], 5)?;
        // 256 argument bytes: the host id, then zeros. (winIDEA's padding byte
        // after them is stale buffer contents: 00 in one capture, 01 in another.)
        let mut g = vec![0u8; 256];
        g[..2].copy_from_slice(&HOST_ID);
        let config = self.command(b'G', &g, 0x102)?;
        self.command(b'L', &[], 2)?;
        Ok(Handshake { kernel_running, ident, info, config })
    }

    /// Loads and starts `kernel`. Only on a probe whose handshake found none running.
    /// `progress` gets the bytes sent so far.
    pub fn load_kernel(&mut self, kernel: &Kernel, progress: impl FnMut(usize)) -> Result<()> {
        let a = self.command(b'L', &[], 2)?;
        if a.get(1).is_some_and(|b| *b != b'L') {
            return Err(Error::Kernel(format!("L answered {}", hex(&a, 2))));
        }
        self.load_chunks(kernel, progress)
    }

    /// The kernel's `W` chunks, then `K` to start it.
    fn load_chunks(&mut self, kernel: &Kernel, mut progress: impl FnMut(usize)) -> Result<()> {
        let mut sent = 0;
        for (addr, data) in kernel.chunks() {
            let mut args = Vec::with_capacity(6 + data.len());
            args.extend_from_slice(&addr.to_be_bytes());
            args.extend_from_slice(&(data.len() as u16).to_be_bytes());
            args.extend_from_slice(data);
            let a = self.command(b'W', &args, 2)?;
            if !acked(&a, 0x33, b'W') {
                return Err(Error::Kernel(format!("W 0x{addr:08x}+{} answered {}", data.len(), hex(&a, 2))));
            }
            sent += data.len();
            progress(sent);
        }
        let a = self.command(b'K', &ENTRY.to_be_bytes(), 2)?;
        if !acked(&a, 0x33, b'K') {
            return Err(Error::Kernel(format!("K answered {}", hex(&a, 2))));
        }
        Ok(())
    }

    /// Sends a root or node-1 control message (kind 0x11) and reads its `answer`-byte answer.
    pub fn control(&mut self, path: &str, body: &[u8], answer: usize) -> Result<Vec<u8>> {
        self.send(&message::encode(KIND_CONTROL, path, body))?;
        self.low_read(answer)
    }

    /// Hands a freshly started kernel the session value.
    pub fn set_session(&mut self, value: &[u8]) -> Result<()> {
        let a = self.control(SESSION_SERVICE, value, 2)?;
        if !acked(&a, 0x01, 0x00) {
            return Err(Error::Kernel(format!("{SESSION_SERVICE} answered {}", hex(&a, 2))));
        }
        Ok(())
    }

    /// Stops the running kernel (`////// 01`), as winIDEA does before loading
    /// another version's. The probe drops its target connection.
    pub fn restart_kernel(&mut self) -> Result<()> {
        let a = self.control("//////", &[0x01], 2)?;
        if !acked(&a, 0x01, 0x00) {
            return Err(Error::Kernel(format!("////// 01 answered {}", hex(&a, 2))));
        }
        Ok(())
    }

    /// Whether the running kernel is winIDEA 9.21.417's, with its session:
    /// `//1///26/` answers `02 ...` and `//1///28/` returns the session value.
    /// 9.21.356's kernel answered `//1///26/` with an echo of the request
    /// header instead, and `//1///28/` too; winIDEA sends both either way
    /// (`ic5000-417-foreign.jsonl`), and so does this. Another kernel's service
    /// numbers differ (see [`crate::kernel`]), so nothing but this check may go
    /// to a kernel that is not ours.
    pub fn kernel_is_ours(&mut self) -> Result<bool> {
        let a = self.control(VERSION_CHECK, &[0; 8], 9)?;
        let s = self.control(SESSION_READ, &[], 23)?;
        Ok(a[0] == 0x02 && s.len() >= 18 && s[..2] == HOST_ID && s[2..18] == session_value(self.model())[2..18])
    }

    /// `//1///3/ 00`, a service message, before a kernel is stopped.
    fn close_session(&mut self) -> Result<()> {
        self.send(&message::encode(message::KIND, "//1///3/", &[0x00]))?;
        self.fetch(crate::probe::Fetch::new(crate::frame::KIND_READ, crate::frame::READ_WINDOW, CHANNEL_LOW))?;
        let r = self.link_mut().read(PIPE_IN, crate::frame::REPLY_CAPACITY)?;
        if !message::is_reply(&r) {
            return Err(Error::Kernel(format!("//1///3/ answered {}", hex(&r, 24))));
        }
        Ok(())
    }

    /// Stops another version's kernel, as winIDEA 9.21.417 did with 9.21.356's
    /// (`tests/data/attach_ic5000_foreign.txt`): `L`, `//1///3/ 00`, `////// 01`. Load
    /// ours right after with [`Probe::load_kernel_after_stop`].
    pub fn stop_foreign_kernel(&mut self) -> Result<()> {
        self.command(b'L', &[], 2)?;
        self.close_session()?;
        self.restart_kernel()
    }

    /// Loads and starts `kernel` right after [`Probe::stop_foreign_kernel`]:
    /// the chunks and `K`, without the `L` a cold load starts with.
    pub fn load_kernel_after_stop(&mut self, kernel: &Kernel, progress: impl FnMut(usize)) -> Result<()> {
        self.load_chunks(kernel, progress)
    }
}

/// Node-1 service that tells whether the running kernel is this version's, in 9.21.417's kernel.
pub const VERSION_CHECK: &str = "//1///26/";
/// Node-1 service that returns the session value of a running kernel.
pub const SESSION_READ: &str = "//1///28/";

/// Whose kernel runs on the probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelState {
    /// None (`I` state 00, after a power cycle).
    None,
    /// winIDEA 9.21.417's, with its session.
    Ours,
    /// Another one, e.g. another winIDEA version's.
    Foreign,
}

impl<L: Link> Probe<L> {
    /// After [`Probe::handshake`]: whose kernel runs. Asks the kernel
    /// ([`Probe::kernel_is_ours`]) unless the `I` state says none runs.
    pub fn kernel_state(&mut self, hs: &Handshake) -> Result<KernelState> {
        if hs.no_kernel() {
            return Ok(KernelState::None);
        }
        Ok(if self.kernel_is_ours()? { KernelState::Ours } else { KernelState::Foreign })
    }
}

/// Opens the probe and runs the handshake until the probe is in a known state:
/// a usable kernel (`I` state 2f) or none (00). Any other state is retried every
/// second for up to `wait` (a BlueBox reports such states while it boots after a
/// power cycle). If it stays, the kernel is stopped (`////// 01`) and the probe
/// opened once more. `note` hears about each wait.
pub fn open_ready<L: Link>(
    mut open: impl FnMut() -> Result<L>,
    wait: std::time::Duration,
    mut note: impl FnMut(&str),
) -> Result<(Probe<L>, Handshake)> {
    let t0 = std::time::Instant::now();
    loop {
        let mut p = Probe::start(open()?)?;
        let hs = p.handshake()?;
        if !hs.needs_restart() {
            return Ok((p, hs));
        }
        if t0.elapsed() < wait {
            note(&format!("probe state 0x{:02x}, waiting for it", hs.state()));
            drop(p);
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        let state = hs.state();
        note(&format!("probe state 0x{state:02x} after {} s; restarting its kernel", wait.as_secs()));
        // A probe that does not take the restart is beyond what the host can fix.
        let stuck = |e: Error| Error::Kernel(format!("the probe stayed in state 0x{state:02x} and did not take a kernel restart ({e}); power-cycle the BlueBox"));
        p.restart_kernel().map_err(stuck)?;
        drop(p);
        std::thread::sleep(std::time::Duration::from_millis(200));
        let mut p = Probe::start(open()?).map_err(stuck)?;
        let hs = p.handshake().map_err(stuck)?;
        return Ok((p, hs));
    }
}

/// Opens, handshakes and makes sure winIDEA 9.21.417's kernel runs with its
/// session: none running, it loads `kernel`; another one running, it stops that
/// and loads `kernel`. What a winIDEA 9.21.417 attach does before its target
/// connection. Returns the handshake and what ran before.
pub fn bring_up<L: Link>(
    link: L,
    kernel: Option<&Kernel>,
    progress: impl FnMut(usize),
) -> Result<(Probe<L>, Handshake, KernelState)> {
    let mut p = Probe::start(link)?;
    let hs = p.handshake()?;
    let state = p.kernel_state(&hs)?;
    p.ensure_kernel(state, kernel, progress)?;
    Ok((p, hs, state))
}

impl<L: Link> Probe<L> {
    /// Given what [`Probe::kernel_state`] found, makes winIDEA 9.21.417's kernel run with its session.
    pub fn ensure_kernel(&mut self, state: KernelState, kernel: Option<&Kernel>, progress: impl FnMut(usize)) -> Result<()> {
        if state == KernelState::Ours {
            return Ok(());
        }
        let k = kernel.ok_or_else(|| {
            Error::Kernel(match state {
                KernelState::Foreign => format!(
                    "the probe runs another winIDEA version's kernel, and no {} of 9.21.417 was given to replace it",
                    self.model().kernel_file()
                ),
                _ => format!("the probe has no kernel running (power cycled?) and no {} was given", self.model().kernel_file()),
            })
        })?;
        if state == KernelState::Foreign {
            self.stop_foreign_kernel()?;
            self.load_kernel_after_stop(k, progress)?;
        } else {
            self.load_kernel(k, progress)?;
        }
        let session = session_value(self.model());
        self.set_session(&session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{compare, parse as fixture};

    /// The 9.21.417 kernels' shapes (contents do not matter for the framing).
    fn ic6k_shape() -> Kernel {
        Kernel { segments: vec![(0x0010_0000, vec![0x5a; 0x278278])] }
    }
    fn ic3k_shape() -> Kernel {
        Kernel { segments: vec![(0x0010_0000, vec![0x5a; 0x24c334]), (0x0034_c338, vec![0xa5; 0x2c018])] }
    }

    #[test]
    fn a_warm_attach_checks_the_kernel_like_winidea() {
        let fx = fixture(include_str!("../tests/data/attach_ic5000_warm_check.txt"));
        let (mut p, hs, state) = bring_up(fx.link_to(crate::Model::Ic5000), None, |_| {}).unwrap();
        assert_eq!((hs.state(), state), (STATE_KERNEL, KernelState::Ours));
        assert_eq!(hs.board_ids(), ["0000R000005", "0000R000006"]);
        compare(&p.link_mut().written, &fx);
    }

    #[test]
    fn an_ic5700_checks_its_kernel_like_winidea() {
        // iC5700 B without active probe, winIDEA 9.21.417's kernel running (no-ap-417-warm.jsonl).
        let fx = fixture(include_str!("../tests/data/attach_warm_check.txt"));
        let (mut p, hs, state) = bring_up(fx.link(), None, |_| {}).unwrap();
        assert_eq!((hs.state(), state), (STATE_KERNEL, KernelState::Ours));
        assert_eq!(hs.board_ids(), ["0000R000003", "0000R000004"]);
        compare(&p.link_mut().written, &fx);
    }

    #[test]
    fn a_cold_attach_loads_the_kernel_like_winidea() {
        // An iC5700 after a power cycle (first.jsonl, winIDEA 9.21.417).
        let fx = fixture(include_str!("../tests/data/attach_cold.txt"));
        let k = ic6k_shape();
        let mut sent = 0;
        let (mut p, hs, state) = bring_up(fx.link(), Some(&k), |n| sent = n).unwrap();
        assert_eq!((hs.state(), state), (STATE_NO_KERNEL, KernelState::None));
        assert_eq!(sent, k.len());
        compare(&p.link_mut().written, &fx);
    }

    #[test]
    fn another_versions_kernel_is_stopped_and_replaced_like_winidea() {
        // An iC5000 running winIDEA 9.21.356's kernel: the check and the session
        // read both answer an echo; then the stop, the chunks of iC3kKrnl.s, the session.
        let fx = fixture(include_str!("../tests/data/attach_ic5000_foreign.txt"));
        let k = ic3k_shape();
        let (mut p, hs, state) = bring_up(fx.link_to(crate::Model::Ic5000), Some(&k), |_| {}).unwrap();
        assert_eq!((hs.state(), state), (STATE_KERNEL, KernelState::Foreign));
        assert!(!hs.needs_restart(), "a known state: no waiting");
        compare(&p.link_mut().written, &fx);
    }

    #[test]
    fn the_ic5000_s_states_without_kernel_are_known() {
        let hs = |s: u8| Handshake { kernel_running: s == STATE_KERNEL, ident: vec![0x48, 0x56, 3, 2, 0, 0, s], info: vec![], config: vec![] };
        for s in [0xef, 0xeb, 0x00] {
            assert!(hs(s).no_kernel() && !hs(s).needs_restart(), "{s:02x}");
        }
        assert!(!hs(0x2f).no_kernel() && !hs(0x2f).needs_restart());
        // A boot loader a handshake left waiting for a kernel: loaded at once, never restarted.
        assert!(hs(STATE_LOAD_MODE).no_kernel() && !hs(STATE_LOAD_MODE).needs_restart());
        assert!(hs(0x40).needs_restart(), "the iC5700's stuck state");
    }

    #[test]
    fn short_answers_are_accepted_by_their_first_byte() {
        assert!(acked(&[0x33, b'W'], 0x33, b'W'));
        assert!(acked(&[0x33], 0x33, b'W'), "an iC5000 leaves the second byte out");
        assert!(!acked(&[0x33, b'X'], 0x33, b'W'));
        assert!(!acked(&[0x01], 0x33, b'W'));
        assert!(!acked(&[], 0x33, b'W'));
    }

    #[test]
    fn a_probe_without_a_kernel_file_is_an_error() {
        let cold = fixture(include_str!("../tests/data/attach_cold.txt"));
        assert!(matches!(bring_up(cold.link(), None, |_| {}), Err(Error::Kernel(_))));
        let foreign = fixture(include_str!("../tests/data/attach_ic5000_foreign.txt"));
        assert!(matches!(bring_up(foreign.link_to(crate::Model::Ic5000), None, |_| {}), Err(Error::Kernel(_))));
    }
}
