//! Batched PL0 read/write transactions and their PL2 packet encoding.

use crate::proto::*;
use crate::{Error, Result};

/// A read or write queued in a [`Batch`].
#[derive(Debug, Clone)]
pub(crate) enum OpKind {
    Read { len: usize },
    Write { data: Vec<u8> },
}

#[derive(Debug, Clone)]
pub(crate) struct Op {
    pub addr: u64,
    pub addr_map: u8,
    pub kind: OpKind,
}

/// Index of an operation in a batch, used to look up its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpHandle(pub usize);

/// Outcome of one queued operation.
#[derive(Debug, Clone, Default)]
pub struct OpResult {
    /// PL0 error code of the first failing piece, or `PL0_ERR_NO_ERROR`.
    pub err: u8,
    /// Bytes read or written successfully, counted from the start of the op.
    pub bytes_ok: usize,
    /// Read data (zero-filled beyond `bytes_ok`). Empty for writes.
    pub data: Vec<u8>,
}

impl OpResult {
    pub fn ok(&self) -> bool {
        self.err == PL0_ERR_NO_ERROR
    }
    pub fn u32(&self) -> u32 {
        le32(&self.data, 0)
    }
}

/// An ordered list of reads and writes executed atomically per PL2 packet.
#[derive(Debug, Default, Clone)]
pub struct Batch {
    pub(crate) ops: Vec<Op>,
}

impl Batch {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.ops.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
    pub fn read(&mut self, addr: u64, len: usize) -> OpHandle {
        self.read_am(addr, len, 0)
    }
    pub fn read_am(&mut self, addr: u64, len: usize, addr_map: u8) -> OpHandle {
        self.ops.push(Op { addr, addr_map, kind: OpKind::Read { len } });
        OpHandle(self.ops.len() - 1)
    }
    pub fn read32(&mut self, addr: u64) -> OpHandle {
        self.read(addr, 4)
    }
    pub fn write(&mut self, addr: u64, data: &[u8]) -> OpHandle {
        self.write_am(addr, data, 0)
    }
    pub fn write_am(&mut self, addr: u64, data: &[u8], addr_map: u8) -> OpHandle {
        self.ops.push(Op { addr, addr_map, kind: OpKind::Write { data: data.to_vec() } });
        OpHandle(self.ops.len() - 1)
    }
    pub fn write32(&mut self, addr: u64, value: u32) -> OpHandle {
        self.write(addr, &value.to_le_bytes())
    }
    /// Read-only view of the queued operations (used by simulated adapters).
    pub fn iter(&self) -> impl Iterator<Item = BatchOp<'_>> {
        self.ops.iter().map(|op| match &op.kind {
            OpKind::Read { len } => BatchOp::Read { addr: op.addr, len: *len, addr_map: op.addr_map },
            OpKind::Write { data } => BatchOp::Write { addr: op.addr, data, addr_map: op.addr_map },
        })
    }
}

/// Public view of a queued operation.
#[derive(Debug, Clone, Copy)]
pub enum BatchOp<'a> {
    Read { addr: u64, len: usize, addr_map: u8 },
    Write { addr: u64, data: &'a [u8], addr_map: u8 },
}

/// One PL0 read/write transaction (a piece of an [`Op`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Piece {
    pub op: usize,
    pub offset: usize,
    pub addr: u64,
    pub len: usize,
    pub addr_map: u8,
    pub write: bool,
}

/// Splits an op into aligned PL0 pieces the way the TAS client API does:
/// odd byte, 16 bit, 32 bit heads up to 64 bit alignment, then 64 bit aligned
/// blocks of up to 1 KB, then 32/16/8 bit tails.
pub(crate) fn split_op(op_index: usize, op: &Op, out: &mut Vec<Piece>) {
    let (write, total) = match &op.kind {
        OpKind::Read { len } => (false, *len),
        OpKind::Write { data } => (true, data.len()),
    };
    let mut a = op.addr;
    let mut off = 0usize;
    let mut nb = total;
    let mut push = |a: u64, off: usize, len: usize| {
        out.push(Piece { op: op_index, offset: off, addr: a, len, addr_map: op.addr_map, write });
    };
    if nb > 0 && a & 1 != 0 {
        push(a, off, 1);
        a += 1; off += 1; nb -= 1;
    }
    if nb >= 2 && a & 2 != 0 {
        push(a, off, 2);
        a += 2; off += 2; nb -= 2;
    }
    if nb >= 4 && a & 4 != 0 {
        push(a, off, 4);
        a += 4; off += 4; nb -= 4;
    }
    while nb >= 8 {
        let blk = (nb & !7).min(PL0_DATA_BLK_SIZE);
        push(a, off, blk);
        a += blk as u64; off += blk; nb -= blk;
    }
    if nb >= 4 {
        push(a, off, 4);
        a += 4; off += 4; nb -= 4;
    }
    if nb >= 2 {
        push(a, off, 2);
        a += 2; off += 2; nb -= 2;
    }
    if nb > 0 {
        push(a, off, 1);
    }
}

fn round4(n: usize) -> usize {
    (n + 3) & !3
}

/// Request bytes of a piece's PL0 command (excluding addr map / base address).
fn piece_rq_size(p: &Piece) -> usize {
    match (p.write, p.len) {
        (false, 1 | 2 | 4 | 8) => 4,
        (false, _) => 8,
        (true, 1 | 2 | 4) => 8,
        (true, 8) => 12,
        (true, n) => 4 + round4(n),
    }
}

/// Response bytes of a piece.
fn piece_rsp_size(p: &Piece) -> usize {
    if p.write {
        4
    } else {
        4 + round4(p.len.max(4))
    }
}

pub(crate) struct Pl2Packet {
    pub bytes: Vec<u8>,
    pub pl1_cnt: u16,
    /// Range into the piece list.
    pub pieces: std::ops::Range<usize>,
}

pub(crate) struct Limits {
    pub max_rq: usize,
    pub max_rsp: usize,
    pub max_num_rw: usize,
}

struct PktBuilder {
    w: Wr,
    map: u8,
    map_mask: u16,
    base: Option<u64>,
    rsp_size: usize,
    n: usize,
    first_piece: usize,
}

impl PktBuilder {
    fn new(first_piece: usize) -> Self {
        let mut w = Wr::default();
        w.u32(0); // PL2 length, patched in finish()
        w.u32(0).u32(0); // PL0_START, patched in finish()
        Self { w, map: 0, map_mask: 0, base: None, rsp_size: 4 + 4 + 4, n: 0, first_piece }
    }

    /// Bytes this piece needs in the request, including addr map/base changes.
    fn rq_needed(&self, p: &Piece) -> usize {
        let mut n = piece_rq_size(p);
        if p.addr_map != self.map {
            n += 4;
        }
        if !self.base_covers(p.addr) {
            n += if p.addr < 0x1_0000_0000 { 4 } else { 8 };
        }
        n
    }

    fn base_covers(&self, addr: u64) -> bool {
        matches!(self.base, Some(b) if addr >= b && addr <= b + 0xFFFF)
    }

    fn map_compatible(&self, map: u8) -> bool {
        let m = 1u16 << map;
        // Address maps >= 12 must be used exclusively in a PL1 packet.
        self.map_mask == 0 || self.map_mask == m || (m & 0x0FFF != 0 && self.map_mask & 0xF000 == 0)
    }

    fn fits(&self, p: &Piece, lim: &Limits) -> bool {
        self.n < lim.max_num_rw
            && self.map_compatible(p.addr_map)
            && self.w.len() + self.rq_needed(p) + 4 <= lim.max_rq
            && self.rsp_size + piece_rsp_size(p) <= lim.max_rsp
    }

    fn add(&mut self, p: &Piece, data: &[u8]) {
        if p.addr_map != self.map {
            self.w.u8(0).u8(PL0_ADDR_MAP).u8(p.addr_map).u8(0);
            self.map = p.addr_map;
        }
        self.map_mask |= 1 << p.addr_map;
        if !self.base_covers(p.addr) {
            let base = p.addr & !0xFFFF;
            if p.addr < 0x1_0000_0000 {
                self.w.u8(0).u8(PL0_BASE_ADDR32).u16((p.addr >> 16) as u16);
            } else {
                self.w.u8(1).u8(PL0_BASE_ADDR64).u16((p.addr >> 16) as u16).u32((p.addr >> 32) as u32);
            }
            self.base = Some(base);
        }
        let a15 = (p.addr & 0xFFFF) as u16;
        if p.write {
            match p.len {
                1 | 2 | 4 => {
                    let cmd = match p.len { 1 => PL0_WR8, 2 => PL0_WR16, _ => PL0_WR32 };
                    let mut word = [0u8; 4];
                    word[..p.len].copy_from_slice(data);
                    self.w.u8(1).u8(cmd).u16(a15).bytes(&word);
                }
                8 => {
                    self.w.u8(2).u8(PL0_WR64).u16(a15).bytes(data);
                }
                n => {
                    let words = n / 4;
                    self.w.u8(if words == 256 { 0 } else { words as u8 }).u8(PL0_WRBLK).u16(a15).bytes(data);
                }
            }
        } else {
            match p.len {
                1 | 2 | 4 | 8 => {
                    let cmd = match p.len { 1 => PL0_RD8, 2 => PL0_RD16, 4 => PL0_RD32, _ => PL0_RD64 };
                    self.w.u8(0).u8(cmd).u16(a15);
                }
                n => {
                    let words = n / 4;
                    self.w.u8(1).u8(PL0_RDBLK).u16(a15).u8(if words == 256 { 0 } else { words as u8 }).u8(0).u16(0);
                }
            }
        }
        self.rsp_size += piece_rsp_size(p);
        self.n += 1;
    }

    fn finish(mut self, pl1_cnt: u16, piece_end: usize) -> Pl2Packet {
        self.w.u8(0).u8(PL1_PL0_END).u16(self.n as u16);
        let len = self.w.len() as u32;
        let b = &mut self.w.buf;
        b[0..4].copy_from_slice(&len.to_le_bytes());
        // tas_pl1rq_pl0_start_st: wl=1, cmd, con_id, protoc_ver, pl1_cnt, addr_map_mask
        b[4] = 1;
        b[5] = PL1_PL0_START;
        b[6] = 0xFF;
        b[7] = PL0_PROTOC_VER;
        b[8..10].copy_from_slice(&pl1_cnt.to_le_bytes());
        let mask = if self.map_mask == 0 { 1 } else { self.map_mask };
        b[10..12].copy_from_slice(&mask.to_le_bytes());
        Pl2Packet { bytes: std::mem::take(b), pl1_cnt, pieces: self.first_piece..piece_end }
    }
}

/// Encodes the pieces into PL2 packets respecting the connection limits.
pub(crate) fn encode(
    batch: &Batch,
    pieces: &[Piece],
    lim: &Limits,
    pl1_cnt: &mut u16,
) -> Vec<Pl2Packet> {
    let mut pkts = Vec::new();
    let mut cur = PktBuilder::new(0);
    for (i, p) in pieces.iter().enumerate() {
        if cur.n > 0 && !cur.fits(p, lim) {
            let next = PktBuilder::new(i);
            let done = std::mem::replace(&mut cur, next);
            *pl1_cnt = pl1_cnt.wrapping_add(1);
            pkts.push(done.finish(*pl1_cnt, i));
        }
        let data: &[u8] = match &batch.ops[p.op].kind {
            OpKind::Write { data } => &data[p.offset..p.offset + p.len],
            OpKind::Read { .. } => &[],
        };
        cur.add(p, data);
    }
    if cur.n > 0 {
        *pl1_cnt = pl1_cnt.wrapping_add(1);
        pkts.push(cur.finish(*pl1_cnt, pieces.len()));
    }
    pkts
}

/// Parses one PL2 response packet into the results. Returns whether the
/// server flagged a device reset in PL0_START.
pub(crate) fn decode(
    rsp: &[u8],
    pkt: &Pl2Packet,
    pieces: &[Piece],
    results: &mut [OpResult],
) -> Result<bool> {
    let proto = |m: &str| Error::Protocol(m.to_string());
    if rsp.len() < 16 {
        return Err(proto("PL0 response too short"));
    }
    // PL0_START response: wl=0, cmd, con_id, err
    if rsp[5] != PL1_PL0_START {
        return Err(Error::Protocol(format!("expected PL0_START response, got cmd 0x{:02X}", rsp[5])));
    }
    let mut reset = false;
    match rsp[7] {
        ERR_NO_ERROR => {}
        PL1_ERR_DEV_RESET => reset = true,
        PL1_ERR_DEV_ACCESS => return Err(Error::DeviceAccess),
        PL1_ERR_DEV_LOCKED => return Err(Error::DeviceLocked),
        e => {
            return Err(Error::Command { cmd: PL1_PL0_START, err: e, msg: "PL0 frame refused".into() })
        }
    }
    let mut off = 8;
    for p in &pieces[pkt.pieces.clone()] {
        if off + 4 > rsp.len() {
            return Err(proto("PL0 response truncated"));
        }
        let wl = rsp[off] as usize;
        let cmd = rsp[off + 1];
        let wlrw = rsp[off + 2] as usize;
        let err = rsp[off + 3];
        let res = &mut results[p.op];
        if p.write {
            if !matches!(cmd, PL0_WR8 | PL0_WR16 | PL0_WR32 | PL0_WR64 | PL0_WRBLK) {
                return Err(Error::Protocol(format!("expected write response, got cmd 0x{cmd:02X}")));
            }
            let ok_bytes = if err == PL0_ERR_NO_ERROR {
                p.len
            } else if cmd == PL0_WRBLK {
                (wlrw * 4).min(p.len)
            } else {
                0
            };
            account(res, p, err, ok_bytes);
            off += 4 + wl * 4;
        } else {
            let (words, data_off) = if cmd == PL0_RDBLK1KB { (256, off + 4) } else { (wl, off + 4) };
            if !matches!(cmd, PL0_RD8 | PL0_RD16 | PL0_RD32 | PL0_RD64 | PL0_RDBLK | PL0_RDBLK1KB) {
                return Err(Error::Protocol(format!("expected read response, got cmd 0x{cmd:02X}")));
            }
            if data_off + words * 4 > rsp.len() {
                return Err(proto("PL0 read data truncated"));
            }
            let ok_bytes = if err == PL0_ERR_NO_ERROR {
                p.len
            } else if cmd == PL0_RDBLK {
                (wlrw * 4).min(p.len)
            } else {
                0
            };
            let n = ok_bytes.min(words * 4);
            res.data[p.offset..p.offset + n].copy_from_slice(&rsp[data_off..data_off + n]);
            account(res, p, err, ok_bytes);
            off = data_off + words * 4;
        }
    }
    if off + 4 > rsp.len() || rsp[off + 1] != PL1_PL0_END {
        return Err(proto("missing PL0_END response"));
    }
    let cnt = le16(rsp, off + 2);
    if cnt != pkt.pl1_cnt {
        return Err(Error::Protocol(format!("PL1 count mismatch: sent {}, got {cnt}", pkt.pl1_cnt)));
    }
    Ok(reset)
}

fn account(res: &mut OpResult, p: &Piece, err: u8, ok_bytes: usize) {
    // Bytes only count while all previous pieces of the op succeeded.
    if res.err == PL0_ERR_NO_ERROR && res.bytes_ok == p.offset {
        res.bytes_ok += ok_bytes;
        if err != PL0_ERR_NO_ERROR {
            res.err = err;
        }
    } else if res.err == PL0_ERR_NO_ERROR && err != PL0_ERR_NO_ERROR {
        res.err = err;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces_of(addr: u64, len: usize) -> Vec<(u64, usize)> {
        let op = Op { addr, addr_map: 0, kind: OpKind::Read { len } };
        let mut v = Vec::new();
        split_op(0, &op, &mut v);
        v.iter().map(|p| (p.addr, p.len)).collect()
    }

    #[test]
    fn split_aligned_word() {
        assert_eq!(pieces_of(0xF881FE08, 4), vec![(0xF881FE08, 4)]);
        assert_eq!(pieces_of(0xF881FE00, 4), vec![(0xF881FE00, 4)]);
    }

    #[test]
    fn split_unaligned_block() {
        assert_eq!(
            pieces_of(0x70000001, 2000),
            vec![(0x70000001, 1), (0x70000002, 2), (0x70000004, 4), (0x70000008, 1024), (0x70000408, 968), (0x700007D0, 1)]
        );
    }

    #[test]
    fn encode_single_read32() {
        let mut b = Batch::new();
        b.read32(0xF881FE08);
        let mut pieces = Vec::new();
        split_op(0, &b.ops[0], &mut pieces);
        let lim = Limits { max_rq: 1024, max_rsp: 1024, max_num_rw: 16 };
        let mut cnt = 0;
        let pkts = encode(&b, &pieces, &lim, &mut cnt);
        assert_eq!(pkts.len(), 1);
        let w: Vec<u32> = pkts[0].bytes.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(w[0], 24); // PL2 length
        assert_eq!(w[1], 0x00FF_B001); // wl=1, cmd=B0, con_id=FF, ver=0
        assert_eq!(w[2], 0x0001_0001); // pl1_cnt=1, map mask=1
        assert_eq!(w[3], 0xF881_1500); // BASE_ADDR32
        assert_eq!(w[4], 0xFE08_2500); // RD32 @ 0xFE08
        assert_eq!(w[5], 0x0001_B100); // PL0_END, 1 transaction
    }

    #[test]
    fn decode_read32() {
        let mut b = Batch::new();
        b.read32(0xF881FE08);
        let mut pieces = Vec::new();
        split_op(0, &b.ops[0], &mut pieces);
        let lim = Limits { max_rq: 1024, max_rsp: 1024, max_num_rw: 16 };
        let mut cnt = 0;
        let pkts = encode(&b, &pieces, &lim, &mut cnt);
        let mut rsp = Wr::default();
        rsp.u32(20).u8(0).u8(PL1_PL0_START).u8(0).u8(ERR_NO_ERROR);
        rsp.u8(1).u8(PL0_RD32).u8(1).u8(PL0_ERR_NO_ERROR).u32(0x8000_1234);
        rsp.u8(0).u8(PL1_PL0_END).u16(1);
        let mut results = vec![OpResult { err: PL0_ERR_NO_ERROR, bytes_ok: 0, data: vec![0; 4] }];
        let reset = decode(&rsp.buf, &pkts[0], &pieces, &mut results).unwrap();
        assert!(!reset);
        assert!(results[0].ok());
        assert_eq!(results[0].u32(), 0x8000_1234);
    }

    #[test]
    fn packets_split_on_num_rw() {
        let mut b = Batch::new();
        for i in 0..10 {
            b.read32(0x70000000 + i * 4);
        }
        let mut pieces = Vec::new();
        for (i, op) in b.ops.iter().enumerate() {
            split_op(i, op, &mut pieces);
        }
        let lim = Limits { max_rq: 1024, max_rsp: 1024, max_num_rw: 4 };
        let mut cnt = 0;
        let pkts = encode(&b, &pieces, &lim, &mut cnt);
        assert_eq!(pkts.len(), 3);
        assert_eq!(pkts[2].pieces, 8..10);
    }
}
