//! Safe wrapper for winIDEA's **DAQ** (data acquisition) API.
//!
//! DAQ periodically samples target variables in the background and buffers them
//! on the debug hardware, so you can stream signal values out of a running ECU
//! without halting it.
//!
//! Typical flow:
//!
//! ```no_run
//! use winidea_sdk::{WinIdea, ConnectionMgr, daq::{ConfigItem, DaqController}};
//!
//! # fn main() -> Result<(), winidea_sdk::Error> {
//! let winidea = WinIdea::load(&winidea_sdk::default_search_paths())?;
//! let mut conn = ConnectionMgr::new(&winidea)?;
//! conn.connect_mru("C:/ws/MyProject.xjrf")?;
//!
//! let mut daq = DaqController::new(&winidea, &conn)?;
//!
//! // Not every target has DAQ hardware.
//! if !daq.info()?.is_supported() {
//!     return Ok(());
//! }
//!
//! // Configure BEFORE enabling: the SDK rejects configure() while enabled.
//! let handles = daq.configure(&[
//!     ConfigItem::symbol("gCounter"),
//!     ConfigItem::symbol("gState"),
//! ])?;
//! daq.enable_global(true)?;
//!
//! for sample in daq.read()? {
//!     // `index` matches one of the handles returned by configure().
//!     match &sample.value {
//!         Some(v) => println!("t={} idx={} = {v}", sample.time_ns, sample.index),
//!         None => println!("t={} idx={} <undecodable>", sample.time_ns, sample.index),
//!     }
//! }
//!
//! daq.enable_global(false)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Handle ownership
//!
//! Every SDK object is an opaque pointer minted by a `new_*` export and released
//! by the matching `delete_*`. Each wrapper owns exactly one handle and frees it
//! on `Drop`, so handles cannot leak or be double-freed. Handles borrowed from a
//! container (e.g. an element of a sample vector) are **not** owned and are not
//! freed -- they stay valid only while the container lives, which the borrow
//! checker enforces via lifetimes.

use crate::runtime::{self, Error, Owned};
use crate::value::{self, Value};
use crate::{ConnectionMgr, WinIdea};

/// How often a DAQ item is sampled.
///
/// The SDK encodes this as a bitflag enum (`CDAQController::EDAQSamplingFlags`);
/// the discriminants below come from the C# SDK's generated constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SamplingFlags {
    /// Sample as fast as the hardware allows.
    #[default]
    Max,
    Every1ms,
    Every10ms,
    Every100ms,
    Every1s,
    /// An encoding not covered by the named variants.
    Raw(i32),
}

impl SamplingFlags {
    fn to_raw(self) -> i32 {
        match self {
            Self::Max => 0,
            Self::Every1ms => 1,
            Self::Every10ms => 2,
            Self::Every100ms => 3,
            Self::Every1s => 4,
            Self::Raw(v) => v,
        }
    }

    /// The closest tier at or **faster** than `ms`.
    ///
    /// The hardware offers only the fixed tiers above, so an arbitrary interval
    /// cannot be honoured exactly. Rounding down (faster) rather than to the
    /// nearest keeps the promise a requested rate makes: ask for 50 ms and you
    /// get 10 ms updates, never 100 ms ones. `0` means "as fast as possible".
    ///
    /// Pair with [`SamplingFlags::interval_ms`] to show the user what they
    /// actually got.
    ///
    /// Note: `effectiveRateMs` in `frontend/src/components/pickers/
    /// winideaTreeSource.tsx` mirrors these boundaries so the picker can show
    /// the snapped rate without a round trip per keystroke. This function is
    /// authoritative; update that copy alongside any change here.
    pub fn from_interval_ms(ms: u32) -> Self {
        // Each arm selects the tier whose period is <= the request, so the
        // result is never SLOWER than asked for. The boundaries are therefore
        // "up to but not including the next tier": 99ms still gets 10ms
        // updates, and only a full 100ms request drops to the 100ms tier.
        match ms {
            0 => Self::Max,
            1..=9 => Self::Every1ms,
            10..=99 => Self::Every10ms,
            100..=999 => Self::Every100ms,
            _ => Self::Every1s,
        }
    }

    /// The interval this tier actually samples at, in milliseconds.
    ///
    /// `None` for [`SamplingFlags::Max`] (hardware-dependent, not a fixed
    /// period) and for [`SamplingFlags::Raw`] (an encoding this crate does not
    /// interpret).
    pub fn interval_ms(self) -> Option<u32> {
        match self {
            Self::Every1ms => Some(1),
            Self::Every10ms => Some(10),
            Self::Every100ms => Some(100),
            Self::Every1s => Some(1000),
            Self::Max | Self::Raw(_) => None,
        }
    }
}

/// Where the DAQ acquisition loop runs.
///
/// Mirrors `CDAQConfig::EMode`; values pinned by `tests/enum_values.rs`.
///
/// [`AcquisitionMode::Auto`] is the SDK's own default and is verified working
/// against real hardware — prefer it unless you have a specific reason to pin
/// the loop to the debugger's software or hardware engine.
///
/// **This whole type requires winIDEA 9.21.417 or newer.** `CDAQConfig` was
/// introduced there; 9.21.356 has no acquisition-mode concept at all and does
/// not export the family, so [`DaqController::configure_with_mode`] fails with
/// [`Error::MissingSymbol`] on an older SDK. Plain [`DaqController::configure`]
/// works on both — it never touches `CDAQConfig`.
///
/// Note: if `read()` fails with a winIDEA-internal path error such as
/// *"Cannot resolve path: .ReadCompressed"*, that is **not** a mode problem —
/// and on 9.21.356 it cannot be, since the mode API does not exist there. It
/// means the loaded `isystemConnect.dll` does not match the winIDEA it is
/// driving. Point `winidea.sdkPath` at an SDK matching the install rather than
/// changing the mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AcquisitionMode {
    /// Let the SDK pick the best available mode.
    #[default]
    Auto,
    /// Run the acquisition loop in the debugger's software.
    Sw,
    /// Run the acquisition loop in the debugger's hardware.
    Hw,
}

impl AcquisitionMode {
    fn to_raw(self) -> i32 {
        match self {
            Self::Auto => 0,
            Self::Sw => 1,
            Self::Hw => 2,
        }
    }
}

/// One item to acquire, for [`DaqController::configure`].
///
/// Either a named symbol (winIDEA resolves the address from debug info) or a
/// raw address, for targets where the variable has no symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigItem<'a> {
    /// A variable named in the target's debug information.
    Symbol {
        name: &'a str,
        sampling: SamplingFlags,
    },
    /// A raw memory location.
    Address {
        /// Size of the item, in bytes.
        size: u8,
        mem_area: u8,
        address: u32,
        sampling: SamplingFlags,
    },
}

impl ConfigItem<'_> {
    /// Convenience: a symbol sampled as fast as the hardware allows.
    pub fn symbol(name: &str) -> ConfigItem<'_> {
        ConfigItem::Symbol {
            name,
            sampling: SamplingFlags::Max,
        }
    }

    /// Build the SDK-side `CDAQConfigItem`. The caller owns the result.
    fn build<'w>(&self, winidea: &'w WinIdea) -> Result<Owned<'w>, Error> {
        let sdk = winidea.raw();
        let ptr = match self {
            Self::Symbol { name, sampling } => {
                let c = runtime::to_cstring(name)?;
                // SAFETY: the (symbol, sampling) constructor; `c` outlives the
                // call and the SDK copies the string.
                unsafe { (sym!(sdk, new_CDAQConfigItem__SWIG_7))(c.as_ptr(), sampling.to_raw()) }
            }
            Self::Address {
                size,
                mem_area,
                address,
                sampling,
            } => {
                // SAFETY: the (size, memArea, address, sampling) constructor.
                unsafe {
                    (sym!(sdk, new_CDAQConfigItem__SWIG_3))(
                        *size,
                        *mem_area,
                        *address,
                        sampling.to_raw(),
                    )
                }
            }
        };
        // SAFETY: `ptr` is a fresh CDAQConfigItem.
        unsafe { Owned::adopt(winidea, ptr, sdk.delete_CDAQConfigItem) }
    }
}

/// One acquired sample, copied out of the SDK's vector.
///
/// Copied rather than borrowed so the caller is not tied to the lifetime of the
/// underlying `DAQSampleVector`.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// Index of the configured DAQ item this sample belongs to.
    pub index: u16,
    /// Target timestamp, in the units reported by [`DaqInfo::tick_ns`].
    pub time_ns: u64,
    /// The acquired value, decoded by the SDK according to the item's type.
    ///
    /// `None` when the decode failed — a sample whose raw payload the SDK could
    /// not interpret still carries a valid index and timestamp, so it is
    /// reported rather than dropped.
    ///
    /// The value comes from `CDAQController::getDataValue`, **not** from
    /// `CDAQSample::getData`. `getData` hands back an undocumented opaque
    /// handle; `getDataValue` is the SDK's own accessor and returns a
    /// `CValueType` — the same tagged value every other typed read produces, so
    /// it decodes through [`crate::value`] like all the rest. (Confirmed from
    /// `IConnectCSLib.dll`'s IL: `getDataValue` constructs a `CValueType` with
    /// `cMemoryOwn: true`, so the handle is ours to free.)
    pub value: Option<Value>,
}

/// Static properties of the DAQ engine on the connected hardware.
///
/// `max_items == 0` means the target has no DAQ support; the SDK's own example
/// checks exactly that before configuring anything.
///
/// Note there is no `num_items` field. `CDAQInfo::getNumItems()` is deprecated
/// and **raises** rather than returning a value ("Method CDAQInfo::getNumItems()
/// has been deprecated"), so calling it would fail the whole `info()` query.
/// Use [`DaqController::num_items`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaqInfo {
    /// Maximum items this hardware can acquire. 0 = no DAQ support.
    pub max_items: u16,
    pub buf_size: u32,
    pub max_item_size: u8,
    /// Duration of one timestamp tick, in nanoseconds.
    pub tick_ns: u32,
}

impl DaqInfo {
    /// Whether the connected hardware supports DAQ at all.
    pub fn is_supported(&self) -> bool {
        self.max_items > 0
    }
}

/// Live status of the DAQ engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaqStatus {
    pub time: u64,
    pub last_loop_time: u32,
    pub num_samples_available: u32,
    pub num_loops: u32,
    /// The buffer overflowed: samples were lost.
    pub overflow: bool,
    pub global_enable: bool,
    pub core_enable: bool,
}

/// Controls DAQ acquisition on a connected winIDEA instance.
///
/// Borrows the [`ConnectionMgr`] it was built from, so the connection cannot be
/// dropped while the controller is still alive.
pub struct DaqController<'a> {
    inner: Owned<'a>,
}

impl<'a> DaqController<'a> {
    /// Create a DAQ controller bound to an existing connection.
    pub fn new(winidea: &'a WinIdea, conn: &'a ConnectionMgr<'a>) -> Result<Self, Error> {
        let sdk = winidea.raw();
        // SAFETY: `conn` is a live ConnectionMgr for at least `'a`, so its
        // handle is valid for the whole life of this controller.
        let ptr = unsafe { (sym!(sdk, new_CDAQController))(conn.as_ptr()) };
        // SAFETY: `ptr` is a fresh CDAQController.
        let inner = unsafe { Owned::adopt(winidea, ptr, sdk.delete_CDAQController)? };
        Ok(Self { inner })
    }

    fn sdk(&self) -> &crate::generated::ffi::Sdk {
        self.inner.sdk()
    }

    /// Attach to the DAQ hardware.
    ///
    /// **Does nothing.** The SDK marks this deprecated since 9.12.288 ("this
    /// call is no longer required, it does nothing") and its own examples go
    /// straight to [`DaqController::configure`]. Kept only so ported code keeps
    /// working; there is no reason to call it.
    #[deprecated(note = "no-op since SDK 9.12.288; call configure() directly")]
    pub fn attach_hw(&mut self) -> Result<(), Error> {
        // SAFETY: self.inner.ptr is a live CDAQController handle.
        unsafe { (sym!(self.sdk(), CDAQController_attachHW))(self.inner.ptr) };
        runtime::check()
    }

    /// Release the DAQ hardware.
    ///
    /// **Does nothing** -- see [`DaqController::attach_hw`].
    #[deprecated(note = "no-op since SDK 9.12.288")]
    pub fn detach_hw(&mut self) -> Result<(), Error> {
        // SAFETY: as above.
        unsafe { (sym!(self.sdk(), CDAQController_detachHW))(self.inner.ptr) };
        runtime::check()
    }

    /// Drop all configured items.
    pub fn config_reset(&mut self) -> Result<(), Error> {
        // SAFETY: as above.
        unsafe { (sym!(self.sdk(), CDAQController_configReset))(self.inner.ptr) };
        runtime::check()
    }

    /// Configure a variable for acquisition, by symbol name.
    ///
    /// Returns the item handle assigned by the SDK. That handle is what
    /// [`Sample::index`] reports, so keep it to attribute samples to variables.
    ///
    /// Deprecated by the SDK in 9.21.2 in favour of [`DaqController::configure`],
    /// which sets the whole item list at once. Kept because it is convenient for
    /// a single item, but prefer `configure` for anything real.
    pub fn config_add(&mut self, symbol: &str, flags: SamplingFlags) -> Result<u32, Error> {
        let c = runtime::to_cstring(symbol)?;
        // SAFETY: `c` outlives the call; the SDK copies the string.
        let h = unsafe {
            (sym!(self.sdk(), CDAQController_configAdd__SWIG_2))(
                self.inner.ptr,
                c.as_ptr(),
                flags.to_raw(),
            )
        };
        runtime::check()?;
        Ok(h)
    }

    /// Configure the full set of items to acquire, replacing any previous set.
    ///
    /// This is the SDK's current API (`configAdd` is deprecated since 9.21.2).
    /// Returns one handle per item, in the order given; those handles are what
    /// [`Sample::index`] reports.
    ///
    /// The SDK rejects this while acquisition is enabled, so call it before
    /// [`DaqController::enable_global`].
    ///
    /// Leaves the acquisition mode at the SDK's default, so this never touches
    /// `CDAQConfig` and works on every winIDEA version — unlike
    /// [`DaqController::configure_with_mode`], which needs 9.21.417 or newer.
    /// On a pre-9.21.417 DLL it transparently uses that SDK's unsuffixed,
    /// void-returning `configure` and synthesises the item handles, which the
    /// old export does not return. Verified end-to-end on a real 9.21.356.
    ///
    /// If [`DaqController::read`] later fails with a winIDEA-internal path error
    /// such as *"Cannot resolve path: .ReadCompressed"*, that is a DLL/winIDEA
    /// version mismatch, not a transport choice — see [`AcquisitionMode`].
    pub fn configure(&mut self, items: &[ConfigItem<'_>]) -> Result<Vec<i32>, Error> {
        self.configure_inner(items, None)
    }

    /// Configure with an explicit acquisition mode.
    ///
    /// See [`AcquisitionMode`] — the mode decides how samples are drained, and a
    /// mismatch shows up as an obscure failure in `read()` rather than here.
    pub fn configure_with_mode(
        &mut self,
        items: &[ConfigItem<'_>],
        mode: AcquisitionMode,
    ) -> Result<Vec<i32>, Error> {
        self.configure_inner(items, Some(mode))
    }

    fn configure_inner(
        &mut self,
        items: &[ConfigItem<'_>],
        mode: Option<AcquisitionMode>,
    ) -> Result<Vec<i32>, Error> {
        let sdk = self.sdk();

        // SAFETY: creates an empty DAQConfigVector we own.
        let vec_ptr = unsafe { (sym!(sdk, new_DAQConfigVector__SWIG_0))() };
        // SAFETY: `vec_ptr` is a fresh DAQConfigVector.
        let list =
            unsafe { Owned::adopt(self.inner.winidea, vec_ptr, sdk.delete_DAQConfigVector)? };

        // Each item is built, appended (the vector copies it), then freed.
        for item in items {
            let owned = item.build(self.inner.winidea)?;
            // SAFETY: both handles are live; DAQConfigVector_Add copies the
            // item into the vector, so freeing `owned` afterwards is correct.
            unsafe { (sym!(sdk, DAQConfigVector_Add))(list.ptr, owned.ptr) };
            runtime::check()?;
        }

        // Pre-9.21.417 DLLs export a single, unsuffixed `configure` returning
        // **void** instead of the modern overloads returning an IntVector of
        // item handles. Take that path when the modern one is absent.
        //
        // The return type is why this cannot be a name alias: calling the old
        // export through the new signature reads a garbage pointer out of the
        // return register, which is then adopted and freed -- a silent process
        // crash inside `configure`, reproduced on a real 9.21.356 winIDEA.
        if mode.is_none() && sdk.CDAQController_configure__SWIG_0.is_none() {
            // SAFETY: live controller + config vector; this export returns void.
            unsafe { (sym!(sdk, CDAQController_configure_legacy_void))(self.inner.ptr, list.ptr) };
            runtime::check()?;

            // No handles are returned, so mirror what the modern overload gives
            // back. The SDK assigns them sequentially from 0 in the order the
            // items were passed -- the same contract `configure` documents, and
            // what `Sample::index` reports.
            return Ok((0..items.len() as i32).collect());
        }

        // With a mode, build a CDAQConfig and use the (config, items) overload;
        // otherwise the items-only one, which leaves the mode at the SDK default.
        let handles = match mode {
            None => {
                // SAFETY: live controller + config vector. Returns an IntVector
                // we own.
                unsafe { (sym!(sdk, CDAQController_configure__SWIG_0))(self.inner.ptr, list.ptr) }
            }
            Some(m) => {
                // SAFETY: default-constructs a CDAQConfig we own.
                let cfg_ptr = unsafe { (sym!(sdk, new_CDAQConfig))() };
                // SAFETY: `cfg_ptr` is a fresh CDAQConfig.
                let cfg =
                    unsafe { Owned::adopt(self.inner.winidea, cfg_ptr, sdk.delete_CDAQConfig)? };

                // `mode()` is a builder returning a shared_ptr to the SAME
                // object (the SDK documents these as "return reference to self").
                // The returned handle is therefore an alias, not a new object --
                // it must NOT be adopted, or the config is freed twice.
                // SAFETY: live CDAQConfig handle.
                let _alias = unsafe { (sym!(sdk, CDAQConfig_mode))(cfg.ptr, m.to_raw()) };
                runtime::check()?;

                // SAFETY: live controller, config and item vector.
                unsafe {
                    (sym!(sdk, CDAQController_configure__SWIG_1))(self.inner.ptr, cfg.ptr, list.ptr)
                }
            }
        };
        // SAFETY: `handles` is a fresh IntVector.
        let handles = unsafe { Owned::adopt(self.inner.winidea, handles, sdk.delete_IntVector)? };

        // SAFETY: live IntVector handle.
        let n = unsafe { (sym!(sdk, IntVector_size))(handles.ptr) };
        runtime::check()?;

        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            // SAFETY: 0 <= i < size.
            out.push(unsafe { (sym!(sdk, IntVector_getitem))(handles.ptr, i as i32) });
            runtime::check()?;
        }
        Ok(out)
    }

    /// Enable or disable acquisition globally.
    pub fn enable_global(&mut self, enable: bool) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CDAQController_enableGlobal))(self.inner.ptr, enable) };
        runtime::check()
    }

    /// Enable or disable acquisition for the current core.
    pub fn enable_core(&mut self, enable: bool) -> Result<(), Error> {
        // SAFETY: live handle.
        unsafe { (sym!(self.sdk(), CDAQController_enableCore))(self.inner.ptr, enable) };
        runtime::check()
    }

    /// Number of configured items.
    pub fn num_items(&self) -> Result<u32, Error> {
        // SAFETY: live handle.
        let n = unsafe { (sym!(self.sdk(), CDAQController_getNumItems))(self.inner.ptr) };
        runtime::check()?;
        Ok(n)
    }

    /// Static properties of the DAQ engine.
    pub fn info(&self) -> Result<DaqInfo, Error> {
        let sdk = self.sdk();
        // SAFETY: live handle; the returned CDAQInfo is owned by us.
        let h = unsafe { (sym!(sdk, CDAQController_info))(self.inner.ptr) };
        // SAFETY: `h` is a fresh CDAQInfo.
        let owned = unsafe { Owned::adopt(self.inner.winidea, h, sdk.delete_CDAQInfo)? };
        // SAFETY: `owned.ptr` is a live CDAQInfo for the whole block.
        let info = unsafe {
            DaqInfo {
                max_items: (sym!(sdk, CDAQInfo_getMaxItems))(owned.ptr),
                // NB: no CDAQInfo_getNumItems -- it is deprecated and raises.
                buf_size: (sym!(sdk, CDAQInfo_getBufSize))(owned.ptr),
                max_item_size: (sym!(sdk, CDAQInfo_getMaxItemSize))(owned.ptr),
                tick_ns: (sym!(sdk, CDAQInfo_getTick_ns))(owned.ptr),
            }
        };
        runtime::check()?;
        Ok(info)
    }

    /// Live status of the DAQ engine.
    pub fn status(&self) -> Result<DaqStatus, Error> {
        let sdk = self.sdk();
        // SAFETY: live handle; the returned CDAQStatus is owned by us.
        let h = unsafe { (sym!(sdk, CDAQController_status))(self.inner.ptr) };
        // SAFETY: `h` is a fresh CDAQStatus.
        let owned = unsafe { Owned::adopt(self.inner.winidea, h, sdk.delete_CDAQStatus)? };
        // SAFETY: `owned.ptr` is a live CDAQStatus for the whole block.
        let st = unsafe {
            DaqStatus {
                time: (sym!(sdk, CDAQStatus_getTime))(owned.ptr),
                last_loop_time: (sym!(sdk, CDAQStatus_getLastLoopTime))(owned.ptr),
                num_samples_available: (sym!(sdk, CDAQStatus_getNumSamplesAvailable))(owned.ptr),
                num_loops: (sym!(sdk, CDAQStatus_getNumLoops))(owned.ptr),
                overflow: (sym!(sdk, CDAQStatus_getOverflow))(owned.ptr),
                global_enable: (sym!(sdk, CDAQStatus_getGlobalEnable))(owned.ptr),
                core_enable: (sym!(sdk, CDAQStatus_getCoreEnable))(owned.ptr),
            }
        };
        runtime::check()?;
        Ok(st)
    }

    /// The SDK's own human-readable rendering of the current status.
    ///
    /// Useful for logging and for cross-checking [`DaqController::status`]
    /// against what winIDEA itself reports.
    pub fn status_string(&self) -> Result<String, Error> {
        let sdk = self.sdk();
        // SAFETY: live handle; the returned CDAQStatus is owned by us.
        let h = unsafe { (sym!(sdk, CDAQController_status))(self.inner.ptr) };
        // SAFETY: `h` is a fresh CDAQStatus.
        let owned = unsafe { Owned::adopt(self.inner.winidea, h, sdk.delete_CDAQStatus)? };
        // SAFETY: live CDAQStatus handle.
        let p = unsafe { (sym!(sdk, CDAQStatus_toString))(owned.ptr) };
        runtime::check()?;
        // SAFETY: `p` is null or a NUL-terminated string from the SDK, produced
        // through our registered string callback.
        Ok(unsafe { runtime::from_cstr(p) }.unwrap_or_default())
    }

    /// Read every sample currently buffered.
    ///
    /// Samples are copied out of the SDK's vector, which is released before
    /// returning, so the result borrows nothing.
    ///
    /// Note there is no count limit. The SDK's `maxSamples` parameter is
    /// documented as **"not used in SDK versions 9.21.2 and later"**, so
    /// accepting one would be a lie: it always returns everything acquired
    /// since the last read. Poll often enough, or watch
    /// [`DaqStatus::overflow`], to avoid losing samples.
    pub fn read(&mut self) -> Result<Vec<Sample>, Error> {
        let sdk = self.sdk();
        // SAFETY: creates an empty DAQSampleVector we own.
        let vec_ptr = unsafe { (sym!(sdk, new_DAQSampleVector__SWIG_0))() };
        // SAFETY: `vec_ptr` is a fresh, empty DAQSampleVector.
        let vec = unsafe { Owned::adopt(self.inner.winidea, vec_ptr, sdk.delete_DAQSampleVector)? };

        // SAFETY: both handles are live; the SDK fills `vec`.
        unsafe { (sym!(sdk, CDAQController_read__SWIG_1))(self.inner.ptr, vec.ptr) };
        runtime::check()?;

        // SAFETY: live vector handle.
        let n = unsafe { (sym!(sdk, DAQSampleVector_size))(vec.ptr) };
        runtime::check()?;

        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            // SAFETY: 0 <= i < size, so the element pointer is valid. It is
            // BORROWED from the vector -- do not free it.
            let s = unsafe { (sym!(sdk, DAQSampleVector_getitem))(vec.ptr, i as i32) };
            runtime::check()?;
            if s.is_null() {
                continue;
            }
            // SAFETY: `s` is a live CDAQSample owned by `vec`.
            let (index, time_ns) = unsafe {
                (
                    (sym!(sdk, CDAQSample_getIndex))(s),
                    (sym!(sdk, CDAQSample_getTime))(s),
                )
            };
            runtime::check()?;

            // Ask the controller to interpret the sample's payload. A failure
            // here must not abort the whole drain: the remaining samples are
            // still good, and an undecodable one keeps its index/timestamp.
            // SAFETY: both handles are live; the returned CValueType is ours.
            let raw = unsafe { (sym!(sdk, CDAQController_getDataValue))(self.inner.ptr, s) };
            let value = match runtime::check() {
                // SAFETY: `raw` is null or a CValueType we own.
                Ok(()) => unsafe { value::decode_owned(self.inner.winidea, raw) }.ok(),
                Err(_) => None,
            };

            out.push(Sample {
                index,
                time_ns,
                value,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag encodings are load-bearing: a wrong discriminant silently
    /// configures the wrong sampling rate rather than failing.
    #[test]
    fn sampling_flag_encoding() {
        assert_eq!(SamplingFlags::Max.to_raw(), 0);
        assert_eq!(SamplingFlags::Every1ms.to_raw(), 1);
        assert_eq!(SamplingFlags::Every10ms.to_raw(), 2);
        assert_eq!(SamplingFlags::Every100ms.to_raw(), 3);
        assert_eq!(SamplingFlags::Every1s.to_raw(), 4);
        assert_eq!(SamplingFlags::Raw(42).to_raw(), 42);
        assert_eq!(SamplingFlags::default(), SamplingFlags::Max);
    }

    /// A requested interval must never resolve to a SLOWER tier: the user asked
    /// for updates at least that often, and silently halving the rate would be
    /// a worse lie than visibly over-sampling.
    #[test]
    fn interval_snaps_down_to_a_tier() {
        assert_eq!(SamplingFlags::from_interval_ms(0), SamplingFlags::Max);
        assert_eq!(SamplingFlags::from_interval_ms(1), SamplingFlags::Every1ms);
        // The default rate for a debugger variable.
        assert_eq!(
            SamplingFlags::from_interval_ms(10),
            SamplingFlags::Every10ms
        );

        // Between tiers, the FASTER one wins -- a requested rate is a "at least
        // this often" promise, so 5ms must not silently become 10ms updates and
        // 50ms must not become 100ms ones.
        assert_eq!(SamplingFlags::from_interval_ms(5), SamplingFlags::Every1ms);
        assert_eq!(
            SamplingFlags::from_interval_ms(50),
            SamplingFlags::Every10ms
        );
        assert_eq!(
            SamplingFlags::from_interval_ms(99),
            SamplingFlags::Every10ms
        );
        assert_eq!(
            SamplingFlags::from_interval_ms(100),
            SamplingFlags::Every100ms
        );
        assert_eq!(
            SamplingFlags::from_interval_ms(500),
            SamplingFlags::Every100ms
        );
        assert_eq!(
            SamplingFlags::from_interval_ms(1000),
            SamplingFlags::Every1s
        );
        assert_eq!(
            SamplingFlags::from_interval_ms(60_000),
            SamplingFlags::Every1s
        );

        // The invariant behind all of the above, stated directly.
        for ms in [1u32, 5, 10, 50, 99, 100, 500, 1000, 5000] {
            if let Some(eff) = SamplingFlags::from_interval_ms(ms).interval_ms() {
                assert!(
                    eff <= ms,
                    "requesting {ms}ms resolved to {eff}ms, which is SLOWER than asked"
                );
            }
        }

        for ms in [1u32, 10, 100, 1000] {
            assert_eq!(
                SamplingFlags::from_interval_ms(ms).interval_ms(),
                Some(ms),
                "an exact tier value must round-trip"
            );
        }
        // Max has no fixed period, so it must not claim one.
        assert_eq!(SamplingFlags::Max.interval_ms(), None);
        assert_eq!(SamplingFlags::Raw(7).interval_ms(), None);
    }
}
