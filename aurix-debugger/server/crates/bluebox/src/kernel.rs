//! The probe's kernel: a PowerPC program the probe loses at power-off.
//!
//! winIDEA ships it as Motorola S-records in its install folder:
//! `iC6kKrnl.s` for the iC5700, `iC3kKrnl.s` for the iC5000
//! ([`crate::Model::kernel_file`]); both are loaded the same way. After a power cycle of the BlueBox, winIDEA loads it into probe
//! RAM before anything else. Every later attach finds it running. The
//! upload captured after a power cycle is byte-identical to the S-record
//! data (2,452,880 bytes at 0x100000 for winIDEA 9.21.356).
//!
//! Each winIDEA version has its own kernel. Its services are numbered
//! differently (9.21.417's node-1 services are two higher), so the kernel and
//! the paths a client uses must come from the same winIDEA version: this
//! crate's are 9.21.356's ([`WINIDEA_VERSION`]), and [`find_kernel`] only takes
//! the kernel from an install of that version.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Data per command: winIDEA sends 48 KiB per `W` command.
pub const CHUNK: usize = 0xc000;

/// Where winIDEA starts the kernel (`K` command); also its load address.
pub const ENTRY: u32 = 0x0010_0000;

/// File name of the iC5700 kernel in a winIDEA install.
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

/// The winIDEA version this crate's protocol comes from: its kernel, service
/// numbers, session value and attach. Another version's kernel speaks
/// another numbering, so only this one may be loaded.
pub const WINIDEA_VERSION: &str = "9.21.356";

/// A winIDEA install: a folder with a BlueBox kernel file, and its version as its
/// `deploy.json` gives it (`{"Deploy": {"Version": "9.21.356"}}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub dir: PathBuf,
    pub version: Option<String>,
}

impl Install {
    /// The install in `dir`, if it holds a kernel file.
    pub fn at(dir: &Path) -> Option<Install> {
        if !crate::Model::ALL.iter().any(|m| dir.join(m.kernel_file()).is_file()) {
            return None;
        }
        let version = std::fs::read_to_string(dir.join("deploy.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.pointer("/Deploy/Version").and_then(|s| s.as_str()).map(str::to_string));
        Some(Install { dir: dir.to_path_buf(), version })
    }

    pub fn kernel(&self, model: crate::Model) -> PathBuf {
        self.dir.join(model.kernel_file())
    }

    /// This is the winIDEA version the crate needs.
    pub fn fits(&self) -> bool {
        self.version.as_deref() == Some(WINIDEA_VERSION)
    }

    fn describe(&self) -> String {
        match &self.version {
            Some(v) => format!("winIDEA {v} in {}", self.dir.display()),
            None => format!("a winIDEA of unknown version in {} (no deploy.json)", self.dir.display()),
        }
    }
}

/// Folders winIDEA is usually installed in: `WINIDEA_DIR`, `C:\winIDEA`, and
/// `C:\winIDEA*` and `C:\iSYSTEM\*` folders (installs side by side).
fn usual_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(d) = std::env::var("WINIDEA_DIR") {
        dirs.push(d.into());
    }
    dirs.push(PathBuf::from(r"C:\winIDEA"));
    let listed = |parent: &str, prefix: &str| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(parent)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.to_ascii_lowercase().starts_with(prefix)))
            .collect();
        v.sort();
        v
    };
    dirs.extend(listed(r"C:\", "winidea"));
    dirs.extend(listed(r"C:\iSYSTEM", ""));
    let mut seen = Vec::new();
    dirs.retain(|d| {
        let k = d.to_string_lossy().to_ascii_lowercase();
        let new = !seen.contains(&k);
        seen.push(k);
        new
    });
    dirs
}

/// The winIDEA installs in the usual folders.
pub fn installs() -> Vec<Install> {
    usual_dirs().iter().filter_map(|d| Install::at(d)).collect()
}

/// `model`'s kernel file of winIDEA [`WINIDEA_VERSION`]: from `dir` if given, else
/// from the first install of that version in the usual folders. The error
/// says what was found instead.
pub fn find_kernel(dir: Option<&Path>, model: crate::Model) -> std::result::Result<PathBuf, String> {
    let file = model.kernel_file();
    let found = |i: &Install| {
        let k = i.kernel(model);
        if k.is_file() { Ok(k) } else { Err(format!("{} has no {file}", i.describe())) }
    };
    match dir {
        Some(d) => match Install::at(d) {
            Some(i) if i.fits() => found(&i),
            Some(i) => Err(format!("{} holds {}, not winIDEA {WINIDEA_VERSION}", d.display(), i.describe())),
            None => Err(format!("{} is not a winIDEA install (no {file})", d.display())),
        },
        None => pick(&installs()).and_then(|i| found(&i)),
    }
}

/// The install of winIDEA [`WINIDEA_VERSION`] among `found`.
fn pick(found: &[Install]) -> std::result::Result<Install, String> {
    if let Some(i) = found.iter().find(|i| i.fits()) {
        return Ok(i.clone());
    }
    if found.is_empty() {
        return Err(format!("winIDEA {WINIDEA_VERSION} is not installed (no winIDEA found in C:\\winIDEA)"));
    }
    let list: Vec<String> = found.iter().map(Install::describe).collect();
    Err(format!("winIDEA {WINIDEA_VERSION} is not installed (found {})", list.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake install in a fresh temporary folder.
    fn install(name: &str, version: Option<&str>) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bluebox-kernel-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(FILE_NAME), "S00600004844521B\n").unwrap();
        if let Some(v) = version {
            std::fs::write(d.join("deploy.json"), format!(r#"{{"Deploy": {{"Deployment": "winIDEA", "Version": "{v}"}}}}"#)).unwrap();
        }
        d
    }

    #[test]
    fn only_the_right_winidea_version_gives_the_kernel() {
        let good = install("good", Some(WINIDEA_VERSION));
        let other = install("other", Some("9.21.417"));
        let unknown = install("unknown", None);
        let m = crate::Model::Ic5700;
        assert_eq!(find_kernel(Some(&good), m).unwrap(), good.join(FILE_NAME));
        let e = find_kernel(Some(&good), crate::Model::Ic5000).unwrap_err();
        assert!(e.contains("iC3kKrnl.s"), "the iC5000 kernel is another file: {e}");
        let e = find_kernel(Some(&other), m).unwrap_err();
        assert!(e.contains("winIDEA 9.21.417") && e.contains(WINIDEA_VERSION), "{e}");
        assert!(find_kernel(Some(&unknown), m).unwrap_err().contains("unknown version"));
        let none = std::env::temp_dir().join("bluebox-kernel-test-none");
        assert!(find_kernel(Some(&none), m).unwrap_err().contains("not a winIDEA install"));

        let found = [Install::at(&other).unwrap(), Install::at(&good).unwrap()];
        assert_eq!(pick(&found).unwrap().kernel(m), good.join(FILE_NAME), "the right one among several");
        let e = pick(&found[..1]).unwrap_err();
        assert!(e.contains("not installed") && e.contains("9.21.417"), "{e}");
        assert!(pick(&[]).unwrap_err().contains("no winIDEA found"));
        for d in [good, other, unknown] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

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
