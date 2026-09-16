//! Program images for flashing: ELF (loadable segments with file data),
//! Intel HEX and raw binary (OpenOCD `write_image` types elf/ihex/bin).

use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub addr: u32,
    pub data: Vec<u8>,
}

impl Section {
    pub fn end(&self) -> u64 {
        self.addr as u64 + self.data.len() as u64
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Image {
    /// Sorted by address, non-overlapping, adjacent sections merged.
    pub sections: Vec<Section>,
}

fn u16le(b: &[u8], o: usize) -> Result<u16, String> {
    b.get(o..o + 2).map(|x| u16::from_le_bytes([x[0], x[1]])).ok_or_else(|| "truncated ELF file".to_string())
}

fn u32le(b: &[u8], o: usize) -> Result<u32, String> {
    b.get(o..o + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]])).ok_or_else(|| "truncated ELF file".to_string())
}

impl Image {
    /// Loads an image file. `kind` is OpenOCD's image type (elf, ihex, bin); by default it is
    /// detected from the content and extension. `offset` is added to all addresses (the base
    /// address for binary files).
    pub fn load(path: &Path, offset: u32, kind: Option<&str>) -> Result<Image, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("couldn't open image file {}: {e}", path.display()))?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        let kind = match kind {
            Some(k) => k.to_ascii_lowercase(),
            None if bytes.starts_with(b"\x7fELF") => "elf".into(),
            None if matches!(ext.as_str(), "hex" | "ihex") => "ihex".into(),
            None => "bin".into(),
        };
        let mut img = match kind.as_str() {
            "elf" => Image::from_elf(&bytes)?,
            "ihex" => Image::from_ihex(std::str::from_utf8(&bytes).map_err(|_| "Intel HEX file is not text")?)?,
            "bin" => Image::from_sections(vec![Section { addr: 0, data: bytes }]),
            other => return Err(format!("unsupported image type '{other}' (supported: elf, ihex, bin)")),
        };
        if offset != 0 {
            for s in &mut img.sections {
                s.addr = s.addr.wrapping_add(offset);
            }
        }
        Ok(img)
    }

    pub fn from_sections(mut sections: Vec<Section>) -> Image {
        sections.retain(|s| !s.data.is_empty());
        sections.sort_by_key(|s| s.addr);
        let mut out: Vec<Section> = Vec::new();
        for s in sections {
            match out.last_mut() {
                Some(last) if last.end() >= s.addr as u64 => {
                    // Adjacent or overlapping: later data wins.
                    let start = (s.addr - last.addr) as usize;
                    let need = start + s.data.len();
                    if last.data.len() < need {
                        last.data.resize(need, 0);
                    }
                    last.data[start..need].copy_from_slice(&s.data);
                }
                _ => out.push(s),
            }
        }
        Image { sections: out }
    }

    /// Loadable segments with file data (32-bit little-endian ELF). Physical addresses are used,
    /// unless all of them are zero (as written by some toolchains, e.g. GHS).
    pub fn from_elf(b: &[u8]) -> Result<Image, String> {
        if !b.starts_with(b"\x7fELF") {
            return Err("not an ELF file".into());
        }
        if b.get(4) != Some(&1) || b.get(5) != Some(&1) {
            return Err("only 32-bit little-endian ELF files are supported".into());
        }
        let phoff = u32le(b, 0x1C)? as usize;
        let phentsize = u16le(b, 0x2A)? as usize;
        let phnum = u16le(b, 0x2C)? as usize;
        let mut segs = Vec::new();
        for i in 0..phnum {
            let o = phoff + i * phentsize;
            const PT_LOAD: u32 = 1;
            if u32le(b, o)? != PT_LOAD {
                continue;
            }
            let (off, vaddr, paddr, filesz) = (u32le(b, o + 4)? as usize, u32le(b, o + 8)?, u32le(b, o + 12)?, u32le(b, o + 16)? as usize);
            if filesz == 0 {
                continue;
            }
            let data = b.get(off..off + filesz).ok_or("ELF segment outside of the file")?.to_vec();
            segs.push((vaddr, paddr, data));
        }
        let use_paddr = segs.iter().any(|(_, p, _)| *p != 0);
        Ok(Image::from_sections(
            segs.into_iter().map(|(v, p, data)| Section { addr: if use_paddr { p } else { v }, data }).collect(),
        ))
    }

    pub fn from_ihex(text: &str) -> Result<Image, String> {
        let mut sections = Vec::new();
        let mut base: u32 = 0;
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let rec = line.strip_prefix(':').ok_or_else(|| format!("Intel HEX line {}: missing ':'", n + 1))?;
            let bytes = (0..rec.len() / 2)
                .map(|i| u8::from_str_radix(&rec[2 * i..2 * i + 2], 16))
                .collect::<Result<Vec<u8>, _>>()
                .map_err(|_| format!("Intel HEX line {}: invalid hex digits", n + 1))?;
            if bytes.len() < 5 || bytes.len() != bytes[0] as usize + 5 {
                return Err(format!("Intel HEX line {}: invalid record length", n + 1));
            }
            if bytes.iter().fold(0u8, |a, x| a.wrapping_add(*x)) != 0 {
                return Err(format!("Intel HEX line {}: checksum mismatch", n + 1));
            }
            let addr = u16::from_be_bytes([bytes[1], bytes[2]]) as u32;
            let data = &bytes[4..bytes.len() - 1];
            match bytes[3] {
                0 => sections.push(Section { addr: base.wrapping_add(addr), data: data.to_vec() }),
                1 => break,
                2 if data.len() == 2 => base = (u16::from_be_bytes([data[0], data[1]]) as u32) << 4,
                4 if data.len() == 2 => base = (u16::from_be_bytes([data[0], data[1]]) as u32) << 16,
                3 | 5 => {}
                t => return Err(format!("Intel HEX line {}: unsupported record type {t:02X}", n + 1)),
            }
        }
        Ok(Image::from_sections(sections))
    }

    pub fn size(&self) -> usize {
        self.sections.iter().map(|s| s.data.len()).sum()
    }

    /// Image byte at an address, if the image has data there.
    pub fn byte_at(&self, addr: u32) -> Option<u8> {
        let i = self.sections.partition_point(|s| s.addr <= addr);
        let s = self.sections.get(i.checked_sub(1)?)?;
        s.data.get((addr - s.addr) as usize).copied()
    }

    /// Whether the image has data in `[start, end)`.
    pub fn touches(&self, start: u32, end: u64) -> bool {
        self.sections.iter().any(|s| (s.addr as u64) < end && s.end() > start as u64)
    }

    /// The image without its data in `[start, end)`, and the number of bytes removed.
    pub fn without(&self, start: u32, end: u64) -> (Image, usize) {
        let mut kept = Vec::new();
        let mut removed = 0;
        for s in &self.sections {
            let cut_lo = (start as u64).clamp(s.addr as u64, s.end());
            let cut_hi = end.clamp(cut_lo, s.end());
            let (lo, hi) = ((cut_lo - s.addr as u64) as usize, (cut_hi - s.addr as u64) as usize);
            removed += hi - lo;
            kept.push(Section { addr: s.addr, data: s.data[..lo].to_vec() });
            kept.push(Section { addr: cut_hi as u32, data: s.data[hi..].to_vec() });
        }
        (Image::from_sections(kept), removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_sections() {
        let img = Image::from_sections(vec![
            Section { addr: 0x10, data: vec![3, 4] },
            Section { addr: 0x0E, data: vec![1, 2] },
            Section { addr: 0x20, data: vec![9] },
            Section { addr: 0x30, data: vec![] },
        ]);
        assert_eq!(img.sections, vec![Section { addr: 0x0E, data: vec![1, 2, 3, 4] }, Section { addr: 0x20, data: vec![9] }]);
        assert_eq!(img.byte_at(0x11), Some(4));
        assert_eq!(img.byte_at(0x12), None);
        assert!(img.touches(0x1F, 0x21) && !img.touches(0x12, 0x20));
    }

    #[test]
    fn removes_range() {
        let img = Image::from_sections(vec![Section { addr: 0x10, data: vec![1, 2, 3, 4] }, Section { addr: 0x20, data: vec![5, 6] }]);
        let (cut, n) = img.without(0x11, 0x21);
        assert_eq!(n, 4);
        assert_eq!(cut.sections, vec![Section { addr: 0x10, data: vec![1] }, Section { addr: 0x21, data: vec![6] }]);
        assert_eq!(img.without(0x30, 0x40), (img.clone(), 0));
        assert_eq!(img.without(0, 0x100).0.sections, vec![]);
    }

    #[test]
    fn intel_hex() {
        let text = ":0200000480007A\n:0400100001020304E2\n:00000001FF\n";
        let img = Image::from_ihex(text).unwrap();
        assert_eq!(img.sections, vec![Section { addr: 0x8000_0010, data: vec![1, 2, 3, 4] }]);
        assert!(Image::from_ihex(":0400100001020304E3\n").is_err());
    }

    #[test]
    fn real_elf() {
        let p = Path::new("C:/workspace/app/build/bin/app.elf");
        if !p.exists() {
            return;
        }
        let img = Image::load(p, 0, None).unwrap();
        let first = &img.sections[0];
        assert_eq!(first.addr, 0x8012_0400);
        assert!(img.sections.iter().all(|s| s.addr >= 0x8000_0000 && s.end() <= 0x8060_0000), "{:x?}", img.sections.iter().map(|s| (s.addr, s.data.len())).collect::<Vec<_>>());
        assert!(img.size() > 1_000_000);
    }
}
