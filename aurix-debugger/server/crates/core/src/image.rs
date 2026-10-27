//! Program images for flashing: ELF (loadable segments with file data), Intel HEX,
//! Motorola S-records and raw binary (OpenOCD `write_image` types elf/ihex/s19/bin).

use std::fmt::Write as _;
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

/// An image file to program: `offset` is added to its addresses (the base address of a binary
/// file), `kind` is its type (detected when `None`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageFile {
    pub path: String,
    pub offset: u32,
    pub kind: Option<String>,
}

impl ImageFile {
    pub fn new(path: &str) -> ImageFile {
        ImageFile { path: path.to_string(), offset: 0, kind: None }
    }

    pub fn load(&self) -> Result<Image, String> {
        Image::load(Path::new(&self.path), self.offset, self.kind.as_deref())
    }
}

fn u16le(b: &[u8], o: usize) -> Result<u16, String> {
    b.get(o..o + 2).map(|x| u16::from_le_bytes([x[0], x[1]])).ok_or_else(|| "truncated ELF file".to_string())
}

fn u32le(b: &[u8], o: usize) -> Result<u32, String> {
    b.get(o..o + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]])).ok_or_else(|| "truncated ELF file".to_string())
}

/// Pairs of hex digits as bytes.
fn hex_bytes(s: &[u8]) -> Option<Vec<u8>> {
    let nibble = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.chunks(2).map(|c| Some(nibble(c[0])? << 4 | nibble(c[1])?)).collect()
}

/// Comment lines in text images, as written by flash container tools between the data blocks.
fn is_comment(line: &[u8]) -> bool {
    matches!(line.first(), Some(b';' | b'#'))
}

/// Appends an Intel HEX record.
fn ihex_record(out: &mut String, addr: u16, kind: u8, data: &[u8]) {
    let mut bytes = vec![data.len() as u8, (addr >> 8) as u8, addr as u8, kind];
    bytes.extend_from_slice(data);
    bytes.push(bytes.iter().fold(0u8, |a, x| a.wrapping_sub(*x)));
    out.push(':');
    for b in bytes {
        let _ = write!(out, "{b:02X}");
    }
    out.push_str("\r\n");
}

impl Image {
    /// Loads an image file. `kind` is OpenOCD's image type (elf, ihex, s19, bin; srec is an
    /// alias of s19); by default it is detected with [`Image::detect`]. `offset` is added to all
    /// addresses (the base address for binary files).
    pub fn load(path: &Path, offset: u32, kind: Option<&str>) -> Result<Image, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("couldn't open image file {}: {e}", path.display()))?;
        let kind = kind.map_or_else(|| Image::detect(path, &bytes).to_string(), |k| k.to_ascii_lowercase());
        let text = |what: &str| std::str::from_utf8(&bytes).map_err(|_| format!("{what} file is not text"));
        let mut img = match kind.as_str() {
            "elf" => Image::from_elf(&bytes)?,
            "ihex" => Image::from_ihex(text("Intel HEX")?)?,
            "s19" | "srec" => Image::from_srec(text("S-record")?)?,
            "bin" => Image::from_sections(vec![Section { addr: 0, data: bytes }]),
            other => return Err(format!("unsupported image type '{other}' (supported: elf, ihex, s19, bin)")),
        };
        if offset != 0 {
            for s in &mut img.sections {
                s.addr = s.addr.wrapping_add(offset);
            }
        }
        Ok(img)
    }

    /// The image type of a file (elf, ihex, s19 or bin): from its ELF magic, its extension, or
    /// for other text files the first line that is not a comment.
    pub fn detect(path: &Path, bytes: &[u8]) -> &'static str {
        if bytes.starts_with(b"\x7fELF") {
            return "elf";
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        match ext.as_str() {
            "hex" | "ihex" => return "ihex",
            "srec" | "s19" | "s28" | "s37" | "mot" | "sx" => return "s19",
            _ => {}
        }
        let first = bytes.split(|&b| b == b'\n').map(<[u8]>::trim_ascii).find(|l| !l.is_empty() && !is_comment(l));
        match first {
            Some([b':', rest @ ..]) if hex_bytes(rest).is_some() => "ihex",
            Some([b'S' | b's', t, rest @ ..]) if t.is_ascii_digit() && hex_bytes(rest).is_some() => "s19",
            _ => "bin",
        }
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
            let bytes = hex_bytes(rec.as_bytes()).ok_or_else(|| format!("Intel HEX line {}: invalid hex digits", n + 1))?;
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

    /// Motorola S-records (S1/S2/S3 data, S7/S8/S9 end the file). Lines starting with `;` or `#`
    /// are comments.
    pub fn from_srec(text: &str) -> Result<Image, String> {
        let mut sections: Vec<Section> = Vec::new();
        let mut records: u32 = 0;
        for (n, line) in text.lines().enumerate() {
            let line = line.trim().as_bytes();
            if line.is_empty() || is_comment(line) {
                continue;
            }
            let err = |what: &str| format!("S-record line {}: {what}", n + 1);
            let [b'S' | b's', kind, rec @ ..] = line else { return Err(err("not an S-record")) };
            let addr_len = match kind {
                b'0' | b'1' | b'5' | b'9' => 2,
                b'2' | b'6' | b'8' => 3,
                b'3' | b'7' => 4,
                _ => return Err(err(&format!("unsupported record type S{}", *kind as char))),
            };
            let bytes = hex_bytes(rec).ok_or_else(|| err("invalid hex digits"))?;
            if bytes.len() < addr_len + 2 || bytes.len() != bytes[0] as usize + 1 {
                return Err(err("invalid record length"));
            }
            if bytes.iter().fold(0u8, |a, x| a.wrapping_add(*x)) != 0xFF {
                return Err(err("checksum mismatch"));
            }
            let addr = bytes[1..=addr_len].iter().fold(0u32, |a, x| a << 8 | *x as u32);
            let data = &bytes[addr_len + 1..bytes.len() - 1];
            match kind {
                b'1' | b'2' | b'3' => {
                    records += 1;
                    // Consecutive records usually continue each other: merge them here, so that
                    // from_sections does not sort tens of thousands of small sections.
                    match sections.last_mut() {
                        Some(last) if last.end() == addr as u64 => last.data.extend_from_slice(data),
                        _ => sections.push(Section { addr, data: data.to_vec() }),
                    }
                }
                b'5' | b'6' => {
                    let mask = if *kind == b'5' { 0xFFFF } else { 0xFF_FFFF };
                    if addr != records & mask {
                        crate::log_warn!("S-record line {}: counts {addr} data records, the file has {records}", n + 1);
                    }
                }
                b'7' | b'8' | b'9' => break,
                _ => {}
            }
        }
        Ok(Image::from_sections(sections))
    }

    /// The image as Intel HEX: 32 data bytes per record, extended linear addresses.
    pub fn to_ihex(&self) -> String {
        let mut out = String::new();
        let mut upper = None;
        for s in &self.sections {
            let mut pos = s.addr as u64;
            while pos < s.end() {
                if upper != Some(pos >> 16) {
                    upper = Some(pos >> 16);
                    ihex_record(&mut out, 0, 4, &((pos >> 16) as u16).to_be_bytes());
                }
                // A record must not cross a 64 KB boundary.
                let n = 32.min(s.end() - pos).min(0x1_0000 - (pos & 0xFFFF));
                let from = (pos - s.addr as u64) as usize;
                ihex_record(&mut out, pos as u16, 0, &s.data[from..from + n as usize]);
                pos += n;
            }
        }
        ihex_record(&mut out, 0, 1, &[]);
        out
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

    /// The image data inside `[start, end)`.
    pub fn within(&self, start: u32, end: u64) -> Image {
        Image::from_sections(
            self.sections
                .iter()
                .filter_map(|s| {
                    let (lo, hi) = ((start as u64).max(s.addr as u64), end.min(s.end()));
                    let (from, to) = ((lo - s.addr as u64) as usize, (hi.max(lo) - s.addr as u64) as usize);
                    (lo < hi).then(|| Section { addr: lo as u32, data: s.data[from..to].to_vec() })
                })
                .collect(),
        )
    }

    /// This image with `top` laid over it: where both have data, `top` wins. Also returns the
    /// number of bytes of this image that `top` replaces with different data.
    pub fn overlay(&self, top: &Image) -> (Image, usize) {
        let mut changed = 0;
        let mut base = self.clone();
        for s in &top.sections {
            if !base.touches(s.addr, s.end()) {
                continue;
            }
            changed += s.data.iter().enumerate().filter(|(i, b)| base.byte_at(s.addr + *i as u32).is_some_and(|x| x != **b)).count();
            base = base.without(s.addr, s.end()).0;
        }
        base.sections.extend(top.sections.iter().cloned());
        (Image::from_sections(base.sections), changed)
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
    fn overlays_later_image() {
        // The lower section of `top` overlaps the higher one of `base`: sorting alone would let
        // the base data win.
        let base = Image::from_sections(vec![Section { addr: 0x10, data: vec![1, 2, 3, 4] }]);
        let top = Image::from_sections(vec![Section { addr: 0x08, data: vec![9; 10] }]);
        let (img, changed) = base.overlay(&top);
        assert_eq!(img.sections, vec![Section { addr: 0x08, data: vec![9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 3, 4] }]);
        assert_eq!(changed, 2);
        assert_eq!(top.overlay(&base).0.sections[0].data, vec![9, 9, 9, 9, 9, 9, 9, 9, 1, 2, 3, 4]);
    }

    #[test]
    fn removes_range() {
        let img = Image::from_sections(vec![Section { addr: 0x10, data: vec![1, 2, 3, 4] }, Section { addr: 0x20, data: vec![5, 6] }]);
        let (cut, n) = img.without(0x11, 0x21);
        assert_eq!(n, 4);
        assert_eq!(cut.sections, vec![Section { addr: 0x10, data: vec![1] }, Section { addr: 0x21, data: vec![6] }]);
        assert_eq!(img.within(0x11, 0x21).sections, vec![Section { addr: 0x11, data: vec![2, 3, 4] }, Section { addr: 0x20, data: vec![5] }]);
        assert!(img.within(0x14, 0x20).sections.is_empty());
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
    fn s_records() {
        // Comments between the records as in flash container files, CRLF, lowercase digits and
        // all three address sizes; the S3 records are consecutive.
        let text = ";Name: test\r\nS00600004844521B\r\nS10500100102E7\r\n\r\n;=====\r\n# block 2\r\n\
                    s205012345038e\r\nS3078000000004056F\r\nS306800000020671\r\nS5030004F8\r\n;end\r\nS9030000FC\r\n\
                    S10500100102E7\r\n";
        let img = Image::from_srec(text).unwrap();
        assert_eq!(
            img.sections,
            vec![
                Section { addr: 0x10, data: vec![1, 2] },
                Section { addr: 0x01_2345, data: vec![3] },
                Section { addr: 0x8000_0000, data: vec![4, 5, 6] },
            ]
        );
        for (bad, why) in [
            ("S10500100102E8", "checksum"),
            ("S10600100102E6", "length"),
            ("S105001001E8", "length"),
            ("S10500100102E", "hex digits"),
            ("S40500100102E7", "type S4"),
            ("X10500100102E7", "not an S-record"),
            ("S", "not an S-record"),
        ] {
            let e = Image::from_srec(&format!(";c\n{bad}\n")).unwrap_err();
            assert!(e.starts_with("S-record line 2:") && e.contains(why), "{bad}: {e}");
        }
        // A wrong count only warns.
        assert_eq!(Image::from_srec("S10500100102E7\nS5030003F9\n").unwrap().size(), 2);
    }

    #[test]
    fn detects_type() {
        let srec = b";Name: x\r\n;Format: S3\r\nS3078000000004056F\r\n";
        assert_eq!(Image::detect(Path::new("a.srec"), b""), "s19");
        assert_eq!(Image::detect(Path::new("a.MOT"), b""), "s19");
        assert_eq!(Image::detect(Path::new("a.hex"), srec), "ihex");
        assert_eq!(Image::detect(Path::new("a.out"), b"\x7fELF\x01\x01"), "elf");
        assert_eq!(Image::detect(Path::new("a.txt"), srec), "s19");
        assert_eq!(Image::detect(Path::new("a"), b"\n:0400100001020304E2\n"), "ihex");
        assert_eq!(Image::detect(Path::new("a.bin"), b"S1\x00\xff\n"), "bin");
        assert_eq!(Image::detect(Path::new("a.bin"), b";\xff\n:\x01\n"), "bin");

        let path = std::env::temp_dir().join(format!("aurix-image-test-{}.dat", std::process::id()));
        std::fs::write(&path, srec).unwrap();
        let img = Image::load(&path, 0x10, None);
        let bin = Image::load(&path, 0, Some("bin"));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(img.unwrap().sections, vec![Section { addr: 0x8000_0010, data: vec![4, 5] }]);
        assert_eq!(bin.unwrap().size(), srec.len());
    }

    #[test]
    fn writes_intel_hex() {
        // Records split at 32 bytes and at the 64 KB boundary inside the first section.
        let img = Image::from_sections(vec![
            Section { addr: 0x8000_FFF0, data: (0..100).collect() },
            Section { addr: 0xA000_0000, data: vec![7] },
        ]);
        let hex = img.to_ihex();
        assert!(hex.starts_with(":0200000480007A\r\n:10FFF000000102030405060708090A0B0C0D0E0F"), "{hex}");
        assert!(hex.ends_with(":00000001FF\r\n"));
        assert_eq!(Image::from_ihex(&hex).unwrap(), img);
    }

    #[test]
    fn real_srec() {
        // An S-record file (AURIX_TEST_SREC) gives the same image as its Intel HEX conversion.
        let Some(p) = std::env::var_os("AURIX_TEST_SREC") else { return };
        let p = Path::new(&p);
        if !p.exists() {
            return;
        }
        let img = Image::load(p, 0, None).unwrap();
        assert!(img.size() > 0);
        assert_eq!(Image::from_ihex(&img.to_ihex()).unwrap(), img);
    }

    #[test]
    fn real_elf() {
        // An application ELF (AURIX_TEST_ELF) gives a non-empty image of disjoint sections.
        let Some(p) = std::env::var_os("AURIX_TEST_ELF") else { return };
        let p = Path::new(&p);
        if !p.exists() {
            return;
        }
        let img = Image::load(p, 0, None).unwrap();
        assert!(img.size() > 0);
        let mut ranges: Vec<(u64, u64)> = img.sections.iter().map(|s| (s.addr as u64, s.end())).collect();
        ranges.sort();
        assert!(ranges.windows(2).all(|w| w[0].1 <= w[1].0), "{ranges:x?}");
    }
}
