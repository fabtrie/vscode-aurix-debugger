//! Trace RAM framing: paragraphs of LSB-first bit streams, split into messages.
//!
//! A message is a 4-bit code, then:
//! - `0x0` tick (one time step); `0xE` multick + count:8 (0 = alignment filler);
//! - `0x1` error: core:5 err_cnt:7 (messages lost);
//! - `0xF` length:8 (0xFF = end of trace), then a body of that length;
//! - `0x2..0xD` a body of a fixed length ([`SYMLEN`]).
//!
//! A body is core:5 tt:3 data; core 0 is a skip message padding the rest of the paragraph.

/// Body length in bits for the codes 0x2..0xD.
pub const SYMLEN: [u16; 16] = [0, 0, 12, 16, 20, 24, 32, 40, 44, 56, 64, 76, 92, 112, 0, 0];

/// Core IDs of the trace sources (TC3xx).
pub const CORE_DTU0: u8 = 0x09;
pub const CORE_DTU1: u8 = 0x0B;
pub const CORE_PTU: u8 = 0x0C;
pub const CORE_TSU: u8 = 0x0E;
pub const CORE_WTU: u8 = 0x0F;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg {
    Tick(u32),
    /// Messages were lost (`count` per the hardware's error counter).
    Error { core: u8, count: u8 },
    /// Rest of the paragraph is padding.
    Skip,
    /// End of the recorded data.
    End,
    /// An invalid length code: the paragraph is damaged from here (e.g. overwritten while read).
    Bad { code: u8 },
    /// A message of a trace unit.
    Unit { core: u8, tt: u8, data: u128, bits: u16 },
}

/// LSB-first bit reader over a byte slice.
pub struct Bits<'a> {
    data: &'a [u8],
    pub pos: usize,
    end: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, end: data.len() * 8 }
    }
    pub fn left(&self) -> usize {
        self.end - self.pos
    }
    /// Takes `n` <= 128 bits.
    pub fn take(&mut self, n: usize) -> u128 {
        debug_assert!(n <= 128 && n <= self.left());
        let mut v: u128 = 0;
        let mut got = 0usize;
        while got < n {
            let p = self.pos + got;
            let byte = self.data[p / 8] as u128;
            let off = p % 8;
            let take = (8 - off).min(n - got);
            let bits = (byte >> off) & ((1u128 << take) - 1);
            v |= bits << got;
            got += take;
        }
        self.pos += n;
        v
    }
}

/// The messages of one paragraph with their bit positions.
pub fn messages(par: &[u8]) -> Vec<(usize, Msg)> {
    let mut out = Vec::new();
    let mut b = Bits::new(par);
    while b.left() >= 4 {
        let at = b.pos;
        let code = b.take(4) as u8;
        match code {
            0x0 => out.push((at, Msg::Tick(1))),
            0xE => {
                if b.left() < 8 {
                    break;
                }
                let n = b.take(8) as u32;
                if n != 0 {
                    out.push((at, Msg::Tick(n)));
                }
            }
            0x1 => {
                if b.left() < 12 {
                    break;
                }
                let core = b.take(5) as u8;
                let count = b.take(7) as u8;
                out.push((at, Msg::Error { core, count }));
            }
            _ => {
                let len = if code == 0xF {
                    if b.left() < 8 {
                        break;
                    }
                    let l = b.take(8) as u16;
                    if l == 0xFF {
                        out.push((at, Msg::End));
                        break;
                    }
                    l
                } else {
                    let l = SYMLEN[code as usize];
                    if l == 0 {
                        out.push((at, Msg::Bad { code }));
                        break;
                    }
                    l
                };
                if len as usize > b.left() {
                    break; // truncated at the paragraph end
                }
                if len < 8 {
                    b.take(len as usize);
                    continue;
                }
                let core = b.take(5) as u8;
                if core == 0 {
                    out.push((at, Msg::Skip));
                    break;
                }
                let tt = b.take(3) as u8;
                let bits = len - 8;
                if bits > 128 {
                    // No unit sends that much: damaged data (e.g. overwritten while it was read).
                    out.push((at, Msg::Bad { code }));
                    break;
                }
                let data = b.take(bits as usize);
                out.push((at, Msg::Unit { core, tt, data, bits }));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::ParagraphWriter;

    #[test]
    fn oversized_body_is_damage_not_a_panic() {
        // code 0xF, length 0xF0 (232 bits: more than any unit sends), then ones.
        let mut par = vec![0xFFu8; 64];
        par[0] = 0x0F; // code F, low nibble of the length 0
        par[1] = 0x0F; // length high nibble F -> 0xF0
        let m = messages(&par);
        assert!(matches!(m.last(), Some((_, Msg::Bad { .. }))), "{m:?}");
    }

    #[test]
    fn roundtrip() {
        let mut w = ParagraphWriter::new(1024);
        w.tick(1);
        w.tick(5);
        w.unit(CORE_PTU, 0, 0x8021_D25A, 32);
        w.error(CORE_DTU0, 3);
        w.unit(CORE_TSU, 1, 0x28, 8);
        let p = w.finish_skip();
        let m: Vec<Msg> = messages(&p).into_iter().map(|x| x.1).collect();
        assert_eq!(
            m,
            vec![
                Msg::Tick(1),
                Msg::Tick(5),
                Msg::Unit { core: CORE_PTU, tt: 0, data: 0x8021_D25A, bits: 32 },
                Msg::Error { core: CORE_DTU0, count: 3 },
                Msg::Unit { core: CORE_TSU, tt: 1, data: 0x28, bits: 8 },
                Msg::Skip,
            ]
        );
    }
}
