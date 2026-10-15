//! The DAQ sampler against the simulated TC37x.
//!
//! Every test drives `daq::tick_at` with its own instants, so scheduling and timestamps are
//! deterministic: no sleeps, no flakes.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use aurix_core::adapter::BatchLimits;
use aurix_core::daq::{self, DaqMsg, DaqState, PointCfg, SampleMode, SessionCfg, SessionId, Space};
use aurix_core::ops;
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
fn a_tick_over_the_packet_budget_is_truncated_and_shared_out_fairly() {
    let s = server();
    let t0 = Instant::now();
    with_sim(&s, |sim| sim.limits = BatchLimits { max_num_rw: 2, ..BatchLimits::default() });
    let (id, rx) = open(&s, 4096, t0);
    // A stride wider than MAX_POINT_LEN, so no two points can ever share a read however large
    // the coalescing gap is: this test is about the packet budget, not about merging.
    const STRIDE: u32 = 2048;
    let pts: Vec<PointCfg> = (0..40).map(|i| PointCfg::new(A + STRIDE * i, 4, Space::Host, 1000)).collect();
    add(&s, id, &pts, t0);

    daq::tick_at(&s.cx(), t0, Some(t0));
    let msgs = drain(&rx);
    let first: Vec<u32> = msgs
        .iter()
        .flat_map(|m| match m {
            DaqMsg::Samples { values, .. } => values.iter().map(|(p, _)| *p).collect(),
            _ => Vec::new(),
        })
        .collect();
    // max_num_rw 2 * MAX_PKTS_PER_TICK 2 = 4 runs per tick.
    assert_eq!(first.len(), 4, "the tick is capped: {msgs:?}");
    assert!(
        msgs.iter().any(|m| matches!(m, DaqMsg::State { state: DaqState::Stalled, .. })),
        "the client is told it is stalled: {msgs:?}"
    );

    // The budget must not fall on the same points every tick. Before rotation the four lowest
    // addresses were served forever and everything above them starved - and since peripherals
    // sort last, those were the points most likely to be lost.
    let at = t0 + Duration::from_millis(1);
    daq::tick_at(&s.cx(), at, Some(at));
    let second: Vec<u32> = drain(&rx)
        .iter()
        .flat_map(|m| match m {
            DaqMsg::Samples { values, .. } => values.iter().map(|(p, _)| *p).collect(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(second.len(), 4, "still capped");
    assert!(second.iter().all(|p| !first.contains(p)), "a different four: {first:?} then {second:?}");

    // Over enough ticks every point gets its turn, and the skipped ones are never replayed as a
    // burst - each tick still costs exactly the budget.
    let mut seen: Vec<u32> = first.into_iter().chain(second).collect();
    for k in 2..40u64 {
        let at = t0 + Duration::from_millis(k);
        daq::tick_at(&s.cx(), at, Some(at));
        let got: Vec<u32> = drain(&rx)
            .iter()
            .flat_map(|m| match m {
                DaqMsg::Samples { values, .. } => values.iter().map(|(p, _)| *p).collect(),
                _ => Vec::new(),
            })
            .collect();
        assert_eq!(got.len(), 4, "every tick costs the budget, no catch-up burst");
        seen.extend(got);
    }
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 40, "every point was served at least once: {} of 40", seen.len());
}

#[test]
fn nearby_points_are_folded_into_one_read_but_peripherals_are_not() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 256, t0);
    // Two words 16 bytes apart: reading the 24-byte span costs far less than a second bus
    // access, so the sampler takes the hole with it.
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 1000), PointCfg::new(A + 16, 4, Space::Host, 1000)], t0);
    take_reads(&s);
    daq::tick_at(&s.cx(), t0, Some(t0));
    assert_eq!(take_reads(&s), vec![(A, 20)], "one read covering both points");
    assert_eq!(samples(&rx).len(), 2, "and both points still get their own value");

    // The same spacing in peripheral space must NOT be folded: a byte read there that nobody
    // asked for can clear a flag.
    const PERI: u32 = 0xF000_1010;
    let (id2, _rx2) = open(&s, 256, t0);
    add(&s, id2, &[PointCfg::new(PERI, 4, Space::Host, 1000), PointCfg::new(PERI + 16, 4, Space::Host, 1000)], t0);
    s.cx().with(|c| c.daq.close(id));
    take_reads(&s);
    let at = t0 + Duration::from_millis(1);
    daq::tick_at(&s.cx(), at, Some(at));
    assert_eq!(take_reads(&s), vec![(PERI, 4), (PERI + 16, 4)], "two reads, nothing in between");
}

#[test]
fn the_coalescing_gap_can_be_turned_off() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 256, t0);
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 1000), PointCfg::new(A + 16, 4, Space::Host, 1000)], t0);
    s.cx().with(|c| c.daq_gap = 0);
    take_reads(&s);
    daq::tick_at(&s.cx(), t0, Some(t0));
    assert_eq!(take_reads(&s), vec![(A, 4), (A + 16, 4)], "gap 0 merges only adjacent points");

    // Adjacent points still merge with the gap off - that reads nothing extra.
    let _ = drain(&rx);
    s.cx().with(|c| c.daq.remove_points(id, &[1, 2]).unwrap());
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 1000), PointCfg::new(A + 4, 4, Space::Host, 1000)], t0);
    take_reads(&s);
    let at = t0 + Duration::from_millis(1);
    daq::tick_at(&s.cx(), at, Some(at));
    assert_eq!(take_reads(&s), vec![(A, 8)]);
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
fn the_schedule_corrects_itself_towards_the_requested_rate() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 4096, t0);
    // 1 kHz asked for.
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 1000)], t0);

    // Simulate what the OS actually does: every tick lands ~400 us later than it was scheduled,
    // which is why aiming a sleep at the deadline delivers ~30 % under the request.
    const LATE: Duration = Duration::from_micros(400);
    let mut now = t0;
    let mut ticks = 0;
    let mut last = None;
    let mut intervals = Vec::new();
    for _ in 0..400 {
        let Some(due) = s.cx().with(|c| c.daq.next_due()) else { break };
        now = due + LATE;
        daq::tick_at(&s.cx(), now, Some(now));
        ticks += 1;
        if let Some(prev) = last.replace(now) {
            intervals.push(now.duration_since(prev).as_micros() as u64);
        }
    }
    assert!(ticks > 50, "the sampler kept running: {ticks}");

    // Without correction every interval would be period + LATE = 1400 us (714 Hz). The controller
    // shortens the scheduled period until what comes out is the 1000 us that was asked for.
    let settled = &intervals[intervals.len() / 2..];
    let mean: u64 = settled.iter().sum::<u64>() / settled.len() as u64;
    assert!(
        (900..=1100).contains(&mean),
        "delivered interval should settle near the requested 1000 us, got {mean} (first {:?}, last {:?})",
        &intervals[..3.min(intervals.len())],
        &settled[settled.len().saturating_sub(3)..]
    );

    // And it is never *under*-delivering: at least as many samples as the request implies.
    let span = now.duration_since(t0).as_secs_f64();
    let rate = ticks as f64 / span;
    assert!(rate >= 950.0, "at_least must not deliver under the request: {rate:.0} Hz");

    // A point that misses nothing reports no gaps: falling behind is what it is correcting.
    assert!(!drain(&rx).iter().any(|m| matches!(m, DaqMsg::Gap { .. })), "correcting for lateness is not a gap");
}

#[test]
fn steady_lateness_is_corrected_while_a_real_stall_is_reported() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 4096, t0);
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 1000)], t0);

    // Running late every tick is the normal case the controller exists for, so it must not be
    // reported as loss - the client is still getting its rate.
    const LATE: Duration = Duration::from_micros(400);
    for _ in 0..200 {
        let Some(due) = s.cx().with(|c| c.daq.next_due()) else { break };
        let now = due + LATE;
        daq::tick_at(&s.cx(), now, Some(now));
    }
    let stats = s.cx().with(|c| c.daq.stats(id).unwrap());
    assert!(stats.seq > 50, "it kept delivering: {stats:?}");
    assert_eq!(stats.missed, 0, "ordinary lateness is corrected for, not counted as a gap");
    // The requested period is never rewritten; only the schedule derived from it moves.
    let (period, eff) = s.cx().with(|c| {
        let p = &c.daq.get(id).unwrap().points[0];
        (p.period, p.eff_us())
    });
    assert_eq!(period, Duration::from_micros(1000), "the client's own period is reported back unchanged");
    // A constant offset needs no correction - the interval between ticks *is* the scheduled
    // period - so the schedule settles back at the request. It may never exceed it.
    assert!(eff <= 1000, "the schedule never runs slower than the client asked for: {eff} us");
    let _ = drain(&rx);

    // A stall of many periods is something else, and it is counted.
    let at = s.cx().with(|c| c.daq.next_due().unwrap()) + Duration::from_millis(100);
    daq::tick_at(&s.cx(), at, Some(at));
    let msgs = drain(&rx);
    let gap = msgs.iter().find_map(|m| match m {
        DaqMsg::Gap { missed, .. } => Some(*missed),
        _ => None,
    });
    assert!(gap.is_some_and(|g| g >= 90), "a stall is reported as a gap: {msgs:?}");
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

/// Batches executed since the last call: one per probe round trip, which is what merging saves.
fn take_execs(s: &Server) -> usize {
    with_sim(s, |sim| std::mem::take(&mut sim.execs))
}

/// A server with the real board cfg, so `poll_all` has the 20 targets it polls on hardware and
/// its batch is not empty. The plain `server()` has none, which would make "one batch" trivial.
fn board_server() -> Server {
    aurix_core::log::set_debug_level(0);
    let repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap();
    let path = |p: std::path::PathBuf| p.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/");
    let s = Server::new();
    s.cx().with(|c| {
        c.adapter_override = Some("sim".into());
        c.search_dirs.push(std::path::PathBuf::from(openocd_tcl::TCL_DIR));
    });
    s.interp.eval_global("script board/infineon/kit_a2g_tc377tx_3v3_trb.cfg").unwrap();
    s.interp.eval_global(&format!("script {}", jim::quote(&path(repo.join("tc37x_ocds_init.cfg"))))).unwrap();
    s.interp.eval_global("set tc3xx_hsm_ready_ms 0").unwrap();
    s.interp.eval_global("init").unwrap();
    s
}

/// Makes a point due right now, without disturbing its error counters.
fn make_due(s: &Server, id: SessionId, point: u32, interval_us: u32) {
    s.cx().with(|c| c.daq.set_interval(id, point, interval_us, Instant::now()).unwrap());
}

#[test]
fn a_due_point_rides_in_the_run_state_poll_instead_of_its_own_batch() {
    let s = board_server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 256, t0);
    // 50 ms, so "due" and "not due" are decided by the test and not by how long a poll took.
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 50_000)], t0);
    set_mem(&s, A, 0xabcd_1234);
    // Let the first polls examine the targets, so the counting below sees a steady state.
    for _ in 0..3 {
        ops::poll_all(&s.cx());
    }
    let _ = drain(&rx);
    assert!(s.cx().with(|c| c.targets.len()) >= 3, "the cfg must have created targets to poll");

    make_due(&s, id, 1, 50_000);
    take_execs(&s);
    take_reads(&s);
    ops::poll_all(&s.cx());
    let execs = take_execs(&s);
    assert!(take_reads(&s).contains(&(A, 4)), "the DAQ read went into the run-state poll's batch");
    assert_eq!(samples(&rx), vec![(1, 0xabcd_1234)], "and the sample came back out of it");

    // The point cost the poll nothing: without the merge this poll plus the sampler's own tick
    // would have been two round trips.
    make_due(&s, id, 1, 50_000);
    take_execs(&s);
    ops::poll_all(&s.cx());
    assert_eq!(take_execs(&s), execs, "a due point adds no round trip to the poll that carries it");

    // Nothing due: the point is never pulled forward just because a poll happens to run.
    let _ = drain(&rx);
    take_reads(&s);
    ops::poll_all(&s.cx());
    assert!(!take_reads(&s).contains(&(A, 4)), "a point that is not due yet is left alone");
    assert!(samples(&rx).is_empty());
}

#[test]
fn the_sampler_still_runs_when_the_run_state_poll_is_off() {
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = open(&s, 256, t0);
    add(&s, id, &[PointCfg::new(A, 4, Space::Host, 50_000)], t0);
    set_mem(&s, A, 0x0f0f_0f0f);
    ops::poll_all(&s.cx());
    let _ = drain(&rx);

    // `poll_all` returns before it builds a batch, so the merge cannot fire and the standalone
    // tick has to carry the session on its own.
    s.cx().with(|c| c.poll_enabled = false);
    make_due(&s, id, 1, 50_000);
    take_execs(&s);
    ops::poll_all(&s.cx());
    assert_eq!(take_execs(&s), 0, "polling is off, so nothing was executed");
    assert!(samples(&rx).is_empty());

    let now = Instant::now();
    daq::tick_at(&s.cx(), now, Some(now));
    assert_eq!(take_execs(&s), 1, "the sampler ran its own batch");
    assert_eq!(samples(&rx), vec![(1, 0x0f0f_0f0f)]);
}
