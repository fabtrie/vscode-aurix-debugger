//! The probe's debug FPGA: which image an iC5700 holds; loading an iC5000's ([`ic5000`]).
//!
//! The image is persistent: the probe keeps it in flash, and winIDEA only
//! loads one when the probe's differs from the one its version needs, that is
//! after another winIDEA version used the probe (`first.jsonl`: 9.21.356
//! replaced 9.21.417's image, about 47 s, then restarted the kernel). This
//! crate does not load images: the ones winIDEA ships (`ICWHW32.HRS`) are not
//! decoded. It only checks, after the attach's first messages, that the probe
//! holds 9.21.356's, the version this crate's attach and service numbers are
//! from. A probe with another image is brought back by connecting once with
//! winIDEA 9.21.356.
//!
//! Status query, kind 0x11 on node 1: `//1///35/ 01`, answered with 34 bytes:
//! `<type u32 BE> <size u32 BE> 42 00 00 01 00 00 00 00 <MD5> 01 03`. The MD5
//! is that of the first `size` bytes winIDEA uploaded.

use crate::{hex, Error, Link, Probe, Result};

pub const STATUS: &str = "//1///35/";

/// The debug FPGA's type, as its status gives it.
pub const DEBUG_FPGA: u32 = 0x0094_7a5c;

/// An image the probe can hold: its size and MD5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wanted {
    pub size: u32,
    pub md5: [u8; 16],
}

/// What winIDEA 9.21.356 needs (the status after it loaded its image).
pub const WANTED_9_21_356: Wanted = Wanted {
    size: 0x0030_553a,
    md5: [0x84, 0xa3, 0x47, 0x0a, 0x80, 0x2f, 0x45, 0x4c, 0xe0, 0xca, 0xbd, 0x26, 0x9f, 0x66, 0xf5, 0x5a],
};

/// The answer to [`STATUS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub kind: u32,
    pub size: u32,
    pub md5: [u8; 16],
}

impl Status {
    pub fn parse(a: &[u8]) -> Result<Status> {
        if a.len() < 32 {
            return Err(Error::Fpga(format!("status answer {}", hex(a, 40))));
        }
        let be = |o: usize| u32::from_be_bytes(a[o..o + 4].try_into().unwrap());
        Ok(Status { kind: be(0), size: be(4), md5: a[16..32].try_into().unwrap() })
    }

    /// The probe holds `w`.
    pub fn holds(&self, w: &Wanted) -> bool {
        self.kind == DEBUG_FPGA && self.size == w.size && self.md5 == w.md5
    }

    /// The MD5 in hex, for messages.
    pub fn md5_hex(&self) -> String {
        self.md5.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl<L: Link> Probe<L> {
    /// The debug FPGA image the probe holds.
    pub fn fpga_status(&mut self) -> Result<Status> {
        let a = self.control(STATUS, &[0x01], 34)?;
        Status::parse(&a)
    }
}

/// The iC5000's debug FPGA, which winIDEA 9.21.356 uploads on every attach,
/// warm or cold (`ic5000.jsonl`, `ic5000-warm.jsonl`): after `//1///41/`,
///
/// 1. `//1///7/ 02`,
/// 2. five `//1///10/` messages, each `<length u32 LE> <data>`, together the
///    image's 266,599 bytes,
/// 3. `//1///9/ 02`.
///
/// Each is answered with a service reply whose byte 0x1e is 2. The image is
/// stored as-is in winIDEA's `ICWHW32.HRS`, as an entry
/// `01 83 12 12 09 ff <length u32 LE> <data>`: the second of six entries of
/// that type. [`ic5000::find_image`] picks it by length and hash, so nothing of it is
/// kept here. Its chunks are not of one size (61,401 to 61,405 bytes, then the
/// rest); they are taken over as winIDEA cut them ([`ic5000::IMAGE_9_21_356`]).
pub mod ic5000 {
    use crate::frame::{READ_WINDOW, REPLY_CAPACITY};
    use crate::probe::{Exchange, Fetch};
    use crate::{hex, message, Error, Link, Probe, Result};

    /// winIDEA's resource file that holds the image.
    pub const HRS_FILE: &str = "ICWHW32.HRS";
    /// Starts the load; body `02`.
    pub const BEGIN: &str = "//1///7/";
    /// One piece of the image.
    pub const CHUNK: &str = "//1///10/";
    /// Ends the load; body `02`.
    pub const END: &str = "//1///9/";
    /// Header of an image entry in [`HRS_FILE`]; the length follows.
    pub const ENTRY_TAG: [u8; 6] = [0x01, 0x83, 0x12, 0x12, 0x09, 0xff];

    /// An image: its length, FNV-1a 64 hash and the lengths of its chunks.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Image {
        pub len: usize,
        pub fnv: u64,
        pub chunks: [usize; 5],
    }

    /// The image winIDEA 9.21.356 uploads (MD5 `919fd732af530d8d48a8f247939e5982`).
    pub const IMAGE_9_21_356: Image =
        Image { len: 266_599, fnv: 0xed41_5f83_0f80_98b4, chunks: [61_405, 61_405, 61_401, 61_403, 20_985] };

    pub fn fnv1a(d: &[u8]) -> u64 {
        d.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3))
    }

    /// The entry of [`HRS_FILE`]'s contents that is `want`.
    pub fn find_image(hrs: &[u8], want: &Image) -> Option<Vec<u8>> {
        let mut at = 0;
        while let Some(i) = hrs[at..].windows(ENTRY_TAG.len()).position(|w| w == ENTRY_TAG) {
            let start = at + i + ENTRY_TAG.len() + 4;
            let len = hrs.get(start - 4..start).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
            if len == Some(want.len) {
                if let Some(d) = hrs.get(start..start + want.len) {
                    if fnv1a(d) == want.fnv {
                        return Some(d.to_vec());
                    }
                }
            }
            at = start - 4;
        }
        None
    }

    /// The image from the winIDEA install in `dir`.
    pub fn image_from(dir: &std::path::Path) -> std::result::Result<Vec<u8>, String> {
        let p = dir.join(HRS_FILE);
        let hrs = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        find_image(&hrs, &IMAGE_9_21_356).ok_or_else(|| {
            format!(
                "{} holds no iC5000 debug FPGA image of winIDEA {} ({} bytes)",
                p.display(),
                crate::kernel::WINIDEA_VERSION,
                IMAGE_9_21_356.len
            )
        })
    }

    fn exchange(path: &str, body: &[u8], channel: u8) -> Exchange {
        Exchange {
            payload: message::encode(message::KIND, path, body),
            fetch: Fetch::new(0x0e, READ_WINDOW, channel),
            reads: vec![REPLY_CAPACITY],
            expect: Vec::new(),
            pause_ms: 0,
        }
    }

    impl<L: Link> Probe<L> {
        /// Uploads the iC5000's debug FPGA `image` (from [`image_from`]), as winIDEA does.
        pub fn load_fpga_ic5000(&mut self, image: &[u8]) -> Result<()> {
            let want = &IMAGE_9_21_356;
            if image.len() != want.len || fnv1a(image) != want.fnv {
                return Err(Error::Fpga(format!("not winIDEA 9.21.356's iC5000 image ({} bytes)", image.len())));
            }
            let mut steps = vec![exchange(BEGIN, &[0x02], 0x02)];
            let mut at = 0;
            for n in want.chunks {
                let mut body = (n as u32).to_le_bytes().to_vec();
                body.extend_from_slice(&image[at..at + n]);
                at += n;
                steps.push(exchange(CHUNK, &body, 0x10));
            }
            steps.push(exchange(END, &[0x02], 0x02));
            for x in &steps {
                let r = self.exchange(x)?.remove(0);
                if r.len() <= 0x1e || !message::is_reply(&r) || r[0x1e] != 2 {
                    let what = message::decode(&x.payload).map(|(_, p, _)| p.to_string()).unwrap_or_default();
                    return Err(Error::Fpga(format!("{what} answered {}", hex(&r, 32))));
                }
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::link::ScriptedLink;

        #[test]
        fn the_chunks_make_the_image() {
            assert_eq!(IMAGE_9_21_356.chunks.iter().sum::<usize>(), IMAGE_9_21_356.len);
        }

        #[test]
        fn the_image_is_found_by_length_and_hash() {
            let want = Image { len: 5, fnv: fnv1a(b"abcde"), chunks: [1; 5] };
            let mut hrs = b"junk".to_vec();
            for d in [&b"abcdf"[..], b"abc", b"abcde"] {
                hrs.extend_from_slice(&ENTRY_TAG);
                hrs.extend_from_slice(&(d.len() as u32).to_le_bytes());
                hrs.extend_from_slice(d);
            }
            assert_eq!(find_image(&hrs, &want).as_deref(), Some(&b"abcde"[..]));
            assert_eq!(find_image(&hrs[..hrs.len() - 1], &want), None, "cut short");
        }

        #[test]
        fn a_wrong_image_is_not_sent() {
            let mut p = Probe::new(ScriptedLink::default(), false);
            assert!(matches!(p.load_fpga_ic5000(&[0; 10]), Err(Error::Fpga(_))));
            assert!(p.link_mut().written.is_empty());
        }

        /// A reply as captured: `02` at 0x1e.
        fn ok_reply() -> Vec<u8> {
            let mut r = vec![0u8; 32];
            r[0] = 0x0f;
            r[0x10] = 0x08;
            r[0x14] = 0x0a;
            r[0x15] = 0x01;
            r[0x1e] = 0x02;
            r
        }

        #[test]
        fn the_installed_image_loads_like_winidea() {
            // Only where winIDEA 9.21.356 is installed.
            let Ok(image) = image_from(std::path::Path::new(r"C:\winIDEA")) else { return };
            let mut link = ScriptedLink::default();
            link.replies.extend(std::iter::repeat_n(ok_reply(), 7));
            let mut p = Probe::new(link, false);
            p.load_fpga_ic5000(&image).unwrap();
            let msgs: Vec<(String, usize)> = p
                .link_mut()
                .written
                .iter()
                .filter_map(|(_, d)| {
                    let m = if d.len() > 8 && d[0] & 0x7f == 0x6a { &d[8..] } else { &d[..] };
                    message::decode(m).map(|(_, p, b)| (p.to_string(), b.len()))
                })
                .collect();
            let want: Vec<(String, usize)> = [(BEGIN, 1), (CHUNK, 61_409), (CHUNK, 61_409), (CHUNK, 61_405), (CHUNK, 61_407), (CHUNK, 20_989), (END, 1)]
                .iter()
                .map(|(p, n)| (p.to_string(), *n))
                .collect();
            assert_eq!(msgs, want);
            let fetch_channels: Vec<u8> =
                p.link_mut().written.iter().filter(|(_, d)| d.len() == 8 && d[0] & 0x7f == 0x0e).map(|(_, d)| d[7]).collect();
            assert_eq!(fetch_channels, [0x02, 0x10, 0x10, 0x10, 0x10, 0x10, 0x02]);
        }

        #[test]
        fn a_refused_chunk_stops_the_load() {
            let Ok(image) = image_from(std::path::Path::new(r"C:\winIDEA")) else { return };
            let mut link = ScriptedLink::default();
            let mut bad = ok_reply();
            bad[0x1e] = 0;
            link.replies.extend([ok_reply(), bad]);
            let mut p = Probe::new(link, false);
            let e = p.load_fpga_ic5000(&image).unwrap_err();
            assert!(e.to_string().contains(CHUNK), "{e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::ScriptedLink;

    /// 9.21.417's image, as 9.21.356 found it (first.jsonl).
    const STATUS_9_21_417: [u8; 34] = [
        0x00, 0x94, 0x7a, 0x5c, 0x00, 0x31, 0x6f, 0xd8, 0x42, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0xed, 0x36, 0xb5,
        0xcf, 0x82, 0xfb, 0xeb, 0x5a, 0x05, 0xd8, 0x02, 0xd4, 0xfa, 0x02, 0x64, 0x05, 0x01, 0x03,
    ];

    #[test]
    fn a_status_parses_and_tells_the_image() {
        let s = Status::parse(&STATUS_9_21_417).unwrap();
        assert_eq!((s.kind, s.size), (DEBUG_FPGA, 0x0031_6fd8));
        assert_eq!(s.md5_hex(), "ed36b5cf82fbeb5a05d802d4fa026405");
        assert!(!s.holds(&WANTED_9_21_356));
        let ours = Status { kind: DEBUG_FPGA, size: WANTED_9_21_356.size, md5: WANTED_9_21_356.md5 };
        assert!(ours.holds(&WANTED_9_21_356));
        assert!(Status::parse(&STATUS_9_21_417[..20]).is_err());
    }

    #[test]
    fn the_status_query_is_winideas() {
        // As captured: header, the message, the 34-byte low-level read.
        let mut link = ScriptedLink::default();
        let mut r = vec![0x08];
        r.extend_from_slice(&STATUS_9_21_417);
        r.push(0x03);
        link.replies.push_back(r);
        let mut p = Probe::new(link, true);
        assert_eq!(p.fpga_status().unwrap().size, 0x0031_6fd8);
        let w = &p.link_mut().written;
        assert!(w[0].1[8..].starts_with(&crate::message::encode(crate::lowlevel::KIND_CONTROL, STATUS, &[0x01])));
        assert_eq!(w[1].1, [0x04, 0x00, 0x22, 0, 0, 0, 0, 0x02]);
    }
}
