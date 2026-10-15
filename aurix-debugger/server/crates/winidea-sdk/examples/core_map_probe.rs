//! Which physical core does each winIDEA instance drive?
//!
//! On AURIX, winIDEA debugs extra cores in their own instances, and the
//! project decides how cores are grouped -- `launchCore(n)` indices are not
//! TriCore core ids. This lists every running instance, attaches to each by
//! TCP port (never launching), and reads what identifies the core: its status,
//! PC and the TriCore `CORE_ID` register. Read-only.

use winidea_sdk::{ConnectionMgr, DebugFacade, ExecutionController, WinIdea};

fn main() {
    let winidea = WinIdea::load(&winidea_sdk::default_search_paths()).expect("load isystemConnect.dll");
    let mut lister = ConnectionMgr::new(&winidea).unwrap();
    let instances = lister.enumerate_instances().expect("enumerate winIDEA instances");
    for inst in &instances {
        println!("\n{}  (port {})", inst.workspace, inst.tcp_port);
        let mut conn = ConnectionMgr::new(&winidea).unwrap();
        if let Err(e) = conn.connect_tcp("", inst.tcp_port) {
            println!("  connect: {e}");
            continue;
        }
        {
            let exec = ExecutionController::new(&winidea, &conn).unwrap();
            match exec.cpu_status() {
                Ok(s) => println!(
                    "  status: running={} stopped={} attached={} pc={:#x}",
                    s.is_running, s.is_stopped, s.is_attached, s.execution_point
                ),
                Err(e) => println!("  status: {e}"),
            }
            let mut dbg = DebugFacade::new(&winidea, &conn).unwrap();
            for reg in ["CORE_ID", "CPU_ID", "PC"] {
                match dbg.read_register(reg) {
                    Ok(v) => println!("  {reg:<8} = {v}"),
                    Err(e) => println!("  {reg:<8} : {e}"),
                }
            }
        }
        let _ = conn.disconnect_keep();
    }
}
