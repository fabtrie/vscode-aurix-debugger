//! The core's health reporting against the simulator: `Event::Health` once per change, and
//! `Event::ProbeLost` only when the probe stays lost for `probe_lost_timeout`.
//!
//! One test function on purpose: events go to every subscriber of the process.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use aurix_core::adapter::LinkHealth;
use aurix_core::sim::SimAdapter;
use aurix_core::{events, ops, Event, Server};

fn server() -> Server {
    aurix_core::log::set_debug_level(0);
    let s = Server::new();
    s.cx().with(|c| c.adapter_override = Some("sim".into()));
    s.interp.eval_global("adapter driver sim").unwrap();
    s.interp.eval_global("init").unwrap();
    s
}

fn set_health(s: &Server, h: Option<LinkHealth>) {
    s.cx().with(|c| c.adapter.as_mut().unwrap().as_any_mut().downcast_mut::<SimAdapter>().unwrap().health = h);
}

/// The health and probe-lost events since the last call (others dropped).
fn take(rx: &Receiver<Event>) -> Vec<String> {
    rx.try_iter()
        .filter_map(|e| match e {
            Event::Health { health } => Some(format!("health {}", health.map_or("none", |h| h.state()))),
            Event::ProbeLost { why } => Some(format!("lost {why}")),
            _ => None,
        })
        .collect()
}

#[test]
fn health_changes_and_probe_lost_timeout() {
    let s = server();
    let rx = events::subscribe();
    let poll = || ops::poll_all(&s.cx());

    // Nothing to report from a probe that cannot tell.
    poll();
    assert!(take(&rx).is_empty());

    // One event per change, none while it stays.
    set_health(&s, Some(LinkHealth::Unpowered { vref_mv: 34 }));
    poll();
    // The supply decaying further is no new state.
    set_health(&s, Some(LinkHealth::Unpowered { vref_mv: 12 }));
    poll();
    assert_eq!(take(&rx), ["health unpowered"]);
    // What `Status` publishes to new frontends (copied by the server loop).
    assert_eq!(s.cx().with(|c| c.health), Some(LinkHealth::Unpowered { vref_mv: 12 }));
    set_health(&s, Some(LinkHealth::Ok));
    poll();
    assert_eq!(take(&rx), ["health ok"]);

    // Probe lost and back before the timeout: the sessions stay.
    s.interp.eval_global("probe_lost_timeout 50").unwrap();
    set_health(&s, Some(LinkHealth::ProbeLost { why: "unplugged" }));
    poll();
    set_health(&s, Some(LinkHealth::Ok));
    std::thread::sleep(Duration::from_millis(60));
    poll();
    assert_eq!(take(&rx), ["health probeLost", "health ok"]);

    // Lost for longer: ProbeLost once.
    set_health(&s, Some(LinkHealth::ProbeLost { why: "unplugged" }));
    poll();
    assert_eq!(take(&rx), ["health probeLost"]);
    std::thread::sleep(Duration::from_millis(60));
    poll();
    poll();
    assert_eq!(take(&rx), ["lost unplugged"]);

    // Timeout 0: never end the sessions.
    set_health(&s, Some(LinkHealth::Ok));
    poll();
    s.interp.eval_global("probe_lost_timeout 0").unwrap();
    set_health(&s, Some(LinkHealth::ProbeLost { why: "unplugged" }));
    poll();
    std::thread::sleep(Duration::from_millis(60));
    poll();
    assert_eq!(take(&rx), ["health ok", "health probeLost"]);
}
