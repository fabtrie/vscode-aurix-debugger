//! `reset_settle <max_ms> <quiet_ms>`: lets the device run until no reset came for `quiet_ms`
//! (e.g. until an application has disarmed an external watchdog), for up to `max_ms`.
//!
//! STM0 is read as one 64-bit value every 2 ms through the device watch, which answers every reset
//! on the way at the access that reports it (OCDS, TL1 routing, peripheral suspend: see
//! [`crate::restore`]), early enough for an application that checks for the debugger at
//! start-up. A reset is one the watch counted, STM0 counting from a lower value, or reads that
//! failed for a while and then find OCDS off. A single failed read is a probe's noise, not a reset.

use std::time::{Duration, Instant};

use tas::Batch;

use crate::arch::{OSTATE, OSTATE_OEN};
use crate::ctx::Cx;

const STM0_TIM0: u32 = 0xF000_1010;
const STM0_CAP: u32 = 0xF000_102C;

/// STM0 as one 64-bit value (reading TIM0 latches bits 63:32 into CAP); `None` if unreadable.
fn stm64(cx: &Cx) -> Option<u64> {
    cx.with(|c| {
        let mut io = c.io().ok()?;
        let mut b = Batch::new();
        b.read32(STM0_TIM0 as u64);
        b.read32(STM0_CAP as u64);
        let r = io.batch(&b).ok()?;
        (r[0].ok() && r[1].ok()).then(|| ((r[1].u32() as u64) << 32) | r[0].u32() as u64)
    })
}

fn watch_count(cx: &Cx) -> Option<u64> {
    cx.with(|c| c.watch().filter(|w| w.is_watching()).map(|w| w.info().count))
}

fn ocds_off(cx: &Cx) -> bool {
    cx.with(|c| c.io().ok().and_then(|mut io| io.read32(OSTATE).ok()).is_none_or(|v| v & OSTATE_OEN == 0))
}

pub struct Settled {
    pub settled: bool,
    pub resets: u32,
    pub elapsed_ms: u64,
    /// How long the device has not answered at the end.
    pub down_ms: u64,
}

pub fn settle(cx: &Cx, max_ms: u64, quiet_ms: u64) -> Settled {
    let t0 = Instant::now();
    let mut calm = t0;
    let mut prev: Option<u64> = None;
    let mut seen = watch_count(cx);
    let mut resets = 0u32;
    let mut down_since: Option<Instant> = None;
    let mut fails = 0;
    loop {
        let now = Instant::now();
        match stm64(cx) {
            None => {
                fails += 1;
                if fails >= 2 && down_since.is_none() {
                    down_since = Some(now);
                }
            }
            Some(s) => {
                fails = 0;
                let count = watch_count(cx);
                // Counted by the watch (it has restored the set-up already), or STM0 restarted.
                let mut reset = count.is_some_and(|n| seen.is_some_and(|m| n != m)) || prev.is_some_and(|p| s < p);
                seen = count;
                // Back after failed reads: a reset only if it cleared OCDS (and the watch did not
                // switch it on again already: then it counted it).
                if down_since.take().is_some() && !reset && ocds_off(cx) {
                    reset = true;
                }
                if reset {
                    // A reset the wait started in (no STM0 value before it) is not counted.
                    if prev.is_some() {
                        resets += 1;
                    }
                    calm = now;
                    prev = stm64(cx).or(Some(s));
                } else {
                    prev = Some(s);
                }
            }
        }
        let elapsed = now.duration_since(t0);
        let down_ms = down_since.map_or(0, |d| now.duration_since(d).as_millis() as u64);
        if down_since.is_none() && now.duration_since(calm) >= Duration::from_millis(quiet_ms) {
            return Settled { settled: true, resets, elapsed_ms: elapsed.as_millis() as u64, down_ms };
        }
        if elapsed >= Duration::from_millis(max_ms) {
            return Settled { settled: false, resets, elapsed_ms: elapsed.as_millis() as u64, down_ms };
        }
        crate::commands::wait_watching(cx, Duration::from_millis(2));
    }
}

/// `reset_settle <max_ms> <quiet_ms>`: dict `settled resets elapsed down`.
pub fn cmd_reset_settle(cx: &Cx, a: &[&str]) -> Result<String, String> {
    let usage = "usage: reset_settle <max_ms> <quiet_ms>";
    let max_ms = crate::util::parse_u64(a.get(1).ok_or(usage)?)?;
    let quiet_ms = crate::util::parse_u64(a.get(2).ok_or(usage)?)?;
    if !cx.with(|c| c.initialized) {
        return Err("reset_settle: run 'init' first".into());
    }
    let r = settle(cx, max_ms, quiet_ms);
    Ok(format!("settled {} resets {} elapsed {} down {}", r.settled as u8, r.resets, r.elapsed_ms, r.down_ms))
}
