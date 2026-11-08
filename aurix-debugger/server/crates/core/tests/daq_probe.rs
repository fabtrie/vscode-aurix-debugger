//! The DAQ registry with a probe sampler ([`aurix_core::adapter::ProbeDaq`]), against the
//! simulator's ([`SimSampler`]), which behaves as the BlueBox's: one value per item after a
//! configure, then only changes; nothing at all for an item it cannot read.
//!
//! The simulator without a sampler is the TAS case; `daq_sim.rs` covers it and must keep passing
//! unchanged.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use aurix_core::daq::{self, DaqMsg, DaqState, PointCfg, SampleMode, SessionCfg, SessionId, Source, Space};
use aurix_core::sim::{SimAdapter, SimSampler, SIM_UNMAPPED};
use aurix_core::Server;

fn server(sampler: bool) -> Server {
    aurix_core::log::set_debug_level(0);
    let s = Server::new();
    s.cx().with(|c| c.adapter_override = Some("sim".into()));
    s.interp.eval_global("adapter driver sim").unwrap();
    s.interp.eval_global("init").unwrap();
    if sampler {
        with_sim(&s, |sim| sim.sampler = Some(SimSampler::default()));
    }
    s
}

fn with_sim<R>(s: &Server, f: impl FnOnce(&mut SimAdapter) -> R) -> R {
    s.cx().with(|c| f(c.adapter.as_mut().unwrap().as_any_mut().downcast_mut::<SimAdapter>().unwrap()))
}

fn sampler<R>(s: &Server, f: impl FnOnce(&mut SimSampler) -> R) -> R {
    with_sim(s, |sim| f(sim.sampler.as_mut().unwrap()))
}

fn open(s: &Server, at: Instant) -> (SessionId, Receiver<DaqMsg>) {
    s.cx().with(|c| c.daq.open(SessionCfg { queue: 1024 }, at).unwrap())
}

fn add(s: &Server, id: SessionId, pts: &[PointCfg], at: Instant) -> Vec<u32> {
    s.cx().with(|c| c.daq.add_points(id, pts, at).unwrap()).into_iter().map(|(p, _)| p).collect()
}

fn source(s: &Server, id: SessionId, p: u32) -> Source {
    s.cx().with(|c| c.daq.source(id, p).unwrap())
}

fn set_mem(s: &Server, addr: u32, v: u32) {
    with_sim(s, |sim| sim.set_mem(addr, v));
}

/// Host reads of `addr` since the last call.
fn host_reads_of(s: &Server, addr: u32) -> usize {
    with_sim(s, |sim| std::mem::take(&mut sim.reads)).iter().filter(|&&(a, n)| a <= addr && addr < a + n as u32).count()
}

/// Wakes at every deadline up to `end`, as the server loop does.
fn run_until(s: &Server, end: Instant) {
    let mut wakes = 0;
    while let Some(due) = s.cx().with(|c| c.daq.next_due()) {
        if due > end {
            break;
        }
        daq::tick_at(&s.cx(), due, Some(due));
        wakes += 1;
        assert!(wakes < 20_000, "the sampler is not making progress");
    }
}

fn samples(rx: &Receiver<DaqMsg>) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    while let Ok(m) = rx.try_recv() {
        if let DaqMsg::Samples { values, .. } = m {
            for (id, data) in values {
                let mut w = [0u8; 4];
                w[..data.len().min(4)].copy_from_slice(&data[..data.len().min(4)]);
                out.push((id, u32::from_le_bytes(w)));
            }
        }
    }
    out
}

const A: u32 = 0x7000_0100;
const B: u32 = 0x7000_0200;
/// Far enough from A and B that no host read merges them in.
const OVER: u32 = 0x7000_0800;
const MS: Duration = Duration::from_millis(1);

#[test]
fn a_plain_bus_point_moves_to_the_probe_and_off_the_host() {
    let s = server(true);
    let t0 = Instant::now();
    set_mem(&s, A, 0x1111);
    let (id, rx) = open(&s, t0);
    let p = add(&s, id, &[PointCfg::new(A, 4, Space::Host, 10_000)], t0)[0];
    // Read by the host until the probe is configured (after the debounce): a value at once.
    assert_eq!(source(&s, id, p), Source::Polled);
    run_until(&s, t0 + 40 * MS);
    assert!(host_reads_of(&s, A) > 0);
    run_until(&s, t0 + 60 * MS);
    assert_eq!(source(&s, id, p), Source::Probe);
    assert_eq!(sampler(&s, |p| p.items.clone()), vec![aurix_core::adapter::ProbeItem { addr: A, size: 4, period_us: 10_000 }]);
    samples(&rx);
    run_until(&s, t0 + 200 * MS);
    assert_eq!(host_reads_of(&s, A), 0, "a probe point is not read by the host");
    // EveryTick: delivered at its period with the probe's value.
    let got = samples(&rx);
    assert!((12..=15).contains(&got.len()), "{} deliveries in 140 ms at 10 ms", got.len());
    assert!(got.iter().all(|&(q, v)| q == p && v == 0x1111));
    set_mem(&s, A, 0x2222);
    run_until(&s, t0 + 260 * MS);
    assert_eq!(samples(&rx).last(), Some(&(p, 0x2222)));
}

#[test]
fn on_change_points_are_delivered_from_the_drain_only_when_they_change() {
    let s = server(true);
    let t0 = Instant::now();
    set_mem(&s, A, 7);
    let (id, rx) = open(&s, t0);
    let p = add(&s, id, &[PointCfg { mode: SampleMode::OnChange, ..PointCfg::new(A, 4, Space::Host, 1_000) }], t0)[0];
    run_until(&s, t0 + 300 * MS);
    assert_eq!(source(&s, id, p), Source::Probe);
    // The host's baseline before the configure, nothing after while the value stands still.
    assert_eq!(samples(&rx), vec![(p, 7)]);
    set_mem(&s, A, 8);
    run_until(&s, t0 + 320 * MS);
    set_mem(&s, A, 9);
    run_until(&s, t0 + 340 * MS);
    assert_eq!(samples(&rx), vec![(p, 8), (p, 9)]);
    // A 1 ms point drains every 5 ms.
    let drains = sampler(&s, |p| p.drains);
    run_until(&s, t0 + 390 * MS);
    let more = sampler(&s, |p| p.drains) - drains;
    assert!((9..=11).contains(&more), "{more} drains in 50 ms");
}

#[test]
fn what_the_probe_cannot_take_stays_on_the_host() {
    let s = server(true);
    with_sim(&s, |sim| sim.sampler.as_mut().unwrap().max_items = 3);
    let t0 = Instant::now();
    let (id, _rx) = open(&s, t0);
    let pts = add(
        &s,
        id,
        &[
            PointCfg::new(A, 16, Space::Host, 10_000), // two items
            PointCfg::new(A + 0x40, 128, Space::Host, 10_000), // too long
            PointCfg::new(A + 0x400, 4, Space::Core(0), 10_000), // core-relative
            PointCfg::new(B, 8, Space::Host, 10_000), // one item: fits (3 of 3)
            PointCfg::new(OVER, 4, Space::Host, 10_000), // over the budget
        ],
        t0,
    );
    run_until(&s, t0 + 100 * MS);
    let got: Vec<Source> = pts.iter().map(|&p| source(&s, id, p)).collect();
    assert_eq!(got, vec![Source::Probe, Source::Polled, Source::Polled, Source::Probe, Source::Polled]);
    assert_eq!(sampler(&s, |p| p.items.len()), 3);
    host_reads_of(&s, 0);
    run_until(&s, t0 + 200 * MS);
    let reads = with_sim(&s, |sim| std::mem::take(&mut sim.reads));
    let covers = |x: u32| reads.iter().any(|&(a, n)| a <= x && x < a + n as u32);
    assert!(covers(OVER), "the point over the budget is read by the host");
    assert!(!covers(A) && !covers(B), "the probe points are not");
}

#[test]
fn an_item_the_probe_never_answers_goes_back_to_the_host() {
    let s = server(true);
    let t0 = Instant::now();
    let bad = SIM_UNMAPPED.start;
    let (id, rx) = open(&s, t0);
    // 100 ms: read once by the host before the probe takes it, so it is not muted yet.
    let pts = add(&s, id, &[PointCfg::new(bad, 4, Space::Host, 100_000), PointCfg::new(A, 4, Space::Host, 10_000)], t0);
    run_until(&s, t0 + 60 * MS);
    assert_eq!(source(&s, id, pts[0]), Source::Probe);
    // No first value within 300 ms (three 100 ms checks): back to the host, whose read fails and
    // reports why; the probe is configured without it.
    run_until(&s, t0 + 400 * MS);
    assert_eq!(source(&s, id, pts[0]), Source::Polled);
    assert_eq!(source(&s, id, pts[1]), Source::Probe);
    assert_eq!(sampler(&s, |p| p.items.iter().map(|i| i.addr).collect::<Vec<_>>()), vec![A]);
    let errs = std::iter::from_fn(|| rx.try_recv().ok()).filter(|m| matches!(m, DaqMsg::PointError { point, .. } if *point == pts[0])).count();
    assert!(errs >= 1, "the host read reports the error");
}

#[test]
fn a_reconnect_configures_the_probe_again() {
    let s = server(true);
    let t0 = Instant::now();
    let (id, _rx) = open(&s, t0);
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 10_000)], t0);
    run_until(&s, t0 + 100 * MS);
    let n = sampler(&s, |p| p.configures);
    sampler(&s, |p| p.generation += 1);
    run_until(&s, t0 + 130 * MS);
    assert_eq!(sampler(&s, |p| p.configures), n + 1);
}

#[test]
fn a_failing_probe_hands_its_points_to_the_host_until_it_works_again() {
    let s = server(true);
    let t0 = Instant::now();
    set_mem(&s, A, 5);
    let (id, rx) = open(&s, t0);
    let p = add(&s, id, &[PointCfg::new(A, 4, Space::Host, 10_000)], t0)[0];
    run_until(&s, t0 + 100 * MS);
    assert_eq!(source(&s, id, p), Source::Probe);
    sampler(&s, |p| p.fail = true);
    run_until(&s, t0 + 150 * MS);
    assert_eq!(source(&s, id, p), Source::Polled);
    let states: Vec<DaqState> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|m| if let DaqMsg::State { state, .. } = m { Some(state) } else { None })
        .collect();
    assert!(states.contains(&DaqState::Unavailable));
    host_reads_of(&s, A);
    run_until(&s, t0 + 250 * MS);
    assert!(host_reads_of(&s, A) > 0, "the host reads it meanwhile");
    sampler(&s, |p| p.fail = false);
    run_until(&s, t0 + 600 * MS);
    assert_eq!(source(&s, id, p), Source::Probe);
}

#[test]
fn closing_the_last_session_empties_the_probe() {
    let s = server(true);
    let t0 = Instant::now();
    let (id, _rx) = open(&s, t0);
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 10_000)], t0);
    run_until(&s, t0 + 100 * MS);
    assert_eq!(sampler(&s, |p| p.items.len()), 1);
    s.cx().with(|c| c.daq.close(id));
    run_until(&s, Instant::now() + 100 * MS);
    assert_eq!(sampler(&s, |p| p.items.len()), 0);
}

#[test]
fn a_probe_only_point_is_refused_without_a_sampler_and_never_read() {
    let s = server(false);
    let t0 = Instant::now();
    let (id, _rx) = open(&s, t0);
    let p = add(&s, id, &[PointCfg { probe_only: true, ..PointCfg::new(A, 4, Space::Host, 10_000) }], t0)[0];
    assert_eq!(source(&s, id, p), Source::Pending);
    host_reads_of(&s, A);
    run_until(&s, t0 + 200 * MS);
    assert_eq!(source(&s, id, p), Source::Refused);
    assert_eq!(host_reads_of(&s, A), 0);
}

#[test]
fn without_a_sampler_every_point_stays_on_the_host() {
    let s = server(false);
    let t0 = Instant::now();
    let (id, rx) = open(&s, t0);
    let p = add(&s, id, &[PointCfg::new(A, 4, Space::Host, 10_000)], t0)[0];
    run_until(&s, t0 + 200 * MS);
    assert_eq!(source(&s, id, p), Source::Polled);
    assert!(host_reads_of(&s, A) >= 18);
    assert!(samples(&rx).len() >= 18);
}

#[test]
fn a_point_wider_than_an_item_is_assembled() {
    let s = server(true);
    let t0 = Instant::now();
    set_mem(&s, B, 0x0403_0201);
    set_mem(&s, B + 4, 0x0807_0605);
    set_mem(&s, B + 8, 0x0c0b_0a09);
    let (id, rx) = open(&s, t0);
    let p = add(&s, id, &[PointCfg { mode: SampleMode::OnChange, ..PointCfg::new(B + 4, 8 + 4, Space::Host, 10_000) }], t0)[0];
    run_until(&s, t0 + 100 * MS);
    assert_eq!(sampler(&s, |p| p.items.iter().map(|i| (i.addr, i.size)).collect::<Vec<_>>()), vec![(B + 4, 4), (B + 8, 8)]);
    samples(&rx);
    set_mem(&s, B + 12, 0xdead_beef);
    run_until(&s, t0 + 140 * MS);
    let v = s.cx().with(|c| c.daq.get(id).unwrap().points.iter().find(|x| x.id == p).unwrap().probe_value().map(|v| v.to_vec()));
    assert_eq!(v, Some(vec![5, 6, 7, 8, 9, 10, 11, 12, 0xef, 0xbe, 0xad, 0xde]));
}
