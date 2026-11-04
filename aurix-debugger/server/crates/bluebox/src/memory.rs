//! The memory service `//2///16/`.
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
//! 0x78  01 01 01           access flags; this is what the SDK's real-time direct access sends
//! 0x7b  N u32 LE           bytes of write data that follow (0 for a read)
//! 0x7f  data, N bytes
//! then  8 x 00, e8 03 00 01, zeros to the end
//! ```
//!
//! Every byte outside address, length, direction and data was identical
//! in the 1382 requests captured.
//!
//! The reply comes in one IN transfer, also for 8192 bytes, the largest
//! read tried:
//!
//! ```text
//! 0x00  u32 LE: reply length - 16 (unpadded)
//! 0x04  12 x 55
//! 0x10  08 00 00 00, then a counter
//! 0x27  B: bitmap length u32 LE = ceil(N / 8), or 0
//! 0x2b  bitmap, one bit per byte, set = that byte failed
//! 0x2b+B  N: data length u32 LE (0 for a write)
//! 0x2f+B  data
//! then  24 bytes
//! ```

use crate::{hex, message, Error, Result};

pub const SERVICE: &str = "//2///16/";

/// Largest block read in one request so far (8192 bytes came back in a
/// single reply). Longer accesses are split.
pub const MAX_CHUNK: usize = 8192;

const BODY_LEN: usize = 182;
const DATA: usize = 0x7f;

/// The service message for one access: a read of `len` bytes, or a write of `data`.
pub fn request(addr: u32, len: usize, data: Option<&[u8]>) -> Vec<u8> {
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
    b[0x78..0x7b].copy_from_slice(&[1, 1, 1]);
    b[0x7b..0x7f].copy_from_slice(&(n as u32).to_le_bytes());
    if let Some(d) = data {
        b[DATA..DATA + n].copy_from_slice(d);
    }
    let t = DATA + n + 8;
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
    if r.len() < 0x2f || r[4..16].iter().any(|b| *b != 0x55) {
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

fn u32le(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|x| u8::from_str_radix(x, 16).unwrap()).collect()
    }

    fn zeros(n: usize) -> String {
        vec!["00"; n].join(" ")
    }

    /// The 202 meaningful bytes winIDEA sent to read 4 bytes at 0x80000004.
    fn captured_read() -> Vec<u8> {
        unhex(&format!(
            "23 00 00 00 00 01 09 2f 2f 32 2f 2f 2f 31 36 2f
             00 00 00 b6 01 00 00 00 02 00 00 00 00 00 04 00
             00 80 00 00 00 00 04 00 00 00 00 00 00 00 01 00
             00 00 00 01 {z12}
             {z12} 01 00 04 00
             00 80 00 00 00 00 04 00 00 00 00 00 00 00 01 00
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
        let ours = request(0x8000_0004, 4, None);
        assert_eq!(ours.len(), 202);
        assert_eq!(ours, captured_read());
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
        assert_eq!(&m[0x8c..0x93], &[0x01, 0x01, 0x01, 0x40, 0x00, 0x00, 0x00]);
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
    fn garbage_is_a_protocol_error() {
        assert!(matches!(parse_reply(&[0u8; 80], 4), Err(Error::Protocol(_))));
    }
}
