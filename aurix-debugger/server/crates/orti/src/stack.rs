//! Stack coverage: how much of each OS stack has ever been touched.
//!
//! The OS fills every stack with a pattern at startup, so the high-water mark is the far end of
//! the region that still holds it. ORTI describes exactly what is needed for that - `SIZE`,
//! `BASEADDRESS`, `STACKDIRECTION` and `FILLPATTERN` on the `STACK` object - and those four names
//! *are* specified by ORTI, unlike the object type names, so keying on them is fair.
//!
//! This is the one part of OS awareness that is not cheap: finding the mark means reading the
//! untouched part of every stack. It is therefore on demand, never on a refresh timer.

use symbols::expr::Target;

use crate::awareness::Awareness;

/// Which way a stack grows, and therefore which end keeps the fill pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Grows towards lower addresses: the untouched part is at the bottom.
    Down,
    /// Grows towards higher addresses: the untouched part is at the top.
    Up,
}

/// One stack, measured.
#[derive(Clone, Debug)]
pub struct StackUsage {
    pub name: String,
    pub base: u32,
    pub size: u32,
    pub direction: Direction,
    pub pattern: u32,
    /// Bytes ever used, or `None` when it could not be measured.
    pub used: Option<u32>,
    /// Why it could not be measured - a protected region, or a missing attribute. The stack is
    /// still listed: "not readable" is worth saying, and far better than reporting 0%.
    pub error: Option<String>,
}

impl StackUsage {
    /// Used bytes as a percentage, rounded down.
    pub fn percent(&self) -> Option<u32> {
        match (self.used, self.size) {
            (Some(u), s) if s > 0 => Some(u.saturating_mul(100) / s),
            _ => None,
        }
    }
}

/// How much of one read is scanned at a time. One bus access costs far more than the bytes in it,
/// so this is large; it also bounds the memory a measurement holds.
pub const CHUNK: u32 = 1024;

fn as_u32(bytes: &[u8]) -> u32 {
    let mut v = 0u32;
    for (i, &b) in bytes.iter().take(4).enumerate() {
        v |= (b as u32) << (8 * i);
    }
    v
}

/// Reads one attribute of an object as a number.
fn number(aw: &Awareness, kind: &str, obj: &str, attr: &str, t: &mut dyn Target) -> Result<u32, String> {
    let o = aw.objects_of(kind).find(|o| o.name == obj).ok_or_else(|| format!("no {kind} named {obj}"))?;
    let a = o.attrs.iter().find(|a| a.name == attr).ok_or_else(|| format!("{obj} has no {attr}"))?;
    Ok(as_u32(&aw.read(kind, a, t)?))
}

/// Reads one attribute of an object as text.
fn text(aw: &Awareness, kind: &str, obj: &str, attr: &str) -> Option<String> {
    let o = aw.objects_of(kind).find(|o| o.name == obj)?;
    let a = o.attrs.iter().find(|a| a.name == attr)?;
    Some(a.show(&[]).text).filter(|s| !s.is_empty())
}

/// Walks the fill pattern in from the untouched end and returns the bytes ever used.
///
/// The first byte that is not the pattern is the high-water mark, and everything beyond it counts
/// as used - so a stack that happens to hold the pattern in the middle of live data is reported as
/// more used, never as less.
pub fn scan(base: u32, size: u32, direction: Direction, pattern: u32, t: &mut dyn Target) -> Result<u32, String> {
    let p = pattern.to_le_bytes();
    let mut scanned = 0u32;
    while scanned < size {
        let len = CHUNK.min(size - scanned);
        let offset = match direction {
            Direction::Down => scanned,
            Direction::Up => size - scanned - len,
        };
        let block = t.read(base + offset, len as usize)?;
        let at = |i: usize| p[(offset as usize + i) % 4];
        let hit = match direction {
            Direction::Down => block.iter().enumerate().find(|(i, &b)| b != at(*i)).map(|(i, _)| i as u32),
            // Coming down from the top, the mark is the *last* mismatch in the block.
            Direction::Up => block.iter().enumerate().rev().find(|(i, &b)| b != at(*i)).map(|(i, _)| i as u32),
        };
        if let Some(i) = hit {
            return Ok(match direction {
                Direction::Down => size - (offset + i),
                Direction::Up => offset + i + 1,
            });
        }
        scanned += len;
    }
    // Every byte still holds the pattern: never used.
    Ok(0)
}

/// Measures one stack by walking its fill pattern.
pub fn measure_one(aw: &Awareness, name: &str, t: &mut dyn Target) -> StackUsage {
    let direction = match text(aw, "STACK", name, "STACKDIRECTION").as_deref() {
        Some("UP") => Direction::Up,
        // ORTI's default, and what every TriCore OS does.
        _ => Direction::Down,
    };
    let mut u = StackUsage { name: name.to_string(), base: 0, size: 0, direction, pattern: 0, used: None, error: None };

    let size = match number(aw, "STACK", name, "SIZE", t) {
        Ok(v) => v,
        Err(e) => {
            u.error = Some(e);
            return u;
        }
    };
    let base = match number(aw, "STACK", name, "BASEADDRESS", t) {
        Ok(v) => v,
        Err(e) => {
            u.error = Some(e);
            return u;
        }
    };
    // A missing pattern is not an error: ORTI allows it, and zero is the conventional default.
    let pattern = number(aw, "STACK", name, "FILLPATTERN", t).unwrap_or(0);
    u.size = size;
    u.base = base;
    u.pattern = pattern;
    if size == 0 {
        u.used = Some(0);
        return u;
    }
    match scan(base, size, direction, pattern, t) {
        Ok(used) => u.used = Some(used),
        Err(e) => u.error = Some(e),
    }
    u
}

/// Measures every stack the ORTI file describes, or only the named ones.
pub fn measure(aw: &Awareness, only: Option<&[String]>, t: &mut dyn Target) -> Vec<StackUsage> {
    aw.objects_of("STACK")
        .filter(|o| only.is_none_or(|list| list.iter().any(|n| *n == o.name)))
        .map(|o| measure_one(aw, &o.name, t))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;


    /// A block of memory at a base address; reads outside it fail, as a protected region does.
    struct Mem {
        base: u32,
        bytes: Vec<u8>,
    }

    impl Target for Mem {
        fn pc(&self) -> Option<u32> {
            None
        }
        fn reg(&self, _: u16) -> Option<u32> {
            None
        }
        fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
            let off = addr.checked_sub(self.base).ok_or("below the region")? as usize;
            self.bytes.get(off..off + len).map(<[u8]>::to_vec).ok_or_else(|| format!("0x{addr:08x} is not readable"))
        }
        fn symbol(&mut self, _: &str) -> Option<symbols::expr::SymbolRef> {
            None
        }
    }

    const BASE: u32 = 0x7000_1000;
    const PAT: u32 = 0xAAAA_AAAA;

    /// A stack of `size` bytes with `used` bytes touched at the growing end.
    fn stack(size: usize, used: usize, dir: Direction) -> Mem {
        let mut bytes = vec![0xAAu8; size];
        for i in 0..used {
            // Anything that is not the pattern.
            let at = if dir == Direction::Down { size - 1 - i } else { i };
            bytes[at] = 0x5A;
        }
        Mem { base: BASE, bytes }
    }

    #[test]
    fn a_downward_stack_is_measured_from_its_low_end() {
        // The untouched part of a downward stack is at the bottom, so the mark is the first byte
        // from the base that is no longer the pattern.
        for used in [0usize, 1, 100, 1023, 1024, 1025, 4095, 4096] {
            let mut m = stack(4096, used, Direction::Down);
            assert_eq!(scan(BASE, 4096, Direction::Down, PAT, &mut m), Ok(used as u32), "{used} used");
        }
    }

    #[test]
    fn an_upward_stack_is_measured_from_its_high_end() {
        for used in [0usize, 1, 100, 1023, 1024, 1025, 4095, 4096] {
            let mut m = stack(4096, used, Direction::Up);
            assert_eq!(scan(BASE, 4096, Direction::Up, PAT, &mut m), Ok(used as u32), "{used} used");
        }
    }

    #[test]
    fn the_pattern_stays_aligned_to_the_stack_across_chunks() {
        // A pattern whose four bytes differ catches a scan that re-anchors per chunk instead of
        // per region: the second chunk starts at offset 1024, which is 0 mod 4 - so make the
        // stack deliberately unaligned in length to shift the boundary.
        let size = 1026usize;
        let p = 0x0403_0201u32;
        let bytes: Vec<u8> = (0..size).map(|i| p.to_le_bytes()[i % 4]).collect();
        let mut m = Mem { base: BASE, bytes };
        assert_eq!(scan(BASE, size as u32, Direction::Down, p, &mut m), Ok(0));
        // Touch one byte in the second chunk.
        m.bytes[1025] = 0xFF;
        assert_eq!(scan(BASE, size as u32, Direction::Down, p, &mut m), Ok(size as u32 - 1025));
    }

    #[test]
    fn an_unreadable_stack_reports_why_instead_of_zero() {
        // What the SMPU does to a protected stack: the read fails, and reporting 0% would be a lie.
        let mut m = Mem { base: BASE + 0x1000, bytes: vec![0xAA; 16] };
        let e = scan(BASE, 4096, Direction::Down, PAT, &mut m).unwrap_err();
        assert!(e.contains("below the region"), "{e}");
    }

    #[test]
    fn live_data_that_looks_like_the_pattern_is_counted_as_used() {
        // A hole of pattern bytes inside the used area must not shorten the answer: the mark is
        // the deepest mismatch, not the shallowest.
        let mut m = stack(256, 64, Direction::Down);
        for i in 200..210 {
            m.bytes[i] = 0xAA;
        }
        assert_eq!(scan(BASE, 256, Direction::Down, PAT, &mut m), Ok(64));
    }

    #[test]
    fn percent_is_only_reported_when_there_is_something_to_report() {
        let s = |used, size| StackUsage {
            name: "s".into(),
            base: 0,
            size,
            direction: Direction::Down,
            pattern: 0,
            used,
            error: None,
        };
        assert_eq!(s(Some(512), 1024).percent(), Some(50));
        assert_eq!(s(Some(1023), 1024).percent(), Some(99));
        assert_eq!(s(Some(1024), 1024).percent(), Some(100));
        assert_eq!(s(None, 1024).percent(), None);
        // A zero-sized stack must not divide by zero.
        assert_eq!(s(Some(0), 0).percent(), None);
    }
}
