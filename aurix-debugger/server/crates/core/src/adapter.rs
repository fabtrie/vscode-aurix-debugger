//! Debug adapter abstraction (OpenOCD `adapter driver`).

use std::any::Any;

use tas::{Batch, OpResult};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default)]
pub struct AdapterSettings {
    pub speed_khz: Option<u32>,
    pub serial: Option<String>,
    /// `reset_config connect_assert_srst`: connect with reset and halt.
    pub connect_under_srst: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    /// JTAG ID of the device (OCMTS TAP ID).
    pub device_id: u32,
    pub description: String,
}

pub trait Adapter {
    fn driver_name(&self) -> &'static str;
    /// Driver-specific Tcl command groups, e.g. `tas-client`.
    fn command_groups(&self) -> &'static [&'static str] {
        &[]
    }
    /// Handles a driver-specific command (`args[0]` is the group name).
    fn command(&mut self, args: &[&str]) -> Result<String> {
        Err(Error::Usage(format!("unknown command '{}'", args.join(" "))))
    }
    fn init(&mut self, settings: &AdapterSettings) -> Result<DeviceInfo>;
    fn initialized(&self) -> bool;
    fn execute(&mut self, batch: &Batch) -> Result<Vec<OpResult>>;
    fn assert_srst(&mut self) -> Result<()>;
    /// Releases reset. `halt_after_reset` requests a halt at the reset vector.
    fn deassert_srst(&mut self, halt_after_reset: bool) -> Result<()>;
    /// Number of device resets noticed since the last call.
    fn take_resets(&mut self) -> u64;
    fn set_speed_khz(&mut self, khz: u32) -> Result<()>;
    fn quit(&mut self) {}
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

pub const DRIVERS: &[&str] = &["tas_client", "sim"];

pub fn create(name: &str) -> Option<Box<dyn Adapter>> {
    match name {
        "tas_client" => Some(Box::new(crate::tas_adapter::TasAdapter::default())),
        "sim" => Some(Box::new(crate::sim::SimAdapter::new())),
        _ => None,
    }
}

/// Convenience memory access on top of an adapter.
pub struct Io<'a> {
    pub adapter: &'a mut dyn Adapter,
}

impl<'a> Io<'a> {
    pub fn new(adapter: &'a mut dyn Adapter) -> Self {
        Io { adapter }
    }

    pub fn batch(&mut self, b: &Batch) -> Result<Vec<OpResult>> {
        self.adapter.execute(b)
    }

    pub fn read32(&mut self, addr: u32) -> Result<u32> {
        let mut b = Batch::new();
        b.read32(addr as u64);
        let r = self.batch(&b)?;
        check(&r[0], addr as u64, false)?;
        Ok(r[0].u32())
    }

    pub fn write32(&mut self, addr: u32, value: u32) -> Result<()> {
        let mut b = Batch::new();
        b.write32(addr as u64, value);
        let r = self.batch(&b)?;
        check(&r[0], addr as u64, true)
    }

    /// Writes all (address, value) pairs in one transaction, in order.
    pub fn write32_seq(&mut self, writes: &[(u32, u32)]) -> Result<()> {
        let mut b = Batch::new();
        for &(a, v) in writes {
            b.write32(a as u64, v);
        }
        let r = self.batch(&b)?;
        for (res, &(a, _)) in r.iter().zip(writes) {
            check(res, a as u64, true)?;
        }
        Ok(())
    }

    pub fn read32_many(&mut self, addrs: &[u32]) -> Result<Vec<u32>> {
        let mut b = Batch::new();
        for &a in addrs {
            b.read32(a as u64);
        }
        let r = self.batch(&b)?;
        r.iter().zip(addrs).map(|(res, &a)| check(res, a as u64, false).map(|_| res.u32())).collect()
    }

    pub fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>> {
        let mut b = Batch::new();
        b.read(addr as u64, len);
        let mut r = self.batch(&b)?;
        check(&r[0], addr as u64, false)?;
        Ok(std::mem::take(&mut r[0].data))
    }

    pub fn write(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        let mut b = Batch::new();
        b.write(addr as u64, data);
        let r = self.batch(&b)?;
        check(&r[0], addr as u64, true)
    }
}

pub fn check(r: &OpResult, addr: u64, write: bool) -> Result<()> {
    if r.ok() {
        Ok(())
    } else {
        Err(Error::Memory { addr: addr + r.bytes_ok as u64, write, detail: tas::pl_err_name(r.err).to_string() })
    }
}
