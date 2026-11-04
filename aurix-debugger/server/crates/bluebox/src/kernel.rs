//! The probe's kernel: a PowerPC program the probe loses at power-off.
//!
//! winIDEA ships it as Motorola S-records, `iC6kKrnl.s` in its install
//! folder. After a power cycle of the BlueBox, winIDEA loads it into probe
//! RAM before anything else. Every later attach finds it running. The
//! upload captured after a power cycle is byte-identical to the S-record
//! data (2,452,880 bytes at 0x100000 for winIDEA 9.21.356).
//!
//! Each winIDEA version has its own kernel. Its services are numbered
//! differently (9.21.417's node-1 services are two higher), so the kernel and
//! the paths a client uses must come from the same winIDEA version.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Data per command: winIDEA sends 48 KiB per `W` command.
pub const CHUNK: usize = 0xc000;

/// Where winIDEA starts the kernel (`K` command); also its load address.
pub const ENTRY: u32 = 0x0010_0000;

/// File name of the BlueBox kernel in a winIDEA install.
pub const FILE_NAME: &str = "iC6kKrnl.s";

/// A kernel image: contiguous segments in address order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kernel {
    pub segments: Vec<(u32, Vec<u8>)>,
}

impl Kernel {
    pub fn from_file(path: &Path) -> Result<Kernel> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Kernel(format!("{}: {e}", path.display())))?;
        Kernel::from_srec(&text).map_err(|e| Error::Kernel(format!("{}: {e}", path.display())))
    }

    /// Parses S1/S2/S3 data records (checksums verified); other records are skipped.
    pub fn from_srec(text: &str) -> std::result::Result<Kernel, String> {
        let mut records: Vec<(u32, Vec<u8>)> = Vec::new();
        for (no, line) in text.lines().enumerate() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            let bad = |why: &str| format!("line {}: {why}", no + 1);
            if !t.starts_with('S') || t.len() < 4 {
                return Err(bad("not an S-record"));
            }
            let alen = match &t[1..2] {
                "1" => 2,
                "2" => 3,
                "3" => 4,
                _ => continue, // S0 header, S5 count, S7-S9 start address
            };
            let raw = (2..t.len())
                .step_by(2)
                .map(|i| t.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
                .collect::<Option<Vec<u8>>>()
                .ok_or_else(|| bad("bad hex"))?;
            if raw.is_empty() || raw[0] as usize != raw.len() - 1 || raw.len() < 1 + alen + 1 {
                return Err(bad("bad length"));
            }
            let sum = raw[..raw.len() - 1].iter().fold(0u8, |s, b| s.wrapping_add(*b));
            if !sum != raw[raw.len() - 1] {
                return Err(bad("checksum"));
            }
            let addr = raw[1..1 + alen].iter().fold(0u32, |a, b| a << 8 | *b as u32);
            records.push((addr, raw[1 + alen..raw.len() - 1].to_vec()));
        }
        records.sort_by_key(|r| r.0);
        let mut segments: Vec<(u32, Vec<u8>)> = Vec::new();
        for (addr, data) in records {
            match segments.last_mut() {
                Some((a, d)) if *a as u64 + d.len() as u64 == addr as u64 => d.extend_from_slice(&data),
                Some((a, d)) if (addr as u64) < *a as u64 + d.len() as u64 => {
                    return Err(format!("records overlap at 0x{addr:08x}"))
                }
                _ => segments.push((addr, data)),
            }
        }
        if segments.is_empty() {
            return Err("no data records".into());
        }
        Ok(Kernel { segments })
    }

    pub fn len(&self) -> usize {
        self.segments.iter().map(|s| s.1.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The `W` commands, as `(address, data)`: each segment from its start in
    /// [`CHUNK`] pieces, as winIDEA splits it.
    pub fn chunks(&self) -> impl Iterator<Item = (u32, &[u8])> {
        self.segments.iter().flat_map(|(a, d)| {
            d.chunks(CHUNK).enumerate().map(move |(i, c)| (a + (i * CHUNK) as u32, c))
        })
    }
}

/// `iC6kKrnl.s` in the usual winIDEA install folders, newest name first.
pub fn find_in_winidea() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(d) = std::env::var("WINIDEA_DIR") {
        dirs.push(d.into());
    }
    dirs.push(PathBuf::from(r"C:\winIDEA"));
    dirs.into_iter().map(|d| d.join(FILE_NAME)).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: char, addr: u32, data: &[u8]) -> String {
        let alen = match kind {
            '1' => 2,
            '2' => 3,
            _ => 4,
        };
        let mut raw = vec![(alen + data.len() + 1) as u8];
        raw.extend_from_slice(&addr.to_be_bytes()[4 - alen..]);
        raw.extend_from_slice(data);
        let sum = !raw.iter().fold(0u8, |s, b| s.wrapping_add(*b));
        raw.push(sum);
        format!("S{kind}{}", raw.iter().map(|b| format!("{b:02X}")).collect::<String>())
    }

    #[test]
    fn records_merge_into_segments() {
        let text = [
            "S00F0000433A2F4A656E6B696E732F4F".to_string(),
            rec('3', 0x0010_0000, &[1, 2, 3, 4]),
            rec('3', 0x0010_0004, &[5, 6]),
            rec('3', 0x0010_0010, &[7]),
            "S70500000000FA".to_string(),
        ]
        .join("\n");
        let k = Kernel::from_srec(&text).unwrap();
        assert_eq!(k.segments, vec![(0x0010_0000, vec![1, 2, 3, 4, 5, 6]), (0x0010_0010, vec![7])]);
        assert_eq!(k.len(), 7);
    }

    #[test]
    fn a_bad_checksum_is_rejected() {
        let mut r = rec('3', 0x0010_0000, &[1, 2, 3, 4]);
        r.replace_range(r.len() - 2.., "00");
        assert!(Kernel::from_srec(&r).unwrap_err().contains("checksum"));
    }

    #[test]
    fn chunks_restart_at_each_segment() {
        // The 9.21.356 kernel: 0x100000 + 0x223d5c, then 0x323d60 + 0x33034.
        let k = Kernel { segments: vec![(0x0010_0000, vec![0; 0x223d5c]), (0x0032_3d60, vec![0; 0x33034])] };
        let c: Vec<(u32, usize)> = k.chunks().map(|(a, d)| (a, d.len())).collect();
        assert_eq!(c.len(), 51);
        assert_eq!(c[0], (0x0010_0000, CHUNK));
        assert_eq!(c[45], (0x0031_c000, 0x7d5c));
        assert_eq!(c[46], (0x0032_3d60, CHUNK));
        assert_eq!(c[50], (0x0035_3d60, 0x3034));
    }
}
