//! Line-based Tcl console on a TCP port (OpenOCD `telnet_port`).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::events::Event;
use crate::server::ServerHandle;

pub const PROMPT: &str = "> ";

/// `localhost` may resolve to `::1` first on Windows, while clients (and
/// OpenOCD's `bindto localhost`) use IPv4 - bind 127.0.0.1 instead.
pub fn bind_address(bind: &str) -> &str {
    if bind.eq_ignore_ascii_case("localhost") {
        "127.0.0.1"
    } else {
        bind
    }
}

pub fn start(handle: ServerHandle, bind: &str, port: u16) -> std::io::Result<u16> {
    let listener = TcpListener::bind((bind_address(bind), port))?;
    let actual = listener.local_addr()?.port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let h = handle.clone();
            thread::spawn(move || session(h, stream));
        }
    });
    Ok(actual)
}

fn session(h: ServerHandle, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let Ok(write_half) = stream.try_clone() else { return };
    let writer = Arc::new(Mutex::new(write_half));
    let alive = Arc::new(AtomicBool::new(true));

    let events = h.subscribe();
    {
        let writer = writer.clone();
        let alive = alive.clone();
        thread::spawn(move || {
            while alive.load(Ordering::Relaxed) {
                match events.recv_timeout(Duration::from_millis(200)) {
                    Ok(Event::Log { text }) => {
                        let mut w = writer.lock().unwrap();
                        if w.write_all(format!("{}\r\n", text.replace('\n', "\r\n")).as_bytes()).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Shutdown) => break,
                    Ok(_) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => break,
                }
            }
        });
    }

    let send = |s: &str| writer.lock().unwrap().write_all(s.as_bytes()).is_ok();
    if !send(&format!("aurix-dbg Tcl console (OpenOCD-compatible)\r\n{PROMPT}")) {
        return;
    }
    let reader = BufReader::new(stream);
    for line in reader.split(b'\n') {
        let Ok(line) = line else { break };
        // Drop CR and telnet negotiation bytes (IAC sequences use 0xF0..0xFF).
        let bytes: Vec<u8> = line.into_iter().filter(|&b| b != b'\r' && b < 0xF0).collect();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let cmd = text.trim();
        if cmd.is_empty() {
            if !send(PROMPT) {
                break;
            }
            continue;
        }
        if cmd == "exit" {
            break;
        }
        let (res, lines) = h.tcl_with_output(cmd);
        let out = match res {
            Ok(s) => s,
            Err(e) => e,
        };
        let mut msg = String::new();
        for l in lines {
            msg.push_str(&l.replace('\n', "\r\n"));
            msg.push_str("\r\n");
        }
        if !out.is_empty() {
            msg.push_str(&out.replace('\n', "\r\n"));
            msg.push_str("\r\n");
        }
        msg.push_str(PROMPT);
        if !send(&msg) {
            break;
        }
    }
    alive.store(false, Ordering::Relaxed);
}
