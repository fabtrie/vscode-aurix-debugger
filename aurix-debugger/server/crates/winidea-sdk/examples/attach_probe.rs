//! Probe the ATTACH path the app uses when no workspace is configured.
//!
//! `smoke.rs` proves the DLL loads and the SWIG callbacks are sound; it never
//! asks whether there is a winIDEA to talk to. This answers exactly that, in
//! the same order the `winidea` backend does when no workspace is configured
//! (`ConnectTarget::Attach`):
//!
//!   1. `enumerate_instances()` — the backend's "is anything running?" check.
//!      An empty list is precisely what surfaces as `NoRunningInstance`, the
//!      most common reason an attach finds nothing.
//!   2. `connect_tcp(port)` — attach to a found instance. Never starts
//!      winIDEA, matching the backend's attach mode.
//!   3. `DaqController::info()` — whether anything can actually be SAMPLED.
//!      `max_items == 0` is "browse works, acquisition cannot" (`can_acquire`).
//!
//! ```text
//! cargo build -p winidea-sdk --example attach_probe --profile dev-release
//! # -> backend/target/dev-release/examples/attach_probe.exe
//! ```

use winidea_sdk::{ConnectionMgr, WinIdea, daq::DaqController};

fn main() {
    println!("winIDEA attach probe");
    println!("====================\n");

    let winidea = match WinIdea::load(&winidea_sdk::default_search_paths()) {
        Ok(w) => {
            println!("[1] isystemConnect.dll loaded (auto-discovered)");
            w
        }
        Err(e) => {
            println!("[1] FAIL: could not load isystemConnect.dll: {e}");
            println!("\n=> The SDK itself is unavailable; nothing else can work.");
            std::process::exit(1);
        }
    };

    let mut mgr = match ConnectionMgr::new(&winidea) {
        Ok(m) => m,
        Err(e) => {
            println!("[2] FAIL: ConnectionMgr::new: {e}");
            std::process::exit(1);
        }
    };

    // ── The decisive step ────────────────────────────────────────────────
    let instances = match mgr.enumerate_instances() {
        Ok(v) => v,
        Err(e) => {
            println!("[2] FAIL: enumerate_instances: {e}");
            std::process::exit(1);
        }
    };

    println!("[2] enumerate_instances -> {} instance(s)", instances.len());
    for (i, inst) in instances.iter().enumerate() {
        println!(
            "      #{i}: {} | id={:?} workspace={:?} tcp_port={}",
            inst.label(),
            inst.id,
            inst.workspace,
            inst.tcp_port
        );
    }

    if instances.is_empty() {
        println!("\n=> NOTHING IS RUNNING to attach to.");
        println!("   This is what the backend reports as NoRunningInstance, and");
        println!("   why the picker's 'Debugger variables' root stays empty.");
        println!("   Start winIDEA, open a workspace, then re-run this probe.");
        return;
    }

    // ── Attach, exactly as the backend's attach mode does ────────────────
    let inst = &instances[0];
    println!("\n[3] connect_tcp(\"\", {}) ...", inst.tcp_port);
    if let Err(e) = mgr.connect_tcp("", inst.tcp_port) {
        println!("    FAIL: {e}");
        println!("\n=> Found an instance but could NOT attach to it.");
        return;
    }
    // `verify: true` asks the SDK rather than trusting the local handle —
    // the same distinction `winidea_ensure_connected` relies on.
    match mgr.is_connected(true) {
        Ok(c) => println!("    is_connected(verify) = {c}"),
        Err(e) => println!("    is_connected failed: {e}"),
    }

    // ── Can anything be sampled? ─────────────────────────────────────────
    match DaqController::new(&winidea, &mgr) {
        Ok(daq) => match daq.info() {
            Ok(info) => {
                println!(
                    "\n[4] DAQ: max_items={} tick_ns={} supported={}",
                    info.max_items,
                    info.tick_ns,
                    info.is_supported()
                );
                if !info.is_supported() {
                    println!("    => No DAQ hardware: symbols browse, but nothing samples.");
                }
            }
            Err(e) => println!("\n[4] FAIL: daq.info(): {e}"),
        },
        Err(e) => println!("\n[4] FAIL: DaqController::new: {e}"),
    }

    println!("\nDone — the attach path is reachable from this machine.");
}
