//! A session with the probe: message toggling, request/reply, memory access.

use crate::frame::{self, CHANNEL_SERVICE, PIPE_IN, PIPE_OUT, REPLY_CAPACITY};
use crate::memory::{self, Access, MemReply, MAX_CHUNK};
use crate::{Error, Link, Result};

/// What winIDEA reads after every write: `STM0_TIM0` on TC3xx, always
/// readable. Its read carries any bus error of the write before it.
pub const WRITE_CHECK_ADDR: u32 = 0xF000_1010;

pub struct Probe<L: Link> {
    link: L,
    /// The toggle the next service message carries. A fetch frame (the read
    /// request after a message, on any channel) carries this value too, that
    /// is, the inverse of the message it answers: checked on all 3498 fetches
    /// of a captured attach.
    toggle: bool,
}

/// How the reply to one message is fetched: a header-only frame, kept whole
/// (bytes 1-7) because not every kind leaves bytes 3-6 zero: the bulk fetch
/// `0c 00 00 00 0e 00 00 05` carries its size, 0x0e0000, there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fetch {
    /// `0x0e` (service reply), `0x04` (low-level answer), `0x0c` (bulk, channel 5).
    pub kind: u8,
    /// Header bytes 1..8: the length field, bytes 3-6, and the channel.
    pub rest: [u8; 7],
}

impl Fetch {
    /// The usual form: a length field and a channel, bytes 3-6 zero.
    pub fn new(kind: u8, len: u16, channel: u8) -> Fetch {
        let l = len.to_be_bytes();
        Fetch { kind, rest: [l[0], l[1], 0, 0, 0, 0, channel] }
    }

    pub fn from_header(b: &[u8]) -> Option<Fetch> {
        Some(Fetch { kind: *b.first()? & 0x7f, rest: b.get(1..8)?.try_into().ok()? })
    }

    pub fn channel(&self) -> u8 {
        self.rest[6]
    }

    pub fn encode(&self, toggle: bool) -> [u8; 8] {
        let mut h = [0u8; 8];
        h[0] = (self.kind & 0x7f) | ((toggle as u8) << 7);
        h[1..].copy_from_slice(&self.rest);
        h
    }
}

/// One message and how its reply is read: the unit an attach is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    pub payload: Vec<u8>,
    pub fetch: Fetch,
    /// IN transfers that follow, by capacity (usually one; two for a bulk `0x0c`).
    pub reads: Vec<usize>,
    /// The replies captured with it, for comparison.
    pub expect: Vec<Vec<u8>>,
    /// How long winIDEA paused before sending it, after the last reply, in ms
    /// (0: none worth keeping). [`Probe::exchange`] waits as long.
    pub pause_ms: u32,
}

impl<L: Link> Probe<L> {
    /// A session on `link`. `toggle` is what the probe expects next: after a
    /// [`crate::replay::Replay`], take [`crate::replay::Replay::toggle_after`].
    pub fn new(link: L, toggle: bool) -> Self {
        Probe { link, toggle }
    }

    /// Sends a fetch frame for the last message (carrying the current toggle).
    pub fn fetch(&mut self, f: Fetch) -> Result<()> {
        self.link.write(PIPE_OUT, &f.encode(self.toggle))
    }

    /// Sends one exchange as captured, with this session's toggles, and returns the replies.
    pub fn exchange(&mut self, x: &Exchange) -> Result<Vec<Vec<u8>>> {
        if x.pause_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(x.pause_ms as u64));
        }
        self.send(&x.payload)?;
        self.fetch(x.fetch)?;
        x.reads.iter().map(|&cap| self.link.read(PIPE_IN, cap)).collect()
    }

    pub fn toggle(&self) -> bool {
        self.toggle
    }

    pub fn set_toggle(&mut self, toggle: bool) {
        self.toggle = toggle;
    }

    /// Which BlueBox this session talks to.
    pub fn model(&self) -> crate::Model {
        self.link.model()
    }

    pub fn link_mut(&mut self) -> &mut L {
        &mut self.link
    }

    pub fn into_link(self) -> L {
        self.link
    }

    /// Sends one service message.
    pub fn send(&mut self, msg: &[u8]) -> Result<()> {
        let pad = self.link.model().frame_pad();
        for (pipe, bytes) in frame::message_transfers_padded(msg, self.toggle, CHANNEL_SERVICE, pad) {
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

    /// Runs `acc` (each at most [`MAX_CHUNK`]) in one message. The probe runs
    /// them all, also after one fails.
    pub fn batch(&mut self, acc: &[Access]) -> Result<Vec<MemReply>> {
        let r = self.transact(&memory::batch_request(acc))?;
        let lens: Vec<usize> = acc.iter().map(|a| a.len()).collect();
        let out = memory::parse_batch_reply(&r, &lens)?;
        for (a, m) in acc.iter().zip(&out) {
            if !a.is_write() && m.data.len() != a.len() {
                return Err(Error::Protocol(format!(
                    "read 0x{:08x}+{}: the reply carries {} bytes",
                    a.addr(),
                    a.len(),
                    m.data.len()
                )));
            }
        }
        Ok(out)
    }

    /// Runs `ops` in order, in as few messages as [`memory::plan`] allows,
    /// splitting longer ones at [`MAX_CHUNK`]: reads share messages, every
    /// write has one of its own, followed by the check read. Stops at the first op with a failed byte, like a TAS packet:
    /// the result is one reply per op up to and including that one, whose bytes
    /// after the failure are flagged (and zero for a read). The probe may have
    /// run reads after it in the same message; their results are dropped.
    pub fn access(&mut self, ops: &[Access]) -> Result<Vec<MemReply>> {
        // Pieces of at most MAX_CHUNK, by op.
        let mut pieces: Vec<(usize, Access)> = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            let mut off = 0;
            while off < op.len() {
                let n = (op.len() - off).min(MAX_CHUNK);
                let addr = op.addr().wrapping_add(off as u32);
                pieces.push((
                    i,
                    match *op {
                        Access::Read { .. } => Access::Read { addr, len: n },
                        Access::Write { data, .. } => Access::Write { addr, data: &data[off..off + n] },
                    },
                ));
                off += n;
            }
        }
        let mut out: Vec<MemReply> = ops.iter().map(|_| MemReply { data: Vec::new(), failed: Vec::new() }).collect();
        let mut failed_op = None;
        'messages: for range in memory::plan(&pieces.iter().map(|p| p.1).collect::<Vec<_>>()) {
            let part = &pieces[range];
            let mut msg: Vec<Access> = part.iter().map(|p| p.1).collect();
            let write = msg.last().is_some_and(|a| a.is_write());
            if write {
                msg.push(memory::CHECK);
            }
            let rep = self.batch(&msg)?;
            let late = write && !rep.last().is_some_and(|m| m.all_ok());
            for (k, (op, a)) in part.iter().enumerate() {
                let m = &rep[k];
                let n = a.len();
                let failed = if late && a.is_write() {
                    vec![true; n]
                } else if m.failed.is_empty() {
                    vec![false; n]
                } else {
                    m.failed.clone()
                };
                let bad = failed.iter().any(|f| *f);
                out[*op].data.extend_from_slice(&m.data);
                out[*op].failed.extend(failed);
                if bad {
                    failed_op = Some(*op);
                    break 'messages;
                }
            }
        }
        if let Some(i) = failed_op {
            out.truncate(i + 1);
            let (m, op) = (&mut out[i], &ops[i]);
            if !op.is_write() {
                m.data.resize(op.len(), 0);
            }
            m.failed.resize(op.len(), true);
        }
        Ok(out)
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

    fn ok(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
        (vec![0; data.len().div_ceil(8).max(1)], data.to_vec())
    }

    /// The access blocks' addresses of the batch message sent as transfer `i`.
    fn addresses(p: &mut Probe<ScriptedLink>, i: usize) -> Vec<u32> {
        let m = &p.link_mut().written[i].1;
        let n = u32::from_le_bytes(m[20..24].try_into().unwrap()) as usize;
        let mut o = 24;
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(u32::from_le_bytes(m[o + 6..o + 10].try_into().unwrap()));
            let write = m[o + 0x64] == 1;
            let len = u32::from_le_bytes(m[o + 0x77..o + 0x7b].try_into().unwrap()) as usize;
            o += 166 + if write { len } else { 0 };
        }
        out
    }

    #[test]
    fn reads_share_one_message() {
        let mut link = ScriptedLink::default();
        link.replies.push_back(memory::batch_reply(&[ok(&[1; 4]), ok(&[2; 2]), ok(&[3; 4])]));
        let mut p = Probe::new(link, false);
        let ops = [
            Access::Read { addr: 0x7000_0000, len: 4 },
            Access::Read { addr: 0x7000_0010, len: 2 },
            Access::Read { addr: 0xF003_6000, len: 4 },
        ];
        let m = p.access(&ops).unwrap();
        assert_eq!(m.iter().map(|m| m.data.clone()).collect::<Vec<_>>(), [vec![1; 4], vec![2; 2], vec![3; 4]]);
        assert!(m.iter().all(|m| m.all_ok() && m.failed.len() == m.data.len()));
        assert_eq!(p.link_mut().written.len(), 3, "one message: header, body, fetch");
        assert_eq!(addresses(&mut p, 1), [0x7000_0000, 0x7000_0010, 0xF003_6000]);
    }

    #[test]
    fn a_write_has_a_message_of_its_own_with_the_check_read() {
        let mut link = ScriptedLink::default();
        link.replies.push_back(memory::batch_reply(&[ok(&[1; 4])]));
        link.replies.push_back(memory::batch_reply(&[ok(&[]), ok(&[0; 4])]));
        link.replies.push_back(memory::batch_reply(&[ok(&[5; 4])]));
        let mut p = Probe::new(link, false);
        let d = [9u8; 4];
        let ops = [
            Access::Read { addr: 0x7000_0000, len: 4 },
            Access::Write { addr: 0x7000_0020, data: &d },
            Access::Read { addr: 0x7000_0030, len: 4 },
        ];
        let m = p.access(&ops).unwrap();
        assert_eq!(m.len(), 3);
        assert!(m.iter().all(|m| m.all_ok()));
        assert_eq!(m[2].data, [5; 4]);
        assert_eq!(addresses(&mut p, 1), [0x7000_0000]);
        assert_eq!(addresses(&mut p, 4), [0x7000_0020, WRITE_CHECK_ADDR]);
        assert_eq!(addresses(&mut p, 7), [0x7000_0030]);
    }

    #[test]
    fn a_failed_read_ends_the_results() {
        let mut link = ScriptedLink::default();
        link.replies.push_back(memory::batch_reply(&[ok(&[1; 4]), (vec![0x0e], vec![7, 0, 0, 0]), ok(&[3; 4])]));
        let mut p = Probe::new(link, false);
        let ops = [Access::Read { addr: 0, len: 4 }, Access::Read { addr: 4, len: 4 }, Access::Read { addr: 8, len: 4 }];
        let m = p.access(&ops).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[1].bytes_ok(4), 1);
        assert_eq!(m[1].data, [7, 0, 0, 0]);
    }

    #[test]
    fn a_late_bus_error_fails_the_write_and_nothing_runs_after_it() {
        let mut link = ScriptedLink::default();
        link.replies.push_back(memory::batch_reply(&[ok(&[]), (vec![0x0f], vec![0; 4])]));
        let mut p = Probe::new(link, false);
        let d = [0x80, 0x77, 0x22, 0x89];
        let ops = [Access::Write { addr: 0xF003_6140, data: &d }, Access::Read { addr: 0x7000_0000, len: 4 }];
        let m = p.access(&ops).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].failed, vec![true; 4]);
        assert_eq!(p.link_mut().written.len(), 3, "the read after the write was not sent");
    }

    #[test]
    fn long_ops_are_split_into_pieces_of_one_message() {
        let mut link = ScriptedLink::default();
        link.replies.push_back(memory::batch_reply(&[ok(&vec![0xaa; MAX_CHUNK]), ok(&[0xbb; 10])]));
        let mut p = Probe::new(link, false);
        let m = p.access(&[Access::Read { addr: 0x8000_0000, len: MAX_CHUNK + 10 }]).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].data.len(), MAX_CHUNK + 10);
        assert_eq!(m[0].data[MAX_CHUNK], 0xbb);
        assert_eq!(m[0].failed.len(), MAX_CHUNK + 10);
        assert_eq!(addresses(&mut p, 1), [0x8000_0000, 0x8000_0000 + MAX_CHUNK as u32]);
    }

    #[test]
    fn a_short_reply_is_an_error() {
        let mut link = ScriptedLink::default();
        link.replies.push_back(reply(&[1, 2]));
        let mut p = Probe::new(link, false);
        assert!(matches!(p.read_memory(0x7000_0000, 4), Err(Error::Protocol(_))));
    }
}
