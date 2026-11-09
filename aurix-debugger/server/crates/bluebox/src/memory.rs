//! The memory service `//2///14/` (`//2///16/` in winIDEA 9.21.356; same format).
//!
//! One request reads or writes one contiguous block. Its body is a fixed
//! 182-byte template plus the write data. Offsets are into the body, which
//! starts 20 bytes into the message:
//!
//! ```text
//! 0x00  01                 command
//! 0x04  02
//! 0x0a  address u64 LE     0x12  length u64 LE     (0x1a, 0x1f: 01)
//! 0x3a  address u64 LE     0x42  length u64 LE     (0x38, 0x4a, 0x4f: 01)  the same again
//! 0x68  01 = write, 00 = read
//! 0x78  3 bytes            access flags, see below
//! 0x7b  N u32 LE           bytes of write data that follow (0 for a read)
//! 0x7f  data, N bytes
//! then  8 x 00, e8 03 00 01, zeros to the end
//! ```
//!
//! Every byte outside address, length, direction, flags and data was
//! identical in the 1382 requests captured.
//!
//! # Access flags
//!
//! winIDEA sends three kinds, the same in 9.21.356 and 9.21.417 (one hour of
//! winIDEA 9.21.417 sessions, `cycle-417.jsonl`, `no-ap-417.jsonl`):
//!
//! - `00 01 00` ([`FLAGS_SFR`]): its own accesses to segment `0xF` (SCU, CBS,
//!   STM, ...), also on a running target.
//! - `00 01 01` ([`FLAGS_MEMORY`]): its own accesses to RAM and flash
//!   (segments `0x6`, `0x7`, `0x8` seen).
//! - `01 01 01` ([`FLAGS_REALTIME`]): the SDK's real-time direct access
//!   (`CDataController2`). **This driver sends it for everything.**
//!   `01 01 00` once, for winIDEA's TL1 setup.
//!
//! Measured on the TC377 board (iC5700 B, 9.21.417), with the application running
//! and `STM0_ACCEN0` = 0x4, `STM1_ACCEN0` = 0x40 (`examples/probe.rs --batch
//! ... --flags`):
//!
//! - Both kinds read the same values, also SFRs (`SCU_CHIPID`, CBS, DBGSR, STM).
//! - With `01 01 01` a write that the bus refuses (`STMx_OCS` here) shows as a
//!   failed access right after it, as the module docs below describe.
//! - With `00 01 00` the same write is acknowledged and the next access
//!   succeeds, but the register is unchanged: the probe hides the refusal.
//!   Results carry no bitmap (B = 0), and a failed access only `lead` = 1.
//!
//! So winIDEA's own kinds do not get past an application's access protection;
//! they only stop reporting it. That is why the traces suggested winIDEA's
//! `STMx_OCS` writes landed. The driver keeps the real-time kind, which
//! reports refused writes.
//!
//! The reply comes in one IN transfer, also for 8192 bytes, the largest
//! read tried:
//!
//! ```text
//! 0x00  u32 LE: reply length - 16 (unpadded)
//! 0x04  12 filler bytes: 55 on one probe, stale bytes on another (not checked)
//! 0x10  08 00 00 00, then a counter
//! 0x27  B: bitmap length u32 LE = ceil(N / 8), or 0
//! 0x2b  bitmap, one bit per byte, set = that byte failed
//! 0x2b+B  N: data length u32 LE (0 for a write)
//! 0x2f+B  data
//! then  28 bytes (24 in winIDEA 9.21.356's kernel)
//! ```
//!
//! # Batches
//!
//! One message can carry several accesses: the body is a u32 LE count (1 in
//! the single form above), then per access the single body without its count
//! and its last 12 bytes (166 + N bytes), then 12 zero bytes. The reply has
//! the count at 0x1f and the results packed one after the other from 0x23,
//! each `u32 0, B, bitmap[B], N, data[N]` and 28 bytes (24 with 9.21.356's
//! kernel: checked on every reply of the captures of both versions).
//!
//! Measured on a TC37x: the probe runs every access of a message, also after
//! one fails (only that one is flagged); a write's bus error shows on the
//! next access; each access costs about 0.45 ms inside the probe whatever the
//! debug clock, so a message of 64 small reads takes about 30 ms; a reply
//! over 61,440 bytes comes back as 16 bytes of nothing.

use crate::{hex, message, Error, Result};

pub const SERVICE: &str = "//2///14/";

/// Largest block read in one request so far (8192 bytes came back in a
/// single reply). Longer accesses are split.
pub const MAX_CHUNK: usize = 8192;

const BODY_LEN: usize = 182;
const DATA: usize = 0x7f;

/// Accesses per batch message. Each costs the same in the probe, so larger
/// messages gain nothing (the fixed cost is about 2% at 64) and only hold up
/// what comes next.
pub const MAX_BATCH: usize = 64;
/// Largest reply a batch is planned for; the probe fails above 61,440 bytes.
pub const MAX_BATCH_REPLY: usize = 60_000;
/// Largest batch request; 256 reads (42.5 KB) were measured.
pub const MAX_BATCH_REQUEST: usize = 48 * 1024;

/// Bytes of a message before the first access block: header, count.
const REQUEST_HEAD: usize = 20 + 4;
/// Bytes of a batch body after the last block.
const REQUEST_TAIL: usize = 12;
/// One access block without its data.
const BLOCK_LEN: usize = BODY_LEN - 4 - REQUEST_TAIL;
/// Reply bytes before the first result.
const REPLY_HEAD: usize = 0x23;
/// Bytes of a result besides its bitmap and data (9.21.417; 36 with 9.21.356).
const RESULT_LEN: usize = 40;

/// winIDEA's own access to special function registers (segment `0xF`).
pub const FLAGS_SFR: [u8; 3] = [0x00, 0x01, 0x00];
/// winIDEA's own access to RAM and flash.
pub const FLAGS_MEMORY: [u8; 3] = [0x00, 0x01, 0x01];
/// The SDK's real-time direct access.
pub const FLAGS_REALTIME: [u8; 3] = [0x01, 0x01, 0x01];

/// The access flags winIDEA itself uses at `addr` (see the module docs):
/// [`FLAGS_SFR`] in segment `0xF`, [`FLAGS_MEMORY`] elsewhere. For comparison
/// with captures; the driver sends [`FLAGS_REALTIME`].
pub fn winidea_flags(addr: u32) -> [u8; 3] {
    if addr >> 28 == 0xF {
        FLAGS_SFR
    } else {
        FLAGS_MEMORY
    }
}

/// One access of a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access<'a> {
    Read { addr: u32, len: usize },
    Write { addr: u32, data: &'a [u8] },
}

impl Access<'_> {
    pub fn addr(&self) -> u32 {
        match *self {
            Access::Read { addr, .. } | Access::Write { addr, .. } => addr,
        }
    }

    pub fn len(&self) -> usize {
        match *self {
            Access::Read { len, .. } => len,
            Access::Write { data, .. } => data.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_write(&self) -> bool {
        matches!(self, Access::Write { .. })
    }

    /// Bytes this access adds to a batch request.
    fn request_size(&self) -> usize {
        BLOCK_LEN + if self.is_write() { self.len() } else { 0 }
    }

    /// Bytes this access adds to the reply, at most: the bitmap, and the data of a read.
    fn reply_size(&self) -> usize {
        RESULT_LEN + self.len().div_ceil(8) + if self.is_write() { 0 } else { self.len() }
    }
}

/// The read that follows every write to catch its late bus error (see
/// [`crate::probe::WRITE_CHECK_ADDR`]).
pub const CHECK: Access<'static> = Access::Read { addr: crate::probe::WRITE_CHECK_ADDR, len: 4 };

/// Splits `acc` (each at most [`MAX_CHUNK`]) into messages, in order. The
/// probe runs every access of a message, also after a failed one, so a write
/// gets a message of its own, to which the caller appends [`CHECK`]: it then
/// runs only when everything before it succeeded, and nothing runs after it
/// before its outcome is known. Reads share messages of at most [`MAX_BATCH`]
/// accesses, [`MAX_BATCH_REQUEST`] request and [`MAX_BATCH_REPLY`] reply bytes.
pub fn plan(acc: &[Access]) -> Vec<std::ops::Range<usize>> {
    const EMPTY: (usize, usize, usize) = (0, REQUEST_HEAD + REQUEST_TAIL, REPLY_HEAD);
    let mut out = Vec::new();
    let mut start = 0;
    let (mut n, mut rq, mut rsp) = EMPTY;
    for (i, a) in acc.iter().enumerate() {
        let (mut an, mut arq, mut arsp) = (1, a.request_size(), a.reply_size());
        if a.is_write() {
            an += 1;
            arq += CHECK.request_size();
            arsp += CHECK.reply_size();
        }
        let full = n + an > MAX_BATCH || rq + arq > MAX_BATCH_REQUEST || rsp + arsp > MAX_BATCH_REPLY;
        if i > start && (a.is_write() || full) {
            out.push(start..i);
            start = i;
            (n, rq, rsp) = EMPTY;
        }
        n += an;
        rq += arq;
        rsp += arsp;
        if a.is_write() {
            out.push(start..i + 1);
            start = i + 1;
            (n, rq, rsp) = EMPTY;
        }
    }
    if start < acc.len() {
        out.push(start..acc.len());
    }
    out
}

/// The service message for several accesses, run in order.
pub fn batch_request(acc: &[Access]) -> Vec<u8> {
    let mut b = (acc.len() as u32).to_le_bytes().to_vec();
    for a in acc {
        let one = match *a {
            Access::Read { addr, len } => body(addr, len, None),
            Access::Write { addr, data } => body(addr, data.len(), Some(data)),
        };
        b.extend_from_slice(&one[4..one.len() - REQUEST_TAIL]);
    }
    b.extend_from_slice(&[0; REQUEST_TAIL]);
    message::encode(message::KIND, SERVICE, &b)
}

/// The service message for one access: a read of `len` bytes, or a write of `data`.
pub fn request(addr: u32, len: usize, data: Option<&[u8]>) -> Vec<u8> {
    request_with(addr, len, data, FLAGS_REALTIME)
}

/// [`request`] with other access flags (see the module docs).
pub fn request_with(addr: u32, len: usize, data: Option<&[u8]>, flags: [u8; 3]) -> Vec<u8> {
    message::encode(message::KIND, SERVICE, &body_with(addr, len, data, flags))
}

fn body(addr: u32, len: usize, data: Option<&[u8]>) -> Vec<u8> {
    body_with(addr, len, data, FLAGS_REALTIME)
}

fn body_with(addr: u32, len: usize, data: Option<&[u8]>, flags: [u8; 3]) -> Vec<u8> {
    let n = data.map_or(0, |d| d.len());
    if let Some(d) = data {
        assert_eq!(d.len(), len, "a write's length is its data's");
    }
    let mut b = vec![0u8; BODY_LEN + n];
    b[0x00] = 0x01;
    b[0x04] = 0x02;
    for (a, l) in [(0x0a, 0x12), (0x3a, 0x42)] {
        b[a..a + 8].copy_from_slice(&(addr as u64).to_le_bytes());
        b[l..l + 8].copy_from_slice(&(len as u64).to_le_bytes());
    }
    for o in [0x1a, 0x1f, 0x38, 0x4a, 0x4f] {
        b[o] = 0x01;
    }
    b[0x68] = data.is_some() as u8;
    b[0x78..0x7b].copy_from_slice(&flags);
    b[0x7b..0x7f].copy_from_slice(&(n as u32).to_le_bytes());
    if let Some(d) = data {
        b[DATA..DATA + n].copy_from_slice(d);
    }
    let t = DATA + n + 8;
    b[t..t + 4].copy_from_slice(&[0xe8, 0x03, 0x00, 0x01]);
    b
}

/// The memory of a core the probe reaches through that core's own debug interface instead of the
/// host bus -- the TC3xx standby controller (SCR, core 3) -- as winIDEA 9.21.417 reads it
/// (`scr2-417.jsonl`). The same service, with a shorter body:
///
/// ```text
/// 0x00  01                 count
/// 0x04  01                 (02 for the host bus)
/// 0x09  memory space       00 code, 01 data (winIDEA's memory areas)
/// 0x0a  address u64 LE     0x12  length u64 LE
/// 0x1a  01   0x1b  core    0x23  ff
/// 0x38  01 = write, 00 = read
/// 0x48  3 bytes            access flags (winIDEA: 01 01 01, as [`FLAGS_REALTIME`])
/// 0x4b  N u32 LE           bytes of write data that follow (0 for a read)
/// 0x4f  data, then 8 x 00, e8 03 00 01, zeros to 134 bytes
/// ```
///
/// The reply has the host-bus format ([`parse_reply`]).
pub const CORE_BODY_LEN: usize = 134;
const CORE_DATA: usize = 0x4f;

/// A read of `len` bytes at `addr` in memory space `space` of core `core` (see [`CORE_BODY_LEN`]).
pub fn core_read_request(core: u8, space: u8, addr: u32, len: usize) -> Vec<u8> {
    core_request(core, space, addr, len, None)
}

/// A read of `len` bytes, or a write of `data`, at `addr` in memory space `space` of core `core`
/// (see [`CORE_BODY_LEN`]; a write sets 0x38 to `01` and carries its data, checked against
/// winIDEA's SCR writes in `scr3-417.jsonl`).
pub fn core_request(core: u8, space: u8, addr: u32, len: usize, data: Option<&[u8]>) -> Vec<u8> {
    let n = data.map_or(0, |d| d.len());
    if let Some(d) = data {
        assert_eq!(d.len(), len, "a write's length is its data's");
    }
    let mut b = vec![0u8; CORE_BODY_LEN + n];
    b[0x00] = 0x01;
    b[0x04] = 0x01;
    b[0x09] = space;
    b[0x0a..0x12].copy_from_slice(&(addr as u64).to_le_bytes());
    b[0x12..0x1a].copy_from_slice(&(len as u64).to_le_bytes());
    b[0x1a] = 0x01;
    b[0x1b] = core;
    b[0x23] = 0xff;
    b[0x38] = data.is_some() as u8;
    b[0x48..0x4b].copy_from_slice(&FLAGS_REALTIME);
    b[0x4b..0x4f].copy_from_slice(&(n as u32).to_le_bytes());
    if let Some(d) = data {
        b[CORE_DATA..CORE_DATA + n].copy_from_slice(d);
    }
    let t = CORE_DATA + n + 8;
    b[t..t + 4].copy_from_slice(&[0xe8, 0x03, 0x00, 0x01]);
    message::encode(message::KIND, SERVICE, &b)
}

/// A parsed memory reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemReply {
    /// Data read; empty for a write.
    pub data: Vec<u8>,
    /// One flag per byte of the access: `true` = that byte failed. Empty
    /// when the reply carries no bitmap.
    pub failed: Vec<bool>,
}

impl MemReply {
    /// Bytes that succeeded, counted from the start of the access.
    pub fn bytes_ok(&self, len: usize) -> usize {
        if self.failed.is_empty() {
            return len;
        }
        self.failed.iter().take(len).take_while(|f| !**f).count()
    }

    pub fn all_ok(&self) -> bool {
        !self.failed.iter().any(|f| *f)
    }
}

/// Parses the reply to a [`request`] of `len` bytes.
pub fn parse_reply(r: &[u8], len: usize) -> Result<MemReply> {
    let bad = |why: &str| Error::Protocol(format!("memory reply: {why}: {}", hex(r, 64)));
    if r.len() < 0x2f || !message::is_reply(r) {
        return Err(bad("no reply header"));
    }
    let total = u32le(r, 0) as usize + 16;
    if total > r.len() {
        return Err(bad("shorter than its length field"));
    }
    let nb = u32le(r, 0x27) as usize;
    let bitmap = r.get(0x2b..0x2b + nb).ok_or_else(|| bad("bitmap past the end"))?;
    let failed: Vec<bool> = if nb == 0 {
        Vec::new()
    } else {
        (0..len).map(|i| bitmap.get(i / 8).is_some_and(|b| b >> (i % 8) & 1 != 0)).collect()
    };
    let p = 0x2b + nb;
    let n = u32le(r, p) as usize;
    let data = r.get(p + 4..p + 4 + n).ok_or_else(|| bad("data past the end"))?.to_vec();
    Ok(MemReply { data, failed })
}

/// Parses the reply to a [`batch_request`] whose accesses have the lengths `lens`.
pub fn parse_batch_reply(r: &[u8], lens: &[usize]) -> Result<Vec<MemReply>> {
    let bad = |why: String| Error::Protocol(format!("batch reply: {why}: {}", hex(r, 64)));
    if r.len() < REPLY_HEAD || !message::is_reply(r) {
        return Err(bad("no reply header".into()));
    }
    if u32le(r, 0) as usize + 16 > r.len() {
        return Err(bad("shorter than its length field".into()));
    }
    let count = u32le(r, 0x1f) as usize;
    if count != lens.len() {
        return Err(bad(format!("{count} results for {} accesses", lens.len())));
    }
    let field = |at: usize| r.get(at..at + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()));
    let mut out = Vec::with_capacity(count);
    let mut o = REPLY_HEAD;
    for (k, &len) in lens.iter().enumerate() {
        let (Some(lead), Some(nb)) = (field(o), field(o + 4)) else {
            return Err(bad(format!("result {k} past the end")));
        };
        let nb = nb as usize;
        let bitmap = r.get(o + 8..o + 8 + nb).ok_or_else(|| bad(format!("result {k}: bitmap past the end")))?;
        let n = field(o + 8 + nb).ok_or_else(|| bad(format!("result {k} past the end")))? as usize;
        let data = r.get(o + 12 + nb..o + 12 + nb + n).ok_or_else(|| bad(format!("result {k}: data past the end")))?;
        let failed: Vec<bool> = if lead != 0 {
            // With other access flags the probe reports a failure here, without a bitmap.
            vec![true; len]
        } else if nb == 0 {
            Vec::new()
        } else {
            (0..len).map(|i| bitmap.get(i / 8).is_some_and(|b| b >> (i % 8) & 1 != 0)).collect()
        };
        out.push(MemReply { data: data.to_vec(), failed });
        o += RESULT_LEN + nb + n;
    }
    Ok(out)
}

/// A batch reply in the measured layout, with `results` of (bitmap, data).
#[cfg(test)]
pub(crate) fn batch_reply(results: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut r = vec![0u8; REPLY_HEAD];
    r[4..16].fill(0x55);
    r[0x10] = 8;
    r[0x1f..0x23].copy_from_slice(&(results.len() as u32).to_le_bytes());
    for (bm, d) in results {
        r.extend_from_slice(&[0; 4]);
        r.extend_from_slice(&(bm.len() as u32).to_le_bytes());
        r.extend_from_slice(bm);
        r.extend_from_slice(&(d.len() as u32).to_le_bytes());
        r.extend_from_slice(d);
        r.extend_from_slice(&[0; RESULT_LEN - 12]);
    }
    let n = (r.len() - 16) as u32;
    r[0..4].copy_from_slice(&n.to_le_bytes());
    r
}

fn u32le(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scr_read_is_what_winidea_sends() {
        // winIDEA reading 8 bytes of SCR code at 0x0079 (scr2-417.jsonl, "read area 0 0x79 8").
        let msg = "230000000001092f2f322f2f2f31342f000000860100000001000000000079000000000000000800000000000000010300000000000000ff000000000000000000000000000000000000000000000000000000000000000000000000010101000000000000000000000000e80300010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
        let rsp = "44000000feeffbf7ecfffffffb7f97ff080000003f01000000000000000001010000000000000001000000000800000075d420120537e5d500000000000000000000000000000000280500000000000000000000";
        let unhex = |s: &str| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>();
        let (ours, theirs) = (core_read_request(3, 0, 0x79, 8), unhex(msg));
        // The captured transfer carries padding after the message.
        assert_eq!(ours[..], theirs[..ours.len()]);
        assert!(theirs[ours.len()..].iter().all(|&b| b == 0));
        let r = parse_reply(&unhex(rsp), 8).unwrap();
        assert_eq!(r.data, vec![0x75, 0xd4, 0x20, 0x12, 0x05, 0x37, 0xe5, 0xd5]);
        assert!(r.failed.iter().all(|f| !f), "{r:?}");
    }

    fn unhex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|x| u8::from_str_radix(x, 16).unwrap()).collect()
    }

    fn zeros(n: usize) -> String {
        vec!["00"; n].join(" ")
    }

    /// The 202 meaningful bytes winIDEA 9.21.417 sent to read 4 bytes at 0x70000100 (`ic5000-417.jsonl`).
    fn captured_read() -> Vec<u8> {
        unhex(&format!(
            "23 00 00 00 00 01 09 2f 2f 32 2f 2f 2f 31 34 2f
             00 00 00 b6 01 00 00 00 02 00 00 00 00 00 00 01
             00 70 00 00 00 00 04 00 00 00 00 00 00 00 01 00
             00 00 00 01 {z12}
             {z12} 01 00 00 01
             00 70 00 00 00 00 04 00 00 00 00 00 00 00 01 00
             00 00 00 01 {z12}
             {z16}
             {z12} 01 01 01 00
             {z11} e8 03 00 01 00
             {z16} {z16} {z10}",
            z10 = zeros(10),
            z11 = zeros(11),
            z12 = zeros(12),
            z16 = zeros(16)
        ))
    }

    #[test]
    fn read_request_is_byte_identical_to_winidea() {
        let ours = request(0x7000_0100, 4, None);
        assert_eq!(ours.len(), 202);
        assert_eq!(ours, captured_read());
    }

    /// A captured message: its meaningful bytes, then zeros to `len`.
    fn captured(s: &str, len: usize) -> Vec<u8> {
        let mut m: Vec<u8> = (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect();
        assert!(m.len() <= len);
        m.resize(len, 0);
        m
    }

    /// Requests winIDEA 9.21.417 sent for its own accesses (`cycle-417.jsonl`), whole messages.
    #[test]
    fn requests_are_byte_identical_to_winideas_own() {
        // Read of STM0_TIM0 (an SFR), 4 bytes.
        let stm = concat!(
            "230000000001092f2f322f2f2f31342f000000b601000000020000000000101000f000000000040000000000000001\
             00000000010000000000000000000000000000000000000000000000000100101000f0000000000400000000000000\
             0100000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000\
             0100000000000000000000000000e8030001"
        );
        assert_eq!(request_with(0xF000_1010, 4, None, winidea_flags(0xF000_1010)), captured(stm, 202));
        // Read of RAM, 128 bytes.
        let ram = concat!(
            "230000000001092f2f322f2f2f31342f000000b6010000000200000000000099007000000000800000000000000001\
             00000000010000000000000000000000000000000000000000000000000100009900700000000080000000000000\
             0001000000000100000000000000000000000000000000000000000000000000000000000000000000000000000000\
             000101000000000000000000000000e8030001"
        );
        assert_eq!(request_with(0x7000_9900, 128, None, winidea_flags(0x7000_9900)), captured(ram, 202));
        // Read of program flash, 512 bytes.
        let flash = concat!(
            "230000000001092f2f322f2f2f31342f000000b60100000002000000000000800a8000000000000200000000000001",
            "0000000001000000000000000000000000000000000000000000000000010000800a80000000000002000000000000",
            "0100000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            "0101000000000000000000000000e8030001",
        );
        assert_eq!(request_with(0x800A_8000, 512, None, winidea_flags(0x800A_8000)), captured(flash, 202));
        // Write of STM0_OCS (refused by the application's STM0_ACCEN0, but acknowledged with these flags).
        let ocs = concat!(
            "230000000001092f2f322f2f2f31342f000000ba01000000020000000000e81000f000000000040000000000000001\
             00000000010000000000000000000000000000000000000000000000000100e81000f0000000000400000000000000\
             0100000000010000000000000000000000000000000000000000000000000100000000000000000000000000000000\
             010004000000000000120000000000000000e8030001"
        );
        assert_eq!(request_with(0xF000_10E8, 4, Some(&[0, 0, 0, 0x12]), winidea_flags(0xF000_10E8)), captured(ocs, 206));
    }

    #[test]
    fn the_driver_sends_the_real_time_flags_everywhere() {
        for addr in [0xF881_FD00, 0xF000_10E8, 0x7000_0000, 0x8000_0000, 0xD000_0000] {
            assert_eq!(&request(addr, 4, None)[0x8c..0x8f], &FLAGS_REALTIME);
        }
        assert_eq!(winidea_flags(0xF000_0494), FLAGS_SFR);
        assert_eq!(winidea_flags(0x6000_0000), FLAGS_MEMORY);
    }

    #[test]
    fn write_request_matches_the_captured_64_byte_write() {
        let data: Vec<u8> = (0x10..0x50).collect();
        let m = request(0x7000_0000, 64, Some(&data));
        assert_eq!(m.len(), 202 + 64);
        // Captured: header, direction flag, data count and data, the trailer after it.
        assert_eq!(&m[0x10..0x15], &[0x00, 0x00, 0x00, 0xf6, 0x01]);
        assert_eq!(&m[0x1e..0x22], &[0x00, 0x00, 0x00, 0x70]);
        assert_eq!(m[0x7c], 0x01);
        assert_eq!(&m[0x8c..0x8f], &FLAGS_REALTIME);
        assert_eq!(&m[0x8f..0x93], &[0x40, 0x00, 0x00, 0x00]);
        assert_eq!(&m[0x93..0xd3], &data[..]);
        assert_eq!(&m[0xd3..0xdf], &unhex("00 00 00 00 00 00 00 00 e8 03 00 01"));
    }

    #[test]
    fn a_16_byte_flash_read_reply_parses() {
        let r = unhex(&format!(
            "49 00 00 00 {u}
             08 00 00 00 44 01 00 00 00 00 00 00 00 00 01 01
             00 00 00 00 00 00 00 02 00 00 00 00 00 10 00 00
             00 e0 58 65 f1 56 e6 e8 1f d3 99 ff 6e 4e 3b 5b
             67 {z15} {z12}",
            u = vec!["55"; 12].join(" "),
            z15 = zeros(15),
            z12 = zeros(12)
        ));
        let m = parse_reply(&r, 16).unwrap();
        assert_eq!(m.data, unhex("e0 58 65 f1 56 e6 e8 1f d3 99 ff 6e 4e 3b 5b 67"));
        assert!(m.all_ok());
        assert_eq!(m.bytes_ok(16), 16);
    }

    #[test]
    fn failed_bytes_come_from_the_bitmap() {
        // Erased flash at 0x80000100: bitmap 0f, data zeros.
        let r = unhex(&format!(
            "3c 00 00 00 {u}
             08 00 00 00 37 01 00 00 00 00 00 00 00 00 01 01
             00 00 00 00 00 00 00 01 00 00 00 0f 04 00 00 00
             00 00 00 00 {z24}",
            u = vec!["55"; 12].join(" "),
            z24 = zeros(24)
        ));
        assert_eq!(r.len(), 76);
        let m = parse_reply(&r, 4).unwrap();
        assert_eq!(m.data, vec![0; 4]);
        assert_eq!(m.failed, vec![true; 4]);
        assert_eq!(m.bytes_ok(4), 0);
    }

    #[test]
    fn a_write_reply_has_a_bitmap_and_no_data() {
        let r = unhex(&format!(
            "3f 00 00 00 {u}
             08 00 00 00 3a 01 00 00 00 00 00 00 00 00 01 01
             00 00 00 00 00 00 00 08 {z40}",
            u = vec!["55"; 12].join(" "),
            z40 = zeros(40)
        ));
        assert_eq!(r.len(), 80);
        let m = parse_reply(&r, 64).unwrap();
        assert!(m.data.is_empty());
        assert!(m.all_ok());
    }

    #[test]
    fn a_batch_of_one_is_the_single_request() {
        let data = [1, 2, 3, 4];
        assert_eq!(batch_request(&[Access::Read { addr: 0x7000_0100, len: 4 }]), captured_read());
        assert_eq!(
            batch_request(&[Access::Write { addr: 0x7000_0000, data: &data }]),
            request(0x7000_0000, 4, Some(&data))
        );
    }

    #[test]
    fn a_batch_request_has_a_count_and_packed_blocks() {
        let data = [9u8; 3];
        let m = batch_request(&[
            Access::Read { addr: 0x7000_0000, len: 4 },
            Access::Write { addr: 0x7000_0010, data: &data },
            Access::Read { addr: 0xF000_1010, len: 4 },
        ]);
        assert_eq!(m.len(), 20 + 4 + 3 * BLOCK_LEN + 3 + 12);
        assert_eq!(&m[20..24], &3u32.to_le_bytes());
        // Each block starts with 02 and has the address 6 bytes in.
        for (o, addr) in [(24, 0x7000_0000u32), (24 + BLOCK_LEN, 0x7000_0010), (24 + 2 * BLOCK_LEN + 3, 0xF000_1010)] {
            assert_eq!(m[o], 0x02);
            assert_eq!(&m[o + 6..o + 10], &addr.to_le_bytes());
        }
        assert_eq!(m[24 + BLOCK_LEN + 0x64], 1, "the second block is the write");
        assert_eq!(&m[24 + BLOCK_LEN + 0x7b..24 + BLOCK_LEN + 0x7e], &data);
        assert!(m[m.len() - 12..].iter().all(|b| *b == 0));
    }

    #[test]
    fn batch_results_are_parsed_in_order() {
        let r = batch_reply(&[(vec![0], vec![1, 2, 3, 4]), (vec![0x0c], vec![0, 0, 0, 0]), (vec![0], vec![])]);
        let m = parse_batch_reply(&r, &[4, 4, 4]).unwrap();
        assert_eq!(m[0].data, [1, 2, 3, 4]);
        assert!(m[0].all_ok());
        assert_eq!(m[1].failed, [false, false, true, true]);
        assert_eq!(m[1].bytes_ok(4), 2);
        assert!(m[2].data.is_empty() && m[2].all_ok());
        assert!(matches!(parse_batch_reply(&r, &[4, 4]), Err(Error::Protocol(_))));
        assert!(matches!(parse_batch_reply(&r[..r.len() - 30], &[4, 4, 4]), Err(Error::Protocol(_))));
    }

    #[test]
    fn plan_gives_each_write_a_message_of_its_own() {
        let d = [0u8; 4];
        let r = Access::Read { addr: 0, len: 4 };
        let w = Access::Write { addr: 0, data: &d };
        assert_eq!(plan(&[r, r, r, w, r, r]), vec![0..3, 3..4, 4..6]);
        assert_eq!(plan(&[w, w, r]), vec![0..1, 1..2, 2..3]);
        assert!(plan(&[]).is_empty());
    }

    #[test]
    fn plan_keeps_within_the_access_count_and_the_reply_window() {
        let r = Access::Read { addr: 0, len: 4 };
        assert_eq!(plan(&[r; 130]), vec![0..64, 64..128, 128..130]);
        // 8 KiB reads: 9,252 reply bytes each, so six fit (55.5 KB, measured) and seven do not.
        let big = Access::Read { addr: 0, len: MAX_CHUNK };
        assert_eq!(plan(&[big; 13]), vec![0..6, 6..12, 12..13]);
    }

    #[test]
    fn a_captured_batch_reply_of_three_writes_parses() {
        // winIDEA 9.21.417 writing the trigger setup after a reset (ic5000-417.jsonl):
        // three results of 40 bytes, no bitmap, no data, a counter in each tail.
        let r: Vec<u8> = (0..312).step_by(2).map(|i| u8::from_str_radix(&CAPTURED_3_WRITES[i..i + 2], 16).unwrap()).collect();
        assert_eq!(r.len(), 156);
        let out = parse_batch_reply(&r, &[4, 4, 4]).unwrap();
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|m| m.data.is_empty() && m.failed.is_empty()));
    }

    const CAPTURED_3_WRITES: &str = concat!(
        "8b000000ffbffffdf67fdffffdfef7ef0800000086010000000000000000010300000000000000000000000000",
        "000000000000000000000000000000000000990000000000000000000000000000000000000000000000000000",
        "000000000000000000000000008d0000000000000000000000000000000000000000000000000000000000000000",
        "000000000000008c000000000000000000000000"
    );

    #[test]
    fn a_reply_with_another_probes_filler_parses() {
        // iC5700 B: a 4-byte read of 0xf0000494, filler `ea eb da 77 ...`
        // where iC5700 A sends `55` (no-active-probe.jsonl).
        let captured = concat!(
            "3b000000eaebda77a0b3fcfff27f16bf08000000360100000000000000000101000000000000",
            "0000000000040000003f0000b000000000000000000000000000000000000000000000000000",
        );
        let r: Vec<u8> = (0..captured.len()).step_by(2).map(|i| u8::from_str_radix(&captured[i..i + 2], 16).unwrap()).collect();
        assert_eq!(r.len(), 76);
        let m = parse_reply(&r, 4).unwrap();
        assert_eq!(m.data, [0x3f, 0x00, 0x00, 0xb0]);
        assert!(m.all_ok());
        let b = batch_reply(&[(vec![0], vec![1, 2, 3, 4])]);
        let mut other = b.clone();
        other[4..16].copy_from_slice(&r[4..16]);
        assert_eq!(parse_batch_reply(&other, &[4]).unwrap(), parse_batch_reply(&b, &[4]).unwrap());
    }

    #[test]
    fn a_refused_request_is_no_reply() {
        // What came back for a batch whose reply would pass 61,440 bytes: 16 bytes.
        let r = unhex("00 00 00 00 55 55 55 55 55 55 55 55 55 55 55 55");
        assert!(matches!(parse_batch_reply(&r, &[4]), Err(Error::Protocol(_))));
        assert!(matches!(parse_reply(&r, 4), Err(Error::Protocol(_))));
    }

    #[test]
    fn garbage_is_a_protocol_error() {
        assert!(matches!(parse_reply(&[0u8; 80], 4), Err(Error::Protocol(_))));
    }
}
