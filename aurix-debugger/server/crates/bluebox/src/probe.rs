//! A session with the probe: message toggling, request/reply, memory access.

use crate::frame::{self, CHANNEL_SERVICE, PIPE_IN, PIPE_OUT, REPLY_CAPACITY};
use crate::memory::{self, MemReply, MAX_CHUNK};
use crate::{Error, Link, Result};

/// What winIDEA reads after every write: `STM0_TIM0` on TC3xx, always
/// readable. Its read carries any bus error of the write before it.
pub const WRITE_CHECK_ADDR: u32 = 0xF000_1010;

pub struct Probe<L: Link> {
    link: L,
    /// The toggle the next service message carries.
    toggle: bool,
    /// The toggle the next channel-2 read carries (see [`crate::lowlevel`]).
    low_toggle: bool,
}

impl<L: Link> Probe<L> {
    /// A session on `link`. `toggle` is what the probe expects next: after a
    /// [`crate::replay::Replay`], take [`crate::replay::Replay::toggle_after`].
    pub fn new(link: L, toggle: bool) -> Self {
        Probe { link, toggle, low_toggle: true }
    }

    /// A session with both toggles given: what [`Probe::start`] uses after hello.
    pub fn with_toggles(link: L, toggle: bool, low_toggle: bool) -> Self {
        Probe { link, toggle, low_toggle }
    }

    pub(crate) fn next_low_toggle(&mut self) -> bool {
        let t = self.low_toggle;
        self.low_toggle = !t;
        t
    }

    pub fn toggle(&self) -> bool {
        self.toggle
    }

    pub fn set_toggle(&mut self, toggle: bool) {
        self.toggle = toggle;
    }

    pub fn link_mut(&mut self) -> &mut L {
        &mut self.link
    }

    pub fn into_link(self) -> L {
        self.link
    }

    /// Sends one service message.
    pub fn send(&mut self, msg: &[u8]) -> Result<()> {
        for (pipe, bytes) in frame::message_transfers(msg, self.toggle, CHANNEL_SERVICE) {
            self.link.write(pipe, &bytes)?;
        }
        self.toggle = !self.toggle;
        Ok(())
    }

    /// Asks for, and reads, the reply to the last message.
    pub fn receive(&mut self) -> Result<Vec<u8>> {
        self.link.write(PIPE_OUT, &frame::read_request(self.toggle, CHANNEL_SERVICE))?;
        self.link.read(PIPE_IN, REPLY_CAPACITY)
    }

    pub fn transact(&mut self, msg: &[u8]) -> Result<Vec<u8>> {
        self.send(msg)?;
        self.receive()
    }

    /// Reads `len` bytes, in requests of at most [`MAX_CHUNK`]. Bytes the target
    /// could not deliver are zero and flagged in [`MemReply::failed`].
    pub fn read_memory(&mut self, addr: u32, len: usize) -> Result<MemReply> {
        let mut out = MemReply { data: Vec::with_capacity(len), failed: Vec::with_capacity(len) };
        let mut off = 0;
        while off < len {
            let n = (len - off).min(MAX_CHUNK);
            let a = addr.wrapping_add(off as u32);
            let r = self.transact(&memory::request(a, n, None))?;
            let m = memory::parse_reply(&r, n)?;
            if m.data.len() != n {
                return Err(Error::Protocol(format!(
                    "read 0x{a:08x}+{n}: the reply carries {} bytes",
                    m.data.len()
                )));
            }
            out.data.extend_from_slice(&m.data);
            if m.failed.is_empty() {
                out.failed.extend(std::iter::repeat(false).take(n));
            } else {
                out.failed.extend_from_slice(&m.failed);
            }
            off += n;
        }
        Ok(out)
    }

    /// Writes `data`, in requests of at most [`MAX_CHUNK`]. The result's
    /// `failed` flags the bytes the target refused.
    ///
    /// A write's own reply does not report a bus error; the next access does.
    /// On hardware, a write to read-only `SCU_CHIPID` was acknowledged clean and
    /// the read after it failed. So, like winIDEA, each chunk is followed by a
    /// read of [`WRITE_CHECK_ADDR`]. If that read fails, the whole chunk is
    /// counted as failed.
    pub fn write_memory(&mut self, addr: u32, data: &[u8]) -> Result<MemReply> {
        let mut failed = Vec::with_capacity(data.len());
        for (i, chunk) in data.chunks(MAX_CHUNK).enumerate() {
            let a = addr.wrapping_add((i * MAX_CHUNK) as u32);
            let r = self.transact(&memory::request(a, chunk.len(), Some(chunk)))?;
            let m = memory::parse_reply(&r, chunk.len())?;
            let check = self.transact(&memory::request(WRITE_CHECK_ADDR, 4, None))?;
            let late = !memory::parse_reply(&check, 4)?.all_ok();
            if late {
                failed.extend(std::iter::repeat(true).take(chunk.len()));
            } else if m.failed.is_empty() {
                failed.extend(std::iter::repeat(false).take(chunk.len()));
            } else {
                failed.extend_from_slice(&m.failed);
            }
        }
        Ok(MemReply { data: Vec::new(), failed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Header, KIND_READ, KIND_SPLIT, PIPE_OUT_BULK};
    use crate::link::ScriptedLink;

    fn reply(data: &[u8]) -> Vec<u8> {
        let nb = data.len().div_ceil(8);
        let mut r = vec![0u8; 0x2b];
        r[4..16].fill(0x55);
        r[0x10] = 8;
        r[0x27..0x2b].copy_from_slice(&(nb as u32).to_le_bytes());
        r.extend(std::iter::repeat(0).take(nb));
        r.extend_from_slice(&(data.len() as u32).to_le_bytes());
        r.extend_from_slice(data);
        r.extend_from_slice(&[0; 24]);
        let n = (r.len() - 16) as u32;
        r[0..4].copy_from_slice(&n.to_le_bytes());
        r
    }

    #[test]
    fn a_read_is_three_out_transfers_and_the_toggle_advances_once() {
        let mut link = ScriptedLink::default();
        link.replies.push_back(reply(&[1, 2, 3, 4]));
        let mut p = Probe::new(link, true);
        let m = p.read_memory(0x8000_0004, 4).unwrap();
        assert_eq!(m.data, [1, 2, 3, 4]);
        assert!(!p.toggle());
        let w = &p.link_mut().written;
        assert_eq!(w.len(), 3);
        // As captured: e3 00 ca ... 14, the 204-byte body on pipe 6, then 0e f0 00 ... 14.
        assert_eq!(w[0].0, PIPE_OUT);
        assert_eq!(Header::decode(&w[0].1).unwrap(), Header { kind: KIND_SPLIT, toggle: true, len: 202, channel: 0x14 });
        assert_eq!((w[1].0, w[1].1.len()), (PIPE_OUT_BULK, 204));
        assert_eq!(w[1].1[..202], memory::request(0x8000_0004, 4, None)[..]);
        assert_eq!(Header::decode(&w[2].1).unwrap().kind, KIND_READ);
        assert!(!Header::decode(&w[2].1).unwrap().toggle);
    }

    #[test]
    fn long_reads_are_split_into_chunks() {
        let mut link = ScriptedLink::default();
        link.replies.push_back(reply(&vec![0xaa; MAX_CHUNK]));
        link.replies.push_back(reply(&[0xbb; 10]));
        let mut p = Probe::new(link, false);
        let m = p.read_memory(0x8000_0000, MAX_CHUNK + 10).unwrap();
        assert_eq!(m.data.len(), MAX_CHUNK + 10);
        assert_eq!(m.data[MAX_CHUNK], 0xbb);
        assert_eq!(m.failed.len(), MAX_CHUNK + 10);
        let second = &p.link_mut().written[4].1;
        assert_eq!(&second[0x1e..0x22], &(0x8000_0000u32 + MAX_CHUNK as u32).to_le_bytes());
    }

    fn failed_reply(n: usize) -> Vec<u8> {
        let mut r = reply(&vec![0; n]);
        for b in &mut r[0x2b..0x2b + n.div_ceil(8)] {
            *b = 0xff;
        }
        r
    }

    #[test]
    fn a_write_is_confirmed_by_the_next_read() {
        let mut link = ScriptedLink::default();
        link.replies.push_back(reply(&[])); // the write: no data, clean bitmap
        link.replies.push_back(reply(&[0; 4])); // STM0_TIM0 reads fine
        let mut p = Probe::new(link, false);
        assert!(p.write_memory(0x7000_0000, &[1, 2, 3, 4]).unwrap().all_ok());
        let w = &p.link_mut().written;
        assert_eq!(w.len(), 6);
        assert_eq!(&w[4].1[0x1e..0x22], &WRITE_CHECK_ADDR.to_le_bytes());
    }

    #[test]
    fn a_bus_error_reported_late_fails_the_write() {
        let mut link = ScriptedLink::default();
        link.replies.push_back(reply(&[]));
        link.replies.push_back(failed_reply(4));
        let mut p = Probe::new(link, false);
        let m = p.write_memory(0xF003_6140, &[0x80, 0x77, 0x22, 0x89]).unwrap();
        assert_eq!(m.failed, vec![true; 4]);
    }

    #[test]
    fn a_short_reply_is_an_error() {
        let mut link = ScriptedLink::default();
        link.replies.push_back(reply(&[1, 2]));
        let mut p = Probe::new(link, false);
        assert!(matches!(p.read_memory(0x7000_0000, 4), Err(Error::Protocol(_))));
    }
}
