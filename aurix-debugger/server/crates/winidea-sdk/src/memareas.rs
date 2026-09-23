//! Which memory area an address belongs to.
//!
//! Every memory, breakpoint and expression call in the SDK takes an address
//! *and* a `mem_area`: targets with more than one address space need both
//! halves to identify a location. AURIX presents one flat 32-bit space to the
//! debugger, so the server has a plain `u32` and needs to know which area id to
//! pair it with.
//!
//! `CSystemMemoryAreas` reports those ids, which is better than assuming 0:
//! they are a property of the winIDEA target configuration, not a constant.

use crate::runtime::{self, Error, Owned};
use crate::{DataController, WinIdea};

/// The memory area ids a target defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemAreas {
    pub code_physical: i32,
    pub code_virtual: i32,
    pub data_physical: i32,
    pub data_virtual: i32,
    pub sfr: i32,
}

impl MemAreas {
    /// Reads the ids from a connected target.
    pub fn read(winidea: &WinIdea, data: &mut DataController<'_>) -> Result<Self, Error> {
        let sdk = winidea.raw();
        // SAFETY: live handle; the returned object belongs to the controller.
        let ptr = unsafe { (sym!(sdk, CDataController_getSystemMemoryAreas))(data.as_ptr()) };
        runtime::check()?;
        // Borrowed from the data controller, so it is not freed here.
        let owned = unsafe { Owned::adopt(winidea, ptr, None)? };
        // SAFETY: live handle; each getter takes only the object.
        let areas = unsafe {
            MemAreas {
                code_physical: (sym!(sdk, CSystemMemoryAreas_getMemAreaCodePhysical))(owned.ptr),
                code_virtual: (sym!(sdk, CSystemMemoryAreas_getMemAreaCodeVirtual))(owned.ptr),
                data_physical: (sym!(sdk, CSystemMemoryAreas_getMemAreaDataPhysical))(owned.ptr),
                data_virtual: (sym!(sdk, CSystemMemoryAreas_getMemAreaDataVirtual))(owned.ptr),
                sfr: (sym!(sdk, CSystemMemoryAreas_getMemAreaSFR))(owned.ptr),
            }
        };
        runtime::check()?;
        Ok(areas)
    }

    /// The area to use for plain data reads and writes.
    ///
    /// AURIX addresses reach memory and peripherals through one flat space, so
    /// the virtual data area is what a `mdw 0x70000000` means. Falls back to
    /// the physical one when the target reports no virtual area.
    pub fn data_area(self) -> u8 {
        let a = if self.data_virtual >= 0 { self.data_virtual } else { self.data_physical };
        a.max(0) as u8
    }

    /// The area to use for breakpoints and instruction fetch.
    pub fn code_area(self) -> i32 {
        if self.code_virtual >= 0 { self.code_virtual } else { self.code_physical }
    }
}
