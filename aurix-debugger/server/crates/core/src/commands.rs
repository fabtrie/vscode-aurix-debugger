//! OpenOCD-compatible Tcl commands.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jim::{list, CmdResult, Interp};

use crate::adapter;
use crate::ctx::{Cx, Mode, Ocmts, Shared, Tap};
use crate::error::Error;
use crate::events::{emit, Event};
use crate::flash::{self, FlashBank};
use crate::gtm_mcs::DEFAULT_RAM_SIZE;
use crate::image::{Image, ImageFile};
use crate::ops;
use crate::target::{BpKind, Core, TargetEntry, TargetState, WatchAccess};
use crate::util::{parse_bool, parse_u32, parse_u64};
use crate::{log, log_info, log_user, log_warn};

/// Tcl procs normally provided by OpenOCD's startup scripts.
pub const PRELUDE: &str = r#"
proc init_targets {} {}
proc init_board {} {}
proc using_jtag {} { return 0 }
proc using_swd {} { return 0 }
proc using_hla {} { return 0 }
proc ocd_process_reset {MODE} { reset $MODE }
proc write_memory {address width data {phys ""}} { _aurix_write_memory $address $width {*}$data }
"#;

type CmdFn = fn(&Cx, &[&str]) -> CmdResult;

fn e2s(e: Error) -> String {
    e.to_string()
}

fn reg(interp: &Interp, sh: &Shared, name: &str, f: CmdFn) {
    let sh = sh.clone();
    interp.register(name, move |i, a| f(&Cx { interp: i, sh: &sh }, a));
}

fn ok() -> CmdResult {
    Ok(String::new())
}

fn num(s: &str) -> Result<u64, String> {
    parse_u64(s)
}

fn num32(s: &str) -> Result<u32, String> {
    parse_u32(s)
}

pub fn register_all(interp: &Interp, sh: &Shared) {
    // configuration
    reg(interp, sh, "adapter", cmd_adapter);
    reg(interp, sh, "tas-client", cmd_driver_group);
    #[cfg(feature = "winidea")]
    reg(interp, sh, "winidea", cmd_driver_group);
    reg(interp, sh, "transport", cmd_transport);
    reg(interp, sh, "reset_config", cmd_reset_config);
    reg(interp, sh, "ifxdap", cmd_ifxdap);
    reg(interp, sh, "jtag", cmd_jtag);
    reg(interp, sh, "ocmts", cmd_ocmts);
    reg(interp, sh, "target", cmd_target);
    reg(interp, sh, "targets", cmd_targets);
    reg(interp, sh, "flash", cmd_flash);
    reg(interp, sh, "verify_image", cmd_verify_image);
    reg(interp, sh, "program", cmd_program);
    reg(interp, sh, "aurix_eflash", cmd_aurix_eflash);
    reg(interp, sh, "init", cmd_init);
    reg(interp, sh, "noinit", |cx, _| {
        cx.with(|c| c.noinit = true);
        ok()
    });
    reg(interp, sh, "shutdown", |cx, _| {
        cx.with(|c| c.shutdown = true);
        emit(Event::Shutdown);
        Ok("shutdown command invoked".into())
    });
    reg(interp, sh, "find", cmd_find);
    reg(interp, sh, "script", cmd_script);
    reg(interp, sh, "add_script_search_dir", |cx, a| {
        let d = a.get(1).ok_or("usage: add_script_search_dir <directory>")?;
        cx.with(|c| c.search_dirs.push(PathBuf::from(d)));
        ok()
    });
    reg(interp, sh, "echo", cmd_echo);
    reg(interp, sh, "sleep", cmd_sleep);
    reg(interp, sh, "version", |_, _| Ok(format!("aurix-dbg {} (OpenOCD-compatible, TAS)", env!("CARGO_PKG_VERSION"))));
    reg(interp, sh, "debug_level", |_, a| {
        if let Some(l) = a.get(1) {
            log::set_debug_level(num32(l)? as u8);
        }
        Ok(format!("debug_level: {}", log::debug_level()))
    });
    reg(interp, sh, "log_output", |_, a| {
        let f = a.get(1).ok_or("usage: log_output <file>")?;
        log::set_output_file(f).map_err(|e| e.to_string())?;
        ok()
    });
    reg(interp, sh, "poll", cmd_poll);
    reg(interp, sh, "polling_period", |cx, a| {
        if let Some(ms) = a.get(1) {
            let ms = num(ms)?.max(1);
            cx.with(|c| c.poll_period_ms = ms);
        }
        Ok(format!("{}", cx.with(|c| c.poll_period_ms)))
    });
    // Read-only on purpose: DAQ sessions belong to their WebSocket, and a session created from
    // the console would have no owner to die with. This is here so `monitor daq` (or telnet) can
    // answer "what is hammering the probe right now?" without adding an API operation.
    reg(interp, sh, "daq", |cx, a| {
        match a.get(1).copied() {
            None | Some("status") => {}
            // How far the sampler may over-read to fold two points into one bus access. A read
            // costs ~57 us; a byte inside one costs a fraction of that, so a small hole is worth
            // reading. Never applied in peripheral space, whatever this is set to.
            Some("gap") => {
                if let Some(v) = a.get(2) {
                    let g = num(v)? as u32;
                    cx.with(|c| c.daq_gap = g);
                }
                return Ok(format!("{}", cx.with(|c| c.daq_gap)));
            }
            // Whether a due point rides in the run-state poll's batch instead of costing its own
            // round trip. On by default; off is here to measure what it is worth.
            Some("merge") => {
                if let Some(v) = a.get(2) {
                    let on = num(v)? != 0;
                    cx.with(|c| c.daq_merge = on);
                }
                return Ok(format!("{}", cx.with(|c| c.daq_merge as u8)));
            }
            Some(other) => {
                return Err(format!(
                    "usage: daq ?status|gap ?bytes?|merge ?0|1?? (sessions are opened on the DAQ WebSocket, not '{other}')"
                ))
            }
        }
        Ok(cx.with(|c| {
            let mut out = Vec::new();
            // What the probe will take in one packet decides how a tick is split, and therefore
            // how many scattered points can share a round trip.
            if let Some(a) = c.adapter.as_ref() {
                let l = a.limits();
                out.push(format!(
                    "probe: {} read(s) per packet, {} B request, {} B response; coalescing gap {} B",
                    l.max_num_rw, l.max_rq, l.max_rsp, c.daq_gap
                ));
            }
            for s in c.daq.sessions() {
                let st = s.stats();
                out.push(format!("session {}: {} point(s), {} batch(es), {} dropped, {} missed", s.id, st.points, st.seq, st.dropped, st.missed));
                for p in &s.points {
                    let space = match p.space {
                        crate::daq::Space::Host => String::from("bus"),
                        crate::daq::Space::Core(t) => c.targets.get(t).map(|e| e.name.clone()).unwrap_or_else(|| t.to_string()),
                    };
                    let mode = match p.mode {
                        crate::daq::SampleMode::EveryTick => "every_tick",
                        crate::daq::SampleMode::OnChange => "on_change",
                    };
                    out.push(format!(
                        "  {:3} 0x{:08x} {:4} B  asked {:6} us  scheduled {:6} us  achieved {:>6}  delivered {:>6}  {mode:10} via {space}{}",
                        p.id,
                        p.addr,
                        p.len,
                        p.interval_us(),
                        p.eff_us(),
                        p.achieved_us().map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
                        p.delivered_us().map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
                        if p.muted { "  MUTED" } else { "" }
                    ));
                }
            }
            if out.is_empty() {
                out.push("no DAQ sessions".into());
            }
            out.join("\n")
        }))
    });
    reg(interp, sh, "tcf_port", |cx, a| port_cmd(cx, a, |p| &mut p.tcf));
    reg(interp, sh, "telnet_port", |cx, a| port_cmd(cx, a, |p| &mut p.telnet));
    reg(interp, sh, "gdb_port", |cx, a| port_cmd(cx, a, |p| &mut p.gdb));
    reg(interp, sh, "tcl_port", |cx, a| port_cmd(cx, a, |p| &mut p.tcl));
    reg(interp, sh, "api_port", |cx, a| port_cmd(cx, a, |p| &mut p.api));
    reg(interp, sh, "bindto", |cx, a| {
        if let Some(b) = a.get(1) {
            cx.with(|c| c.ports.bind = b.to_string());
        }
        Ok(cx.with(|c| c.ports.bind.clone()))
    });
    for noop in ["add_help_text", "add_usage_text", "usage", "gdb_memory_map", "gdb_flash_program", "gdb_breakpoint_override"] {
        reg(interp, sh, noop, |_, _| ok());
    }
    reg(interp, sh, "help", |_, _| {
        Ok("aurix-dbg supports the OpenOCD commands used by AURIX cfg files: adapter, transport, ifxdap, ocmts, \
            target, targets, flash bank, init, reset, halt, resume, step, reg, bp/rbp, wp/rwp, mdw/mdh/mdb, \
            mww/mwh/mwb, read_memory, write_memory, poll, sleep, echo, find, script, tricore tl1_halt/tl1_step/irq_mask"
            .into())
    });

    // execution
    reg(interp, sh, "mww", |cx, a| cmd_mw(cx, a, 4));
    reg(interp, sh, "mwh", |cx, a| cmd_mw(cx, a, 2));
    reg(interp, sh, "mwb", |cx, a| cmd_mw(cx, a, 1));
    reg(interp, sh, "mdw", |cx, a| cmd_md(cx, a, 4));
    reg(interp, sh, "mdh", |cx, a| cmd_md(cx, a, 2));
    reg(interp, sh, "mdb", |cx, a| cmd_md(cx, a, 1));
    reg(interp, sh, "read_memory", cmd_read_memory);
    reg(interp, sh, "_aurix_write_memory", cmd_write_memory);
    reg(interp, sh, "halt", cmd_halt);
    reg(interp, sh, "wait_halt", cmd_wait_halt);
    reg(interp, sh, "resume", cmd_resume);
    reg(interp, sh, "step", cmd_step);
    reg(interp, sh, "reset", cmd_reset);
    reg(interp, sh, "reg", cmd_reg);
    reg(interp, sh, "bp", cmd_bp);
    reg(interp, sh, "rbp", cmd_rbp);
    reg(interp, sh, "wp", cmd_wp);
    reg(interp, sh, "rwp", cmd_rwp);
    reg(interp, sh, "tricore", cmd_tricore);
    reg(interp, sh, "cortex_m", cmd_tricore);
    reg(interp, sh, "gtm_mcs", cmd_tricore);
}

fn port_cmd(cx: &Cx, a: &[&str], field: fn(&mut crate::ctx::Ports) -> &mut Option<u16>) -> CmdResult {
    if let Some(p) = a.get(1) {
        let v = if *p == "disabled" { None } else { Some(num32(p)? as u16) };
        cx.with(|c| *field(&mut c.ports) = v);
    }
    Ok(cx.with(|c| field(&mut c.ports).map(|p| p.to_string()).unwrap_or_else(|| "disabled".into())))
}

/// The driver name, if this adapter programs flash from its own download list
/// rather than by driving the on-chip flash controller.
fn flash_download_driver(cx: &Cx) -> Option<&'static str> {
    cx.with(|c| {
        let ad = c.adapter.as_deref_mut()?;
        ad.flash_download().is_some().then(|| ad.driver_name())
    })
}

/// Refuses a DMU-level flash command on a backend that programs by download.
///
/// These have no equivalent there, so they fail loudly instead of appearing to
/// succeed. Note this guards the *command*, never `flash::write_image_with`,
/// which calls `probe_bank` internally -- refusing deeper would kill
/// `flash write_image` before its own reroute could run.
fn require_controller_flash(cx: &Cx, what: &str) -> Result<(), String> {
    match flash_download_driver(cx) {
        Some(d) => Err(format!(
            "'flash {what}' is not supported by the '{d}' adapter: it programs flash from its own download list"
        )),
        None => Ok(()),
    }
}

/// Hands the files to a probe that programs from its own download list.
///
/// `Ok(false)` means this adapter drives the flash controller itself, and the
/// caller goes on to the DMU path.
fn flash_by_download(cx: &Cx, files: &[ImageFile]) -> Result<bool, String> {
    cx.with(|c| {
        let Some(ad) = c.adapter.as_deref_mut() else { return Ok(false) };
        let Some(dl) = ad.flash_download() else { return Ok(false) };
        dl.download(files).map(|()| true).map_err(e2s)
    })
}

fn require_init(cx: &Cx, name: &str) -> Result<(), String> {
    if cx.with(|c| c.initialized) {
        Ok(())
    } else {
        Err(format!("The '{name}' command must be used after 'init'."))
    }
}

fn current(cx: &Cx) -> Result<usize, String> {
    cx.with(|c| c.current_target()).map_err(e2s)
}

// --- configuration -------------------------------------------------------------------

fn cmd_adapter(cx: &Cx, a: &[&str]) -> CmdResult {
    match a.get(1).copied() {
        Some("driver") => {
            let name = a.get(2).ok_or("usage: adapter driver <name>")?.to_string();
            if !adapter::DRIVERS.contains(&name.as_str()) {
                return Err(format!(
                    "The specified debug interface was not found ({name})\nThe following debug adapters are available: {}",
                    adapter::DRIVERS.join(", ")
                ));
            }
            cx.with(|c| {
                c.adapter_name = Some(name.clone());
                let effective = c.adapter_override.clone().unwrap_or(name);
                c.adapter = adapter::create(&effective);
            });
            ok()
        }
        // The driver actually in use, which `--adapter` may have overridden.
        // Cfg scripts branch on this to skip what a backend cannot do.
        Some("name") => Ok(cx.with(|c| {
            c.adapter
                .as_deref()
                .map(|ad| ad.driver_name().to_string())
                .or_else(|| c.adapter_name.clone())
                .unwrap_or_default()
        })),
        Some("list") => Ok(adapter::DRIVERS.join("\n")),
        Some("speed") => {
            if let Some(v) = a.get(2) {
                let khz = num32(v)?;
                let init = cx.with(|c| {
                    c.adapter_settings.speed_khz = Some(khz);
                    c.initialized
                });
                if init {
                    cx.with(|c| c.adapter.as_deref_mut().map(|ad| ad.set_speed_khz(khz))).transpose().map_err(e2s)?;
                }
            }
            Ok(format!("adapter speed: {} kHz", cx.with(|c| c.adapter_settings.speed_khz.unwrap_or(0))))
        }
        Some("serial") => {
            let s = a.get(2).ok_or("usage: adapter serial <serial>")?.to_string();
            cx.with(|c| c.adapter_settings.serial = Some(s));
            ok()
        }
        Some("srst") | Some("assert") | Some("deassert") | Some("usb") | Some("gpio") => {
            log_warn!("'adapter {}' is not supported by aurix-dbg and was ignored", a[1..].join(" "));
            ok()
        }
        _ => Err("usage: adapter driver|speed|serial|name|list ...".into()),
    }
}

/// Driver-specific command groups (e.g. `tas-client host`).
///
/// A group the active adapter does not claim is an **error**, not a silent
/// no-op. A cfg meant for another probe has to say so -- guard it with
/// `if {[adapter name] eq "tas_client"} { ... }` -- rather than have its
/// settings quietly ignored and the session then behave in a way the script
/// never asked for.
fn cmd_driver_group(cx: &Cx, a: &[&str]) -> CmdResult {
    cx.with(|c| match c.adapter.as_deref_mut() {
        Some(ad) if ad.command_groups().contains(&a[0]) => ad.command(a).map_err(e2s),
        Some(ad) => {
            let d = ad.driver_name();
            Err(format!("'{}' is not supported by the '{d}' adapter", a[0]))
        }
        None => Err(format!("'{}' requires 'adapter driver' first", a[0])),
    })
}

fn cmd_transport(cx: &Cx, a: &[&str]) -> CmdResult {
    match a.get(1).copied() {
        Some("select") => {
            if let Some(t) = a.get(2) {
                if *t != "ifxdap" && *t != "dap" && *t != "jtag" {
                    return Err(format!("Transport '{t}' not supported by the tas_client adapter (use ifxdap)"));
                }
                cx.with(|c| c.transport = Some(t.to_string()));
                ok()
            } else {
                Ok(cx.with(|c| c.transport.clone().unwrap_or_else(|| "ifxdap".into())))
            }
        }
        Some("list") => Ok("ifxdap".into()),
        Some("init") => ok(),
        _ => Err("usage: transport select [name]|list".into()),
    }
}

fn cmd_reset_config(cx: &Cx, a: &[&str]) -> CmdResult {
    if a.len() > 1 {
        cx.with(|c| {
            c.reset_config = a[1..].iter().map(|s| s.to_string()).collect();
            if a.contains(&"connect_assert_srst") {
                c.adapter_settings.connect_under_srst = true;
            }
            if a.contains(&"connect_deassert_srst") {
                c.adapter_settings.connect_under_srst = false;
            }
        });
    }
    Ok(cx.with(|c| c.reset_config.join(" ")))
}

fn newtap(cx: &Cx, a: &[&str]) -> CmdResult {
    // <chip> <tap> -irlen N -expected-id ID ...
    let chip = a.get(2).ok_or("usage: newtap <chip> <tap> -irlen <n> [-expected-id <id>]")?;
    let tapname = a.get(3).ok_or("usage: newtap <chip> <tap> -irlen <n> [-expected-id <id>]")?;
    let mut tap = Tap { name: format!("{chip}.{tapname}"), irlen: 0, expected_ids: Vec::new() };
    let mut i = 4;
    while i < a.len() {
        match a[i] {
            "-irlen" => {
                tap.irlen = num32(a.get(i + 1).ok_or("-irlen needs a value")?)?;
                i += 2;
            }
            "-expected-id" => {
                tap.expected_ids.push(num32(a.get(i + 1).ok_or("-expected-id needs a value")?)?);
                i += 2;
            }
            "-ircapture" | "-irmask" => i += 2,
            "-enable" | "-disable" | "-ignore-version" | "-ignore-bypass" => i += 1,
            o => return Err(format!("newtap: unknown option {o}")),
        }
    }
    cx.with(|c| {
        c.taps.retain(|t| t.name != tap.name);
        c.taps.push(tap)
    });
    ok()
}

fn cmd_ifxdap(cx: &Cx, a: &[&str]) -> CmdResult {
    match a.get(1).copied() {
        Some("newtap") => newtap(cx, a),
        Some("names") => Ok(list(cx.with(|c| c.taps.iter().map(|t| t.name.clone()).collect::<Vec<_>>()))),
        Some("init") => ok(),
        _ => Err("usage: ifxdap newtap|names".into()),
    }
}

fn cmd_jtag(cx: &Cx, a: &[&str]) -> CmdResult {
    match a.get(1).copied() {
        Some("newtap") => newtap(cx, a),
        Some("names") => Ok(list(cx.with(|c| c.taps.iter().map(|t| t.name.clone()).collect::<Vec<_>>()))),
        Some("tapisenabled") => Ok("1".into()),
        Some("init") | Some("arp_init") | Some("arp_init-reset") => ok(),
        _ => Err(format!("jtag {} is not supported with the tas_client adapter", a.get(1).unwrap_or(&""))),
    }
}

fn cmd_ocmts(cx: &Cx, a: &[&str]) -> CmdResult {
    match a.get(1).copied() {
        Some("create") => {
            let name = a.get(2).ok_or("usage: ocmts create <name> -chain-position <tap>")?.to_string();
            let mut tap = None;
            let mut i = 3;
            while i < a.len() {
                match a[i] {
                    "-chain-position" => {
                        tap = a.get(i + 1).map(|s| s.to_string());
                        i += 2;
                    }
                    o => return Err(format!("ocmts create: unknown option {o}")),
                }
            }
            let tap = tap.ok_or("-chain-position is required")?;
            cx.with(|c| {
                if !c.taps.iter().any(|t| t.name == tap) {
                    return Err("-chain-position is invalid".to_string());
                }
                c.ocmts.retain(|o| o.name != name);
                c.ocmts.push(Ocmts { name, tap });
                Ok(String::new())
            })
        }
        Some("init") => ok(),
        Some("names") => Ok(list(cx.with(|c| c.ocmts.iter().map(|o| o.name.clone()).collect::<Vec<_>>()))),
        _ => Err("usage: ocmts create|init|names".into()),
    }
}

fn cmd_target(cx: &Cx, a: &[&str]) -> CmdResult {
    match a.get(1).copied() {
        Some("create") => {
            let name = a.get(2).ok_or("usage: target create <name> <type> [options]")?.to_string();
            let ty = a.get(3).ok_or("usage: target create <name> <type> [options]")?.to_string();
            if !Core::TYPES.contains(&ty.as_str()) {
                return Err(format!("Unknown target type {ty}, try one of {}", Core::TYPES.join(", ")));
            }
            if cx.with(|c| c.target_by_name(&name).is_some()) || cx.interp.command_exists(&name) {
                return Err(format!("Command/target: {name} Exists"));
            }
            let t = cx.with(|c| {
                c.targets.push(TargetEntry::new(&name, &ty));
                c.targets.len() - 1
            });
            if let Err(e) = configure_target(cx, t, &a[4..]) {
                cx.with(|c| c.targets.pop());
                return Err(e);
            }
            if cx.with(|c| c.targets[t].core.ocmts().is_none() && c.targets[t].core.as_tricore().is_some()) {
                cx.with(|c| c.targets.pop());
                return Err(format!("[{name}] OCMTS not configured for target (-ocmts)"));
            }
            // Channels of one MCS module share the program RAM with its breakpoints and size; a
            // -ramsize given here applies to the whole module.
            cx.with(|c| {
                let Some(m) = c.targets[t].core.as_mcs() else { return };
                let (module, rambase, size) = (m.module, m.rambase, m.ram_size());
                let shared = c.targets[..t]
                    .iter()
                    .filter_map(|e| e.core.as_mcs())
                    .find(|o| o.module == module && o.rambase == rambase)
                    .map(|o| o.ram.clone());
                if let (Some(ram), Some(m)) = (shared, c.targets[t].core.as_mcs_mut()) {
                    m.ram = ram;
                    if size != DEFAULT_RAM_SIZE {
                        let _ = m.set_ram_size(size);
                    }
                }
            });
            cx.with(|c| c.current = Some(t));
            let sh = cx.sh.clone();
            cx.interp.register(&name, move |i, args| target_object(&Cx { interp: i, sh: &sh }, t, args));
            ok()
        }
        Some("names") => Ok(list(cx.with(|c| c.targets.iter().map(|t| t.name.clone()).collect::<Vec<_>>()))),
        Some("current") => {
            let t = current(cx)?;
            Ok(ops::target_name(cx, t))
        }
        Some("types") => Ok(Core::TYPES.join(" ")),
        Some("count") => Ok(cx.with(|c| c.targets.len()).to_string()),
        Some("number") => {
            let n = num(a.get(2).ok_or("usage: target number <n>")?)? as usize;
            cx.with(|c| c.targets.get(n).map(|t| t.name.clone())).ok_or_else(|| format!("Target: number {n} does not exist"))
        }
        Some("init") => ok(),
        Some("smp") => {
            log_warn!("'target smp' is ignored: multi-core synchronisation is done by the cfg scripts");
            ok()
        }
        _ => Err("usage: target create|names|current|types|count|number ...".into()),
    }
}

fn configure_target(cx: &Cx, t: usize, a: &[&str]) -> Result<(), String> {
    let mut i = 0;
    let arg = |i: usize, opt: &str| a.get(i).copied().ok_or_else(|| format!("missing argument to {opt}"));
    while i < a.len() {
        let opt = a[i];
        match opt {
            "-event" => {
                let ev = arg(i + 1, opt)?.to_string();
                let body = arg(i + 2, opt)?.to_string();
                cx.with(|c| c.targets[t].set_event(&ev, &body));
                i += 3;
            }
            "-coreid" => {
                let v = num32(arg(i + 1, opt)?)?;
                cx.with(|c| c.targets[t].core.set_coreid(v));
                i += 2;
            }
            "-dbgbase" => {
                let v = num32(arg(i + 1, opt)?)?;
                cx.with(|c| c.targets[t].core.set_dbgbase(v));
                i += 2;
            }
            "-ocmts" => {
                let o = arg(i + 1, opt)?.to_string();
                cx.with(|c| {
                    let tap = c.ocmts.iter().find(|x| x.name == o).map(|x| x.tap.clone()).ok_or("OCMTS name invalid!")?;
                    let e = &mut c.targets[t];
                    if e.core.ocmts().is_some_and(|cur| cur != o) {
                        return Err("OCMTS assignment cannot be changed!");
                    }
                    e.core.set_ocmts(o);
                    e.chain_position = Some(tap);
                    Ok(())
                })?;
                i += 2;
            }
            "-chain-position" => {
                let v = arg(i + 1, opt)?.to_string();
                cx.with(|c| c.targets[t].chain_position = Some(v));
                i += 2;
            }
            "-module" | "-channel" | "-rambase" | "-ramsize" => {
                let v = num32(arg(i + 1, opt)?)?;
                cx.with(|c| {
                    let m = c.targets[t].core.as_mcs_mut().ok_or_else(|| format!("{opt} is only valid for gtm_mcs targets"))?;
                    match opt {
                        "-module" => m.module = v,
                        "-channel" => m.channel = v,
                        "-ramsize" => m.set_ram_size(v).map_err(e2s)?,
                        _ => m.rambase = Some(v),
                    }
                    m.check_config().map_err(e2s)
                })?;
                i += 2;
            }
            "-defer-examine" => {
                cx.with(|c| c.targets[t].defer_examine = true);
                i += 1;
            }
            "-work-area-phys" | "-work-area-virt" | "-work-area-size" | "-work-area-backup" | "-endian" | "-rtos"
            | "-gdb-port" | "-gdb-max-connections" => {
                let v = arg(i + 1, opt)?.to_string();
                cx.with(|c| c.targets[t].options.insert(opt.to_string(), v));
                i += 2;
            }
            "-type" => return Err("-type is read-only".into()),
            _ => return Err(format!("Unknown option: {opt}")),
        }
    }
    Ok(())
}

fn cget_target(cx: &Cx, t: usize, a: &[&str]) -> CmdResult {
    let opt = a.first().ok_or("usage: cget <option>")?;
    cx.with(|c| {
        let e = &c.targets[t];
        Ok(match *opt {
            "-type" => e.type_name.clone(),
            "-event" => e.event(a.get(1).ok_or("usage: cget -event <name>")?).unwrap_or("").to_string(),
            "-coreid" => e.core.coreid().to_string(),
            "-dbgbase" => format!("0x{:08x}", e.core.dbgbase().unwrap_or(0)),
            "-module" => e.core.as_mcs().map(|m| m.module.to_string()).ok_or("not a gtm_mcs target")?,
            "-channel" => e.core.as_mcs().map(|m| m.channel.to_string()).ok_or("not a gtm_mcs target")?,
            "-ramsize" => e.core.as_mcs().map(|m| format!("0x{:x}", m.ram_size())).ok_or("not a gtm_mcs target")?,
            "-ocmts" => e.core.ocmts().map(str::to_string).ok_or("OCMTS not configured")?,
            "-chain-position" => e.chain_position.clone().unwrap_or_default(),
            "-endian" => "little".into(),
            "-defer-examine" => (e.defer_examine as u8).to_string(),
            o => e.options.get(o).cloned().ok_or_else(|| format!("Unknown option: {o}"))?,
        })
    })
}

/// `<target> <subcommand> ...`
fn target_object(cx: &Cx, t: usize, a: &[&str]) -> CmdResult {
    let sub = a.get(1).copied().ok_or_else(|| format!("{}: missing subcommand", a[0]))?;
    let rest = &a[2..];
    match sub {
        "configure" => configure_target(cx, t, rest).map(|_| String::new()),
        "cget" => cget_target(cx, t, rest),
        "curstate" => Ok(cx.with(|c| c.targets[t].core.state().name().to_string())),
        "debug_reason" => Ok(cx.with(|c| c.targets[t].core.reason().name().to_string())),
        "was_examined" => Ok((cx.with(|c| c.targets[t].core.examined()) as u8).to_string()),
        "examine_deferred" => Ok((cx.with(|c| c.targets[t].defer_examine) as u8).to_string()),
        "invoke-event" => {
            ops::fire(cx, t, rest.first().ok_or("usage: invoke-event <event>")?);
            ok()
        }
        "eventlist" => {
            let mut out = format!("Event actions for target {}\n", ops::target_name(cx, t));
            for (ev, body) in cx.with(|c| c.targets[t].events.clone()) {
                out.push_str(&format!("{ev:<24} | {body}\n"));
            }
            Ok(out)
        }
        "arp_examine" => ops::examine_one(cx, t, false).map(|_| String::new()).map_err(e2s),
        "arp_poll" => {
            ops::poll_all(cx);
            ok()
        }
        "arp_halt" => ops::halt(cx, t).map(|_| String::new()).map_err(e2s),
        "arp_resume" => ops::resume(cx, t, None).map(|_| String::new()).map_err(e2s),
        "arp_step" => ops::step(cx, t, None).map(|_| String::new()).map_err(e2s),
        "arp_reset" => {
            let halt = rest.get(1).map(|h| parse_bool(h)).transpose()?.unwrap_or(false);
            match rest.first().copied() {
                Some("assert") => ops::arp_reset_assert(cx, t, halt).map(|_| String::new()).map_err(e2s),
                Some("deassert") => ops::arp_reset_deassert(cx, t, halt).map(|_| String::new()).map_err(e2s),
                _ => Err("usage: arp_reset assert|deassert <halt>".into()),
            }
        }
        "arp_waitstate" => {
            let state = rest.first().ok_or("usage: arp_waitstate <state> <ms>")?;
            let ms = num(rest.get(1).ok_or("usage: arp_waitstate <state> <ms>")?)?;
            let want = match *state {
                "halted" => TargetState::Halted,
                "running" => TargetState::Running,
                "reset" => TargetState::Reset,
                "unknown" => TargetState::Unknown,
                s => return Err(format!("invalid state {s}")),
            };
            ops::wait_state(cx, t, want, ms).map(|_| String::new()).map_err(e2s)
        }
        _ => {
            // Target-scoped variants of global commands (mww, read_memory, tricore, halt, ...).
            cx.with(|c| c.override_stack.push(t));
            let r = cx.interp.eval(&list(&a[1..]));
            cx.with(|c| {
                c.override_stack.pop();
            });
            r
        }
    }
}

fn cmd_targets(cx: &Cx, a: &[&str]) -> CmdResult {
    if let Some(name) = a.get(1) {
        return cx.with(|c| match c.target_by_name(name) {
            Some(t) => {
                c.current = Some(t);
                Ok(String::new())
            }
            None => Err(format!(
                "Target: {name} is unknown, try one of {}",
                c.targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")
            )),
        });
    }
    Ok(cx.with(|c| {
        let cur = c.current_target().ok();
        let mut out = String::from("    TargetName         Type       Endian TapName            State       \n");
        out.push_str("--  ------------------ ---------- ------ ------------------ ------------\n");
        for (i, t) in c.targets.iter().enumerate() {
            out.push_str(&format!(
                "{i:2}{} {:<18} {:<10} {:<6} {:<18} {}\n",
                if Some(i) == cur { '*' } else { ' ' },
                t.name,
                t.type_name,
                "little",
                t.chain_position.as_deref().unwrap_or(""),
                if t.core.examined() { t.core.state().name() } else { "unexamined" }
            ));
        }
        out.trim_end().to_string()
    }))
}

fn cmd_flash(cx: &Cx, a: &[&str]) -> CmdResult {
    match a.get(1).copied() {
        Some("bank") => {
            // flash bank <name> <driver> <base> <size> <chip_width> <bus_width> <target> [options]
            if a.len() < 9 {
                return Err("usage: flash bank <name> <driver> <base> <size> <chip_width> <bus_width> <target> [driver options ...]".into());
            }
            let driver = a[3].to_string();
            if !flash::DRIVERS.contains(&driver.as_str()) {
                return Err(format!("flash driver '{driver}' not found"));
            }
            let target = a[8].to_string();
            if cx.with(|c| c.target_by_name(&target).is_none()) {
                return Err(format!("target '{target}' not defined"));
            }
            let bank = FlashBank {
                name: a[2].to_string(),
                driver,
                base: num32(a[4])?,
                size: num32(a[5])?,
                chip_width: num32(a[6])?,
                bus_width: num32(a[7])?,
                target,
                extra: a[9..].iter().map(|s| s.to_string()).collect(),
            };
            cx.with(|c| c.flash_banks.push(bank));
            ok()
        }
        Some("banks") | Some("list") => Ok(cx.with(|c| {
            c.flash_banks
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    format!(
                        "#{i} : {} ({}) at 0x{:08x}, size 0x{:08x}, buswidth {}, chipwidth {}",
                        b.name, b.driver, b.base, b.size, b.bus_width, b.chip_width
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })),
        Some("init") => ok(),
        Some("probe") | Some("info") => {
            require_controller_flash(cx, "probe")?;
            require_init(cx, "flash")?;
            let bank = flash::find_bank(cx, a.get(2).ok_or("usage: flash info|probe <bank>")?).map_err(e2s)?;
            let p = flash::probe_bank(cx, &bank).map_err(e2s)?;
            Ok(format!(
                "#{} : {} ({}) at 0x{:08x}, size 0x{:08x}: {}, {} sectors of {} KB, physical sectors of {} KB",
                a[2],
                bank.name,
                bank.driver,
                p.base,
                p.size,
                if p.dflash { "DFLASH" } else { "PFLASH" },
                p.size / p.sector_size,
                p.sector_size / 1024,
                p.phys_sector / 1024
            ))
        }
        Some("erase_check") => {
            require_controller_flash(cx, "erase_check")?;
            require_init(cx, "flash")?;
            let bank = flash::find_bank(cx, a.get(2).ok_or("usage: flash erase_check <bank>")?).map_err(e2s)?;
            let sectors = flash::erase_check(cx, &bank).map_err(e2s)?;
            let erased = sectors.iter().filter(|(_, e)| *e).count();
            let mut out = vec![format!("{}: {erased} of {} sectors erased", bank.name, sectors.len())];
            out.extend(sectors.iter().enumerate().filter(|(_, (_, e))| !*e).map(|(i, (addr, _))| format!("\t#{i:3}: 0x{addr:08x} not erased")));
            Ok(out.join("\n"))
        }
        Some("erase_sector") => {
            require_controller_flash(cx, "erase_sector")?;
            require_init(cx, "flash")?;
            if a.len() != 5 {
                return Err("usage: flash erase_sector <bank> <first> <last|'last'>".into());
            }
            let bank = flash::find_bank(cx, a[2]).map_err(e2s)?;
            let first = num32(a[3])?;
            let last = if a[4] == "last" {
                let p = flash::probe_bank(cx, &bank).map_err(e2s)?;
                p.size / p.sector_size - 1
            } else {
                num32(a[4])?
            };
            flash::erase_sectors(cx, &bank, first, last).map_err(e2s)?;
            Ok(format!("erased sectors {first} through {last} on flash bank {}", bank.name))
        }
        Some("write_image") => {
            require_init(cx, "flash")?;
            let mut args: Vec<&str> = a[2..].to_vec();
            let (mut erase, mut incremental, mut unlock) = (false, false, false);
            while let Some(flag) = args.first().copied() {
                match flag {
                    "erase" => erase = true,
                    "incremental" => incremental = true,
                    "unlock" => unlock = true,
                    _ => break,
                }
                args.remove(0);
            }
            // A probe that programs from its own download list takes the file as
            // it is; the DMU path below only applies when we drive the controller.
            let file = image_file(&args, "flash write_image [erase] [incremental] [unlock] <file> [offset [type]]")?;
            if flash_by_download(cx, std::slice::from_ref(&file))? {
                return Ok(format!("downloaded {}", file.path));
            }
            let img = file.load()?;
            let stats = flash::write_image_with(cx, &img, erase, incremental, unlock).map_err(e2s)?;
            Ok(format!("wrote {} bytes from file {}: {stats}", img.size() - stats.ucb_skipped, file.path))
        }
        Some("write_images") => {
            // Merges the files first (later files win), so a file cannot erase what an earlier
            // one programmed into a shared sector, and incremental writes see the final content.
            const USAGE: &str =
                "usage: flash write_images [erase] [incremental] [unlock] [verify] [-offset <offset>] [-type <type>] <file> ...";
            require_init(cx, "flash")?;
            let mut args: Vec<&str> = a[2..].to_vec();
            let (mut erase, mut incremental, mut unlock, mut verify) = (false, false, false, false);
            while let Some(flag) = args.first().copied() {
                match flag {
                    "erase" => erase = true,
                    "incremental" => incremental = true,
                    "unlock" => unlock = true,
                    "verify" => verify = true,
                    _ => break,
                }
                args.remove(0);
            }
            // -offset and -type apply to the file that follows them.
            let mut files = Vec::new();
            let (mut offset, mut kind) = (None, None);
            let mut it = args.into_iter();
            while let Some(arg) = it.next() {
                match arg {
                    "-offset" => offset = Some(num32(it.next().ok_or(USAGE)?)?),
                    "-type" => kind = Some(it.next().ok_or(USAGE)?.to_string()),
                    path => files.push(ImageFile { path: path.to_string(), offset: offset.take().unwrap_or(0), kind: kind.take() }),
                }
            }
            if files.is_empty() || offset.is_some() || kind.is_some() {
                return Err(USAGE.into());
            }
            let names = || files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>().join(", ");
            // As in write_image: hand the whole list over, already merged by
            // winIDEA's own loader rather than by us.
            if flash_by_download(cx, &files)? {
                return Ok(format!("downloaded {}", names()));
            }
            let mut img = Image::default();
            for file in &files {
                let (merged, changed) = img.overlay(&file.load()?);
                if changed > 0 {
                    crate::log_warn!("flash write_images: {} replaces {changed} bytes of earlier files", file.path);
                }
                img = merged;
            }
            let stats = flash::write_image_with(cx, &img, erase, incremental, unlock).map_err(e2s)?;
            let mut out = format!("wrote {} bytes from {} files: {stats}", img.size() - stats.ucb_skipped, files.len());
            if verify {
                // An incremental write has already compared all unchanged sectors with the image
                // (including the padding of their pages): only the programmed ones are read back.
                let (check, what) = if incremental {
                    let parts = stats.programmed.iter().flat_map(|&s| img.within(s, s as u64 + flash::SECTOR_SIZE as u64).sections);
                    (Image::from_sections(parts.collect()), format!(" of the {} programmed sectors", stats.programmed.len()))
                } else {
                    (img.clone(), String::new())
                };
                let n = flash::verify_image(cx, &check).map_err(e2s)?;
                out += &format!("\nverified {n} bytes{what}");
            }
            Ok(out)
        }
        Some("verify_image") => cmd_verify_image(cx, &a[1..]),
        Some(other) => Err(format!("flash {other}: not supported by aurix-dbg")),
        None => Err("usage: flash bank|banks|list|info|probe|erase_check|erase_sector|write_image|write_images|verify_image ...".into()),
    }
}

/// `<file> [offset [type]]`
fn image_file(args: &[&str], usage_text: &str) -> Result<ImageFile, String> {
    let path = args.first().ok_or_else(|| format!("usage: {usage_text}"))?;
    let offset = args.get(1).map(|o| num32(o)).transpose()?.unwrap_or(0);
    Ok(ImageFile { path: path.to_string(), offset, kind: args.get(2).map(|t| t.to_string()) })
}

fn load_image(args: &[&str], usage_text: &str) -> Result<Image, String> {
    image_file(args, usage_text)?.load()
}

fn cmd_verify_image(cx: &Cx, a: &[&str]) -> CmdResult {
    require_init(cx, a[0])?;
    let img = load_image(&a[1..], "verify_image <file> [offset [type]]")?;
    let n = flash::verify_image(cx, &img).map_err(e2s)?;
    Ok(format!("verified {n} bytes in {} sections", img.sections.len()))
}

/// `program <file> [preverify] [verify] [reset] [exit] [offset]` like OpenOCD's.
fn cmd_program(cx: &Cx, a: &[&str]) -> CmdResult {
    let mut file = None;
    let mut offset = None;
    let (mut preverify, mut verify, mut reset, mut exit) = (false, false, false, false);
    for arg in &a[1..] {
        match *arg {
            "preverify" => preverify = true,
            "verify" => verify = true,
            "reset" => reset = true,
            "exit" => exit = true,
            other if file.is_none() => file = Some(other),
            other => offset = Some(num32(other)?),
        }
    }
    let file = file.ok_or("usage: program <file> [preverify] [verify] [reset] [exit] [offset]")?;
    let mut out = Vec::new();
    let result = (|| -> Result<(), String> {
        if !cx.with(|c| c.initialized) {
            ops::init(cx).map_err(|e| format!("** OpenOCD init failed **: {e}"))?;
        }
        let file = ImageFile { offset: offset.unwrap_or(0), ..ImageFile::new(file) };
        let img = file.load()?;
        ops::reset(cx, "init").map_err(|e| format!("** Unable to reset target **: {e}"))?;
        let mut needs_flash = true;
        if preverify {
            out.push("**pre-verifying**".to_string());
            if flash::verify_image(cx, &img).is_ok() {
                out.push("**Verified OK - No flashing**".into());
                needs_flash = false;
            }
        }
        if needs_flash {
            out.push("** Programming Started **".into());
            // A probe that programs from its own download list gets the file;
            // otherwise we drive the flash controller ourselves.
            if flash_by_download(cx, std::slice::from_ref(&file))? {
                out.push("** Programming Finished **: downloaded".into());
            } else {
                let stats = flash::write_image(cx, &img, true)
                    .map_err(|e| format!("** Programming Failed **: {e}"))?;
                out.push(format!("** Programming Finished **: {stats}"));
            }
            // Verification is ours on either backend: it reads the image back
            // through ordinary memory, so both report the same thing.
            if verify {
                out.push("** Verify Started **".into());
                let n = flash::verify_image(cx, &img).map_err(|e| format!("**Verify Failed**: {e}"))?;
                out.push(format!("** Verified OK ** ({n} bytes)"));
            }
        }
        if reset {
            out.push("** Resetting Target **".into());
            ops::reset(cx, "run").map_err(|e| format!("** Unable to reset target **: {e}"))?;
        }
        Ok(())
    })();
    for line in &out {
        crate::log_info!("{line}");
    }
    if exit {
        cx.with(|c| c.shutdown = true);
        emit(Event::Shutdown);
    }
    match result {
        Ok(()) => Ok(out.join("\n")),
        Err(e) => Err(if out.is_empty() { e } else { format!("{}\n{e}", out.join("\n")) }),
    }
}

/// `aurix_eflash fallback <bank> [on|off]`: aurix-dbg always uses the command
/// sequences (OpenOCD's fallback mode), so this only validates its arguments.
fn cmd_aurix_eflash(cx: &Cx, a: &[&str]) -> CmdResult {
    match (a.get(1).copied(), a.get(2)) {
        (Some("fallback"), Some(bank)) => {
            flash::find_bank(cx, bank).map_err(e2s)?;
            match a.get(3).copied() {
                None | Some("on") | Some("off") => ok(),
                Some(other) => Err(format!("Invalid fallback mode: {other}. Should be 'on' or 'off'.")),
            }
        }
        _ => Err("usage: aurix_eflash fallback <bank> [on|off]".into()),
    }
}

fn cmd_init(cx: &Cx, _a: &[&str]) -> CmdResult {
    ops::init(cx).map(|_| String::new()).map_err(e2s)
}

pub fn find_file(cx: &Cx, name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }
    let dirs = cx.with(|c| c.search_dirs.clone());
    for d in dirs.iter().rev().chain(std::iter::once(&PathBuf::from("."))) {
        let cand = d.join(name);
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}

/// Path for Tcl/Jim: forward slashes, no Windows verbatim (`\\?\`) prefix.
fn tcl_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.replace('\\', "/")
}

fn cmd_find(cx: &Cx, a: &[&str]) -> CmdResult {
    let name = a.get(1).ok_or("usage: find <file>")?;
    find_file(cx, name).map(|p| tcl_path(&p)).ok_or_else(|| format!("Can't find {name}"))
}

fn cmd_script(cx: &Cx, a: &[&str]) -> CmdResult {
    let name = a.get(1).ok_or("usage: script <file>")?;
    let path = find_file(cx, name).ok_or_else(|| format!("Can't find {name}"))?;
    cx.interp.eval_file(&tcl_path(&path))
}

fn cmd_echo(_cx: &Cx, a: &[&str]) -> CmdResult {
    let (msg, _nonl) = match a.get(1) {
        Some(&"-n") => (a[2..].join(" "), true),
        _ => (a[1..].join(" "), false),
    };
    log_user!("{msg}");
    ok()
}

fn cmd_sleep(_cx: &Cx, a: &[&str]) -> CmdResult {
    let ms = num(a.get(1).ok_or("usage: sleep <ms> [busy]")?)?;
    std::thread::sleep(Duration::from_millis(ms));
    ok()
}

fn cmd_poll(cx: &Cx, a: &[&str]) -> CmdResult {
    match a.get(1).copied() {
        Some(v) => {
            let on = parse_bool(v)?;
            cx.with(|c| c.poll_enabled = on);
            ok()
        }
        None => {
            require_init(cx, "poll")?;
            ops::poll_all(cx);
            let t = current(cx)?;
            let (state, reason) = ops::state(cx, t);
            let mut out = format!("background polling: {}\n", if cx.with(|c| c.poll_enabled) { "on" } else { "off" });
            out.push_str(&format!("[{}] state: {}", ops::target_name(cx, t), state.name()));
            if state == TargetState::Halted {
                let pc_idx = cx.with(|c| c.targets[t].core.arch().pc);
                let pc = ops::read_reg(cx, t, pc_idx).map_err(e2s)?;
                out.push_str(&format!(", halted due to {}, pc: 0x{pc:08x}", reason.name()));
            }
            Ok(out)
        }
    }
}

// --- memory --------------------------------------------------------------------------

fn strip_phys<'a>(a: &'a [&'a str]) -> &'a [&'a str] {
    if a.first() == Some(&"phys") {
        &a[1..]
    } else {
        a
    }
}

fn cmd_mw(cx: &Cx, a: &[&str], size: usize) -> CmdResult {
    require_init(cx, a[0])?;
    let args = strip_phys(&a[1..]);
    if args.len() < 2 || args.len() > 3 {
        return Err(format!("usage: {} ['phys'] address value [count]", a[0]));
    }
    let addr = num32(args[0])?;
    let value = num(args[1])?;
    let count = args.get(2).map(|c| num(c)).transpose()?.unwrap_or(1) as usize;
    let word = &value.to_le_bytes()[..size];
    let data: Vec<u8> = word.iter().copied().cycle().take(size * count).collect();
    ops::write_memory(cx, addr, &data).map(|_| String::new()).map_err(e2s)
}

fn cmd_md(cx: &Cx, a: &[&str], size: usize) -> CmdResult {
    require_init(cx, a[0])?;
    let args = strip_phys(&a[1..]);
    if args.is_empty() || args.len() > 2 {
        return Err(format!("usage: {} ['phys'] address [count]", a[0]));
    }
    let addr = num32(args[0])?;
    let count = args.get(1).map(|c| num(c)).transpose()?.unwrap_or(1) as usize;
    let data = ops::read_memory(cx, addr, size * count).map_err(e2s)?;
    let per_line = 32 / size;
    let mut out = Vec::new();
    for (i, chunk) in data.chunks(size * per_line).enumerate() {
        let mut line = format!("0x{:08x}: ", addr as usize + i * size * per_line);
        for v in chunk.chunks(size) {
            let mut b = [0u8; 8];
            b[..size].copy_from_slice(v);
            line.push_str(&format!("{:0w$x} ", u64::from_le_bytes(b), w = size * 2));
        }
        out.push(line);
    }
    Ok(out.join("\n"))
}

fn width_bytes(w: &str) -> Result<usize, String> {
    match w {
        "8" => Ok(1),
        "16" => Ok(2),
        "32" => Ok(4),
        "64" => Ok(8),
        _ => Err("invalid width, must be 8, 16, 32 or 64".into()),
    }
}

fn cmd_read_memory(cx: &Cx, a: &[&str]) -> CmdResult {
    require_init(cx, "read_memory")?;
    if a.len() < 4 || a.len() > 5 {
        return Err("usage: read_memory address width count ['phys']".into());
    }
    let addr = num32(a[1])?;
    let size = width_bytes(a[2])?;
    let count = num(a[3])? as usize;
    if count > 65536 {
        return Err("too large read request, exceeds 64K elements".into());
    }
    let data = ops::read_memory(cx, addr, size * count).map_err(|e| format!("failed to read memory: {e}"))?;
    Ok(data
        .chunks(size)
        .map(|v| {
            let mut b = [0u8; 8];
            b[..size].copy_from_slice(v);
            format!("0x{:x}", u64::from_le_bytes(b))
        })
        .collect::<Vec<_>>()
        .join(" "))
}

fn cmd_write_memory(cx: &Cx, a: &[&str]) -> CmdResult {
    require_init(cx, "write_memory")?;
    if a.len() < 3 {
        return Err("usage: write_memory address width data ['phys']".into());
    }
    let addr = num32(a[1])?;
    let size = width_bytes(a[2])?;
    let mut data = Vec::with_capacity(size * (a.len() - 3));
    for v in &a[3..] {
        if *v == "phys" {
            continue;
        }
        data.extend_from_slice(&num(v)?.to_le_bytes()[..size]);
    }
    ops::write_memory(cx, addr, &data).map(|_| String::new()).map_err(e2s)
}

// --- run control ---------------------------------------------------------------------

fn cmd_halt(cx: &Cx, a: &[&str]) -> CmdResult {
    require_init(cx, "halt")?;
    let t = current(cx)?;
    let ms = a.get(1).map(|m| num(m)).transpose()?.unwrap_or(5000);
    ops::halt(cx, t).map_err(e2s)?;
    ops::wait_state(cx, t, TargetState::Halted, ms).map(|_| String::new()).map_err(e2s)
}

fn cmd_wait_halt(cx: &Cx, a: &[&str]) -> CmdResult {
    require_init(cx, "wait_halt")?;
    let t = current(cx)?;
    let ms = a.get(1).map(|m| num(m)).transpose()?.unwrap_or(5000);
    ops::wait_state(cx, t, TargetState::Halted, ms).map(|_| String::new()).map_err(e2s)
}

fn cmd_resume(cx: &Cx, a: &[&str]) -> CmdResult {
    require_init(cx, "resume")?;
    let t = current(cx)?;
    let addr = a.get(1).map(|v| num32(v)).transpose()?;
    ops::resume(cx, t, addr).map(|_| String::new()).map_err(e2s)
}

fn cmd_step(cx: &Cx, a: &[&str]) -> CmdResult {
    require_init(cx, "step")?;
    let t = current(cx)?;
    let addr = a.get(1).map(|v| num32(v)).transpose()?;
    ops::step(cx, t, addr).map(|_| String::new()).map_err(e2s)
}

fn cmd_reset(cx: &Cx, a: &[&str]) -> CmdResult {
    require_init(cx, "reset")?;
    ops::reset(cx, a.get(1).copied().unwrap_or("run")).map(|_| String::new()).map_err(e2s)
}

fn cmd_reg(cx: &Cx, a: &[&str]) -> CmdResult {
    require_init(cx, "reg")?;
    let t = current(cx)?;
    let arch = cx.with(|c| c.targets[t].core.arch());
    match a.len() {
        1 => {
            let idxs: Vec<usize> = (0..arch.regs.len()).collect();
            let vals = cx
                .with(|c| {
                    let (mut io, e) = c.target_io(t)?;
                    e.core.read_regs(&mut io, &idxs)
                })
                .map_err(e2s)?;
            let mut out = format!("===== {} registers\n", arch.name);
            for (i, v) in vals.iter().enumerate() {
                out.push_str(&format!("({i}) {} (/32): 0x{v:08x}\n", arch.regs[i].name));
            }
            Ok(out.trim_end().to_string())
        }
        2 | 3 => {
            let idx = arch.reg_index(a[1]).ok_or_else(|| format!("register {} not found in current target", a[1]))?;
            if let Some(v) = a.get(2) {
                ops::write_reg(cx, t, idx, num32(v)?).map_err(e2s)?;
            }
            let v = ops::read_reg(cx, t, idx).map_err(e2s)?;
            Ok(format!("{} (/32): 0x{v:08x}", arch.regs[idx].name))
        }
        _ => Err("usage: reg [(register_number|register_name) [value]]".into()),
    }
}

fn cmd_bp(cx: &Cx, a: &[&str]) -> CmdResult {
    require_init(cx, "bp")?;
    let t = current(cx)?;
    if a.len() == 1 {
        return Ok(cx.with(|c| {
            c.targets[t]
                .breakpoints
                .iter()
                .map(|b| format!("Hardware breakpoint(IVA): addr=0x{:08x}, len=0x{:x}, num={}", b.addr, b.len, b.slot))
                .collect::<Vec<_>>()
                .join("\n")
        }));
    }
    if a.len() < 3 {
        return Err("usage: bp <address> <length> ['hw']".into());
    }
    let addr = num32(a[1])?;
    let len = num32(a[2])?;
    let kind = match a.get(3).copied() {
        None | Some("hw") => BpKind::Hard,
        Some(o) => return Err(format!("bp: unsupported option {o} (only 'hw')")),
    };
    ops::add_breakpoint(cx, t, addr, len, kind).map_err(e2s)?;
    Ok(format!("breakpoint set at 0x{addr:08x}"))
}

fn cmd_rbp(cx: &Cx, a: &[&str]) -> CmdResult {
    require_init(cx, "rbp")?;
    let t = current(cx)?;
    match a.get(1).copied() {
        Some("all") => {
            let addrs: Vec<u32> = cx.with(|c| c.targets[t].breakpoints.iter().map(|b| b.addr).collect());
            for addr in addrs {
                ops::remove_breakpoint(cx, t, addr).map_err(e2s)?;
            }
            ok()
        }
        Some(v) => ops::remove_breakpoint(cx, t, num32(v)?).map(|_| String::new()).map_err(e2s),
        None => Err("usage: rbp 'all' | address".into()),
    }
}

fn cmd_wp(cx: &Cx, a: &[&str]) -> CmdResult {
    require_init(cx, "wp")?;
    let t = current(cx)?;
    if a.len() == 1 {
        return Ok(cx.with(|c| {
            c.targets[t]
                .watchpoints
                .iter()
                .map(|w| format!("address: 0x{:08x}, len: 0x{:08x}, r/w/a: {}", w.addr, w.len, w.access.letter()))
                .collect::<Vec<_>>()
                .join("\n")
        }));
    }
    if a.len() < 3 {
        return Err("usage: wp <address> <length> [r|w|a]".into());
    }
    let access = match a.get(3).copied() {
        None | Some("a") => WatchAccess::Access,
        Some("r") => WatchAccess::Read,
        Some("w") => WatchAccess::Write,
        Some(o) => return Err(format!("wp: invalid access type {o}")),
    };
    if a.len() > 4 {
        return Err("wp: value/mask conditions are not supported".into());
    }
    ops::add_watchpoint(cx, t, num32(a[1])?, num32(a[2])?, access).map(|_| String::new()).map_err(e2s)
}

fn cmd_rwp(cx: &Cx, a: &[&str]) -> CmdResult {
    require_init(cx, "rwp")?;
    let t = current(cx)?;
    match a.get(1).copied() {
        Some("all") => {
            let addrs: Vec<u32> = cx.with(|c| c.targets[t].watchpoints.iter().map(|w| w.addr).collect());
            for addr in addrs {
                ops::remove_watchpoint(cx, t, addr).map_err(e2s)?;
            }
            ok()
        }
        Some(v) => ops::remove_watchpoint(cx, t, num32(v)?).map(|_| String::new()).map_err(e2s),
        None => Err("usage: rwp 'all' | address".into()),
    }
}

fn cmd_tricore(cx: &Cx, a: &[&str]) -> CmdResult {
    let t = current(cx)?;
    match a.get(1).copied() {
        Some("tl1_halt") => {
            if let Some(v) = a.get(2) {
                let on = parse_bool(v)?;
                cx.with(|c| c.targets[t].core.run_mut().tl1_halt = on);
            }
            Ok(format!("tl1_halt {}", if cx.with(|c| c.targets[t].core.run().tl1_halt) { "on" } else { "off" }))
        }
        Some("tl1_step") => {
            if let Some(v) = a.get(2) {
                let on = parse_bool(v)?;
                cx.with(|c| c.targets[t].core.run_mut().tl1_step = on);
            }
            Ok(format!("tl1_step {}", if cx.with(|c| c.targets[t].core.run().tl1_step) { "on" } else { "off" }))
        }
        Some("irq_mask") => {
            if a.len() > 2 {
                let m = crate::tricore::IrqMask::parse(&a[2..]).map_err(|e| format!("tricore irq_mask: {e}"))?;
                cx.with(|c| c.targets[t].core.run_mut().irq_mask = m);
            }
            Ok(format!("irq_mask {}", cx.with(|c| c.targets[t].core.run().irq_mask.describe())))
        }
        Some("triggers") => Ok(cx.with(|c| match &c.targets[t].core {
            Core::Tricore(core) => core
                .triggers
                .iter()
                .enumerate()
                .map(|(i, tr)| {
                    format!(
                        "TR{i}: {} addr=0x{:08x} {}{}{}",
                        if tr.enable { "on " } else { "off" },
                        tr.addr,
                        if tr.pc { "pc" } else { "data" },
                        if tr.bbm { " bbm" } else { "" },
                        if tr.range { " range" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Core::CortexM(core) => {
                let bps = core.fpb.iter().enumerate().map(|(i, b)| match b {
                    Some(a) => format!("FP_COMP{i}: on  addr=0x{a:08x}"),
                    None => format!("FP_COMP{i}: off"),
                });
                let wps = core.dwt.iter().enumerate().map(|(i, w)| match w {
                    Some(w) => format!("DWT_COMP{i}: on  addr=0x{:08x} len={} access={}", w.addr, w.len, w.access),
                    None => format!("DWT_COMP{i}: off"),
                });
                bps.chain(wps).collect::<Vec<_>>().join("\n")
            }
            Core::Mcs(core) => core.describe_breakpoints().join("\n"),
        })),
        // Memory as the core sees it (HSM RAM, boot ROM and peripherals through the debug window).
        Some("mdw") => {
            let addr = num32(a.get(2).ok_or_else(|| format!("usage: {} mdw <address> [count]", a[0]))?)?;
            let count = a.get(3).map(|c| num32(c)).transpose()?.unwrap_or(1) as usize;
            let data = ops::read_memory_as(cx, t, addr, count * 4).map_err(e2s)?;
            let mut out = String::new();
            for (i, w) in data.chunks(4).enumerate() {
                if i % 8 == 0 {
                    if i > 0 {
                        out.push('\n');
                    }
                    out.push_str(&format!("0x{:08x}:", addr + 4 * i as u32));
                }
                out.push_str(&format!(" {:08x}", u32::from_le_bytes([w[0], w[1], w[2], w[3]])));
            }
            Ok(out)
        }
        _ => Err(format!(
            "usage: {0} tl1_halt [on|off] | {0} tl1_step [on|off] | {0} irq_mask [off|on|step step_over step_out] | {0} triggers | {0} mdw <address> [count]",
            a[0]
        )),
    }
}

#[allow(dead_code)]
fn log_mode(cx: &Cx) {
    let m = cx.with(|c| c.mode);
    log_info!("mode: {}", if m == Mode::Config { "config" } else { "exec" });
}
