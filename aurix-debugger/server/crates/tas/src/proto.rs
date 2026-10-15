//! Constants and little-endian helpers from `tas_pkt.h`.

pub const DEFAULT_PORT: u16 = 24817;

// Error codes (tas_pl_err_et)
pub const ERR_NO_ERROR: u8 = 0x08;
pub const ERR_PARAM: u8 = 0x10;
pub const ERR_NOT_SUPPORTED: u8 = 0x12;
pub const ERR_USAGE: u8 = 0x14;
pub const ERR_PROTOCOL: u8 = 0x1E;
pub const PL1_ERR_CMD_FAILED: u8 = 0x20;
pub const PL1_ERR_SERVER_LOCKED: u8 = 0x32;
pub const PL1_ERR_SESSION: u8 = 0x38;
pub const PL1_ERR_DEV_RESET: u8 = 0x44;
pub const PL1_ERR_DEV_LOCKED: u8 = 0x46;
pub const PL1_ERR_DEV_ACCESS: u8 = 0x48;
pub const PL0_ERR_NO_ERROR: u8 = 0x80;
pub const PL0_ERR_ACC_MODE: u8 = 0x89;
pub const PL0_ERR_ADDR_MAP: u8 = 0x8A;
pub const PL0_ERR_ADDR_BLOCKED: u8 = 0x8B;
pub const PL0_ERR_DATA: u8 = 0x8D;
pub const PL0_ERR_CONSEQUENTIAL: u8 = 0x98;

// PL0 commands
pub const PL0_ADDR_MAP: u8 = 0x11;
pub const PL0_BASE_ADDR32: u8 = 0x15;
pub const PL0_BASE_ADDR64: u8 = 0x16;
pub const PL0_WR8: u8 = 0x20;
pub const PL0_RD8: u8 = 0x21;
pub const PL0_WR16: u8 = 0x22;
pub const PL0_RD16: u8 = 0x23;
pub const PL0_WR32: u8 = 0x24;
pub const PL0_RD32: u8 = 0x25;
pub const PL0_WR64: u8 = 0x26;
pub const PL0_RD64: u8 = 0x27;
pub const PL0_WRBLK: u8 = 0x2A;
pub const PL0_RDBLK: u8 = 0x2C;
pub const PL0_RDBLK1KB: u8 = 0x2D;

// PL1 commands
pub const PL1_SERVER_CONNECT: u8 = 0x80;
pub const PL1_GET_TARGETS: u8 = 0x86;
pub const PL1_SESSION_START: u8 = 0xA1;
pub const PL1_PING: u8 = 0xA2;
pub const PL1_DEVICE_CONNECT: u8 = 0xA8;
pub const PL1_PL0_START: u8 = 0xB0;
pub const PL1_PL0_END: u8 = 0xB1;
pub const PL1_ERROR: u8 = 0xEE;

pub const CLIENT_TYPE_RW: u8 = 1;
/// Protocol version put into PL0_START by the RW packet handler.
pub const PL0_PROTOC_VER: u8 = 0;
/// Protocol version the session/ping responses must support.
pub const PROTOC_VER: u8 = 1;

pub const PL2_MAX_PKT_SIZE: u32 = 0x10010;
pub const PL2_MAX_PKT_SIZE_MIN: u32 = 128;
pub const PL0_DATA_BLK_SIZE: usize = 1024;

pub fn pl_err_name(err: u8) -> &'static str {
    match err {
        ERR_NO_ERROR | PL0_ERR_NO_ERROR => "no error",
        ERR_PARAM => "wrong parameter",
        ERR_NOT_SUPPORTED => "not supported",
        ERR_USAGE => "wrong usage",
        ERR_PROTOCOL => "protocol error",
        PL1_ERR_CMD_FAILED => "command failed",
        PL1_ERR_SERVER_LOCKED => "server locked",
        PL1_ERR_SESSION => "session name or password mismatch",
        PL1_ERR_DEV_RESET => "device reset detected",
        PL1_ERR_DEV_LOCKED => "device locked",
        PL1_ERR_DEV_ACCESS => "device access failed",
        PL0_ERR_ACC_MODE => "access mode not supported",
        PL0_ERR_ADDR_MAP => "address map not supported",
        PL0_ERR_ADDR_BLOCKED => "address blocked",
        PL0_ERR_DATA => "read/write access not (completely) successful",
        PL0_ERR_CONSEQUENTIAL => "consequential error",
        _ => "unknown error",
    }
}

/// Little-endian writer for word-aligned packets.
#[derive(Default)]
pub struct Wr {
    pub buf: Vec<u8>,
}

impl Wr {
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// Fixed-size, NUL-padded C string (always NUL terminated).
    pub fn cstr(&mut self, s: &str, len: usize) -> &mut Self {
        let b = s.as_bytes();
        let n = b.len().min(len - 1);
        self.buf.extend_from_slice(&b[..n]);
        self.buf.resize(self.buf.len() + (len - n), 0);
        self
    }
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }
    pub fn len(&self) -> usize {
        self.buf.len()
    }
}

pub fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
pub fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
pub fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}
pub fn cstr_at(b: &[u8], off: usize, len: usize) -> String {
    let s = &b[off..off + len];
    let end = s.iter().position(|&c| c == 0).unwrap_or(len);
    String::from_utf8_lossy(&s[..end]).into_owned()
}
