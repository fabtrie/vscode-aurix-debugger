//! Live sets: the regions a live view shows (a Live Watch instance), read at the view's interval.
//!
//! **The server decides where each value comes from**; the client sends the same commands
//! whatever probe is connected:
//!
//! * **`daq`**: the probe has a sampler of its own ([`aurix_core::adapter::ProbeDaq`], the
//!   BlueBox), the region is plain bus memory and small enough
//!   ([`daq::MAX_PROBE_POINT_LEN`]). It becomes a `probe_only` DAQ point at the set's period; the
//!   probe checks it and sends its changes, and a collector thread keeps the latest value.
//! * **`read`**: everything else - every region on a TAS probe, winIDEA or the simulator, and on
//!   the BlueBox core-relative regions, large ones and those its probe refused. Read when the
//!   values are asked for, all in one batch ([`memory_get_many`], the code a group of
//!   `Memory.get`s runs): the same results as the client's `Memory.get`s, in one transaction.
//!
//! * **`trace`** (a set in trace mode): every bus region becomes a traced DAQ point: the miniMCDS
//!   data trace records every write. The value is the one after the last write, with the count
//!   of writes and the target time of the last one. A region that cannot be traced (outside the
//!   one traced range, another client tracing, no MCDS) is read at the set's period and carries
//!   the reason.
//!
//! A `daq` region without a value yet (just configured, or its probe failed and it waits to be
//! configured again) is answered with a `read` too, so the client always gets a value.
//!
//! Commands (`AurixTarget` service): `setLiveSet(key, [{ctx, addr, size}], periodUs, trace?)`,
//! `getLiveValues(key)` -> `[{data, source, tUs?, writes?, tTargetNs?, lost?, reason?} | {error}]`
//! in the set's order, `clearLiveSet(key)`. Sets belong to the connection and end with it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};

use aurix_core::adapter::PartialRead;
use aurix_core::daq::{self, DaqMsg, PointCfg, PointId, SampleMode, SessionCfg, SessionId, Source, Space};
use aurix_core::ops;

use crate::contexts::find_core;
use crate::Shared;

/// One region of a set, as the client sent it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Region {
    pub ctx: String,
    pub addr: u32,
    pub size: usize,
}

/// What [`memory_get`] read: the two shapes `Memory.get` replies come from.
pub enum MemGet {
    /// A read that may fail for parts of the range (core-relative, word-only memory).
    Partial(Result<PartialRead, String>),
    /// One bus read.
    Op(Result<tas::OpResult, String>),
}

/// The region as `Memory.get` routes it: read through the core `ctx` names when that core owns
/// the address, word by word where only words can be read, otherwise as one bus read.
enum Route {
    Own(usize),
    Words,
    Bus,
}

fn route(sh: &Shared, ctx: &str, addr: u32, size: usize) -> Route {
    let idx = find_core(&sh.handle, ctx).map(|c| c.idx);
    if let Some(t) = sh.handle.call(move |cx| ops::own_access_target(cx, idx, addr, size)).flatten() {
        Route::Own(t)
    } else if aurix_core::cortex_m::word_only(addr, size) {
        Route::Words
    } else {
        Route::Bus
    }
}

/// `Memory.get`'s read of `size` bytes at `addr` for context `ctx`.
pub fn memory_get(sh: &Shared, ctx: &str, addr: u32, size: usize) -> MemGet {
    read_routed(sh, route(sh, ctx, addr, size), addr, size)
}

/// [`memory_get`] of several regions `(ctx, addr, size)`, one result each in order, in one call
/// to the core thread: the bus reads share one batch ([`aurix_core::adapter::Io::read_ops`]: one
/// exchange of packets on TAS, shared messages on the BlueBox) instead of a round trip each. The
/// results are the ones the single reads would give.
pub fn memory_get_many(sh: &Shared, regions: &[(String, u32, usize)]) -> Vec<MemGet> {
    // The cores the contexts name, before the core thread call (`find_core` calls it itself).
    let mut cores: HashMap<&str, Option<usize>> = HashMap::new();
    let reqs: Vec<(Option<usize>, u32, usize)> = regions
        .iter()
        .map(|(ctx, addr, size)| (*cores.entry(ctx.as_str()).or_insert_with(|| find_core(&sh.handle, ctx).map(|c| c.idx)), *addr, *size))
        .collect();
    let n = reqs.len();
    sh.handle
        .call(move |cx| read_many_on_core(cx, &reqs))
        .unwrap_or_else(|| (0..n).map(|_| MemGet::Op(Err("server stopped".to_string()))).collect())
}

fn read_many_on_core(cx: &aurix_core::Cx, reqs: &[(Option<usize>, u32, usize)]) -> Vec<MemGet> {
    let routes: Vec<Route> = reqs
        .iter()
        .map(|&(idx, addr, size)| match ops::own_access_target(cx, idx, addr, size) {
            Some(t) => Route::Own(t),
            None if aurix_core::cortex_m::word_only(addr, size) => Route::Words,
            None => Route::Bus,
        })
        .collect();
    // The reads that go into the batch: (request, host address, as a core-relative read). A
    // TriCore's local scratchpad alias (stack variables linked to 0xD000_0000) is its global
    // address on the bus, read as `Core::read_memory` reads it.
    let tricore = |t: usize| cx.with(|c| match &c.targets[t].core {
        aurix_core::target::Core::Tricore(tc) => Some(tc.coreid),
        _ => None,
    });
    let mut batched: Vec<(usize, u32, bool)> = Vec::new();
    for (k, r) in routes.iter().enumerate() {
        let (_, addr, size) = reqs[k];
        match *r {
            Route::Bus => batched.push((k, addr, false)),
            Route::Own(t) if size > 0 => {
                if let Some(coreid) = tricore(t) {
                    batched.push((k, aurix_core::tricore::global_address(coreid, addr), true));
                }
            }
            _ => {}
        }
    }
    let mut out: Vec<Option<MemGet>> = (0..reqs.len()).map(|_| None).collect();
    if !batched.is_empty() {
        let ranges: Vec<(u32, usize)> = batched.iter().map(|&(k, a, _)| (a, reqs[k].2)).collect();
        match cx.with(|c| -> Result<Vec<tas::OpResult>, String> { c.io().map_err(|e| e.to_string())?.read_ops(&ranges).map_err(|e| e.to_string()) }) {
            Ok(rs) => {
                for (&(k, a, partial), r) in batched.iter().zip(rs) {
                    out[k] = Some(if partial {
                        // As `Io::read` of the global address: the whole range or its error.
                        MemGet::Partial(aurix_core::adapter::check(&r, a as u64, false).map(|_| PartialRead { data: r.data, unreadable: Vec::new() }).map_err(|e| e.to_string()))
                    } else {
                        MemGet::Op(Ok(r))
                    });
                }
            }
            Err(e) => {
                for &(k, _, partial) in &batched {
                    out[k] = Some(if partial { MemGet::Partial(Err(e.clone())) } else { MemGet::Op(Err(e.clone())) });
                }
            }
        }
    }
    for (k, r) in routes.into_iter().enumerate() {
        if out[k].is_some() {
            continue;
        }
        let (_, addr, size) = reqs[k];
        match r {
            Route::Own(t) => out[k] = Some(MemGet::Partial(ops::read_memory_partial_as(cx, t, addr, size).map_err(|e| e.to_string()))),
            Route::Words => out[k] = Some(MemGet::Partial(ops::read_memory_partial(cx, addr, size).map_err(|e| e.to_string()))),
            Route::Bus => {}
        }
    }
    out.into_iter().map(|m| m.unwrap_or_else(|| MemGet::Op(Err("not read".to_string())))).collect()
}

fn read_routed(sh: &Shared, r: Route, addr: u32, size: usize) -> MemGet {
    fn stopped<T>() -> Result<T, String> {
        Err("server stopped".to_string())
    }
    match r {
        Route::Own(t) => MemGet::Partial(sh.handle.call(move |cx| ops::read_memory_partial_as(cx, t, addr, size).map_err(|e| e.to_string())).unwrap_or_else(stopped)),
        Route::Words => MemGet::Partial(sh.handle.call(move |cx| ops::read_memory_partial(cx, addr, size).map_err(|e| e.to_string())).unwrap_or_else(stopped)),
        Route::Bus => MemGet::Op(
            sh.handle
                .call(move |cx| cx.with(|c| -> Result<tas::OpResult, String> { c.io().map_err(|e| e.to_string())?.read_op(addr, size).map_err(|e| e.to_string()) }))
                .unwrap_or_else(stopped),
        ),
    }
}

/// A region's value as `getLiveValues` reports it: the bytes, or the first error a `Memory.get`
/// of it would have reported.
fn read_value(m: MemGet, size: usize) -> Value {
    match m {
        MemGet::Partial(Ok(r)) if r.unreadable.is_empty() => json!({ "data": B64.encode(&r.data), "source": "read" }),
        MemGet::Partial(Ok(r)) => json!({ "data": B64.encode(&r.data), "source": "read", "error": r.unreadable[0].2 }),
        MemGet::Partial(Err(e)) | MemGet::Op(Err(e)) => json!({ "error": e, "source": "read" }),
        MemGet::Op(Ok(r)) if r.ok() => json!({ "data": B64.encode(&r.data), "source": "read" }),
        MemGet::Op(Ok(r)) => {
            let mut d = r.data;
            d.resize(size, 0);
            json!({ "data": B64.encode(&d), "source": "read", "error": tas::pl_err_name(r.err) })
        }
    }
}

/// What the collector thread keeps per point.
#[derive(Clone, Debug, Default)]
struct Val {
    bytes: Vec<u8>,
    /// Session time of the last delivery.
    t_us: u64,
    /// Writes recorded by the data trace.
    writes: u64,
    /// Target time of the last traced write (ns since the trace started).
    t_target_ns: Option<u64>,
}

/// Latest value per point, the trace refusals and the trace losses, kept by the collector.
#[derive(Default)]
struct LiveData {
    vals: HashMap<PointId, Val>,
    reasons: HashMap<PointId, String>,
    lost: u64,
}

type Latest = Arc<Mutex<LiveData>>;

struct Entry {
    region: Region,
    /// The DAQ point serving it, while the probe may take it.
    point: Option<PointId>,
}

/// One live set.
pub struct LiveSet {
    period_us: u32,
    /// Every write of the regions, by the data trace.
    trace: bool,
    entries: Vec<Entry>,
    /// The DAQ session of the `daq` regions, opened with the first one.
    session: Option<SessionId>,
    latest: Latest,
    stop: Arc<AtomicBool>,
}

impl LiveSet {
    fn close(&mut self, sh: &Shared) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(id) = self.session.take() {
            sh.handle.call(move |cx| cx.with(|c| c.daq.close(id)));
        }
    }
}

/// Live sets by owner (the TCF connection) and key.
#[derive(Default)]
pub struct LiveSets(HashMap<(u64, String), LiveSet>);

/// Whether the probe samples by itself.
fn probe_samples(sh: &Shared) -> bool {
    sh.handle.call(|cx| cx.with(|c| c.adapter.as_deref_mut().is_some_and(|a| a.initialized() && a.probe_daq().is_some()))).unwrap_or(false)
}

/// Keeps the latest value of each point until the set closes.
fn collect(rx: Receiver<DaqMsg>, latest: Latest, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(DaqMsg::Samples { t_us, values, .. }) => {
                let mut l = latest.lock().unwrap();
                for (id, bytes) in values {
                    let v = l.vals.entry(id).or_default();
                    v.bytes = bytes;
                    v.t_us = t_us;
                }
            }
            Ok(DaqMsg::Trace { t_us, values }) => {
                let mut l = latest.lock().unwrap();
                for (id, t, bytes) in values {
                    let v = l.vals.entry(id).or_default();
                    v.bytes = bytes;
                    v.t_us = t_us;
                    v.writes += 1;
                    v.t_target_ns = t.or(v.t_target_ns);
                }
            }
            Ok(DaqMsg::TraceLost { messages, laps, .. }) => latest.lock().unwrap().lost += messages + laps,
            Ok(DaqMsg::SourceChanged { point, reason, .. }) => {
                let mut l = latest.lock().unwrap();
                match reason {
                    Some(r) => l.reasons.insert(point, r),
                    None => l.reasons.remove(&point),
                };
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Replaces the set `key` of `owner`: its regions and its period.
pub fn set(sh: &Shared, owner: u64, key: &str, regions: Vec<Region>, period_us: u32, trace: bool) -> Result<(), String> {
    let period_us = daq::quantize(period_us);
    let probe = !trace && probe_samples(sh);
    // Which regions get a DAQ point (the probe's sampler, or the data trace); routing asks the
    // core thread, so outside the lock.
    let wants: Vec<bool> = regions
        .iter()
        .map(|r| {
            let bus = r.size > 0 && matches!(route(sh, &r.ctx, r.addr, r.size), Route::Bus);
            if trace {
                bus && r.size as u32 <= daq::MAX_POINT_LEN
            } else {
                probe && bus && r.size as u32 <= daq::MAX_PROBE_POINT_LEN
            }
        })
        .collect();

    let mut sets = sh.live.lock().unwrap();
    let k = (owner, key.to_string());
    let mut old = sets.0.remove(&k);
    if old.as_ref().is_some_and(|o| o.trace != trace) {
        // Switching between trace and sampling: start over with new points.
        if let Some(mut o) = old.take() {
            o.close(sh);
        }
    }
    let mut kept: HashMap<Region, PointId> = HashMap::new();
    if let Some(o) = &mut old {
        for e in o.entries.drain(..) {
            if let Some(p) = e.point {
                kept.insert(e.region, p);
            }
        }
    }
    let mut ls = old.unwrap_or_else(|| LiveSet {
        period_us,
        trace,
        entries: Vec::new(),
        session: None,
        latest: Arc::new(Mutex::new(LiveData::default())),
        stop: Arc::new(AtomicBool::new(false)),
    });
    let retime = ls.period_us != period_us;
    ls.period_us = period_us;

    let mut add: Vec<(usize, Region)> = Vec::new();
    for (i, (r, want)) in regions.into_iter().zip(wants).enumerate() {
        let point = if want { kept.remove(&r) } else { None };
        if want && point.is_none() {
            add.push((i, r.clone()));
        }
        ls.entries.push(Entry { region: r, point });
    }
    let gone: Vec<PointId> = kept.into_values().collect();

    if ls.session.is_none() && !add.is_empty() {
        let (id, rx) = sh
            .handle
            .call(|cx| cx.with(|c| c.daq.open(SessionCfg::default(), Instant::now())))
            .ok_or("the debug server stopped")?
            .map_err(|e| e.to_string())?;
        let (latest, stop) = (ls.latest.clone(), ls.stop.clone());
        thread::Builder::new().name(format!("live-{id}")).spawn(move || collect(rx, latest, stop)).map_err(|e| e.to_string())?;
        ls.session = Some(id);
    }
    if let Some(id) = ls.session {
        let cfgs: Vec<PointCfg> = add
            .iter()
            .map(|(_, r)| PointCfg { mode: SampleMode::OnChange, probe_only: !trace, trace, ..PointCfg::new(r.addr, r.size as u32, Space::Host, period_us) })
            .collect();
        let kept_ids: Vec<PointId> = ls.entries.iter().filter_map(|e| e.point).collect();
        let latest = ls.latest.clone();
        let added = sh
            .handle
            .call(move |cx| {
                cx.with(|c| -> aurix_core::error::Result<Vec<PointId>> {
                    if !gone.is_empty() {
                        c.daq.remove_points(id, &gone)?;
                        let mut l = latest.lock().unwrap();
                        for p in &gone {
                            l.vals.remove(p);
                            l.reasons.remove(p);
                        }
                    }
                    if retime {
                        let now = Instant::now();
                        for &p in &kept_ids {
                            c.daq.set_interval(id, p, period_us, now)?;
                        }
                    }
                    Ok(if cfgs.is_empty() { Vec::new() } else { c.daq.add_points(id, &cfgs, Instant::now())?.into_iter().map(|(p, _)| p).collect() })
                })
            })
            .ok_or("the debug server stopped")?
            .map_err(|e| e.to_string())?;
        for ((i, _), p) in add.iter().zip(added) {
            ls.entries[*i].point = Some(p);
        }
    }
    if ls.entries.iter().all(|e| e.point.is_none()) {
        ls.close(sh);
        ls.stop = Arc::new(AtomicBool::new(false));
        ls.latest = Arc::new(Mutex::new(LiveData::default()));
    }
    sets.0.insert(k, ls);
    Ok(())
}

/// The values of set `key` of `owner`, in its order.
pub fn values(sh: &Shared, owner: u64, key: &str) -> Result<Value, String> {
    let k = (owner, key.to_string());
    // What to read and what the probe has, decided under the lock; the reads run after it.
    let (session, plan): (Option<SessionId>, Vec<(Region, Option<PointId>)>) = {
        let sets = sh.live.lock().unwrap();
        let ls = sets.0.get(&k).ok_or_else(|| format!("no live set {key}"))?;
        (ls.session, ls.entries.iter().map(|e| (e.region.clone(), e.point)).collect())
    };
    let pids: Vec<PointId> = plan.iter().filter_map(|(_, p)| *p).collect();
    let sources: HashMap<PointId, Source> = match session {
        Some(id) if !pids.is_empty() => sh
            .handle
            .call(move |cx| cx.with(|c| pids.iter().filter_map(|&p| c.daq.source(id, p).map(|s| (p, s))).collect()))
            .unwrap_or_default(),
        _ => HashMap::new(),
    };
    let (latest, reasons, lost): (HashMap<PointId, Val>, HashMap<PointId, String>, u64) = {
        let sets = sh.live.lock().unwrap();
        sets.0
            .get(&k)
            .map(|ls| {
                let l = ls.latest.lock().unwrap();
                (l.vals.clone(), l.reasons.clone(), l.lost)
            })
            .unwrap_or_default()
    };
    let mut refused: Vec<PointId> = Vec::new();
    let mut out: Vec<Value> = Vec::with_capacity(plan.len());
    // The regions read now (slot in `out`, region, why it is not traced), all in one batch.
    let mut reads: Vec<(usize, Region, Option<String>)> = Vec::new();
    for (r, point) in plan {
        if let Some(p) = point {
            match sources.get(&p) {
                Some(Source::Probe) => {
                    if let Some(v) = latest.get(&p) {
                        out.push(json!({ "data": B64.encode(&v.bytes), "source": "daq", "tUs": v.t_us }));
                        continue;
                    }
                }
                Some(Source::Trace) => {
                    if let Some(v) = latest.get(&p) {
                        out.push(json!({
                            "data": B64.encode(&v.bytes), "source": "trace", "tUs": v.t_us,
                            "writes": v.writes, "tTargetNs": v.t_target_ns, "lost": lost,
                        }));
                        continue;
                    }
                }
                Some(Source::Polled) if reasons.contains_key(&p) => {
                    // Asked to be traced, read instead: the reason goes with the value.
                    reads.push((out.len(), r, Some(reasons[&p].clone())));
                    out.push(Value::Null);
                    continue;
                }
                Some(Source::Refused) | None => refused.push(p),
                _ => {}
            }
        }
        reads.push((out.len(), r, None));
        out.push(Value::Null);
    }
    if !reads.is_empty() {
        let regions: Vec<(String, u32, usize)> = reads.iter().map(|(_, r, _)| (r.ctx.clone(), r.addr, r.size)).collect();
        for ((slot, r, reason), m) in reads.into_iter().zip(memory_get_many(sh, &regions)) {
            let mut v = read_value(m, r.size);
            if let (Some(reason), Value::Object(m)) = (reason, &mut v) {
                m.insert("reason".into(), json!(reason));
            }
            out[slot] = v;
        }
    }
    if !refused.is_empty() {
        // The probe will not take these: from now on they are plain reads.
        let mut sets = sh.live.lock().unwrap();
        if let Some(ls) = sets.0.get_mut(&k) {
            for e in &mut ls.entries {
                if e.point.is_some_and(|p| refused.contains(&p)) {
                    e.point = None;
                }
            }
            if let Some(id) = ls.session {
                sh.handle.call(move |cx| cx.with(|c| c.daq.remove_points(id, &refused).ok()));
            }
        }
    }
    Ok(Value::Array(out))
}

/// Ends set `key` of `owner`; ending one that does not exist is not an error (a view closing
/// says so blindly).
pub fn clear(sh: &Shared, owner: u64, key: &str) {
    let removed = sh.live.lock().unwrap().0.remove(&(owner, key.to_string()));
    if let Some(mut ls) = removed {
        ls.close(sh);
    }
}

/// Ends every set of `owner` (its connection closed).
pub fn clear_owner(sh: &Shared, owner: u64) {
    let removed: Vec<LiveSet> = {
        let mut sets = sh.live.lock().unwrap();
        let keys: Vec<(u64, String)> = sets.0.keys().filter(|(o, _)| *o == owner).cloned().collect();
        keys.into_iter().filter_map(|k| sets.0.remove(&k)).collect()
    };
    for mut ls in removed {
        ls.close(sh);
    }
}

/// Parses `setLiveSet`'s region list.
pub fn parse_regions(v: &Value) -> Result<Vec<Region>, String> {
    let arr = v.as_array().ok_or("regions must be an array")?;
    arr.iter()
        .map(|r| {
            let ctx = r.get("ctx").and_then(Value::as_str).ok_or("region without ctx")?.to_string();
            let addr = r.get("addr").and_then(Value::as_u64).ok_or("region without addr")?;
            let size = r.get("size").and_then(Value::as_u64).ok_or("region without size")?;
            if addr > u32::MAX as u64 || size == 0 || size > 0x10000 {
                return Err(format!("bad region 0x{addr:x} ({size} B)"));
            }
            Ok(Region { ctx, addr: addr as u32, size: size as usize })
        })
        .collect()
}
