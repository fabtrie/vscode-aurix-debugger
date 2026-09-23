//! Does the winIDEA instance have debug symbols loaded? `sym_probe <workspace>`. Read-only.
use winidea_sdk::{ConnectionMgr, DataController, IdeController, WinIdea};
fn main() {
    let ws = std::env::args().nth(1).unwrap();
    let w = WinIdea::load(&winidea_sdk::default_search_paths()).unwrap();
    let norm = |s: &str| s.replace(char::from(92u8), "/").to_ascii_lowercase();
    let inst = ConnectionMgr::new(&w).unwrap().enumerate_instances().unwrap().into_iter()
        .find(|i| norm(&i.workspace) == norm(&ws)).unwrap();
    let mut c = ConnectionMgr::new(&w).unwrap();
    c.connect_tcp("", inst.tcp_port).unwrap();
    {
        let mut d = DataController::new(&w, &c).unwrap();
        match d.partitions() {
            Ok(p) => { println!("partitions: {}", p.len()); for x in &p { println!("  {x:?}"); } }
            Err(e) => println!("partitions: {e}"),
        }
        let mut ide = IdeController::new(&w, &c).unwrap();
        for opt in ["/IDE/Debug.DownloadFiles.DefaultFile", "/IDE/Debug.DownloadFiles.File[0].Path",
                    "/IDE/Debug.DownloadFiles.File[0].Options.LoadSymbols", "/IDE/Debug.DownloadFiles.File[0].Options.LoadCode"] {
            println!("{opt} = {:?}", ide.option_str(opt));
        }
    }
    let _ = c.disconnect_keep();
}
