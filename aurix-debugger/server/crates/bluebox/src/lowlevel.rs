//! Link bring-up: hello frames, low-level commands, kernel load, session.
//!
//! The order is what winIDEA 9.21.356 does on every attach (`tests/data/attach_*.txt`):
//!
//! 1. **Hello**: four frames on channel 0 (`42`, `85`, `c9`, `02`). They put both
//!    toggles in a known state: next service message 0, next channel-2 read 1.
//! 2. **Handshake**: low-level commands `03 21 <letter> <args>`, each sent as a
//!    service-channel frame. Its answer is fetched with a channel-2 read
//!    (`04 <len BE16> 00 00 00 00 02`) and comes back as `08` + answer. The
//!    answer to `I` says whether a kernel runs: last byte `2f` yes, `00` no.
//! 3. **Kernel** (only when none runs): `L`, then one `W <addr BE32> <len BE16>
//!    <data>` per [`Kernel::chunks`], then `K <entry BE32>` to start it.
//! 4. **Session**: `//1///28/` with a 22-byte value, kind 0x11. A running kernel
//!    returns it through `//1///27/`.
//!
//! The values in `G` and in the session message are winIDEA 9.21.356's: identical in
//! every session captured, version-specific, meaning not decoded.

use crate::frame::{padded, Header, PIPE_IN, PIPE_OUT};
use crate::kernel::{Kernel, ENTRY};
use crate::{hex, message, Error, Link, Probe, Result};

/// Channel of the reads that fetch low-level answers.
pub const CHANNEL_LOW: u8 = 0x02;
/// Frame type of such a read; its length field is the answer's length.
pub const KIND_LOW_READ: u8 = 0x04;

/// Kind byte of root and node-1 control messages (`//////`, `//1///28/`).
pub const KIND_CONTROL: u8 = 0x11;

/// Host id in `G`, and (byte-swapped) the start of the session value. winIDEA 9.21.356.
pub const HOST_ID: [u8; 2] = [0xa6, 0x64];

/// The session value winIDEA 9.21.356 sends to a freshly loaded kernel.
pub const SESSION_9_21_356: [u8; 22] = [
    0x64, 0xa6, 0xbf, 0xdf, 0xe0, 0x70, 0xd3, 0xf4, 0xc3, 0x54, 0xed, 0xc1, 0x93, 0x36, 0xb4, 0x42, 0x55, 0xc2,
    0x00, 0x00, 0x00, 0x00,
];

/// Node-1 service that takes the session value, in 9.21.356's kernel.
pub const SESSION_SERVICE: &str = "//1///28/";

/// The hello frames, with the reply length each expects (if any).
const HELLO: [([u8; 8], Option<usize>); 4] = [
    ([0x42, 0x00, 0x02, 0, 0, 0, 0, 0x00], None),
    ([0x85, 0x00, 0x00, 0, 0, 0, 0, 0x00], Some(8)),
    ([0xc9, 0x00, 0x00, 0, 0, 0, 0, 0x00], Some(8)),
    ([0x02, 0x00, 0x02, 0, 0, 0, 0, 0x00], None),
];

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
        hello(&mut link)?;
        Ok(Probe::with_toggles(link, false, true))
    }

    /// Reads the answer, `n` bytes, to the last low-level command or control message.
    pub fn low_read(&mut self, n: usize) -> Result<Vec<u8>> {
        let t = self.next_low_toggle();
        let h = Header { kind: KIND_LOW_READ, toggle: t, len: n as u16, channel: CHANNEL_LOW }.encode();
        self.link_mut().write(PIPE_OUT, &h)?;
        let r = self.link_mut().read(PIPE_IN, padded(n + 1))?;
        if r.len() < n + 1 || r[0] != 0x08 {
            return Err(Error::Protocol(format!("answer of {n} bytes: {}", hex(&r, 24))));
        }
        Ok(r[1..1 + n].to_vec())
    }

    /// Sends a low-level command (`03 21 <letter> <args>`) and reads its `answer`-byte answer.
    pub fn command(&mut self, letter: u8, args: &[u8], answer: usize) -> Result<Vec<u8>> {
        self.send(&cmd(letter, args))?;
        self.low_read(answer)
    }

    /// The handshake winIDEA runs after hello.
    pub fn handshake(&mut self) -> Result<Handshake> {
        let ident = self.command(b'I', &[], 7)?;
        let kernel_running = match ident.last() {
            Some(0x2f) => true,
            Some(0x00) => false,
            _ => return Err(Error::Protocol(format!("answer to I: {}", hex(&ident, 8)))),
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
    pub fn load_kernel(&mut self, kernel: &Kernel, mut progress: impl FnMut(usize)) -> Result<()> {
        let a = self.command(b'L', &[], 2)?;
        if a[1] != b'L' {
            return Err(Error::Kernel(format!("L answered {}", hex(&a, 2))));
        }
        let mut sent = 0;
        for (addr, data) in kernel.chunks() {
            let mut args = Vec::with_capacity(6 + data.len());
            args.extend_from_slice(&addr.to_be_bytes());
            args.extend_from_slice(&(data.len() as u16).to_be_bytes());
            args.extend_from_slice(data);
            let a = self.command(b'W', &args, 2)?;
            if a != [0x33, b'W'] {
                return Err(Error::Kernel(format!("W 0x{addr:08x}+{} answered {}", data.len(), hex(&a, 2))));
            }
            sent += data.len();
            progress(sent);
        }
        let a = self.command(b'K', &ENTRY.to_be_bytes(), 2)?;
        if a != [0x33, b'K'] {
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
        if a != [0x01, 0x00] {
            return Err(Error::Kernel(format!("{SESSION_SERVICE} answered {}", hex(&a, 2))));
        }
        Ok(())
    }

    /// Stops the running kernel (`////// 01`), as winIDEA does before loading
    /// another version's. The probe drops its target connection.
    pub fn restart_kernel(&mut self) -> Result<()> {
        let a = self.control("//////", &[0x01], 2)?;
        if a != [0x01, 0x00] {
            return Err(Error::Kernel(format!("////// 01 answered {}", hex(&a, 2))));
        }
        Ok(())
    }
}

/// Opens, handshakes and, if no kernel runs, loads `kernel` and sets the
/// session: what a winIDEA 9.21.356 attach does before its target connection.
/// Returns the handshake and whether the kernel was loaded.
pub fn bring_up<L: Link>(
    link: L,
    kernel: Option<&Kernel>,
    progress: impl FnMut(usize),
) -> Result<(Probe<L>, Handshake, bool)> {
    let mut p = Probe::start(link)?;
    let hs = p.handshake()?;
    if hs.kernel_running {
        return Ok((p, hs, false));
    }
    let k = kernel.ok_or_else(|| {
        Error::Kernel(format!(
            "the probe has no kernel running (power cycled?) and no {} was given",
            crate::kernel::FILE_NAME
        ))
    })?;
    p.load_kernel(k, progress)?;
    p.set_session(&SESSION_9_21_356)?;
    Ok((p, hs, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::ScriptedLink;

    /// A capture fixture: host writes (`W`, `K` = kernel chunk header) and replies (`R`).
    struct Fixture {
        writes: Vec<(u8, Vec<u8>, Option<usize>)>, // pipe, bytes (or chunk head), chunk length
        replies: Vec<Vec<u8>>,
    }

    fn fixture(text: &str) -> Fixture {
        let mut f = Fixture { writes: Vec::new(), replies: Vec::new() };
        let unhex = |s: &str| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>();
        for line in text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
            let p: Vec<&str> = line.split_whitespace().collect();
            match p[0] {
                "W" => f.writes.push((u8::from_str_radix(p[1], 16).unwrap(), unhex(p[2]), None)),
                "K" => f.writes.push((u8::from_str_radix(p[1], 16).unwrap(), unhex(p[3]), Some(p[2].parse().unwrap()))),
                "R" => f.replies.push(unhex(p[2])),
                _ => panic!("{line}"),
            }
        }
        f
    }

    /// Byte-for-byte, except a split payload's padding beyond the length its
    /// header gives: winIDEA leaves stale bytes there, this crate zeros.
    fn compare(ours: &[(u8, Vec<u8>)], theirs: &Fixture) {
        assert_eq!(ours.len(), theirs.writes.len(), "number of host writes");
        let mut split_len: Option<usize> = None;
        for (i, ((pipe, data), (tp, td, chunk))) in ours.iter().zip(&theirs.writes).enumerate() {
            assert_eq!(pipe, tp, "write {i}: pipe");
            let meaningful = match split_len.take() {
                Some(n) if *pipe == crate::frame::PIPE_OUT_BULK => n,
                _ => data.len(),
            };
            if *pipe == PIPE_OUT && data.len() == 8 {
                split_len = Header::decode(data).filter(|h| h.kind == crate::frame::KIND_SPLIT).map(|h| h.len as usize);
            }
            match chunk {
                None => {
                    assert_eq!(data.len(), td.len(), "write {i}: length");
                    assert_eq!(
                        &data[..meaningful],
                        &td[..meaningful],
                        "write {i} differs:\n ours  {}\n theirs {}",
                        hex(data, 64),
                        hex(td, 64)
                    )
                }
                Some(n) => {
                    assert_eq!(data.len(), *n, "write {i}: chunk length");
                    assert_eq!(&data[..9], &td[..], "write {i}: chunk header");
                }
            }
        }
    }

    /// The 9.21.356 kernel's shape (contents do not matter for the framing).
    fn kernel_shape() -> Kernel {
        Kernel { segments: vec![(0x0010_0000, vec![0x5a; 0x223d5c]), (0x0032_3d60, vec![0xa5; 0x33034])] }
    }

    #[test]
    fn a_warm_attach_is_byte_identical_to_winidea() {
        let fx = fixture(include_str!("../tests/data/attach_warm.txt"));
        let link = ScriptedLink { replies: fx.replies.iter().cloned().collect(), ..Default::default() };
        let (mut p, hs, loaded) = bring_up(link, None, |_| {}).unwrap();
        assert!(hs.kernel_running && !loaded);
        assert_eq!(hs.board_ids(), ["0000R000001", "0000R000002"]);
        compare(&p.link_mut().written, &fx);
    }

    #[test]
    fn a_cold_attach_loads_the_kernel_like_winidea() {
        let fx = fixture(include_str!("../tests/data/attach_cold.txt"));
        let link = ScriptedLink { replies: fx.replies.iter().cloned().collect(), ..Default::default() };
        let k = kernel_shape();
        let mut sent = 0;
        let (mut p, hs, loaded) = bring_up(link, Some(&k), |n| sent = n).unwrap();
        assert!(!hs.kernel_running && loaded);
        assert_eq!(sent, k.len());
        compare(&p.link_mut().written, &fx);
    }

    #[test]
    fn a_cold_probe_without_a_kernel_file_is_an_error() {
        let fx = fixture(include_str!("../tests/data/attach_cold.txt"));
        let link = ScriptedLink { replies: fx.replies.iter().cloned().collect(), ..Default::default() };
        assert!(matches!(bring_up(link, None, |_| {}), Err(Error::Kernel(_))));
    }
}
