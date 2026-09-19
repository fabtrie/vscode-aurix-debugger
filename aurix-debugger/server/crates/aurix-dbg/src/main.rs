//! aurix-dbg: OpenOCD-compatible AURIX debug server over Infineon TAS.
//!
//! Usage (same as OpenOCD):
//!   aurix-dbg -s <dir> -f board/infineon/kit_a2g_tc377tx_3v3_trb.cfg -f tc37x_ocds_init.cfg [-c cmd] [-d3]
//! Remote console client:
//!   aurix-dbg --remote [host:]port "halt" "reg pc" "resume"

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::exit;
use std::time::Duration;

use aurix_core::{log, log_error, log_info, log_user, telnet, Server};

const HELP: &str = "\
aurix-dbg - OpenOCD-compatible AURIX debug server over Infineon TAS

Usage: aurix-dbg [options]
  -f, --file <cfg>        run a configuration script (searched in -s dirs)
  -c, --command <cmd>     run a Tcl command
  -s, --search <dir>      add a script search directory
  -d<n>, --debug <n>      debug level 0..3 (default 2)
  -l, --log_output <file> append log to file
      --adapter <driver>  force the adapter driver (tas_client, sim)
  -h, --help              this help
  -v, --version           version
      --remote [host:]port <cmd>...   send commands to a running server's console
";

enum Action {
    File(String),
    Command(String),
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--remote") {
        exit(remote(&args[1..]));
    }
    // Server::new asks for the 1 ms timer resolution the poll loop and DAQ need.
    let server = Server::new();
    let cx = server.cx();
    let mut actions = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let value = |i: usize| -> String {
            args.get(i + 1).cloned().unwrap_or_else(|| {
                eprintln!("missing argument for {a}");
                exit(1)
            })
        };
        match a {
            "-f" | "--file" => {
                actions.push(Action::File(value(i)));
                i += 1;
            }
            "-c" | "--command" => {
                actions.push(Action::Command(value(i)));
                i += 1;
            }
            "-s" | "--search" => {
                let d = value(i);
                cx.with(|c| c.search_dirs.push(d.into()));
                i += 1;
            }
            "-l" | "--log_output" => {
                let f = value(i);
                if let Err(e) = log::set_output_file(&f) {
                    eprintln!("cannot open log file {f}: {e}");
                    exit(1);
                }
                i += 1;
            }
            "--adapter" => {
                let d = value(i);
                cx.with(|c| c.adapter_override = Some(d));
                i += 1;
            }
            "--debug" => {
                log::set_debug_level(value(i).parse().unwrap_or(3));
                i += 1;
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return;
            }
            "-v" | "--version" => {
                println!("aurix-dbg {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            s if s.starts_with("-d") => log::set_debug_level(s[2..].parse().unwrap_or(3)),
            other => {
                eprintln!("unknown option: {other}\n\n{HELP}");
                exit(1);
            }
        }
        i += 1;
    }
    if actions.is_empty() && Path::new("openocd.cfg").exists() {
        actions.push(Action::File("openocd.cfg".into()));
    }

    log_info!("aurix-dbg {} - OpenOCD-compatible AURIX debugger over TAS", env!("CARGO_PKG_VERSION"));
    for act in actions {
        let r = match &act {
            Action::File(f) => server.interp.eval_global(&format!("script {}", jim::quote(f))),
            Action::Command(c) => server.interp.eval_global(c),
        };
        match r {
            // Like OpenOCD: show -c output, but not the value of a sourced script.
            Ok(out) if !out.is_empty() && matches!(act, Action::Command(_)) => log_user!("{out}"),
            Ok(_) => {}
            Err(e) => {
                log_error!("{e}");
                exit(1);
            }
        }
        if cx.with(|c| c.shutdown) {
            return;
        }
    }
    if !cx.with(|c| c.initialized || c.noinit) {
        if let Err(e) = server.interp.eval_global("init") {
            log_error!("{e}");
            exit(1);
        }
    }

    let (bind, telnet_port, tcf_port, api_port) = cx.with(|c| (c.ports.bind.clone(), c.ports.telnet, c.ports.tcf, c.ports.api));
    if let Some(port) = telnet_port {
        match telnet::start(server.handle(), &bind, port) {
            Ok(p) => log_info!("Listening on port {p} for telnet connections"),
            Err(e) => log_error!("cannot listen on telnet port {port}: {e}"),
        }
    }
    // One session for the TCF and API frontends: they share breakpoints and stepping state.
    let session = tcf::session(server.handle());
    if let Some(port) = tcf_port {
        match tcf::listen(&session, &bind, port) {
            Ok(p) => log_info!("Listening on port {p} for TCF connections"),
            Err(e) => log_error!("cannot listen on TCF port {port}: {e}"),
        }
    }
    #[cfg(feature = "api")]
    if let Some(port) = api_port {
        match api::start(server.handle(), session.clone(), &bind, port) {
            Ok(p) => log_info!("Listening on port {p} for REST/MCP connections (API docs: http://{bind}:{p}/docs, MCP: http://{bind}:{p}/mcp)"),
            Err(e) => log_error!("cannot listen on API port {port}: {e}"),
        }
    }
    #[cfg(not(feature = "api"))]
    let _ = (api_port, &session);
    server.run();
}

fn read_until_prompt(s: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1024];
    loop {
        let n = s.read(&mut byte)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&byte[..n]);
        if buf.ends_with(telnet::PROMPT.as_bytes()) {
            buf.truncate(buf.len() - telnet::PROMPT.len());
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).replace("\r\n", "\n"))
}

fn remote(args: &[String]) -> i32 {
    let (addr, cmds) = match args.first() {
        Some(a) if a.contains(':') => (a.clone(), &args[1..]),
        Some(a) if a.parse::<u16>().is_ok() => (format!("localhost:{a}"), &args[1..]),
        _ => ("localhost:4444".to_string(), args),
    };
    let mut s = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot connect to {addr}: {e}");
            return 2;
        }
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(120)));
    if read_until_prompt(&mut s).is_err() {
        return 2;
    }
    for c in cmds {
        println!("> {c}");
        if s.write_all(format!("{c}\n").as_bytes()).is_err() {
            return 2;
        }
        match read_until_prompt(&mut s) {
            Ok(out) => {
                let out = out.trim_end();
                if !out.is_empty() {
                    println!("{out}");
                }
            }
            Err(e) => {
                eprintln!("read error: {e}");
                return 2;
            }
        }
    }
    0
}
