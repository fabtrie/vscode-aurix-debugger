//! Trace RAM layout: paragraph order of a stopped recording, and incremental reading of a
//! running one (streaming data trace).

use crate::frame::{messages, Msg};
use crate::regs::{PARAGRAPH, PARAGRAPHS, TRAM_SIZE};

/// The paragraphs of a stopped recording, oldest first, each cut at the write pointer where it
/// holds the newest data. `ring`: the recorder ran as a ring (stopped by an event or the host),
/// so the paragraphs after the write pointer are older. Paragraphs that are all zero were never
/// written (the RAM is cleared at arm) and are left out.
pub fn ordered(raw: &[u8], write_ptr: u32, ring: bool) -> Vec<(usize, &[u8])> {
    assert_eq!(raw.len(), TRAM_SIZE as usize);
    let wp = write_ptr.min(TRAM_SIZE - 1);
    let cur = (wp / PARAGRAPH) as usize;
    let cut = (wp % PARAGRAPH) as usize;
    let par = |i: usize| &raw[i * PARAGRAPH as usize..(i + 1) * PARAGRAPH as usize];
    let full = write_ptr >= TRAM_SIZE - 0x20;
    let mut order: Vec<usize> = if ring { (cur + 1..PARAGRAPHS).chain(0..=cur).collect() } else { (0..=cur).collect() };
    if full && !ring {
        order = (0..PARAGRAPHS).collect();
    }
    order
        .into_iter()
        .filter_map(|i| {
            let p = par(i);
            let p = if i == cur && !full && cut > 0 { &p[..cut] } else { p };
            if p.iter().all(|&b| b == 0) {
                None
            } else {
                Some((i, p))
            }
        })
        .collect()
}

/// Incremental reader of a running ring: call [`Drain::poll`] with the current write pointer to
/// learn which bytes to read, then [`Drain::take`] with them to get the new messages.
#[derive(Debug, Default)]
pub struct Drain {
    /// Paragraph being read and how many of its messages were delivered.
    par: usize,
    delivered: usize,
    last_wp: Option<u32>,
    /// Paragraphs found overwritten before they were read.
    pub laps: u64,
}

/// What to read next: `(paragraph, byte length)` in order. The last entry is the paragraph the
/// writer is in, up to the write pointer.
pub type Reads = Vec<(usize, usize)>;

impl Drain {
    pub fn new() -> Self {
        Self::default()
    }

    /// The reads needed to catch up with the writer at `write_ptr`.
    pub fn poll(&mut self, write_ptr: u32) -> Reads {
        let wp = write_ptr % TRAM_SIZE;
        let cur = (wp / PARAGRAPH) as usize;
        let cut = (wp % PARAGRAPH) as usize;
        let mut r = Vec::new();
        if self.last_wp.is_none() {
            self.par = 0;
            self.delivered = 0;
        }
        self.last_wp = Some(wp);
        let mut i = self.par;
        while i != cur {
            r.push((i, PARAGRAPH as usize));
            i = (i + 1) % PARAGRAPHS;
            if r.len() > PARAGRAPHS {
                break;
            }
        }
        if cut > 0 {
            r.push((cur, cut));
        }
        r
    }

    /// Feeds the bytes read for `poll`'s entries (same order); returns the new messages and
    /// whether data was lost before them (`true` when a paragraph was overwritten).
    pub fn take(&mut self, chunks: &[(usize, Vec<u8>)]) -> (Vec<Msg>, bool) {
        let mut out = Vec::new();
        let mut lost = false;
        for (i, bytes) in chunks {
            if *i != self.par {
                // The reader fell behind: skip to this paragraph.
                self.par = *i;
                self.delivered = 0;
                lost = true;
                self.laps += 1;
            }
            let msgs: Vec<Msg> = messages(bytes).into_iter().map(|m| m.1).collect();
            let complete = bytes.len() == PARAGRAPH as usize;
            if self.delivered > msgs.len() {
                // Fewer messages than delivered before: overwritten by the next lap.
                lost = true;
                self.laps += 1;
                self.delivered = 0;
            }
            let ended = msgs.iter().any(|m| matches!(m, Msg::Skip | Msg::End));
            // A paragraph still being written: keep its last message back (it may be cut).
            let usable = if complete || ended { msgs.len() } else { msgs.len().saturating_sub(1) };
            if usable > self.delivered {
                out.extend_from_slice(&msgs[self.delivered..usable]);
                self.delivered = usable;
            }
            if complete {
                self.par = (self.par + 1) % PARAGRAPHS;
                self.delivered = 0;
            }
        }
        (out, lost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{paragraphs, Rec};
    use crate::frame::CORE_PTU;

    fn ptu_msgs(n: usize) -> Vec<Rec> {
        (0..n).map(|i| Rec::Unit { core: CORE_PTU, tt: 0, data: 0x8000_0000 + i as u128 * 4, bits: 32 }).collect()
    }

    #[test]
    fn linear_and_ring_order() {
        let mut raw = paragraphs(&ptu_msgs(1600), PARAGRAPH as usize, 8);
        raw.resize(TRAM_SIZE as usize, 0);
        let lin = ordered(&raw, 0x1FE0, false);
        assert_eq!(lin.len(), 8);
        assert_eq!(lin[0].0, 0);
        // Ring stopped with the writer in paragraph 2: oldest is paragraph 3.
        let ring = ordered(&raw, 0x0900, true);
        assert_eq!(ring.iter().map(|p| p.0).collect::<Vec<_>>(), vec![3, 4, 5, 6, 7, 0, 1, 2]);
        assert_eq!(ring.last().unwrap().1.len(), 0x100);
    }

    #[test]
    fn drain_delivers_each_message_once() {
        let msgs = ptu_msgs(1600);
        let mut raw = paragraphs(&msgs, PARAGRAPH as usize, 8);
        raw.resize(TRAM_SIZE as usize, 0);
        let mut d = Drain::new();
        let mut got = 0;
        for wp in [0x100u32, 0x380, 0x500, 0x1000, 0x1600] {
            let reads = d.poll(wp);
            let chunks: Vec<(usize, Vec<u8>)> =
                reads.iter().map(|&(i, n)| (i, raw[i * 0x400..i * 0x400 + n].to_vec())).collect();
            let (m, lost) = d.take(&chunks);
            assert!(!lost);
            got += m.iter().filter(|m| matches!(m, Msg::Unit { .. })).count();
        }
        // Everything up to the last write pointer, except possibly the message at the cut.
        let units = |b: &[u8]| messages(b).iter().filter(|m| matches!(m.1, Msg::Unit { .. })).count();
        let expect: usize = (0..5).map(|i| units(&raw[i * 0x400..(i + 1) * 0x400])).sum::<usize>() + units(&raw[0x1400..0x1600]);
        assert!(got + 1 >= expect && got <= expect, "got {got} expect {expect}");
    }
}
