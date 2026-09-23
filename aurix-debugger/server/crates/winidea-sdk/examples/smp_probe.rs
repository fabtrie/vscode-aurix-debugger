//! Which cores does a winIDEA instance debug, and which one has focus?
//!
//! `smp_probe <workspace>` attaches to the running instance with that
//! workspace (never launches) and prints its SMP core bindings and the
//! focused core. Read-only: nothing on the target or in winIDEA's focus is
//! changed.
//!
//! Signatures are from the managed wrappers in `tools/csil.txt`, which carry
//! the parameter names the P/Invoke stubs lose:
//! `VSPSessionTopology_CoreBinding get_core_SMP_bindings(uint32 dwSoCIndex)`,
//! `CSoCCtrl get_SoC(string strSoC)`, `uint32 SMP_get_focused_core(uint32 dwCoreIndex)`.

use std::ffi::{CStr, CString};

use winidea_sdk::{ConnectionMgr, WinIdea};

fn main() {
    let ws = std::env::args().nth(1).expect("usage: smp_probe <workspace>");
    let winidea = WinIdea::load(&winidea_sdk::default_search_paths()).expect("load isystemConnect.dll");
    let norm = |s: &str| s.replace('\\', "/").to_ascii_lowercase();
    let inst = ConnectionMgr::new(&winidea)
        .unwrap()
        .enumerate_instances()
        .unwrap()
        .into_iter()
        .find(|i| norm(&i.workspace) == norm(&ws))
        .expect("no winIDEA instance with that workspace");
    let mut conn = ConnectionMgr::new(&winidea).unwrap();
    conn.connect_tcp("", inst.tcp_port).unwrap();

    let sdk = winidea.raw();
    let check = |what: &str| {
        if let Err(e) = winidea.take_error() {
            println!("  {what}: {e}");
            false
        } else {
            true
        }
    };
    unsafe {
        let session = (sdk.new_CSessionCtrl.unwrap())(conn.as_ptr());
        if !check("new_CSessionCtrl") || session.is_null() {
            return;
        }
        let cstr = |p: *const std::ffi::c_char| {
            if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
        };
        let topo = (sdk.CSessionCtrl_get_topology.unwrap())(session);
        if check("get_topology") && !topo.is_null() {
            let socs = (sdk.SSessionTopology_SoCs.unwrap())(topo);
            let ns = (sdk.VSPSessionTopology_SoC_size.unwrap())(socs);
            for s in 0..ns as i32 {
                let soc = (sdk.VSPSessionTopology_SoC_getitem.unwrap())(socs, s);
                println!(
                    "topology SoC {s}: name={:?} device={:?}",
                    cstr((sdk.SSessionTopology_SoC_name.unwrap())(soc)),
                    cstr((sdk.SSessionTopology_SoC_device.unwrap())(soc))
                );
                let cores = (sdk.SSessionTopology_SoC_cores.unwrap())(soc);
                let nc = (sdk.VSPSessionTopology_Core_size.unwrap())(cores);
                for c in 0..nc as i32 {
                    let core = (sdk.VSPSessionTopology_Core_getitem.unwrap())(cores, c);
                    println!(
                        "  core index {c}: name={:?} type={:?}",
                        cstr((sdk.SSessionTopology_Core_name.unwrap())(core)),
                        cstr((sdk.SSessionTopology_Core_core_type.unwrap())(core))
                    );
                }
            }
            check("reading topology");
            if let Some(d) = sdk.delete_SSessionTopology {
                d(topo);
            }
        }
        for soc_index in 0..2u32 {
            let b = (sdk.CSessionCtrl_get_core_SMP_bindings__SWIG_0.unwrap())(session, soc_index);
            if !check(&format!("get_core_SMP_bindings({soc_index})")) || b.is_null() {
                continue;
            }
            let n = (sdk.VSPSessionTopology_CoreBinding_size.unwrap())(b);
            println!("SoC {soc_index}: {n} SMP binding(s)");
            for i in 0..n as i32 {
                let item = (sdk.VSPSessionTopology_CoreBinding_getitem.unwrap())(b, i);
                let name = (sdk.SSessionTopology_CoreBinding_name.unwrap())(item);
                let name = if name.is_null() { String::new() } else { CStr::from_ptr(name).to_string_lossy().into_owned() };
                let idx = (sdk.SSessionTopology_CoreBinding_core_indexes.unwrap())(item);
                let m = (sdk.VectorOnChipData_size.unwrap())(idx);
                let cores: Vec<u32> = (0..m as i32).map(|k| (sdk.VectorOnChipData_getitem.unwrap())(idx, k)).collect();
                println!("  binding {i}: name={name:?} core_indexes={cores:?}");
                if let Some(d) = sdk.delete_VectorOnChipData {
                    d(idx);
                }
            }
            check("reading bindings");
            if let Some(d) = sdk.delete_VSPSessionTopology_CoreBinding {
                d(b);
            }
        }
        let empty = CString::new("").unwrap();
        let soc = (sdk.CSessionCtrl_get_SoC.unwrap())(session, empty.as_ptr());
        if check("get_SoC(\"\")") && !soc.is_null() {
            for c in 0..3u32 {
                let f = (sdk.CSoCCtrl_SMP_get_focused_core.unwrap())(soc, c);
                if check(&format!("SMP_get_focused_core({c})")) {
                    println!("SMP_get_focused_core({c}) = {f}");
                }
            }
            if let Some(d) = sdk.delete_CSoCCtrl {
                d(soc);
            }
        }
        if let Some(d) = sdk.delete_CSessionCtrl {
            d(session);
        }
    }
    let _ = conn.disconnect_keep();
}
