//! `adapter driver tas_client`: Infineon TasServer (miniWiggler, DAP).

use std::any::Any;
use std::time::Duration;

use tas::{Batch, ConnectMode, OpResult, TasClient};

use crate::adapter::{check, Adapter, AdapterSettings, DeviceInfo};
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

pub struct TasAdapter {
    host: String,
    port: u16,
    client: Option<TasClient>,
    srst_asserted: bool,
    identifier: String,
}

impl Default for TasAdapter {
    fn default() -> Self {
        TasAdapter { host: "localhost".into(), port: tas::DEFAULT_PORT, client: None, srst_asserted: false, identifier: String::new() }
    }
}

impl TasAdapter {
    fn client(&mut self) -> Result<&mut TasClient> {
        self.client.as_mut().ok_or_else(|| Error::Adapter("TAS client not connected (run 'init')".into()))
    }

    fn am15_write(&mut self, addr: u64, data: &[u8]) -> Result<()> {
        let mut b = Batch::new();
        b.write_am(addr, data, AM15);
        let r = self.client()?.execute(&b)?;
        check(&r[0], addr, true)
    }
}

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
            _ => Err(Error::Usage("usage: tas-client host <host> [port]".into())),
        }
    }

    fn init(&mut self, s: &AdapterSettings) -> Result<DeviceInfo> {
        let (mut c, info) = TasClient::connect(&self.host, self.port, "aurix-dbg")
            .map_err(|e| Error::Adapter(format!("TAS: cannot connect to TasServer at {}:{}: {e}", self.host, self.port)))?;
        let targets = c.get_targets()?;
        let target = targets
            .iter()
            .find(|t| match &s.serial {
                Some(serial) => t.identifier.contains(serial.as_str()),
                None => t.device_type != 0,
            })
            .or_else(|| if s.serial.is_none() { targets.first() } else { None })
            .cloned()
            .ok_or_else(|| Error::Adapter(format!("TAS: no matching target (found: {})", targets.iter().map(|t| t.identifier.as_str()).collect::<Vec<_>>().join(", "))))?;
        c.session_start(&target.identifier, "aurix-dbg", "")?;
        c.set_timeout(Duration::from_millis(3000))?;
        let mode = if s.connect_under_srst { ConnectMode::ResetAndHalt } else { ConnectMode::HotAttach };
        let device_id = c.device_connect(mode)?;
        log_info!(
            "TAS: {} v{}.{}, {} (device 0x{:08x})",
            info.name, info.v_major, info.v_minor, target.identifier, device_id
        );
        self.identifier = target.identifier.clone();
        self.client = Some(c);
        if let Some(khz) = s.speed_khz {
            if let Err(e) = self.set_speed_khz(khz) {
                log_warn!("TAS: failed to set adapter speed {khz} kHz: {e}");
            }
        }
        Ok(DeviceInfo { device_id, description: target.identifier })
    }

    fn initialized(&self) -> bool {
        self.client.is_some()
    }

    fn execute(&mut self, batch: &Batch) -> Result<Vec<OpResult>> {
        Ok(self.client()?.execute(batch)?)
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
        self.am15_write(AM15_RW_ACC_HW_FREQUENCY, &(khz.saturating_mul(1000)).to_le_bytes())
    }

    fn quit(&mut self) {
        self.client = None;
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
