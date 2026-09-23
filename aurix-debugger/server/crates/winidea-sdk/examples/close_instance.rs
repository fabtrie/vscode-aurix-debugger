//! Closes the winIDEA instance with this workspace: `close_instance <workspace>`.
use winidea_sdk::{ConnectionMgr, DetachFlags, CloseServer, AutoSave, WinIdea};
fn main() {
    let ws = std::env::args().nth(1).unwrap();
    let w = WinIdea::load(&winidea_sdk::default_search_paths()).unwrap();
    let norm = |s: &str| s.replace(char::from(92u8), "/").to_ascii_lowercase();
    let inst = ConnectionMgr::new(&w).unwrap().enumerate_instances().unwrap().into_iter()
        .find(|i| norm(&i.workspace) == norm(&ws)).expect("no such instance");
    let mut c = ConnectionMgr::new(&w).unwrap();
    c.connect_tcp("", inst.tcp_port).unwrap();
    println!("{:?}", c.disconnect(DetachFlags { close_server: CloseServer::Unconditional, auto_save: AutoSave::None }));
}
