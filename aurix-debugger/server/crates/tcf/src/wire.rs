//! TCF message framing.

use std::io::{self, Read};

pub const EOM: [u8; 2] = [3, 1];

/// Reads `\x03\x01`-terminated messages and splits them into fields.
pub struct Reader<R: Read> {
    inner: R,
    buf: Vec<u8>,
}

impl<R: Read> Reader<R> {
    pub fn new(inner: R) -> Self {
        Reader { inner, buf: Vec::new() }
    }

    pub fn next_message(&mut self) -> io::Result<Option<Vec<String>>> {
        loop {
            if let Some(pos) = self.buf.windows(2).position(|w| w == EOM) {
                let msg: Vec<u8> = self.buf.drain(..pos + 2).collect();
                return Ok(Some(split_fields(&msg[..pos])));
            }
            let mut tmp = [0u8; 4096];
            let n = self.inner.read(&mut tmp)?;
            if n == 0 {
                return Ok(None);
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }
}

/// Splits a message body on `\0`, dropping the empty field after the last separator.
pub fn split_fields(body: &[u8]) -> Vec<String> {
    let mut fields: Vec<String> = body.split(|&b| b == 0).map(|f| String::from_utf8_lossy(f).into_owned()).collect();
    if fields.last().is_some_and(|f| f.is_empty()) {
        fields.pop();
    }
    fields
}

/// Encodes fields as `f0\0f1\0...\0\x03\x01`.
pub fn encode<S: AsRef<str>>(fields: &[S]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in fields {
        out.extend_from_slice(f.as_ref().as_bytes());
        out.push(0);
    }
    out.extend_from_slice(&EOM);
    out
}

pub fn event(service: &str, name: &str, data: &[String]) -> Vec<u8> {
    let mut fields = vec!["E".to_string(), service.to_string(), name.to_string()];
    fields.extend(data.iter().cloned());
    encode(&fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_command_without_args() {
        let raw = b"C\0RunControl/0\0ContextReset\0getCapabilities\0\x03\x01";
        let mut r = Reader::new(&raw[..]);
        let f = r.next_message().unwrap().unwrap();
        assert_eq!(f, vec!["C", "RunControl/0", "ContextReset", "getCapabilities"]);
        assert!(r.next_message().unwrap().is_none());
    }

    #[test]
    fn two_messages_split_across_reads() {
        let raw = b"E\0Locator\0Hello\0[\"Locator\"]\0\x03\x01C\0t\0Memory\0get\0\"S0.D0.C0\"\0123\0\x03\x01";
        let mut r = Reader::new(&raw[..]);
        assert_eq!(r.next_message().unwrap().unwrap()[3], "[\"Locator\"]");
        let m = r.next_message().unwrap().unwrap();
        assert_eq!(m, vec!["C", "t", "Memory", "get", "\"S0.D0.C0\"", "123"]);
    }

    #[test]
    fn encode_reply() {
        assert_eq!(encode(&["R", "tok", "", "[]"]), b"R\0tok\0\0[]\0\x03\x01".to_vec());
    }
}
