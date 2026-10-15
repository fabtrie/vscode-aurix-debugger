//! Reads 32-bit words through a running winIDEA, without examining or
//! changing anything: `peek <workspace> <addr> [<addr> ...]`.

use winidea_sdk::{AccessFlags, ConnectionMgr, DataController, WinIdea};

fn main() {
    let mut args = std::env::args().skip(1);
    let ws = args.next().expect("usage: peek <workspace> <addr>...");
    let winidea = WinIdea::load(&winidea_sdk::default_search_paths()).expect("load isystemConnect.dll");
    let mut lister = ConnectionMgr::new(&winidea).unwrap();
    let norm = |s: &str| s.replace('\\', "/").to_ascii_lowercase();
    let inst = lister
        .enumerate_instances()
        .unwrap()
        .into_iter()
        .find(|i| norm(&i.workspace) == norm(&ws))
        .expect("no winIDEA instance with that workspace");
    let mut conn = ConnectionMgr::new(&winidea).unwrap();
    conn.connect_tcp("", inst.tcp_port).unwrap();
    {
        let mut data = DataController::new(&winidea, &conn).unwrap();
        for a in args {
            let addr = u64::from_str_radix(a.trim_start_matches("0x"), 16).expect("hex address");
            match data.read_memory_maus(AccessFlags::REAL_TIME, 0, addr, 4, 1) {
                Ok(b) if b.is_fully_readable() => println!(
                    "{addr:#010x} = {:#010x}",
                    u32::from_le_bytes([b.data[0], b.data[1], b.data[2], b.data[3]])
                ),
                Ok(_) => println!("{addr:#010x} = <not readable>"),
                Err(e) => println!("{addr:#010x} : {e}"),
            }
        }
    }
    let _ = conn.disconnect_keep();
}
