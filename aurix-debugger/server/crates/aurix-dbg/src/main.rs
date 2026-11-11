//! aurix-dbg: OpenOCD-compatible AURIX debug server over Infineon TAS, the worker of one board.
//!
//! The hub (`aurix-hub`) starts it with `--hub <url> --session <id>`; the worker then listens on
//! free loopback ports and registers them with the hub. Run by hand (development, tests) it
//! takes the same options as OpenOCD:
//!   aurix-dbg -s <dir> -f board/infineon/kit_a2g_tc375_lite.cfg -f my_board.cfg [-c cmd] [-d3]

use std::path::Path;
use std::process::exit;

use aurix_core::{log, log_error, log_info, log_user, telnet, Server};

const HELP: &str = "\
aurix-dbg - OpenOCD-compatible AURIX debug server over Infineon TAS (the worker of one board)

Usage: aurix-dbg [options]
  -f, --file <cfg>        run a configuration script (searched in -s dirs)
  -c, --command <cmd>     run a Tcl command
  -s, --search <dir>      add a script search directory
  -d<n>, --debug <n>      debug level 0..3 (default 2)
  -l, --log_output <file> append log to file
      --adapter <driver>  force the adapter driver (tas_client, sim, winidea, bluebox)
      --hub <url>         register with the hub at <url> (set by aurix-hub)
      --session <id>      the hub's session ID (set by aurix-hub)
  -h, --help              this help
  -v, --version           version

The servers listen on free loopback ports unless the configuration sets telnet_port, tcf_port
and api_port; the log shows them. Normally aurix-hub starts this program: see `aurix-hub --help`.
";

enum Action {
    File(String),
    Command(String),
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Server::new asks for the 1 ms timer resolution the poll loop and DAQ need.
    let server = Server::new();
    let cx = server.cx();
    let mut actions = Vec::new();
    let mut hub: Option<String> = None;
    let mut session: Option<String> = None;
    let mut log_file: Option<String> = None;
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
                log_file = Some(f);
                i += 1;
            }
            "--adapter" => {
                let d = value(i);
                cx.with(|c| c.adapter_override = Some(d));
                i += 1;
            }
            "--hub" => {
                hub = Some(value(i));
                i += 1;
            }
            "--session" => {
                session = Some(value(i));
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
    // The hub sends the worker's console output to the session log and names it here.
    if log_file.is_none() {
        log_file = std::env::var("AURIX_DBG_LOG_FILE").ok().filter(|f| !f.is_empty());
    }
    if hub.is_some() != session.is_some() {
        eprintln!("--hub and --session go together");
        exit(1);
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

    // A worker of the hub serves only its own machine: the hub is the one exposed port.
    if hub.is_some() {
        cx.with(|c| c.ports.bind = "127.0.0.1".into());
    }
    let (bind, telnet_port, tcf_port, api_port) = cx.with(|c| (c.ports.bind.clone(), c.ports.telnet, c.ports.tcf, c.ports.api));
    let mut telnet_actual = None;
    if let Some(port) = telnet_port {
        match telnet::start(server.handle(), &bind, port) {
            Ok(p) => {
                log_info!("Listening on port {p} for telnet connections");
                telnet_actual = Some(p);
            }
            Err(e) => log_error!("cannot listen on telnet port {port}: {e}"),
        }
    }
    // One session for the TCF and API frontends: they share breakpoints and stepping state.
    let shared = tcf::session(server.handle());
    let mut tcf_actual = None;
    if let Some(port) = tcf_port {
        match tcf::listen(&shared, &bind, port) {
            Ok(p) => {
                log_info!("Listening on port {p} for TCF connections");
                tcf_actual = Some(p);
            }
            Err(e) => log_error!("cannot listen on TCF port {port}: {e}"),
        }
    }
    #[cfg(feature = "api")]
    {
        let meta = api::WorkerMeta { session: session.clone(), log_file: log_file.clone() };
        let api_actual = match api_port {
            Some(port) => match api::start_with(server.handle(), shared.clone(), &bind, port, meta) {
                Ok(p) => {
                    log_info!("Listening on port {p} for REST connections");
                    Some(p)
                }
                Err(e) => {
                    log_error!("cannot listen on API port {port}: {e}");
                    None
                }
            },
            None => None,
        };
        if let (Some(url), Some(id)) = (&hub, &session) {
            let Some(api_port) = api_actual else {
                log_error!("a worker of the hub needs its API port (api_port must not be disabled)");
                exit(1);
            };
            let (adapter, targets) = cx.with(|c| {
                let adapter = c.adapter.as_deref().map(|a| a.driver_name().to_string()).or_else(|| c.adapter_name.clone());
                (adapter, c.targets.iter().map(|t| t.name.clone()).collect::<Vec<_>>())
            });
            let reg = api::hub_client::WorkerRegistration {
                session: id.clone(),
                pid: std::process::id(),
                exe: std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                version: env!("CARGO_PKG_VERSION").into(),
                api_port,
                tcf_port: tcf_actual,
                telnet_port: telnet_actual,
                adapter,
                targets,
                log_file: log_file.clone(),
            };
            let handle = server.handle();
            api::hub_client::start(url.clone(), reg, move |why| {
                log_error!("shutting down: {why}");
                let _ = handle.tcl("shutdown");
            });
        }
    }
    #[cfg(not(feature = "api"))]
    let _ = (api_port, &shared, &log_file, tcf_actual, telnet_actual);
    server.run();
    #[cfg(feature = "api")]
    if let (Some(url), Some(id)) = (&hub, &session) {
        api::hub_client::deregister(url, id);
    }
}
