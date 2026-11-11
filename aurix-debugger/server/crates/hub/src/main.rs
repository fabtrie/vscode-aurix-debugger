// A GUI-subsystem program on Windows: started at logon or by the stdio wrapper it must never
// open a console window. The commands that print attach to the console of the shell that ran
// them (see `attach_console`).
#![cfg_attr(windows, windows_subsystem = "windows")]
//! aurix-hub: the always-on front door of aurix-dbg. See `aurix-hub --help`.

use std::time::Duration;

use hub::{config::HubConfig, hlog, http1, install, paths};

const HELP: &str = "\
aurix-hub - the always-on front door of aurix-dbg: MCP, REST and Swagger on one port,
debug sessions (one aurix-dbg worker per board), forwarding to them.

Usage:
  aurix-hub [serve] [--wait-port] [--foreground]
        Serve (default command). Detaches from the console unless --foreground.
  aurix-hub stdio [--url <url>] [--token <token>]
        MCP over stdin/stdout for clients that start servers as processes. Starts the hub of
        this machine when none answers.
  aurix-hub tcl [--url <url>] [--session <id>] <command>...
        Run Tcl console commands of a session (replaces aurix-dbg --remote).
  aurix-hub install [--autostart] [--remote] [--extension <dir>]
        Copy this build to %LOCALAPPDATA%\\aurix-dbg\\current; --autostart also starts the hub at
        logon (and now); --remote lets other machines in (creates a token in hub.toml).
  aurix-hub uninstall [--autostart-only] [--extension <dir>]
        Stop the hub and remove the logon start (and the files, unless still used).
  aurix-hub stop [--now]       Stop the hub (--now: close its sessions first).
  aurix-hub status             Show the hub, the installation and the paths as JSON.
  aurix-hub --version | --help

Settings: %APPDATA%\\aurix-dbg\\hub.toml; profiles: %APPDATA%\\aurix-dbg\\profiles\\<name>.toml;
logs: %LOCALAPPDATA%\\aurix-dbg\\logs. MCP: http://127.0.0.1:3001/mcp, API docs: /docs.
";

/// Prints to the console of the shell that ran us, when stdout is not redirected.
fn attach_console() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() || h as isize == -1 {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn print_lines(r: Result<Vec<String>, String>) -> i32 {
    match r {
        Ok(lines) => {
            for l in lines {
                println!("{l}");
            }
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn tcl(args: &[String]) -> i32 {
    let mut url = None;
    let mut session = None;
    let mut cmds = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--url" => {
                url = args.get(i + 1).cloned();
                i += 1;
            }
            "--session" => {
                session = args.get(i + 1).cloned();
                i += 1;
            }
            c => cmds.push(c.to_string()),
        }
        i += 1;
    }
    let cfg = HubConfig::load().unwrap_or_default();
    let url = url.unwrap_or_else(|| format!("http://127.0.0.1:{}", cfg.port));
    let base = match &session {
        Some(s) => format!("{}{s}", api_spec::SESSION_PREFIX),
        None => api_spec::SINGLE_SESSION_PREFIX.to_string(),
    };
    let token = std::env::var("AURIX_HUB_TOKEN").ok();
    let auth = token.map(|t| format!("Bearer {t}"));
    let mut headers = vec![(api_spec::CLIENT_HEADER, "aurix-hub-tcl")];
    if let Some(a) = &auth {
        headers.push(("Authorization", a.as_str()));
    }
    for c in cmds {
        println!("> {c}");
        match http1::json(&url, "POST", &format!("{base}/tcl"), &headers, Some(&serde_json::json!({ "command": c })), Duration::from_secs(600)) {
            Ok((200, v)) => {
                let out = v["output"].as_str().unwrap_or_default().trim_end();
                if !out.is_empty() {
                    println!("{out}");
                }
            }
            Ok((status, v)) => {
                eprintln!("error (HTTP {status}): {}", v["message"].as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
                return 1;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        }
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.first().map(String::as_str) {
        Some(c) if !c.starts_with('-') => (c.to_string(), args[1..].to_vec()),
        _ => ("serve".to_string(), args.clone()),
    };
    if flag(&args, "--help") || flag(&args, "-h") {
        attach_console();
        print!("{HELP}");
        return;
    }
    if flag(&args, "--version") || flag(&args, "-v") {
        attach_console();
        println!("aurix-hub {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let code = match cmd.as_str() {
        "serve" => {
            #[cfg(windows)]
            if !flag(&rest, "--foreground") {
                hub::detach::self_detach(&args);
            }
            if flag(&rest, "--foreground") {
                attach_console();
            } else {
                hub::log::quiet();
            }
            hub::log::to_file(&paths::hub_log());
            match HubConfig::load().and_then(|cfg| hub::serve(cfg, flag(&rest, "--wait-port"))) {
                Ok(()) => 0,
                Err(e) => {
                    hlog!("error: {e}");
                    1
                }
            }
        }
        "stdio" => hub::stdio::run(value(&rest, "--url"), value(&rest, "--token")),
        "tcl" => {
            attach_console();
            tcl(&rest)
        }
        "install" => {
            attach_console();
            print_lines(install::install(&install::InstallOptions {
                autostart: flag(&rest, "--autostart"),
                remote: flag(&rest, "--remote"),
                extension: value(&rest, "--extension"),
            }))
        }
        "uninstall" => {
            attach_console();
            print_lines(install::uninstall(flag(&rest, "--autostart-only"), value(&rest, "--extension")))
        }
        "stop" => {
            attach_console();
            match install::stop_hub(flag(&rest, "--now")) {
                Ok(true) => {
                    println!("stopped");
                    0
                }
                Ok(false) => {
                    println!("no hub runs");
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
        "status" => {
            attach_console();
            println!("{}", serde_json::to_string_pretty(&install::status()).unwrap_or_default());
            0
        }
        other => {
            attach_console();
            eprintln!("unknown command: {other}\n\n{HELP}");
            2
        }
    };
    std::process::exit(code);
}
