//! Explore the winIDEA option tree of a running instance.
//!
//! Option paths are the addresses of winIDEA's entire configuration, but they
//! are workspace- and hardware-specific: `Hardware.Emulator` exists on some
//! setups and not others. winIDEA lists them under *Help | Display Option…*;
//! this does the same from the SDK, which is handy when you are automating a
//! machine you are not sitting at.
//!
//! ```text
//! options.exe -w C:\ws\proj.xjrf                # probe a set of common roots
//! options.exe -w C:\ws\proj.xjrf --path Hardware  # dump one subtree as JSON
//! ```

use std::path::PathBuf;

use winidea_sdk::{ConnectionMgr, WinIdea, ide::IdeController};

/// Roots worth probing when nothing specific is asked for. Each is checked with
/// `option_exists` first, so a missing one is reported rather than raising.
const COMMON_ROOTS: &[&str] = &[
    "Hardware",
    "Hardware.Emulator",
    "Hardware.Emulator.Type",
    "IDE",
    "Debug",
    "SoC",
    "Project",
    "Workspace",
    "Analyzer",
    "Tools",
];

fn main() -> std::process::ExitCode {
    let mut dll: Option<PathBuf> = None;
    let mut workspace = String::new();
    let mut path: Option<String> = None;
    let mut depth = 1usize;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dll" => dll = it.next().map(PathBuf::from),
            "--workspace" | "-w" => workspace = it.next().unwrap_or_default(),
            "--path" | "-p" => path = it.next(),
            "--depth" | "-d" => {
                depth = it.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            }
            "--help" | "-h" => {
                println!(
                    "usage: options.exe [--dll <p>] -w <workspace.xjrf> \
                     [--path <option>] [--depth <n>]"
                );
                return std::process::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument `{other}`");
                return std::process::ExitCode::FAILURE;
            }
        }
    }

    let winidea = match &dll {
        Some(p) => WinIdea::load_from(p.as_os_str()),
        None => WinIdea::load(&winidea_sdk::default_search_paths()),
    };
    let winidea = match winidea {
        Ok(w) => w,
        Err(e) => {
            eprintln!("load failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let mut conn = match ConnectionMgr::new(&winidea) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ConnectionMgr: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if let Err(e) = conn.connect_mru(&workspace) {
        eprintln!("connect: {e}");
        return std::process::ExitCode::FAILURE;
    }

    // Scoped so the controller's borrow of `conn` ends before we disconnect.
    {
        let mut ide = match IdeController::new(&winidea, &conn) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("IdeController: {e}");
                return std::process::ExitCode::FAILURE;
            }
        };

        match path {
            Some(p) => dump(&mut ide, &p, depth),
            None => {
                println!("Probing common option roots\n");
                for root in COMMON_ROOTS {
                    match ide.option_exists(root) {
                        Ok(true) => {
                            // Exists: show its value if it is a leaf, else its JSON.
                            let val = ide
                                .option_str(root)
                                .map(|v| format!("{v:?}"))
                                .unwrap_or_else(|_| "<not a string leaf>".into());
                            println!("  [exists] {root:32} {val}");
                        }
                        Ok(false) => println!("  [absent] {root}"),
                        Err(e) => println!("  [error ] {root:32} {e}"),
                    }
                }
                println!(
                    "\nTip: winIDEA's `Help | Display Option ...` menu shows the exact\n\
                 path of any option you can see in the UI."
                );
            }
        }
    }

    let _ = conn.disconnect_keep();
    std::process::ExitCode::SUCCESS
}

fn dump(ide: &mut IdeController<'_>, path: &str, depth: usize) {
    println!("option path: {path}\n");

    match ide.option_exists(path) {
        Ok(true) => println!("  exists: yes"),
        Ok(false) => {
            // Do NOT return: optionExists() proved unreliable in testing
            // (it reports false for paths the getters resolve fine), so the
            // getters below are the real test.
            println!("  exists: reported NO (trying getters anyway)");
        }
        Err(e) => println!("  exists: <query failed: {e}>"),
    }

    for (label, r) in [
        ("as string", ide.option_str(path).map(|v| format!("{v:?}"))),
        ("as int   ", ide.option_int(path).map(|v| v.to_string())),
        ("as float ", ide.option_float(path).map(|v| v.to_string())),
    ] {
        match r {
            Ok(v) => println!("  {label}: {v}"),
            Err(e) => println!("  {label}: <{}>", first_line(&e.to_string())),
        }
    }

    match ide.option_json(path) {
        Ok(j) => {
            println!("\n  JSON ({} bytes):", j.len());
            for line in j.lines().take(depth * 40) {
                println!("    {line}");
            }
        }
        Err(e) => println!("\n  JSON: <{}>", first_line(&e.to_string())),
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_string()
}
