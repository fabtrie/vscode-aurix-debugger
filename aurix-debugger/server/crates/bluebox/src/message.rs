//! Service messages: the payload of a frame on [`crate::frame::CHANNEL_SERVICE`].
//!
//! ```text
//! 23 00 00 00 00 <kind> <n> <path, n ASCII bytes> <len, u32 big-endian> <body, len bytes>
//! ```
//!
//! `kind` is 0x01 for nearly every service (0x11 for a few on node 1). Paths
//! look like `//<node>/<sub>/<sub>/<service>/`. The length rule held for all
//! 1651 messages checked.

pub const KIND: u8 = 0x01;

pub fn encode(kind: u8, path: &str, body: &[u8]) -> Vec<u8> {
    assert!(path.len() <= u8::MAX as usize);
    let mut m = Vec::with_capacity(11 + path.len() + body.len());
    m.extend_from_slice(&[0x23, 0, 0, 0, 0, kind, path.len() as u8]);
    m.extend_from_slice(path.as_bytes());
    m.extend_from_slice(&(body.len() as u32).to_be_bytes());
    m.extend_from_slice(body);
    m
}

/// A message's kind, path and body, if `b` starts with one.
pub fn decode(b: &[u8]) -> Option<(u8, &str, &[u8])> {
    if b.len() < 7 || b[..5] != [0x23, 0, 0, 0, 0] {
        return None;
    }
    let n = b[6] as usize;
    let path = std::str::from_utf8(b.get(7..7 + n)?).ok()?;
    let p = 7 + n;
    let len = u32::from_be_bytes(b.get(p..p + 4)?.try_into().ok()?) as usize;
    Some((b[5], path, b.get(p + 4..p + 4 + len)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let m = encode(KIND, "//2///11/", &[1, 0, 0, 0, 1]);
        assert_eq!(&m[..7], &[0x23, 0, 0, 0, 0, 1, 9]);
        assert_eq!(&m[16..20], &[0, 0, 0, 5]);
        assert_eq!(decode(&m), Some((KIND, "//2///11/", &[1u8, 0, 0, 0, 1][..])));
    }

    #[test]
    fn a_truncated_message_is_rejected() {
        let m = encode(KIND, "//3///10/", &[0; 13]);
        assert!(decode(&m[..m.len() - 1]).is_none());
    }
}
