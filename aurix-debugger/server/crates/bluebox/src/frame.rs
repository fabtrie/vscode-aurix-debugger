//! The 8-byte header in front of everything the host sends.
//!
//! ```text
//! byte 0     bits 0-6: kind, bit 7: toggle
//! bytes 1-2  payload length, big-endian, unpadded (a read request: the reply window)
//! bytes 3-6  zero
//! byte 7     channel (0x14: service messages)
//! ```
//!
//! A payload is padded to 4 bytes (to 2 on an iC5000: [`crate::Model::frame_pad`];
//! its largest inline payload is 56 bytes, its smallest split one 63). One that fits in a single 64-byte USB
//! packet together with its header travels with it on [`PIPE_OUT`]
//! ([`KIND_INLINE`]); a longer one goes alone on [`PIPE_OUT_BULK`] after a
//! header-only transfer ([`KIND_SPLIT`]). In the captures, the largest inline
//! payload is 56 bytes and the smallest split one 60, in 9768 frames.
//!
//! The toggle flips with every message on a channel. A read request carries
//! the value the next message will use (checked on 4884 messages and 743 read
//! requests on channel 0x14).

/// OUT pipe for headers and short frames.
pub const PIPE_OUT: u8 = 0x08;
/// OUT pipe for the payload of a [`KIND_SPLIT`] frame. winIDEA turns on
/// `SHORT_PACKET_TERMINATE` for it.
pub const PIPE_OUT_BULK: u8 = 0x06;
/// IN pipe: every reply.
pub const PIPE_IN: u8 = 0x82;

/// Service messages.
pub const CHANNEL_SERVICE: u8 = 0x14;

/// Header and payload in one transfer.
pub const KIND_INLINE: u8 = 0x6a;
/// Header now, payload in the next transfer on [`PIPE_OUT_BULK`].
pub const KIND_SPLIT: u8 = 0x63;
/// Asks the probe for its reply on [`PIPE_IN`]; no payload.
pub const KIND_READ: u8 = 0x0e;

/// Length field of a read request: always 0xf000 in the captures, which
/// reads as the largest reply the host accepts.
pub const READ_WINDOW: u16 = 0xf000;
/// Buffer winIDEA reads replies into.
pub const REPLY_CAPACITY: usize = 62480;

const USB_PACKET: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: u8,
    pub toggle: bool,
    pub len: u16,
    pub channel: u8,
}

impl Header {
    pub fn encode(&self) -> [u8; 8] {
        [
            (self.kind & 0x7f) | ((self.toggle as u8) << 7),
            (self.len >> 8) as u8,
            self.len as u8,
            0,
            0,
            0,
            0,
            self.channel,
        ]
    }

    pub fn decode(b: &[u8]) -> Option<Header> {
        if b.len() < 8 {
            return None;
        }
        Some(Header {
            kind: b[0] & 0x7f,
            toggle: b[0] & 0x80 != 0,
            len: u16::from_be_bytes([b[1], b[2]]),
            channel: b[7],
        })
    }
}

pub fn padded(n: usize) -> usize {
    (n + 3) & !3
}

/// The OUT transfers, as `(pipe, bytes)`, that carry one message to an iC5700.
pub fn message_transfers(payload: &[u8], toggle: bool, channel: u8) -> Vec<(u8, Vec<u8>)> {
    message_transfers_padded(payload, toggle, channel, 4)
}

/// The same with the payload padded to a multiple of `pad` bytes.
pub fn message_transfers_padded(payload: &[u8], toggle: bool, channel: u8, pad: usize) -> Vec<(u8, Vec<u8>)> {
    assert!(payload.len() <= u16::MAX as usize, "frame payload too long");
    let mut body = payload.to_vec();
    body.resize(payload.len().next_multiple_of(pad), 0);
    let inline = 8 + body.len() <= USB_PACKET;
    let kind = if inline { KIND_INLINE } else { KIND_SPLIT };
    let header = Header { kind, toggle, len: payload.len() as u16, channel }.encode();
    if inline {
        let mut t = header.to_vec();
        t.extend_from_slice(&body);
        vec![(PIPE_OUT, t)]
    } else {
        vec![(PIPE_OUT, header.to_vec()), (PIPE_OUT_BULK, body)]
    }
}

/// The header that asks for the reply to the last message.
pub fn read_request(toggle: bool, channel: u8) -> [u8; 8] {
    Header { kind: KIND_READ, toggle, len: READ_WINDOW, channel }.encode()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_headers_decode() {
        // A memory read (202-byte message) and its read request, from the trace.
        let h = Header::decode(&[0xe3, 0x00, 0xca, 0, 0, 0, 0, 0x14]).unwrap();
        assert_eq!(h, Header { kind: KIND_SPLIT, toggle: true, len: 202, channel: CHANNEL_SERVICE });
        let r = Header::decode(&[0x0e, 0xf0, 0x00, 0, 0, 0, 0, 0x14]).unwrap();
        assert_eq!(r, Header { kind: KIND_READ, toggle: false, len: READ_WINDOW, channel: 0x14 });
        // Lengths above 255 use byte 1.
        assert_eq!(Header::decode(&[0x63, 0x0a, 0x8c, 0, 0, 0, 0, 0x14]).unwrap().len, 2700);
    }

    #[test]
    fn short_messages_go_inline_and_long_ones_split() {
        let t = message_transfers(&[0xaa; 53], false, CHANNEL_SERVICE);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, PIPE_OUT);
        assert_eq!(t[0].1.len(), 8 + 56);
        assert_eq!(&t[0].1[..8], &[0x6a, 0x00, 53, 0, 0, 0, 0, 0x14]);

        let t = message_transfers(&[0xaa; 202], true, CHANNEL_SERVICE);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0], (PIPE_OUT, vec![0xe3, 0x00, 0xca, 0, 0, 0, 0, 0x14]));
        assert_eq!(t[1].0, PIPE_OUT_BULK);
        assert_eq!(t[1].1.len(), 204);
    }

    #[test]
    fn an_ic5000_pads_to_two_bytes() {
        // `E R2) 00 00 80`, 9 bytes, as winIDEA sent it to the iC5000 (ic5000.jsonl).
        let t = message_transfers_padded(&[0x03, 0x21, 0x45, 0x52, 0x32, 0x29, 0x00, 0x00, 0x80], true, CHANNEL_SERVICE, 2);
        assert_eq!(t, vec![(PIPE_OUT, vec![0xea, 0, 9, 0, 0, 0, 0, 0x14, 0x03, 0x21, 0x45, 0x52, 0x32, 0x29, 0, 0, 0x80, 0])]);
        assert_eq!(message_transfers_padded(&[0; 56], false, CHANNEL_SERVICE, 2).len(), 1, "56 bytes inline");
        let t = message_transfers_padded(&[0; 259], false, CHANNEL_SERVICE, 2);
        assert_eq!((t.len(), t[1].1.len()), (2, 260), "G: 259 bytes split, padded to 260");
    }

    #[test]
    fn read_request_matches_the_capture() {
        assert_eq!(read_request(true, CHANNEL_SERVICE), [0x8e, 0xf0, 0x00, 0, 0, 0, 0, 0x14]);
    }
}
