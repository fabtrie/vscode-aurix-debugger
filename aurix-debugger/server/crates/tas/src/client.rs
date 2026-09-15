//! TAS read/write client: server connection, session and batched RW access.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::batch::{self, Batch, Limits, OpKind, OpResult, Piece};
use crate::proto::*;
use crate::{Error, Result};

#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub name: String,
    pub v_major: u16,
    pub v_minor: u16,
    pub date: String,
    pub start_time_us: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TargetInfo {
    /// Unique access hardware name (e.g. miniWiggler serial) or device IP.
    pub identifier: String,
    /// IEEE 1149.1 device ID, 0 if no device connected.
    pub device_type: u32,
    pub device_id: [u32; 4],
    pub dev_con_phys: u8,
    pub num_client: u8,
}

#[derive(Debug, Clone, Default)]
pub struct ConInfo {
    pub max_pl2rq_pkt_size: u32,
    pub max_pl2rsp_pkt_size: u32,
    pub device_type: u32,
    pub device_id: [u32; 4],
    pub dev_con_feat: u16,
    pub dev_con_phys: u8,
    pub pl0_max_num_rw: u8,
    pub pl0_addr_map_mask: u16,
    pub identifier: String,
}

/// `device_connect` option (tas_clnt_dco_et).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ConnectMode {
    HotAttach = 0,
    Reset = 1,
    ResetAndHalt = 3,
}

pub struct TasClient {
    stream: TcpStream,
    timeout: Duration,
    con: Option<ConInfo>,
    pl1_cnt: u16,
    reset_count: u64,
    reset_seen: u64,
}

impl TasClient {
    /// Connects to a TasServer and performs SERVER_CONNECT.
    pub fn connect(host: &str, port: u16, client_name: &str) -> Result<(Self, ServerInfo)> {
        let timeout = Duration::from_millis(2000);
        // TasServer listens on IPv4 only; "localhost" may resolve to ::1 first.
        let mut addrs: Vec<_> = (host, port).to_socket_addrs()?.collect();
        addrs.sort_by_key(|a| a.is_ipv6());
        let mut last_err = Error::Usage(format!("cannot resolve {host}"));
        let mut stream = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, timeout) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last_err = Error::Io(e),
            }
        }
        let stream = stream.ok_or(last_err)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut c = TasClient { stream, timeout, con: None, pl1_cnt: 0, reset_count: 0, reset_seen: 0 };

        let user = std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_default();
        let mut w = Wr::default();
        w.u32(4 + 56);
        w.u8(13).u8(PL1_SERVER_CONNECT).u16(0);
        w.cstr(client_name, 32).cstr(&user, 16).u32(std::process::id());
        let rsp = c.transact(&w.buf)?;
        // tas_pl1rsp_server_connect_st: header 4 + reserved 4 + server_info 120 + challenge 8
        c.check_pl1(&rsp, PL1_SERVER_CONNECT, 136)?;
        match rsp[7] {
            ERR_NO_ERROR => {}
            PL1_ERR_SERVER_LOCKED => {
                return Err(Error::Command { cmd: PL1_SERVER_CONNECT, err: rsp[7], msg: "server is locked".into() })
            }
            e => return Err(Error::Command { cmd: PL1_SERVER_CONNECT, err: e, msg: "server connect".into() }),
        }
        let si = 12;
        let info = ServerInfo {
            name: cstr_at(&rsp, si, 64),
            v_minor: le16(&rsp, si + 64),
            v_major: le16(&rsp, si + 66),
            date: cstr_at(&rsp, si + 96, 16),
            start_time_us: le64(&rsp, si + 112),
        };
        Ok((c, info))
    }

    pub fn set_timeout(&mut self, timeout: Duration) -> Result<()> {
        self.timeout = timeout;
        self.stream.set_read_timeout(Some(timeout))?;
        self.stream.set_write_timeout(Some(timeout))?;
        Ok(())
    }

    /// Lists the targets (access hardware + device) known to the server.
    pub fn get_targets(&mut self) -> Result<Vec<TargetInfo>> {
        let mut out = Vec::new();
        let mut start: u8 = 0;
        loop {
            let mut w = Wr::default();
            w.u32(8).u8(0).u8(PL1_GET_TARGETS).u8(start).u8(0);
            let rsp = self.transact(&w.buf)?;
            if rsp.len() < 12 || rsp[5] != PL1_GET_TARGETS {
                return Err(Error::Protocol("bad GET_TARGETS response".into()));
            }
            if rsp[7] != ERR_NO_ERROR {
                return Err(Error::Command { cmd: PL1_GET_TARGETS, err: rsp[7], msg: "get targets".into() });
            }
            let num_target = rsp[8];
            let num_now = rsp[10] as usize;
            for i in 0..num_now {
                let o = 12 + i * 88;
                if o + 88 > rsp.len() {
                    return Err(Error::Protocol("GET_TARGETS response truncated".into()));
                }
                out.push(TargetInfo {
                    identifier: cstr_at(&rsp, o, 64),
                    device_type: le32(&rsp, o + 64),
                    device_id: [le32(&rsp, o + 68), le32(&rsp, o + 72), le32(&rsp, o + 76), le32(&rsp, o + 80)],
                    dev_con_phys: rsp[o + 84],
                    num_client: rsp[o + 85],
                });
            }
            start = start.wrapping_add(num_now as u8);
            if num_now == 0 || start >= num_target {
                break;
            }
        }
        Ok(out)
    }

    /// Starts a read/write session on the target with the given identifier.
    pub fn session_start(&mut self, identifier: &str, session_name: &str, session_pw: &str) -> Result<ConInfo> {
        let mut w = Wr::default();
        w.u32(4 + 112);
        w.u8(27).u8(PL1_SESSION_START).u8(0).u8(CLIENT_TYPE_RW);
        w.u32(0).u64(0);
        w.cstr(identifier, 64).cstr(session_name, 16).cstr(session_pw, 16);
        let rsp = self.transact(&w.buf)?;
        let con = self.parse_ping(&rsp, PL1_SESSION_START)?;
        self.con = Some(con.clone());
        Ok(con)
    }

    /// PING: refreshes the connection information.
    pub fn ping(&mut self) -> Result<ConInfo> {
        let mut w = Wr::default();
        w.u32(8).u8(0).u8(PL1_PING).u8(0).u8(0);
        let rsp = self.transact(&w.buf)?;
        let con = self.parse_ping(&rsp, PL1_PING)?;
        self.con = Some(con.clone());
        Ok(con)
    }

    fn parse_ping(&self, rsp: &[u8], cmd: u8) -> Result<ConInfo> {
        if rsp.len() == 8 && rsp[5] == cmd {
            return Err(Error::Command { cmd, err: rsp[7], msg: "refused".into() });
        }
        // tas_pl1rsp_ping_st: header 8 + tas_con_info_st 124
        self.check_pl1(rsp, cmd, 132)?;
        match rsp[7] {
            ERR_NO_ERROR => {}
            PL1_ERR_SESSION => return Err(Error::Session),
            PL1_ERR_CMD_FAILED => return Err(Error::DeviceAccess),
            e => return Err(Error::Command { cmd, err: e, msg: "session".into() }),
        }
        if PROTOC_VER < rsp[8] || PROTOC_VER > rsp[9] {
            return Err(Error::Protocol(format!("protocol version {}..{} not supported", rsp[8], rsp[9])));
        }
        let ci = 12;
        let con = ConInfo {
            max_pl2rq_pkt_size: le32(rsp, ci),
            max_pl2rsp_pkt_size: le32(rsp, ci + 4),
            device_type: le32(rsp, ci + 8),
            device_id: [le32(rsp, ci + 12), le32(rsp, ci + 16), le32(rsp, ci + 20), le32(rsp, ci + 24)],
            dev_con_feat: le16(rsp, ci + 32),
            dev_con_phys: rsp[ci + 34],
            pl0_max_num_rw: rsp[ci + 35],
            pl0_addr_map_mask: le16(rsp, ci + 38),
            identifier: cstr_at(rsp, ci + 48, 64),
        };
        let ok_size = |s: u32| s % 4 == 0 && (PL2_MAX_PKT_SIZE_MIN..=PL2_MAX_PKT_SIZE).contains(&s);
        if !ok_size(con.max_pl2rq_pkt_size) || !ok_size(con.max_pl2rsp_pkt_size) {
            return Err(Error::Protocol("invalid packet size limits in connection info".into()));
        }
        Ok(con)
    }

    /// Connects to the device: hot attach, reset, or reset and halt.
    /// Returns the device type (JTAG ID).
    pub fn device_connect(&mut self, mode: ConnectMode) -> Result<u32> {
        let mut w = Wr::default();
        w.u32(12).u8(1).u8(PL1_DEVICE_CONNECT).u8(0).u8(0).u16(mode as u16).u16(0);
        let rsp = self.transact(&w.buf)?;
        if rsp.len() < 8 || rsp[5] != PL1_DEVICE_CONNECT {
            return Err(Error::Protocol("bad DEVICE_CONNECT response".into()));
        }
        let device_type = if rsp.len() >= 16 { le32(&rsp, 12) } else { 0 };
        match rsp[7] {
            ERR_NO_ERROR => Ok(device_type),
            PL1_ERR_DEV_ACCESS => Err(Error::DeviceAccess),
            PL1_ERR_DEV_LOCKED => Err(Error::DeviceLocked),
            e => Err(Error::Command {
                cmd: PL1_DEVICE_CONNECT,
                err: e,
                msg: if device_type == 0 { "no device connected".into() } else { format!("option {mode:?} not supported") },
            }),
        }
    }

    /// Resets seen in PL0 responses since the last call.
    pub fn take_resets(&mut self) -> u64 {
        let n = self.reset_count - self.reset_seen;
        self.reset_seen = self.reset_count;
        n
    }

    /// Executes a batch. Individual read/write failures are reported per op in
    /// the results; `Err` is returned only for connection/device level errors.
    pub fn execute(&mut self, batch: &Batch) -> Result<Vec<OpResult>> {
        let con = self.con.as_ref().ok_or_else(|| Error::Usage("session not started".into()))?;
        let lim = Limits {
            max_rq: con.max_pl2rq_pkt_size.min(PL2_MAX_PKT_SIZE) as usize,
            max_rsp: con.max_pl2rsp_pkt_size.min(PL2_MAX_PKT_SIZE) as usize,
            max_num_rw: (con.pl0_max_num_rw as usize).max(1),
        };
        let mut pieces: Vec<Piece> = Vec::new();
        let mut results = Vec::with_capacity(batch.ops.len());
        for (i, op) in batch.ops.iter().enumerate() {
            batch::split_op(i, op, &mut pieces);
            let data = match &op.kind {
                OpKind::Read { len } => vec![0u8; *len],
                OpKind::Write { .. } => Vec::new(),
            };
            results.push(OpResult { err: PL0_ERR_NO_ERROR, bytes_ok: 0, data });
        }
        if pieces.is_empty() {
            return Ok(results);
        }
        let pkts = batch::encode(batch, &pieces, &lim, &mut self.pl1_cnt);

        // Send everything, then collect the responses in order, like the C API.
        // Keep the number of outstanding packets small so neither side blocks.
        const WINDOW: usize = 8;
        for chunk in pkts.chunks(WINDOW) {
            let mut out = Vec::new();
            for p in chunk {
                out.extend_from_slice(&p.bytes);
            }
            self.stream.write_all(&out)?;
            for p in chunk {
                let rsp = self.recv_pl0_frame()?;
                if batch::decode(&rsp, p, &pieces, &mut results)? {
                    self.reset_count += 1;
                }
            }
        }
        Ok(results)
    }

    // --- convenience -----------------------------------------------------------------

    pub fn read32(&mut self, addr: u64) -> Result<u32> {
        let mut b = Batch::new();
        b.read32(addr);
        let r = self.execute(&b)?;
        check_rw(&r[0], 0, addr, false)?;
        Ok(r[0].u32())
    }

    pub fn write32(&mut self, addr: u64, value: u32) -> Result<()> {
        let mut b = Batch::new();
        b.write32(addr, value);
        let r = self.execute(&b)?;
        check_rw(&r[0], 0, addr, true)
    }

    pub fn read(&mut self, addr: u64, len: usize) -> Result<Vec<u8>> {
        let mut b = Batch::new();
        b.read(addr, len);
        let mut r = self.execute(&b)?;
        check_rw(&r[0], 0, addr, false)?;
        Ok(std::mem::take(&mut r[0].data))
    }

    pub fn write(&mut self, addr: u64, data: &[u8]) -> Result<()> {
        let mut b = Batch::new();
        b.write(addr, data);
        let r = self.execute(&b)?;
        check_rw(&r[0], 0, addr, true)
    }

    // --- transport -------------------------------------------------------------------

    fn transact(&mut self, pkt: &[u8]) -> Result<Vec<u8>> {
        self.stream.write_all(pkt)?;
        self.recv_pl2()
    }

    fn recv_pl2(&mut self) -> Result<Vec<u8>> {
        let mut hdr = [0u8; 4];
        self.stream.read_exact(&mut hdr)?;
        let len = u32::from_le_bytes(hdr) as usize;
        if len % 4 != 0 || len < 8 || len > PL2_MAX_PKT_SIZE as usize {
            return Err(Error::Protocol(format!("invalid PL2 packet length {len}")));
        }
        let mut buf = vec![0u8; len];
        buf[..4].copy_from_slice(&hdr);
        self.stream.read_exact(&mut buf[4..])?;
        Ok(buf)
    }

    /// Receives the next PL0 frame response, counting unsolicited reset
    /// notifications (PL1 ERROR packets) that may be interleaved.
    fn recv_pl0_frame(&mut self) -> Result<Vec<u8>> {
        loop {
            let rsp = self.recv_pl2()?;
            match rsp[5] {
                PL1_PL0_START => return Ok(rsp),
                PL1_ERROR => {
                    if rsp[7] == PL1_ERR_DEV_RESET {
                        self.reset_count += 1;
                    }
                }
                c => return Err(Error::Protocol(format!("unexpected PL1 packet 0x{c:02X}"))),
            }
        }
    }

    fn check_pl1(&self, rsp: &[u8], cmd: u8, pl1_size: usize) -> Result<()> {
        if rsp.len() != 4 + pl1_size || rsp[5] != cmd {
            return Err(Error::Protocol(format!(
                "unexpected response for command 0x{cmd:02X} (len {}, cmd 0x{:02X})",
                rsp.len(),
                rsp.get(5).copied().unwrap_or(0)
            )));
        }
        Ok(())
    }
}

pub(crate) fn check_rw(r: &OpResult, op: usize, addr: u64, write: bool) -> Result<()> {
    if r.ok() {
        Ok(())
    } else {
        Err(Error::Rw { op, addr: addr + r.bytes_ok as u64, err: r.err, write })
    }
}
