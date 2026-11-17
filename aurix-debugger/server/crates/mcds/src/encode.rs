//! The inverse of [`crate::frame`]: builds trace RAM contents. Used by the tests and by the
//! simulated target, so both exercise the decoders with buffers of known content.

use crate::frame::SYMLEN;

/// Writes messages into one paragraph (LSB-first bit stream).
pub struct ParagraphWriter {
    buf: Vec<u8>,
    pos: usize,
}

/// Bits kept free at the end of a paragraph for the closing skip / end message.
const RESERVE: usize = 20;

impl ParagraphWriter {
    pub fn new(bytes: usize) -> Self {
        Self { buf: vec![0; bytes], pos: 0 }
    }
    pub fn left(&self) -> usize {
        self.buf.len() * 8 - self.pos
    }
    /// The paragraph bytes written so far (the rest is zero).
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }
    /// Bytes touched so far.
    pub fn used(&self) -> usize {
        self.pos.div_ceil(8)
    }
    fn put(&mut self, v: u128, n: usize) {
        for i in 0..n {
            if (v >> i) & 1 == 1 {
                let p = self.pos + i;
                self.buf[p / 8] |= 1 << (p % 8);
            }
        }
        self.pos += n;
    }
    /// Size in bits of a unit message with `bits` of data.
    pub fn unit_size(bits: u16) -> usize {
        let body = 8 + bits;
        match SYMLEN.iter().position(|&l| l >= body && l != 0) {
            Some(c) => 4 + SYMLEN[c] as usize,
            None => 12 + body as usize,
        }
    }
    /// True if a unit message with `bits` of data still fits (leaving room for the skip).
    pub fn fits(&self, bits: u16) -> bool {
        Self::unit_size(bits) + RESERVE <= self.left()
    }
    pub fn tick(&mut self, n: u32) {
        if n == 1 {
            self.put(0, 4);
        } else {
            let mut n = n;
            while n > 0 {
                let k = n.min(255);
                self.put(0xE, 4);
                self.put(k as u128, 8);
                n -= k;
            }
        }
    }
    pub fn error(&mut self, core: u8, count: u8) {
        self.put(0x1, 4);
        self.put(core as u128, 5);
        self.put(count as u128, 7);
    }
    /// A unit message, in the shortest fixed length that holds it (or the 0xF form).
    pub fn unit(&mut self, core: u8, tt: u8, data: u128, bits: u16) {
        let body = 8 + bits;
        match SYMLEN.iter().position(|&l| l >= body && l != 0) {
            Some(c) => {
                self.put(c as u128, 4);
                self.put(core as u128, 5);
                self.put(tt as u128, 3);
                self.put(data, bits as usize);
                self.put(0, (SYMLEN[c] - body) as usize);
            }
            None => {
                self.put(0xF, 4);
                self.put(body as u128, 8);
                self.put(core as u128, 5);
                self.put(tt as u128, 3);
                self.put(data, bits as usize);
            }
        }
    }
    /// Closes the paragraph with a skip message.
    pub fn finish_skip(mut self) -> Vec<u8> {
        if self.left() >= 20 {
            self.put(0xF, 4);
            self.put(8, 8);
            self.put(0, 8);
        }
        self.buf
    }
    /// Closes the paragraph with the end-of-trace marker.
    pub fn finish_end(mut self) -> Vec<u8> {
        if self.left() >= 12 {
            self.put(0xF, 4);
            self.put(0xFF, 8);
        }
        self.buf
    }
}

/// DTU write message with address and data (TT4, uncompressed): len1:4 dsize:2 data addr.
pub fn dtw(value: u32, size: u8, addr: u32) -> (u8, u128, u16) {
    let dsize = match size {
        1 => 0u128,
        2 => 1,
        _ => 2,
    };
    let nbits = (size as u16).min(4) * 8;
    let len1 = (nbits / 4) as u128; // LENGTH1 code: 4 bits per step
    let data = len1 | dsize << 4 | (value as u128) << 6 | (addr as u128) << (6 + nbits);
    (4, data, 6 + nbits + 32)
}

/// Splits a message list into paragraphs of `par_bytes`, like the hardware: each paragraph
/// closed with a skip, the last one with the end marker.
pub fn paragraphs(msgs: &[Rec], par_bytes: usize, count: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = ParagraphWriter::new(par_bytes);
    let mut done = 0;
    for m in msgs {
        let bits = match m {
            Rec::Unit { bits, .. } => *bits,
            _ => 8,
        };
        if !w.fits(bits) {
            out.extend(w.finish_skip());
            done += 1;
            if done == count {
                return out;
            }
            w = ParagraphWriter::new(par_bytes);
        }
        match *m {
            Rec::Tick(n) => w.tick(n),
            Rec::Error { core, count } => w.error(core, count),
            Rec::Unit { core, tt, data, bits } => w.unit(core, tt, data, bits),
        }
    }
    out.extend(w.finish_end());
    out
}

/// A message to encode.
#[derive(Debug, Clone, Copy)]
pub enum Rec {
    Tick(u32),
    Error { core: u8, count: u8 },
    Unit { core: u8, tt: u8, data: u128, bits: u16 },
}
