//! Attaches one further core through winIDEA's session and reports what the
//! returned connection debugs: `core_launch_probe <primary workspace> <core index>`.
//!
//! winIDEA opens the core's window (creating its workspace if needed), which is
//! the normal way it debugs a further AURIX core. Nothing is run, halted or
//! written.

use winidea_sdk::session::SessionCtrl;
use winidea_sdk::{ConnectionMgr, DebugFacade, ExecutionController, IdeController, PathType, WinIdea};

fn main() {
    let mut args = std::env::args().skip(1);
    let ws = args.next().expect("usage: core_launch_probe <workspace> <index>");
    let index: u32 = args.next().expect("core index").parse().expect("numeric core index");

    let winidea = WinIdea::load(&winidea_sdk::default_search_paths()).expect("load isystemConnect.dll");
    let norm = |s: &str| s.replace('\\', "/").to_ascii_lowercase();
    let inst = ConnectionMgr::new(&winidea)
        .unwrap()
        .enumerate_instances()
        .unwrap()
        .into_iter()
        .find(|i| norm(&i.workspace) == norm(&ws))
        .expect("no winIDEA instance with that workspace");
    let mut primary = ConnectionMgr::new(&winidea).unwrap();
    primary.connect_tcp("", inst.tcp_port).unwrap();

    {
        let mut session = SessionCtrl::new(&winidea, &primary).unwrap();
        for soc in session.topology().unwrap() {
            println!("SoC {} ({})", soc.name, soc.device);
            for c in &soc.cores {
                println!(
                    "  {} {:<5} {:<10} attached={:?}",
                    c.index,
                    c.name,
                    c.core_type,
                    session.instance_is_attached(c.index)
                );
            }
        }
        let core = match session.instance_attach(index) {
            Ok(c) => c,
            Err(e) => return println!("instance_attach({index}): {e}"),
        };
        println!("instance_attach({index}) ok; is_attached now {:?}", session.instance_is_attached(index));
        let mut ide = IdeController::new(&winidea, &core).unwrap();
        println!("  workspace: {:?}", ide.path(PathType::WorkspaceFileName));
        let exec = ExecutionController::new(&winidea, &core).unwrap();
        match exec.cpu_status() {
            Ok(s) => println!(
                "  status: running={} stopped={} attached={} pc={:#x}",
                s.is_running, s.is_stopped, s.is_attached, s.execution_point
            ),
            Err(e) => println!("  status: {e}"),
        }
        let mut dbg = DebugFacade::new(&winidea, &core).unwrap();
        println!("  PC register: {:?}", dbg.read_register("PC").map(|v| v.to_string()));
    }
    let _ = primary.disconnect_keep();
}
