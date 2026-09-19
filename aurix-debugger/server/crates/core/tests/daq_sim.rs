//! The DAQ sampler against the simulated TC37x.
//!
//! Every test drives `daq::tick_at` with its own instants, so scheduling and timestamps are
//! deterministic: no sleeps, no flakes.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use aurix_core::adapter::BatchLimits;
use aurix_core::daq::{self, DaqMsg, DaqState, PointCfg, SampleMode, SessionCfg, SessionId, Space};
use aurix_core::sim::SimAdapter;
use aurix_core::Server;

/// A server with the simulator initialised, without loading any cfg script.
fn server() -> Server {
    aurix_core::log::set_debug_level(0);
    let s = Server::new();
    s.cx().with(|c| c.adapter_override = Some("sim".into()));
    s.interp.eval_global("adapter driver sim").unwrap();
    s.interp.eval_global("init").unwrap();
    s
}

fn with_sim<R>(s: &Server, f: impl FnOnce(&mut SimAdapter) -> R) -> R {
    s.cx().with(|c| f(c.adapter.as_mut().unwrap().as_any_mut().downcast_mut::<SimAdapter>().unwrap()))
}

fn open(s: &Server, queue: usize, at: Instant) -> (SessionId, Receiver<DaqMsg>) {
    s.cx().with(|c| c.daq.open(SessionCfg { queue }, at).unwrap())
}

fn add(s: &Server, id: SessionId, pts: &[PointCfg], at: Instant) -> Vec<(u32, u32)> {
    s.cx().with(|c| c.daq.add_points(id, pts, at).unwrap())
}

fn set_mem(s: &Server, addr: u32, v: u32) {
    with_sim(s, |sim| sim.set_mem(addr, v));
}

fn take_reads(s: &Server) -> Vec<(u32, usize)> {
    with_sim(s, |sim| std::mem::take(&mut sim.reads))
}

/// Drives the sampler the way the server loop does: wake at whatever deadline comes next, never
/// on a fixed grid. Phase problems only show up this way - a fixed 1 ms tick hides them, because
/// every skewed deadline is due by the time the next tick comes round.
fn run_until(s: &Server, t0: Instant, end: Duration) -> usize {
    let mut wakes = 0;
    while let Some(due) = s.cx().with(|c| c.daq.next_due()) {
        if due > t0 + end {
            break;
        }
        daq::tick_at(&s.cx(), due, Some(due));
        wakes += 1;
        assert!(wakes < 10_000, "the sampler is not making progress");
    }
    wakes
}

/// The samples of every `Samples` message, as (point, first 4 bytes as u32).
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

fn drain(rx: &Receiver<DaqMsg>) -> Vec<DaqMsg> {
    let mut out = Vec::new();
    while let Ok(m) = rx.try_recv() {
        out.push(m);
    }
    out
}

/// RAM the simulator serves and no cfg script touches.
const A: u32 = 0x7000_0100;
const B: u32 = 0x7000_0200;

#[test]
fn each_point_is_sampled_at_its_own_rate() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 256, t0);
    let got = add(
        &s,
        id,
        &[PointCfg::new(A, 4, Space::Host, 1000), PointCfg::new(B, 4, Space::Host, 5000)],
        t0,
    );
    assert_eq!(got, vec![(1, 1000), (2, 5000)], "granted periods are reported back");
    set_mem(&s, A, 0x1111_1111);
    set_mem(&s, B, 0x2222_2222);

    // 10 ticks, 1 ms apart. The fast point is due every tick, the slow one every fifth.
    for k in 0..10 {
        let at = t0 + Duration::from_millis(k);
        daq::tick_at(&s.cx(), at, Some(at));
    }
    let got = samples(&rx);
    let fast = got.iter().filter(|(p, _)| *p == 1).count();
    let slow = got.iter().filter(|(p, _)| *p == 2).count();
    assert_eq!(fast, 10, "1 kHz point over 10 ms: {got:?}");
    assert_eq!(slow, 2, "200 Hz point over 10 ms: {got:?}");
    assert!(got.iter().all(|&(p, v)| if p == 1 { v == 0x1111_1111 } else { v == 0x2222_2222 }), "{got:?}");
}

#[test]
fn points_added_at_different_moments_still_share_a_tick() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 256, t0);
    // Resolving a name takes a moment, so points of one subscribe reach the registry at
    // different instants. They must still land on the session grid, or each would get its own
    // message and the whole point of batching a tick would be lost.
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 10_000)], t0);
    add(&s, id, &[PointCfg::new(B, 4, Space::Host, 10_000)], t0 + Duration::from_micros(300));
    add(&s, id, &[PointCfg::new(A + 64, 4, Space::Host, 10_000)], t0 + Duration::from_micros(1500));

    // Five periods of 10 ms. Aligned, that is five wakes of three values each; skewed, it would
    // be fifteen wakes of one value each.
    let wakes = run_until(&s, t0, Duration::from_millis(49));
    let batches: Vec<Vec<(u32, u32)>> = drain(&rx)
        .into_iter()
        .filter_map(|m| match m {
            DaqMsg::Samples { values, .. } => Some(values.into_iter().map(|(p, _)| (p, 0)).collect()),
            _ => None,
        })
        .collect();
    assert_eq!(wakes, 5, "one wake per period, not one per point");
    assert_eq!(batches.len(), 5, "one message per tick: {batches:?}");
    // The very first tick carries only the point that was already registered and due; from then
    // on the later arrivals ride the same grid.
    assert!(batches[1..].iter().all(|b| b.len() == 3), "the points share every tick once added: {batches:?}");
}

#[test]
fn harmonic_rates_keep_sharing_ticks() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 512, t0);
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 1000)], t0);
    // Added a fraction of a period later, as a second subscribe would be.
    add(&s, id, &[PointCfg::new(B, 4, Space::Host, 10_000)], t0 + Duration::from_micros(250));

    // 30 ms: the 1 kHz point wakes 30 times and the 100 Hz point must ride along on every tenth
    // of them, never causing a wake or a message of its own.
    let wakes = run_until(&s, t0, Duration::from_millis(29));
    let mut alone = 0;
    let mut slow = 0;
    for m in drain(&rx) {
        if let DaqMsg::Samples { values, .. } = m {
            if values.iter().any(|(p, _)| *p == 2) {
                slow += 1;
                if values.len() == 1 {
                    alone += 1;
                }
            }
        }
    }
    assert_eq!(wakes, 30, "the slow point must not add wakes of its own");
    assert!(slow >= 2, "the 100 Hz point fired: {slow}");
    assert_eq!(alone, 0, "and always shared its tick with the 1 kHz point");
}

#[test]
fn a_stall_reports_one_gap_instead_of_replaying_every_tick() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 256, t0);
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 1000)], t0);

    daq::tick_at(&s.cx(), t0, Some(t0));
    let _ = drain(&rx);
    // 100 ms later, as if a flash run had owned the core thread.
    let at = t0 + Duration::from_millis(100);
    daq::tick_at(&s.cx(), at, Some(at));

    let msgs = drain(&rx);
    let samples = msgs.iter().filter(|m| matches!(m, DaqMsg::Samples { .. })).count();
    let gap = msgs.iter().find_map(|m| match m {
        DaqMsg::Gap { missed, .. } => Some(*missed),
        _ => None,
    });
    assert_eq!(samples, 1, "one sample, not 100 catch-up samples: {msgs:?}");
    assert!(gap.is_some_and(|g| g >= 98), "gap reported once: {msgs:?}");
}

#[test]
fn adjacent_points_are_coalesced_into_one_read() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 256, t0);
    // Eight adjacent words, deliberately added out of order.
    let pts: Vec<PointCfg> = (0..8).rev().map(|i| PointCfg::new(A + 4 * i, 4, Space::Host, 1000)).collect();
    add(&s, id, &pts, t0);

    take_reads(&s);
    daq::tick_at(&s.cx(), t0, Some(t0));
    let reads = take_reads(&s);
    assert_eq!(reads, vec![(A, 32)], "one 32-byte read, not eight: {reads:?}");
    assert_eq!(samples(&rx).len(), 8, "all eight points still get a value");
}

#[test]
fn far_apart_points_stay_separate_and_nothing_between_them_is_read() {
    let s = server();
    let t0 = Instant::now();
    let (id, _rx) = open(&s, 256, t0);
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 1000), PointCfg::new(B, 4, Space::Host, 1000)], t0);

    take_reads(&s);
    daq::tick_at(&s.cx(), t0, Some(t0));
    let reads = take_reads(&s);
    assert_eq!(reads.len(), 2, "two separate reads: {reads:?}");
    // The default coalescing gap is zero, so no word between A and B is touched: reading a
    // peripheral register nobody asked for can have side effects.
    assert!(reads.iter().all(|&(a, l)| (a == A || a == B) && l == 4), "{reads:?}");
}

#[test]
fn a_tick_over_the_packet_budget_is_truncated_and_reported() {
    let s = server();
    let t0 = Instant::now();
    with_sim(&s, |sim| sim.limits = BatchLimits { max_num_rw: 2, ..BatchLimits::default() });
    let (id, rx) = open(&s, 256, t0);
    // 40 points, each its own run (a 16-byte stride leaves a gap).
    let pts: Vec<PointCfg> = (0..40).map(|i| PointCfg::new(A + 16 * i, 4, Space::Host, 1000)).collect();
    add(&s, id, &pts, t0);

    daq::tick_at(&s.cx(), t0, Some(t0));
    let msgs = drain(&rx);
    let n: usize = msgs
        .iter()
        .map(|m| match m {
            DaqMsg::Samples { values, .. } => values.len(),
            _ => 0,
        })
        .sum();
    // max_num_rw 2 * MAX_PKTS_PER_TICK 2 = 4 runs per tick.
    assert_eq!(n, 4, "the tick is capped: {msgs:?}");
    assert!(
        msgs.iter().any(|m| matches!(m, DaqMsg::State { state: DaqState::Stalled, .. })),
        "the client is told it is stalled: {msgs:?}"
    );

    // The skipped points advanced too, so the next tick does not replay them.
    let at = t0 + Duration::from_millis(1);
    daq::tick_at(&s.cx(), at, Some(at));
    let n2: usize = drain(&rx)
        .iter()
        .map(|m| match m {
            DaqMsg::Samples { values, .. } => values.len(),
            _ => 0,
        })
        .sum();
    assert_eq!(n2, 4);
}

#[test]
fn a_slow_consumer_drops_messages_and_is_told_how_many() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 16, t0); // the floor of the queue clamp
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 1000)], t0);

    // 20 ticks without draining: the first 16 fit, the rest are dropped.
    for k in 0..20 {
        let at = t0 + Duration::from_millis(k);
        daq::tick_at(&s.cx(), at, Some(at));
    }
    let stats = s.cx().with(|c| c.daq.stats(id).unwrap());
    assert!(stats.dropped > 0, "the overrun is counted: {stats:?}");

    let msgs = drain(&rx);
    assert_eq!(msgs.len(), 16, "the queue held exactly its capacity: {}", msgs.len());

    // Once there is room again, the client is told what it missed before the next samples.
    let at = t0 + Duration::from_millis(100);
    daq::tick_at(&s.cx(), at, Some(at));
    let msgs = drain(&rx);
    assert!(
        matches!(msgs.first(), Some(DaqMsg::Overrun { dropped, .. }) if *dropped > 0),
        "overrun is reported first: {msgs:?}"
    );
}

#[test]
fn on_change_delivers_a_baseline_then_only_real_changes() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 256, t0);
    let cfg = PointCfg { mode: SampleMode::OnChange, ..PointCfg::new(A, 4, Space::Host, 1000) };
    add(&s, id, &[cfg], t0);
    set_mem(&s, A, 0xAAAA_AAAA);

    daq::tick_at(&s.cx(), t0, Some(t0));
    assert_eq!(samples(&rx), vec![(1, 0xAAAA_AAAA)], "the first read is always delivered");

    take_reads(&s);
    for k in 1..11 {
        let at = t0 + Duration::from_millis(k);
        daq::tick_at(&s.cx(), at, Some(at));
    }
    assert!(samples(&rx).is_empty(), "an unchanged value is not delivered");
    // ... but it was still READ every tick. on_change saves stream bandwidth, not target accesses.
    let reads = take_reads(&s);
    assert_eq!(reads.len(), 10, "the point is polled every period regardless: {reads:?}");

    set_mem(&s, A, 0xBBBB_BBBB);
    let at = t0 + Duration::from_millis(11);
    daq::tick_at(&s.cx(), at, Some(at));
    assert_eq!(samples(&rx), vec![(1, 0xBBBB_BBBB)], "the change is delivered once");

    let at = t0 + Duration::from_millis(12);
    daq::tick_at(&s.cx(), at, Some(at));
    assert!(samples(&rx).is_empty(), "and then it goes quiet again");
}

#[test]
fn on_change_with_a_mask_ignores_a_neighbouring_bit_field() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 256, t0);
    // Only the low byte of the word belongs to this field.
    let cfg = PointCfg {
        mode: SampleMode::OnChange,
        cmp_mask: Some(vec![0xff, 0x00, 0x00, 0x00]),
        ..PointCfg::new(A, 4, Space::Host, 1000)
    };
    add(&s, id, &[cfg], t0);
    set_mem(&s, A, 0x0000_0001);
    daq::tick_at(&s.cx(), t0, Some(t0));
    let _ = drain(&rx);

    // A neighbouring field in the same storage word changes: not our point.
    set_mem(&s, A, 0xFFFF_FF01);
    let at = t0 + Duration::from_millis(1);
    daq::tick_at(&s.cx(), at, Some(at));
    assert!(samples(&rx).is_empty(), "a change outside the mask is not a change");

    set_mem(&s, A, 0xFFFF_FF02);
    let at = t0 + Duration::from_millis(2);
    daq::tick_at(&s.cx(), at, Some(at));
    assert_eq!(samples(&rx), vec![(1, 0xFFFF_FF02)], "a change inside the mask is");
}

#[test]
fn resend_after_redelivers_an_unchanged_point() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 256, t0);
    let cfg = PointCfg {
        mode: SampleMode::OnChange,
        resend_after_us: 5_000,
        ..PointCfg::new(A, 4, Space::Host, 1000)
    };
    add(&s, id, &[cfg], t0);
    set_mem(&s, A, 7);

    for k in 0..16 {
        let at = t0 + Duration::from_millis(k);
        daq::tick_at(&s.cx(), at, Some(at));
    }
    // t=0 baseline, then one every 5 ms: 0, 5, 10, 15.
    assert_eq!(samples(&rx).len(), 4, "a static value still gets a floor on the gap");
}

#[test]
fn a_reset_drops_the_baseline_so_the_next_read_is_delivered() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 256, t0);
    let cfg = PointCfg { mode: SampleMode::OnChange, ..PointCfg::new(A, 4, Space::Host, 1000) };
    add(&s, id, &[cfg], t0);
    set_mem(&s, A, 0x1234_5678);
    daq::tick_at(&s.cx(), t0, Some(t0));
    let _ = drain(&rx);

    s.cx().with(|c| daq::on_reset(c, t0));
    // The sampler backs off first, then resumes with a fresh baseline even though the value in
    // the model did not change - a client must not keep believing a pre-reset value.
    let at = t0 + daq::RETRY_BACKOFF + Duration::from_millis(1);
    daq::tick_at(&s.cx(), at, Some(at));
    assert_eq!(samples(&rx), vec![(1, 0x1234_5678)]);
}

#[test]
fn a_dropped_receiver_stops_the_sampling() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 256, t0);
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 1000)], t0);
    assert!(s.cx().with(|c| c.daq.next_due().is_some()));

    drop(rx);
    let at = t0 + Duration::from_millis(1);
    daq::tick_at(&s.cx(), at, Some(at));

    s.cx().with(|c| {
        assert!(c.daq.is_empty(), "a crashed consumer must not keep hammering the target");
        assert!(c.daq.next_due().is_none(), "and the loop goes back to costing nothing");
        assert!(c.daq.stats(id).is_none());
    });
}

#[test]
fn an_idle_sampler_costs_nothing() {
    let s = server();
    s.cx().with(|c| {
        assert!(c.daq.is_empty());
        assert!(c.daq.next_due().is_none());
    });
    take_reads(&s);
    daq::tick_at(&s.cx(), Instant::now(), None);
    assert!(take_reads(&s).is_empty(), "no session means no target access at all");
}
