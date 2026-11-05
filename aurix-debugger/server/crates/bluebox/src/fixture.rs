//! Capture fixtures for tests (`tests/data/*.txt`, converted from capture traces):
//!
//! ```text
//! W <pipe> <hex>            host write
//! K <pipe> <len> <hex>      host write of a kernel chunk: length and first bytes
//! R <cap> <hex>             probe reply
//! ```

use crate::frame::{Header, KIND_SPLIT, PIPE_OUT, PIPE_OUT_BULK};
use crate::hex;
use crate::link::ScriptedLink;

pub(crate) struct Fixture {
    /// Pipe, bytes (or the first bytes), and for `K` the whole length.
    pub writes: Vec<(u8, Vec<u8>, Option<usize>)>,
    pub replies: Vec<Vec<u8>>,
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

pub(crate) fn parse(text: &str) -> Fixture {
    let mut f = Fixture { writes: Vec::new(), replies: Vec::new() };
    for line in text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
        let p: Vec<&str> = line.split_whitespace().collect();
        match p[0] {
            "W" => f.writes.push((u8::from_str_radix(p[1], 16).unwrap(), unhex(p[2]), None)),
            "K" => f.writes.push((u8::from_str_radix(p[1], 16).unwrap(), unhex(p[3]), Some(p[2].parse().unwrap()))),
            "R" => f.replies.push(unhex(p[2])),
            _ => panic!("{line}"),
        }
    }
    f
}

impl Fixture {
    pub fn link(&self) -> ScriptedLink {
        ScriptedLink { replies: self.replies.iter().cloned().collect(), ..Default::default() }
    }

    /// The same, as a link to `model`.
    pub fn link_to(&self, model: crate::Model) -> ScriptedLink {
        ScriptedLink { model, ..self.link() }
    }
}

/// Byte-for-byte, except a split payload's padding beyond the length its
/// header gives (winIDEA leaves stale bytes there, this crate zeros), and of a
/// `K` write only its first bytes.
pub(crate) fn compare(ours: &[(u8, Vec<u8>)], theirs: &Fixture) {
    let mut split_len: Option<usize> = None;
    for (i, ((pipe, data), (tp, td, chunk))) in ours.iter().zip(&theirs.writes).enumerate() {
        assert_eq!(pipe, tp, "write {i}: pipe ({})", hex(data, 16));
        let meaningful = match split_len.take() {
            Some(n) if *pipe == PIPE_OUT_BULK => n,
            _ => data.len(),
        };
        if *pipe == PIPE_OUT && data.len() == 8 {
            split_len = Header::decode(data).filter(|h| h.kind == KIND_SPLIT).map(|h| h.len as usize);
        }
        match chunk {
            None => {
                assert_eq!(data.len(), td.len(), "write {i}: length ({} vs {})", hex(data, 24), hex(td, 24));
                assert_eq!(
                    &data[..meaningful],
                    &td[..meaningful],
                    "write {i} differs:\n ours  {}\n theirs {}",
                    hex(data, 64),
                    hex(td, 64)
                )
            }
            Some(n) => {
                assert_eq!(data.len(), *n, "write {i}: chunk length");
                assert_eq!(&data[..td.len()], &td[..], "write {i}: chunk head");
            }
        }
    }
    assert_eq!(ours.len(), theirs.writes.len(), "number of host writes");
}
