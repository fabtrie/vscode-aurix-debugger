//! The debug session: which cores a SoC has, and attaching further cores.
//!
//! On AURIX, winIDEA debugs each core beyond the primary in an instance of its
//! own. [`SessionCtrl::instance_attach`] is how a core is attached: winIDEA
//! opens that core's window (creating its workspace if needed) and returns a
//! connection to it. That new window is normal.
//!
//! Core indices are the SoC topology's -- on a TC377: CPU0 = 0, CPU1 = 1,
//! CPU2 = 2, SCR = 3, HSM = 4 -- as [`SessionCtrl::topology`] reports them.
//!
//! Signatures, parameter names and ownership are from the managed wrappers in
//! `tools/csil.txt` (`ConnectionMgr instance_attach(uint32 dwCoreIndex)`,
//! `bool instance_is_attached(uint32 dwCoreIndex)`,
//! `SSessionTopology get_topology()`), whose results are all caller-owned.

use std::ffi::CStr;

use crate::runtime::{self, Error, Owned};
use crate::{ConnectionMgr, WinIdea};

/// One core as the session topology lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyCore {
    /// Its index: what [`SessionCtrl::instance_attach`] takes.
    pub index: u32,
    /// winIDEA's name for it, e.g. `CPU1`, `SCR`, `HSM`.
    pub name: String,
    /// The core architecture, e.g. `TC1.6.2P`, `Cortex-M3`.
    pub core_type: String,
}

/// One SoC of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologySoC {
    pub name: String,
    pub device: String,
    pub cores: Vec<TopologyCore>,
}

/// The session of a connected winIDEA.
///
/// Borrows the connection it was made from (`'c`) for as long as it lives;
/// the connections [`SessionCtrl::instance_attach`] returns depend only on the
/// SDK (`'a`), so they outlive the session object.
pub struct SessionCtrl<'a, 'c> {
    inner: Owned<'a>,
    winidea: &'a WinIdea,
    _conn: std::marker::PhantomData<&'c ()>,
}

impl<'a, 'c> SessionCtrl<'a, 'c> {
    pub fn new(winidea: &'a WinIdea, conn: &'c ConnectionMgr<'_>) -> Result<Self, Error> {
        let sdk = winidea.raw();
        // SAFETY: `conn` outlives this controller, so its handle stays valid.
        let ptr = unsafe { (sym!(sdk, new_CSessionCtrl))(conn.as_ptr()) };
        // SAFETY: `ptr` is a fresh CSessionCtrl.
        let inner = unsafe { Owned::adopt(winidea, ptr, sdk.delete_CSessionCtrl)? };
        Ok(Self { inner, winidea, _conn: std::marker::PhantomData })
    }

    /// Whether the core with this topology index is attached. Read-only.
    pub fn instance_is_attached(&self, index: u32) -> Result<bool, Error> {
        // SAFETY: live handle.
        let v = unsafe { (sym!(self.inner.sdk(), CSessionCtrl_instance_is_attached))(self.inner.ptr, index) };
        runtime::check()?;
        Ok(v)
    }

    /// Attaches the core with this topology index and returns a connection to
    /// the winIDEA instance that debugs it.
    pub fn instance_attach(&mut self, index: u32) -> Result<ConnectionMgr<'a>, Error> {
        let sdk = self.inner.sdk();
        // SAFETY: live handle.
        let ptr = unsafe { (sym!(sdk, CSessionCtrl_instance_attach__SWIG_1))(self.inner.ptr, index) };
        // SAFETY: caller-owned, per the managed wrapper.
        let inner = unsafe { Owned::adopt(self.winidea, ptr, sdk.delete_ConnectionMgr)? };
        Ok(ConnectionMgr::from_owned(inner))
    }

    /// The SoCs and cores of this session, in index order. Read-only.
    pub fn topology(&self) -> Result<Vec<TopologySoC>, Error> {
        let sdk = self.inner.sdk();
        let text = |p: *const std::ffi::c_char| {
            if p.is_null() {
                String::new()
            } else {
                // SAFETY: a NUL-terminated string owned by the topology, copied now.
                unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
            }
        };
        // SAFETY: live handle; the topology is caller-owned and freed below.
        let topo = unsafe { (sym!(sdk, CSessionCtrl_get_topology))(self.inner.ptr) };
        runtime::check()?;
        let topo = unsafe { Owned::adopt(self.winidea, topo, sdk.delete_SSessionTopology)? };
        let mut out = Vec::new();
        // SAFETY: every pointer below is borrowed from `topo`, which outlives
        // this block; strings are copied as they are read.
        unsafe {
            let socs = (sym!(sdk, SSessionTopology_SoCs))(topo.ptr);
            for s in 0..(sym!(sdk, VSPSessionTopology_SoC_size))(socs) as i32 {
                let soc = (sym!(sdk, VSPSessionTopology_SoC_getitem))(socs, s);
                let cores_v = (sym!(sdk, SSessionTopology_SoC_cores))(soc);
                let mut cores = Vec::new();
                for c in 0..(sym!(sdk, VSPSessionTopology_Core_size))(cores_v) as i32 {
                    let core = (sym!(sdk, VSPSessionTopology_Core_getitem))(cores_v, c);
                    cores.push(TopologyCore {
                        index: c as u32,
                        name: text((sym!(sdk, SSessionTopology_Core_name))(core)),
                        core_type: text((sym!(sdk, SSessionTopology_Core_core_type))(core)),
                    });
                }
                out.push(TopologySoC {
                    name: text((sym!(sdk, SSessionTopology_SoC_name))(soc)),
                    device: text((sym!(sdk, SSessionTopology_SoC_device))(soc)),
                    cores,
                });
            }
        }
        runtime::check()?;
        Ok(out)
    }
}
