//! The probe's own sampler (DAQ): the kernel checks a list of target addresses
//! in a loop and buffers what it reads; the host only drains the buffer.
//!
//! Read off winIDEA 9.21.417 driving an iC5700 (`daq-417.jsonl`,
//! `daq-417-tiers.jsonl`). Field names are the kernel's (`SBBK_BB_DAQ*`, `isys::SInf_DAQ`,
//! `isys::SStatus_DAQ`, `SDAQItem`). This is winIDEA's "DAQ method" *BlueBox SW*; the FPGA
//! variant is not available on the probes we have. The service numbers are the 9.21.417
//! kernel's, as everything else in this crate.
//!
//! | Service | Request | Reply (after the service header, little-endian) |
//! |---|---|---|
//! | [`INFO`] | `00` | `Cfg_MaxItems` u16, `MaxItemSize` u8, `Buf_SizeSamples` u32 |
//! | [`CONFIGURE`] | count u32, 16 bytes per [`Item`] | count u32, 6 bytes per item: handle u16, 4 bytes (zero) |
//! | [`CONTROL`] | [`Ctrl`] u32 | [`Status`] |
//! | [`READ`] | `NumRead` u32 (0), `NumReadMax` u32 | [`Status`], data length u32, 19-byte [`Sample`]s |
//!
//! Measured on a TC377:
//!
//! * **Only changes are sent.** An item's tier is how often it is checked; a sample goes out
//!   when the value differs from the last one sent, and once per item after enabling. A
//!   constant item sends one sample and then nothing.
//! * **An unreadable item sends nothing**, not even the first sample, and no error anywhere. It
//!   costs every loop pass the failed access (the loop went from about 8 to 600 us).
//! * 256 items of up to 8 bytes, a buffer of 4096 samples, timestamps in microseconds of the
//!   probe's clock. One 4-byte item at [`Tier::Max`] gives about 3,700 samples a second.
//! * Configuring needs the sampler disabled; winIDEA drains, disables, configures, enables.

use crate::frame::{READ_WINDOW, REPLY_CAPACITY};
use crate::probe::{Exchange, Fetch};
use crate::{hex, message, Error, Link, Probe, Result};

/// `SBBK_BB_DAQInf`: the sampler's limits.
pub const INFO: &str = "//1///19/";
/// `SBBK_BB_DAQCtrl`: enable, disable, status.
pub const CONTROL: &str = "//1///20/";
/// `SBBK_BB_DAQCfg`: the item list, replacing the previous one.
pub const CONFIGURE: &str = "//1///21/";
/// `SBBK_BB_DAQRead`: the buffered samples.
pub const READ: &str = "//1///22/";

/// What winIDEA asks for per read (`NumReadMax`): about 32 KiB of samples.
pub const READ_MAX: u32 = 1724;
/// Bytes per sample in a read reply.
pub const SAMPLE_LEN: usize = 19;

/// Reply bytes before the first field: the 16-byte frame and the service reply header.
const BODY: usize = 16 + 14;

/// How often the probe checks an item (`EDAQSamplingFlags`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// As fast as the loop runs.
    Max,
    Ms1,
    Ms10,
    Ms100,
    S1,
}

impl Tier {
    pub const ALL: [Tier; 5] = [Tier::Max, Tier::Ms1, Tier::Ms10, Tier::Ms100, Tier::S1];

    /// The slowest tier that still checks at least once per `period_us`: a value is never
    /// older than the period asked for. Below 1 ms that is [`Tier::Max`].
    pub fn for_period_us(period_us: u32) -> Tier {
        match period_us {
            0..=999 => Tier::Max,
            1_000..=9_999 => Tier::Ms1,
            10_000..=99_999 => Tier::Ms10,
            100_000..=999_999 => Tier::Ms100,
            _ => Tier::S1,
        }
    }

    /// The check period, or `None` for [`Tier::Max`].
    pub fn period_us(self) -> Option<u32> {
        match self {
            Tier::Max => None,
            Tier::Ms1 => Some(1_000),
            Tier::Ms10 => Some(10_000),
            Tier::Ms100 => Some(100_000),
            Tier::S1 => Some(1_000_000),
        }
    }

    fn code(self) -> u8 {
        0x08 | self as u8
    }
}

/// One address the probe samples: 1, 2, 4 or 8 bytes on the bus (memory area 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Item {
    pub addr: u32,
    pub size: u8,
    pub tier: Tier,
}

impl Item {
    /// The 16 bytes of a [`CONFIGURE`] request:
    /// `<size code << 4> <0x08 | tier> <size> 00 | <address u32> | 00 x 7 | 01`. The size code
    /// is 1, 2, 3, 4 for 1, 2, 4, 8 bytes. Bytes 8 to 15 were the same in every item winIDEA
    /// sent; they carry `MemArea`, `ItemFlags` and the byte order, in an order not known.
    pub fn encode(&self) -> Result<[u8; 16]> {
        let code = match self.size {
            1 => 1,
            2 => 2,
            4 => 3,
            8 => 4,
            n => return Err(Error::Protocol(format!("DAQ item of {n} bytes (1, 2, 4 or 8)"))),
        };
        let mut b = [0u8; 16];
        b[0] = code << 4;
        b[1] = self.tier.code();
        b[2] = self.size;
        b[4..8].copy_from_slice(&self.addr.to_le_bytes());
        b[15] = 1;
        Ok(b)
    }
}

/// `isys::SInf_DAQ`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    pub max_items: u16,
    pub max_item_size: u8,
    pub buffer_samples: u32,
}

/// `SBBK_BB_DAQCtrl` operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctrl {
    Enable = 0,
    Disable = 1,
    Status = 2,
}

/// `isys::SStatus_DAQ`, at the start of every control and read reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Status {
    /// The probe's clock, in microseconds.
    pub time_us: u64,
    /// The longest loop pass since the last report, in microseconds.
    pub max_loop_us: u32,
    /// In a read reply, the samples it carries; 0 in a control reply.
    pub samples: u32,
    /// Loop passes since enabling.
    pub loops: u32,
    /// Samples were lost because the buffer was full.
    pub overflow: bool,
    pub enabled: bool,
}

/// One buffered sample: the item's handle (its index in the configured list), when it was
/// read, and its bytes (`size` of them; the reply pads every value to 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub handle: u16,
    pub time_us: u64,
    pub data: Vec<u8>,
}

/// Little-endian reader with bounds checks.
struct Rd<'a> {
    b: &'a [u8],
    i: usize,
    what: &'static str,
}

impl Rd<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let s = self.b.get(self.i..self.i + n).ok_or_else(|| Error::Protocol(format!("{}: reply ends at {}", self.what, self.i)))?;
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

/// The reply's fields, after checking it is one; the length word at 0 counts the bytes after
/// the frame header.
fn body<'a>(r: &'a [u8], what: &'static str) -> Result<Rd<'a>> {
    if !message::is_reply(r) {
        return Err(Error::Protocol(format!("{what}: {}", hex(r, 32))));
    }
    let len = r.get(..4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    let end = (16 + len).min(r.len());
    Ok(Rd { b: r.get(..end).unwrap_or(r), i: BODY, what })
}

fn status(d: &mut Rd) -> Result<Status> {
    Ok(Status {
        time_us: d.u64()?,
        max_loop_us: d.u32()?,
        samples: d.u32()?,
        loops: d.u32()?,
        overflow: d.u8()? != 0,
        enabled: d.u8()? != 0,
    })
}

pub fn parse_info(r: &[u8]) -> Result<Info> {
    let mut d = body(r, INFO)?;
    Ok(Info { max_items: d.u16()?, max_item_size: d.u8()?, buffer_samples: d.u32()? })
}

/// The handles of the configured items, in order.
pub fn parse_configure(r: &[u8]) -> Result<Vec<u16>> {
    let mut d = body(r, CONFIGURE)?;
    let n = d.u32()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(d.u16()?);
        d.take(4)?;
    }
    Ok(out)
}

pub fn parse_status(r: &[u8]) -> Result<Status> {
    status(&mut body(r, CONTROL)?)
}

pub fn parse_read(r: &[u8]) -> Result<(Status, Vec<Sample>)> {
    let mut d = body(r, READ)?;
    let st = status(&mut d)?;
    let len = d.u32()? as usize;
    if len != st.samples as usize * SAMPLE_LEN {
        return Err(Error::Protocol(format!("{READ}: {len} data bytes for {} samples", st.samples)));
    }
    let mut out = Vec::with_capacity(st.samples as usize);
    for _ in 0..st.samples {
        let handle = d.u16()?;
        let size = d.u8()? as usize;
        let time_us = d.u64()?;
        let value = d.take(8)?;
        if size == 0 || size > 8 {
            return Err(Error::Protocol(format!("{READ}: sample of {size} bytes")));
        }
        out.push(Sample { handle, time_us, data: value[..size].to_vec() });
    }
    Ok((st, out))
}

pub fn configure_request(items: &[Item]) -> Result<Vec<u8>> {
    let mut b = Vec::with_capacity(4 + 16 * items.len());
    b.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for it in items {
        b.extend_from_slice(&it.encode()?);
    }
    Ok(b)
}

fn read_request() -> [u8; 8] {
    let mut b = [0u8; 8];
    b[4..].copy_from_slice(&READ_MAX.to_le_bytes());
    b
}

impl<L: Link> Probe<L> {
    fn daq_exchange(&mut self, service: &str, body: &[u8]) -> Result<Vec<u8>> {
        let x = Exchange {
            payload: message::encode(message::KIND, service, body),
            fetch: Fetch::new(0x0e, READ_WINDOW, 0x02),
            reads: vec![REPLY_CAPACITY],
            expect: Vec::new(),
            pause_ms: 0,
        };
        Ok(self.exchange(&x)?.remove(0))
    }

    /// The sampler's limits ([`INFO`]).
    pub fn daq_info(&mut self) -> Result<Info> {
        let r = self.daq_exchange(INFO, &[0])?;
        parse_info(&r)
    }

    /// Replaces the item list ([`CONFIGURE`]); the sampler must be disabled. Returns each
    /// item's handle, the index samples carry.
    pub fn daq_configure(&mut self, items: &[Item]) -> Result<Vec<u16>> {
        let r = self.daq_exchange(CONFIGURE, &configure_request(items)?)?;
        let handles = parse_configure(&r)?;
        if handles.len() != items.len() {
            return Err(Error::Protocol(format!("{CONFIGURE}: {} handles for {} items", handles.len(), items.len())));
        }
        Ok(handles)
    }

    /// Enables, disables or asks for the status ([`CONTROL`]).
    pub fn daq_control(&mut self, op: Ctrl) -> Result<Status> {
        let r = self.daq_exchange(CONTROL, &(op as u32).to_le_bytes())?;
        parse_status(&r)
    }

    /// Drains up to [`READ_MAX`] buffered samples ([`READ`]).
    pub fn daq_read(&mut self) -> Result<(Status, Vec<Sample>)> {
        let r = self.daq_exchange(READ, &read_request())?;
        parse_read(&r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // Replies from `daq-417.jsonl` (winIDEA 9.21.417, iC5700 B, TC377).
    const INFO_REPLY: &str = "15000000eaebda77a0b3fcfff27f16bf080000001001000000000000000000010800100000";
    const STATUS_IDLE: &str = "24000000eaebda77a0b3fcfff27f16bf080000001f010000000000000000dafb1a63000000000000000000000000000000000000";
    const ENABLED: &str = "24000000eaebda77a0b3fcfff27f16bf080000001f010000000000000000feff3563000000000000000000000000000000000001";
    const CONFIGURE_1: &str = "18000000eaebda77a0b3fcfff27f16bf080000001301000000000000000001000000000000000000";
    const CONFIGURE_5: &str = "30000000eaebda77a0b3fcfff27f16bf080000002b01000000000000000005000000000000000000010000000000020000000000030000000000040000000000";
    const READ_0: &str = "28000000eaebda77a0b3fcfff27f16bf0800000023010000000000000000b685456300000000090000000000000022550000000100000000";
    const READ_1: &str = "3b000000eaebda77a0b3fcfff27f16bf0800000036010000000000000000fd734563000000007800000001000000d6540000000113000000000004ce6c4563000000002adc58ad00000000";
    const READ_8: &str = "c0000000eaebda77a0b3fcfff27f16bf08000000bb0100000000000000008c93d4630000000085010000080000004d3b00000001980000000000045d77d46300000000a69038e500000000000004447ad4630000000034b239e5000000000000040e7ed46300000000b3293be500000000000004fe82d46300000000cb1e3de500000000000004e885d46300000000f1393ee500000000000004598bd46300000000916240e500000000000004e990d46300000000db8e42e500000000000004ff91d463000000008ff542e500000000";

    #[test]
    fn items_encode_as_winidea_sent_them() {
        // daq_multi's five items, byte for byte.
        let items = [
            Item { addr: 0xF000_1010, size: 4, tier: Tier::Ms1 },
            Item { addr: 0xF003_6140, size: 4, tier: Tier::Ms100 },
            Item { addr: 0x7000_0000, size: 2, tier: Tier::Ms10 },
            Item { addr: 0x7000_0004, size: 1, tier: Tier::S1 },
            Item { addr: 0x7000_0008, size: 8, tier: Tier::Ms100 },
        ];
        let want = "0500000030090400101000f00000000000000001300b0400406103f00000000000000001200a0200000000700000000000000001100c0100040000700000000000000001400b0800080000700000000000000001";
        assert_eq!(configure_request(&items).unwrap(), unhex(want));
        let max = Item { addr: 0xF000_1010, size: 4, tier: Tier::Max };
        assert_eq!(max.encode().unwrap()[1], 0x08);
        assert!(Item { addr: 0, size: 3, tier: Tier::Max }.encode().is_err());
    }

    #[test]
    fn the_read_request_asks_for_what_winidea_asks() {
        assert_eq!(read_request(), [0, 0, 0, 0, 0xbc, 0x06, 0, 0]);
    }

    #[test]
    fn info() {
        assert_eq!(parse_info(&unhex(INFO_REPLY)).unwrap(), Info { max_items: 256, max_item_size: 8, buffer_samples: 4096 });
    }

    #[test]
    fn configure_returns_one_handle_per_item() {
        assert_eq!(parse_configure(&unhex(CONFIGURE_1)).unwrap(), vec![0]);
        assert_eq!(parse_configure(&unhex(CONFIGURE_5)).unwrap(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn status_carries_the_probe_clock_and_the_enable() {
        let s = parse_status(&unhex(STATUS_IDLE)).unwrap();
        assert_eq!(s.time_us, 1_662_712_794); // what the SDK reported as CDAQStatus::getTime
        assert!(!s.enabled);
        assert!(parse_status(&unhex(ENABLED)).unwrap().enabled);
    }

    #[test]
    fn reads_with_zero_one_and_eight_samples() {
        let (s, v) = parse_read(&unhex(READ_0)).unwrap();
        assert_eq!((s.samples, s.max_loop_us, s.enabled, v.len()), (0, 9, true, 0));

        let (s, v) = parse_read(&unhex(READ_1)).unwrap();
        assert_eq!(s.samples, 1);
        assert_eq!(v, vec![Sample { handle: 0, time_us: 0x6345_6cce, data: vec![0x2a, 0xdc, 0x58, 0xad] }]);

        let (s, v) = parse_read(&unhex(READ_8)).unwrap();
        assert_eq!((s.samples, s.loops, s.overflow), (8, 0x3b4d, false));
        assert_eq!(v.len(), 8);
        assert!(v.iter().all(|x| x.handle == 0 && x.data.len() == 4));
        assert!(v.windows(2).all(|w| w[0].time_us < w[1].time_us));
        assert_eq!(v[0].data, 0xe538_90a6u32.to_le_bytes());
    }

    #[test]
    fn a_short_or_inconsistent_read_is_an_error() {
        let mut r = unhex(READ_1);
        r.truncate(r.len() - 4);
        r[0] -= 4;
        assert!(parse_read(&r).is_err());
        assert!(parse_read(&unhex(INFO_REPLY)).is_err());
        assert!(parse_info(&[0u8; 16]).is_err());
    }

    #[test]
    fn the_tier_never_checks_less_often_than_asked() {
        assert_eq!(Tier::for_period_us(100), Tier::Max);
        assert_eq!(Tier::for_period_us(1_000), Tier::Ms1);
        assert_eq!(Tier::for_period_us(50_000), Tier::Ms10);
        assert_eq!(Tier::for_period_us(200_000), Tier::Ms100);
        assert_eq!(Tier::for_period_us(999_999), Tier::Ms100);
        assert_eq!(Tier::for_period_us(2_000_000), Tier::S1);
        for t in Tier::ALL {
            if let Some(p) = t.period_us() {
                assert_eq!(Tier::for_period_us(p), t);
            }
        }
    }
}
