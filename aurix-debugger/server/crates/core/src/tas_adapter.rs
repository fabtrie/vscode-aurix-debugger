//! `adapter driver tas_client`: Infineon TasServer (miniWiggler, DAP).

use std::any::Any;
use std::time::{Duration, Instant};

use tas::{Batch, ConnectMode, OpResult, TasClient};

use crate::adapter::{check, Adapter, AdapterSettings, BatchLimits, DeviceInfo};
use crate::error::{Error, Result};
use crate::util::parse_u32;
use crate::{log_info, log_warn};

// TAS address map 15 (TasServer internal), see tas_am15_am14.h
const AM15: u8 = 15;
const AM15_RW_USERPINS: u64 = 0x000A_8000;
const AM15_RW_USERPINS_CNTRL: u64 = 0x000A_8400;
const AM15_RW_ACC_HW_FREQUENCY: u64 = 0x000C_0100;
const UPC_ADD_SFP_RESET: u32 = 1;
const UP_SFP_RESET: u32 = 0x0001_0000;
/// Device type (JTAG ID) the access hardware read, and the override the TasServer operates the
/// device with (0 = use the one read by the hardware). Setting it lets the server talk to a
/// device it would not recognise on its own, e.g. the TC3xx SCR (JTAG ID 0x101EC083) once the
/// application started it in its OCDS SoC DAP mode.
const AM15_R_DEVICE_TYPE_ACC_HW: u64 = 0x00DD_1000;
const AM15_RW_DEVICE_TYPE_SET: u64 = 0x00DD_1080;
const AM15_RW_JTAG_SCAN_MODE_ENABLE: u64 = 0x000C_1100;
const AM15_W_JTAG_SCAN_MODE_DISABLE: u64 = 0x000C_1200;
const AM15_W_JTAG_SET_IR: u64 = 0x000C_1300;
const AM15_W_JTAG_CAPTURE: u64 = 0x000C_1400;
const AM15_RW_JTAG_DATA_SHIFT: u64 = 0x000C_1500;

/// Hex number (optional 0x) as bytes, least significant byte first.
fn hex_bits(s: &str) -> Result<Vec<u8>> {
    let h = s.trim_start_matches("0x").trim_start_matches("0X");
    let h = if h.len() % 2 == 1 { format!("0{h}") } else { h.to_string() };
    (0..h.len())
        .step_by(2)
        .rev()
        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).map_err(|_| Error::Usage(format!("jtag_scan: bad hex '{s}'"))))
        .collect()
}

pub struct TasAdapter {
    host: String,
    port: u16,
    client: Option<TasClient>,
    srst_asserted: bool,
    identifier: String,
    /// `tas-client device_type`: JTAG ID the TasServer should operate the device as (0 = auto).
    device_type: u32,
    /// Settings of `init`, applied when the device is connected.
    settings: AdapterSettings,
    /// Session started on the access hardware.
    session: bool,
    /// Device connected.
    device: bool,
    /// Last device connect attempt.
    last_try: Option<Instant>,
}

impl Default for TasAdapter {
    fn default() -> Self {
        TasAdapter {
            host: "localhost".into(),
            port: tas::DEFAULT_PORT,
            client: None,
            srst_asserted: false,
            identifier: String::new(),
            device_type: 0,
            settings: AdapterSettings::default(),
            session: false,
            device: false,
            last_try: None,
        }
    }
}

impl TasAdapter {
    fn client(&mut self) -> Result<&mut TasClient> {
        self.client.as_mut().ok_or_else(|| Error::Adapter("TAS client not connected (run 'init')".into()))
    }

    /// Native JTAG scans (TasServer JTAG scan mode, tas_utils_jtag.cpp): for every scan an
    /// optional IR scan, then a DR scan of `bits` bits with the data in (bit 0 of byte 0 first).
    /// Returns the captured DR data as hex numbers. Normal memory access is not possible while
    /// the scan mode is on; it is switched off at the end of the transaction.
    fn jtag_scan(&mut self, scans: &[(u32, u32, u32, Vec<u8>)]) -> Result<Vec<String>> {
        let mut b = Batch::new();
        b.write_am(AM15_RW_JTAG_SCAN_MODE_ENABLE, &0u32.to_le_bytes(), AM15);
        let mut reads = Vec::new();
        for (ir_width, ir, bits, data) in scans {
            if *bits == 0 || *bits > 8192 {
                return Err(Error::Usage(format!("jtag_scan: {bits} bits (1..8192)")));
            }
            if *ir_width > 0 {
                let mut set_ir = ir_width.to_le_bytes().to_vec();
                set_ir.extend(ir.to_le_bytes());
                b.write_am(AM15_W_JTAG_SET_IR, &set_ir, AM15);
            }
            b.write_am(AM15_W_JTAG_CAPTURE, &bits.to_le_bytes(), AM15);
            let len = (*bits as usize).div_ceil(64) * 8;
            let mut d = data.clone();
            d.resize(len, 0);
            b.write_am(AM15_RW_JTAG_DATA_SHIFT, &d, AM15);
            reads.push((b.read_am(AM15_RW_JTAG_DATA_SHIFT, len, AM15).0, *bits));
        }
        b.write_am(AM15_W_JTAG_SCAN_MODE_DISABLE, &0u32.to_le_bytes(), AM15);
        let r = self.client()?.execute(&b)?;
        for (i, res) in r.iter().enumerate() {
            check(res, i as u64, false).map_err(|e| Error::Adapter(format!("JTAG scan access {i} failed: {e}")))?;
        }
        Ok(reads
            .iter()
            .map(|&(h, bits)| {
                let bytes = &r[h].data[..(bits as usize).div_ceil(8)];
                format!("0x{}", bytes.iter().rev().map(|x| format!("{x:02x}")).collect::<String>())
            })
            .collect())
    }

    fn am15_read(&mut self, addr: u64) -> Result<u32> {
        let mut b = Batch::new();
        let h = b.read_am(addr, 4, AM15);
        let r = self.client()?.execute(&b)?;
        check(&r[h.0], addr, false)?;
        Ok(r[h.0].u32())
    }

    fn am15_write(&mut self, addr: u64, data: &[u8]) -> Result<()> {
        let mut b = Batch::new();
        b.write_am(addr, data, AM15);
        let r = self.client()?.execute(&b)?;
        check(&r[0], addr, true)
    }

    /// Starts the session on the access hardware (once) and connects the device.
    fn connect_device(&mut self) -> Result<DeviceInfo> {
        let s = self.settings.clone();
        if !self.session {
            let c = self.client()?;
            let targets = c.get_targets()?;
            let target = targets
                .iter()
                .find(|t| match &s.serial {
                    Some(serial) => t.identifier.contains(serial.as_str()),
                    None => t.device_type != 0,
                })
                .or_else(|| if s.serial.is_none() { targets.first() } else { None })
                .cloned()
                .ok_or_else(|| {
                    Error::Adapter(format!(
                        "TAS: no matching target (found: {})",
                        targets.iter().map(|t| t.identifier.as_str()).collect::<Vec<_>>().join(", ")
                    ))
                })?;
            c.session_start(&target.identifier, "aurix-dbg", "")?;
            c.set_timeout(Duration::from_millis(3000))?;
            self.identifier = target.identifier;
            self.session = true;
            // Operate the device as this JTAG ID instead of the one the access hardware read (e.g.
            // the SCR, which only appears once the application started it in its OCDS SoC DAP mode).
            if self.device_type != 0 {
                let dt = self.device_type;
                self.am15_write(AM15_RW_DEVICE_TYPE_SET, &dt.to_le_bytes())?;
                log_info!("TAS: operating the device as type 0x{dt:08x}");
            }
        }
        let mode = if s.connect_under_srst { ConnectMode::ResetAndHalt } else { ConnectMode::HotAttach };
        let device_id = self.client()?.device_connect(mode)?;
        self.device = true;
        log_info!("TAS: {} (device 0x{device_id:08x})", self.identifier);
        if let Some(khz) = s.speed_khz {
            if let Err(e) = self.set_speed_khz(khz) {
                log_warn!("TAS: failed to set adapter speed {khz} kHz: {e}");
            }
        }
        Ok(DeviceInfo { device_id, description: self.identifier.clone() })
    }

    /// Connects the device if it is not yet, at most every [`CONNECT_RETRY`].
    fn ensure_device(&mut self) -> Result<()> {
        if self.device {
            return Ok(());
        }
        if self.last_try.is_some_and(|t| t.elapsed() < CONNECT_RETRY) {
            return Err(Error::Adapter("TAS: no device connected (target not powered?)".into()));
        }
        self.last_try = Some(Instant::now());
        self.connect_device().map(|_| ())
    }
}

/// Interval of device connect attempts while no device is connected.
const CONNECT_RETRY: Duration = Duration::from_millis(500);

impl Adapter for TasAdapter {
    fn driver_name(&self) -> &'static str {
        "tas_client"
    }

    fn command_groups(&self) -> &'static [&'static str] {
        &["tas-client"]
    }

    fn command(&mut self, args: &[&str]) -> Result<String> {
        match args.get(1).copied() {
            Some("host") if args.len() == 3 || args.len() == 4 => {
                self.host = args[2].to_string();
                if let Some(p) = args.get(3) {
                    self.port = parse_u32(p).map_err(Error::Usage)? as u16;
                }
                Ok(String::new())
            }
            Some("host") if args.len() == 2 => Ok(format!("{} {}", self.host, self.port)),
            // Must be set before "init": it is applied just before the device connect.
            Some("device_type") if args.len() == 3 => {
                self.device_type = parse_u32(args[2]).map_err(Error::Usage)?;
                Ok(String::new())
            }
            Some("device_type") => {
                let read = self.client.is_some().then(|| self.am15_read(AM15_R_DEVICE_TYPE_ACC_HW)).transpose()?;
                Ok(match read {
                    Some(hw) => format!("device_type 0x{:08x} (access HW reads 0x{hw:08x})", self.device_type),
                    None => format!("device_type 0x{:08x}", self.device_type),
                })
            }
            // Everything the TasServer enumerates: a second device (e.g. the SCR started in its
            // OCDS SoC DAP mode) would show up here with its own JTAG ID.
            Some("targets") => {
                let targets = self.client()?.get_targets()?;
                Ok(targets
                    .iter()
                    .map(|t| {
                        format!(
                            "{} device_type=0x{:08x} phys=0x{:02x} clients={} id={:08x?}",
                            t.identifier, t.device_type, t.dev_con_phys, t.num_client, t.device_id
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            Some("jtag_scan") if args.len() >= 6 && (args.len() - 2) % 4 == 0 => {
                let scans = args[2..]
                    .chunks(4)
                    .map(|s| -> Result<(u32, u32, u32, Vec<u8>)> {
                        let n = |v: &str| parse_u32(v).map_err(Error::Usage);
                        Ok((n(s[0])?, n(s[1])?, n(s[2])?, hex_bits(s[3])?))
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.jtag_scan(&scans).map(|outs| outs.join(" "))
            }
            _ => Err(Error::Usage(
                "usage: tas-client host <host> [port] | tas-client jtag_scan {<ir_width> <ir> <dr_bits> <hex>}... (ir_width 0 keeps the IR; hex is LSB first shifted)"
                    .into(),
            )),
        }
    }

    /// Connects to the TasServer. The device may be missing (target not powered): the server then
    /// starts anyway and every access tries to connect it again (see [`Self::connect_device`]).
    fn init(&mut self, s: &AdapterSettings) -> Result<DeviceInfo> {
        let (c, info) = TasClient::connect(&self.host, self.port, "aurix-dbg")
            .map_err(|e| Error::Adapter(format!("TAS: cannot connect to TasServer at {}:{}: {e}", self.host, self.port)))?;
        log_info!("TAS: {} v{}.{}", info.name, info.v_major, info.v_minor);
        self.client = Some(c);
        self.settings = s.clone();
        self.last_try = Some(Instant::now());
        match self.connect_device() {
            Ok(info) => Ok(info),
            Err(e) => {
                log_warn!("TAS: no device ({e}) - target not powered? Waiting for it");
                Ok(DeviceInfo { device_id: 0, description: "not connected".into() })
            }
        }
    }

    fn initialized(&self) -> bool {
        self.client.is_some()
    }

    fn execute(&mut self, batch: &Batch) -> Result<Vec<OpResult>> {
        self.ensure_device()?;
        Ok(self.client()?.execute(batch)?)
    }

    fn limits(&self) -> BatchLimits {
        let Some(con) = self.client.as_ref().and_then(|c| c.con_info()) else {
            return BatchLimits::default();
        };
        BatchLimits {
            max_num_rw: (con.pl0_max_num_rw as usize).max(1),
            max_rq: con.max_pl2rq_pkt_size as usize,
            max_rsp: con.max_pl2rsp_pkt_size as usize,
        }
    }

    fn assert_srst(&mut self) -> Result<()> {
        self.am15_write(AM15_RW_USERPINS_CNTRL, &UPC_ADD_SFP_RESET.to_le_bytes())?;
        let mut pins = Vec::with_capacity(8);
        pins.extend_from_slice(&0u32.to_le_bytes()); // value: reset pin low
        pins.extend_from_slice(&UP_SFP_RESET.to_le_bytes()); // mask
        self.am15_write(AM15_RW_USERPINS, &pins)?;
        self.srst_asserted = true;
        Ok(())
    }

    fn deassert_srst(&mut self, halt_after_reset: bool) -> Result<()> {
        if !self.srst_asserted {
            return Ok(());
        }
        self.srst_asserted = false;
        let mode = if halt_after_reset { ConnectMode::ResetAndHalt } else { ConnectMode::Reset };
        match self.client()?.device_connect(mode) {
            Ok(_) => Ok(()),
            // The reset happened, but it cleared OSTATE - OCDS gets re-enabled by the target.
            Err(tas::Error::DeviceLocked) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn take_resets(&mut self) -> u64 {
        self.client.as_mut().map(|c| c.take_resets()).unwrap_or(0)
    }

    fn set_speed_khz(&mut self, khz: u32) -> Result<()> {
        // Applied when the device is connected.
        self.settings.speed_khz = Some(khz);
        if !self.device {
            return Ok(());
        }
        self.am15_write(AM15_RW_ACC_HW_FREQUENCY, &(khz.saturating_mul(1000)).to_le_bytes())
    }

    fn quit(&mut self) {
        self.client = None;
        self.session = false;
        self.device = false;
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
