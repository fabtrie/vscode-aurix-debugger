//! Peripheral registers by name: the device's SVD description (`crates/svd`), loaded once per
//! session, and its registers read and written through the same memory paths as `read_memory`.
//!
//! The description is a bundled one (the SVD files the extension ships, `svd-index.json`) or a
//! file of the client's. Without `load_svd`, the first peripheral operation loads the bundled
//! description of the chip the target configuration names (`tc37x.cpu0` -> `TC37x`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::Value as Json;

use api_spec::types::*;
use api_spec::{ApiError, ErrorCode};
use aurix_core::events::emit;
use aurix_core::{ops, Cx, Event};
use svd::{Access, ModifiedWrite, Node, ReadAction, RegisterInfo};

use crate::core_impl::CoreApi;

/// A loaded description.
pub struct Loaded {
    /// The bundled device (absent for a file of the client's).
    pub device: Option<String>,
    pub file: PathBuf,
    pub extra: Vec<PathBuf>,
    pub source: &'static str,
    pub seconds: f64,
    pub model: svd::Device,
}

impl Loaded {
    fn name(&self) -> String {
        self.device.clone().unwrap_or_else(|| self.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
    }
}

/// The session's description, once loaded. Held while loading, so a second request waits for
/// the first one's conversion instead of doing its own.
pub type Slot = Mutex<Option<Arc<Loaded>>>;

const DEFAULT_LIMIT: u32 = 200;
const MAX_LIMIT: u32 = 2000;
/// Longest run of adjacent registers read in one access.
const MAX_RUN: usize = 4096;

fn catalog() -> svd::Catalog {
    svd::Catalog::bundled()
}

fn svd_err(e: svd::Error) -> ApiError {
    match e {
        svd::Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound => ApiError::not_found(e.to_string()),
        svd::Error::Io(e) => ApiError::failed(e.to_string()),
        e => ApiError::bad_request(e.to_string()),
    }
}

/// The OpenOCD chip name of the target configuration: the prefix of the target names.
fn chip(api: &CoreApi) -> Option<String> {
    let first = api.call(|cx| Ok(cx.with(|c| c.targets.first().map(|t| t.name.clone())))).ok().flatten()?;
    first.split_once('.').map(|(chip, _)| chip.to_string())
}

fn unknown_device(device: &str, cat: &svd::Catalog) -> ApiError {
    let have: Vec<&str> = cat.entries.iter().map(|e| e.device.as_str()).collect();
    ApiError::not_found(format!("no bundled SVD file describes `{device}` (bundled: {}); load a file with `load_svd`", have.join(", ")))
}

fn load_entry(e: &svd::catalog::Entry, source: &'static str) -> Result<Loaded, ApiError> {
    let t0 = Instant::now();
    let model = e.load().map_err(svd_err)?;
    Ok(Loaded { device: Some(e.device.clone()), file: e.path.clone(), extra: e.extra.iter().map(|x| x.path.clone()).collect(), source, seconds: t0.elapsed().as_secs_f64(), model })
}

/// The description, loading the chip's bundled one if none is loaded.
pub fn loaded(api: &CoreApi) -> Result<Arc<Loaded>, ApiError> {
    let mut slot = api.svd.lock().unwrap();
    if let Some(l) = &*slot {
        return Ok(l.clone());
    }
    let cat = catalog();
    let chip = chip(api).ok_or_else(|| ApiError::not_found("no SVD description loaded and no target to tell the chip by: use `load_svd`"))?;
    let entry = cat.find(&chip).ok_or_else(|| unknown_device(&chip, &cat))?;
    let l = Arc::new(load_entry(entry, "chip")?);
    *slot = Some(l.clone());
    Ok(l)
}

/// Loads the bundled description of `device` unless it is the one loaded.
fn ensure_device(api: &CoreApi, device: &str) -> Result<Arc<Loaded>, ApiError> {
    let mut slot = api.svd.lock().unwrap();
    let cat = catalog();
    let entry = cat.find(device).ok_or_else(|| unknown_device(device, &cat))?;
    if let Some(l) = slot.as_ref().filter(|l| l.device.as_deref() == Some(entry.device.as_str())) {
        return Ok(l.clone());
    }
    let l = Arc::new(load_entry(entry, "load_svd")?);
    *slot = Some(l.clone());
    Ok(l)
}

fn info(api: &CoreApi, l: Option<&Loaded>) -> SvdInfo {
    let cat = catalog();
    let chip = chip(api);
    let chip_device = chip.as_deref().and_then(|c| cat.find(c)).map(|e| e.device.clone());
    let path = |p: &Path| p.to_string_lossy().into_owned();
    SvdInfo {
        loaded: l.is_some(),
        device: l.and_then(|l| l.device.clone()),
        file: l.map(|l| path(&l.file)),
        extra_files: l.map(|l| l.extra.iter().map(|p| path(p)).collect()).unwrap_or_default(),
        source: l.map(|l| l.source.to_string()),
        peripherals: l.map_or(0, |l| l.model.peripherals.len() as u32),
        registers: l.map_or(0, |l| l.model.peripherals.iter().map(|p| p.register_count() as u32).sum()),
        load_seconds: l.map(|l| l.seconds),
        chip,
        chip_device,
        available: cat.entries.iter().map(|e| e.device.clone()).collect(),
    }
}

pub fn svd_info(api: &CoreApi) -> SvdInfo {
    let l = api.svd.lock().unwrap().clone();
    info(api, l.as_deref())
}

pub fn load_svd(api: &CoreApi, req: LoadSvdRequest) -> Result<SvdInfo, ApiError> {
    let l = match (req.device.as_deref().map(str::trim).filter(|d| !d.is_empty()), req.file.as_deref().map(str::trim).filter(|f| !f.is_empty())) {
        (Some(_), Some(_)) => return Err(ApiError::bad_request("give either `device` or `file`")),
        (None, None) => return Err(ApiError::bad_request("give the `device` (a bundled SVD file) or the `file` to load")),
        (Some(d), None) => ensure_device(api, d)?,
        (None, Some(f)) => {
            let path = PathBuf::from(f);
            if !path.is_file() {
                return Err(ApiError::not_found(format!("no such file: {f}")));
            }
            let t0 = Instant::now();
            let model = svd::load(&path, &[]).map_err(svd_err)?;
            let l = Arc::new(Loaded { device: None, file: path, extra: Vec::new(), source: "load_svd", seconds: t0.elapsed().as_secs_f64(), model });
            *api.svd.lock().unwrap() = Some(l.clone());
            l
        }
    };
    Ok(info(api, Some(&l)))
}

/// The peripheral viewer's definitions of the description (of `device` when given), as JSON.
pub fn definitions(api: &CoreApi, device: Option<&str>) -> Result<Vec<u8>, ApiError> {
    let l = match device.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => ensure_device(api, d)?,
        None => loaded(api)?,
    };
    serde_json::to_vec(&svd::PeripheralOptions(&l.model.peripherals)).map_err(|e| ApiError::failed(e.to_string()))
}

// --- Describing --------------------------------------------------------------------------------

fn access(a: Access) -> RegisterAccess {
    match a {
        Access::ReadOnly => RegisterAccess::ReadOnly,
        Access::ReadWrite => RegisterAccess::ReadWrite,
        Access::WriteOnly => RegisterAccess::WriteOnly,
    }
}

fn read_action(a: ReadAction) -> String {
    match a {
        ReadAction::Clear => "clear",
        ReadAction::Set => "set",
        ReadAction::Modify => "modify",
        ReadAction::ModifyExternal => "modify_external",
    }
    .into()
}

fn modified_write(m: ModifiedWrite) -> String {
    match m {
        ModifiedWrite::OneToClear => "one_to_clear",
        ModifiedWrite::OneToSet => "one_to_set",
        ModifiedWrite::OneToToggle => "one_to_toggle",
        ModifiedWrite::ZeroToClear => "zero_to_clear",
        ModifiedWrite::ZeroToSet => "zero_to_set",
        ModifiedWrite::ZeroToToggle => "zero_to_toggle",
        ModifiedWrite::Clear => "clear",
        ModifiedWrite::Set => "set",
        ModifiedWrite::Modify => "modify",
    }
    .into()
}

fn page(offset: Option<u32>, limit: Option<u32>) -> (usize, usize) {
    (offset.unwrap_or(0) as usize, limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize)
}

fn not_found(path: &str, l: &Loaded) -> ApiError {
    ApiError::not_found(format!("`{path}` is not in the SVD description of {} (see `list_peripherals`, `describe_peripheral`)", l.name()))
}

/// The registers a path stands for, and the one field a field path names.
fn resolve<'d>(l: &'d Loaded, path: &str) -> Result<(Node<'d>, Vec<RegisterInfo<'d>>, Option<&'d svd::Field>), ApiError> {
    let node = l.model.find(path).ok_or_else(|| not_found(path, l))?;
    let regs = l.model.registers(&node);
    let field = match &node {
        Node::Field(_, f) => Some(*f),
        _ => None,
    };
    Ok((node, regs, field))
}

pub fn list_peripherals(api: &CoreApi, req: ListPeripheralsRequest) -> Result<PeripheralList, ApiError> {
    let l = loaded(api)?;
    let query = req.query.as_deref().map(str::trim).filter(|q| !q.is_empty()).unwrap_or("*");
    let matching: Vec<&svd::Peripheral> = l
        .model
        .peripherals
        .iter()
        .filter(|p| crate::variables::glob(query.as_bytes(), p.name.as_bytes()) || p.group_name.as_deref().is_some_and(|g| crate::variables::glob(query.as_bytes(), g.as_bytes())))
        .collect();
    let (offset, limit) = page(req.offset, req.limit);
    let items = matching
        .iter()
        .skip(offset)
        .take(limit)
        .map(|p| PeripheralSummary {
            name: p.name.to_string(),
            group: p.group_name.as_deref().filter(|g| !g.is_empty()).map(String::from),
            base_address: p.base_address as u32,
            length: p.total_length as u32,
            description: p.description.to_string(),
            registers: p.register_count() as u32,
        })
        .collect();
    Ok(PeripheralList { device: l.name(), total: matching.len() as u32, offset: offset as u32, items })
}

fn describe_register(r: &RegisterInfo, only: Option<&svd::Field>) -> RegisterDescription {
    let reg = r.register;
    let fields = reg
        .fields
        .iter()
        .filter(|f| only.is_none_or(|o| std::ptr::eq(o, *f)))
        .map(|f| FieldDescription {
            name: f.name.to_string(),
            bit_offset: f.offset,
            bit_width: f.width,
            access: access(f.access.unwrap_or(r.access)),
            read_action: f.read_action.map(read_action),
            modified_write: f.modified_write.map(modified_write),
            description: f.description.to_string(),
            values: f.enumeration.iter().flat_map(|e| e.values.iter()).map(|v| FieldValueName { value: v.value, name: v.name.to_string(), description: v.description.to_string() }).collect(),
        })
        .collect();
    RegisterDescription {
        path: r.path.clone(),
        address: r.address as u32,
        size: r.size,
        access: access(r.access),
        reset_value: r.reset_value,
        read_action: reg.read_action.map(read_action),
        modified_write: reg.modified_write.map(modified_write),
        description: reg.description.to_string(),
        fields,
    }
}

pub fn describe_peripheral(api: &CoreApi, req: DescribePeripheralRequest) -> Result<PeripheralDescription, ApiError> {
    let l = loaded(api)?;
    let (node, regs, field) = resolve(&l, &req.path)?;
    let (kind, path, address, description) = match &node {
        Node::Peripheral(p) => (PeripheralNodeKind::Peripheral, p.name.to_string(), p.base_address, p.description.to_string()),
        Node::Cluster(c) => (PeripheralNodeKind::Cluster, c.path.clone(), c.address, c.cluster.description.to_string()),
        Node::Register(r) => (PeripheralNodeKind::Register, r.path.clone(), r.address, r.register.description.to_string()),
        Node::Field(r, f) => (PeripheralNodeKind::Field, format!("{}.{}", r.path, f.name), r.address, f.description.to_string()),
    };
    let (offset, limit) = page(req.offset, req.limit);
    let registers = regs.iter().skip(offset).take(limit).map(|r| describe_register(r, field)).collect();
    Ok(PeripheralDescription { path, kind, address: address as u32, description, total: regs.len() as u32, offset: offset as u32, registers })
}

// --- Reading -----------------------------------------------------------------------------------

/// Why a register is not read without `force`.
fn skip_reason(r: &RegisterInfo) -> Option<String> {
    if r.access == Access::WriteOnly {
        return Some("write-only".into());
    }
    if let Some(action) = r.register.read_action.or_else(|| r.register.fields.iter().find_map(|f| f.read_action)) {
        return Some(format!("reading has a side effect (readAction {})", read_action(action)));
    }
    r.debug_view.as_ref().map(|d| format!("reading it pops or clears its content: `{d}` is its debug view, which reads without that"))
}

/// The target that serves `addr` (the HSM for its peripherals, the SCR for its SFRs), or none
/// for host memory. An address only a core reaches, with no such core in the session, is refused:
/// on the system bus it would be another address (the SCR's map is segment 0 there).
fn route(cx: &Cx, addr: u32, len: usize) -> Result<Option<usize>, String> {
    let t = ops::own_access_target(cx, None, addr, len);
    if t.is_none() {
        if aurix_core::xc800::Xc800::sfr_window(addr) || addr < 0x8_0000 {
            return Err(format!("0x{addr:08x} is an SCR address: the session has no SCR target to read it through"));
        }
        if aurix_core::cortex_m::hsm_peripheral(addr) {
            return Err(format!("0x{addr:08x} is an HSM address: the session has no HSM target to read it through"));
        }
    }
    Ok(t)
}

/// Reads memory through the path that serves it (the HSM's peripherals, the SCR's SFRs).
fn read_routed(cx: &Cx, addr: u32, len: usize) -> Result<Vec<u8>, String> {
    match route(cx, addr, len)? {
        Some(t) => ops::read_memory_as(cx, t, addr, len),
        None => ops::read_memory(cx, addr, len),
    }
    .map_err(|e| e.to_string())
}

fn write_routed(cx: &Cx, addr: u32, data: &[u8]) -> Result<Option<usize>, String> {
    let t = route(cx, addr, data.len())?;
    match t {
        Some(t) => ops::write_memory_as(cx, t, addr, data),
        None => ops::write_memory(cx, addr, data),
    }
    .map_err(|e| e.to_string())?;
    Ok(t)
}

fn mask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

fn value_of(bytes: &[u8], size: u32) -> u64 {
    let mut b = [0u8; 8];
    let n = bytes.len().min(8);
    b[..n].copy_from_slice(&bytes[..n]);
    u64::from_le_bytes(b) & mask(size)
}

fn decode(r: &RegisterInfo, value: u64, only: Option<&svd::Field>) -> Vec<FieldReading> {
    r.register
        .fields
        .iter()
        .filter(|f| only.is_none_or(|o| std::ptr::eq(o, *f)))
        .map(|f| {
            let v = (value >> f.offset.min(63)) & mask(f.width);
            let value_name = f.enumeration.as_ref().and_then(|e| e.values.iter().find(|x| x.value == v)).map(|x| x.name.to_string());
            FieldReading { name: f.name.to_string(), value: v, value_name }
        })
        .collect()
}

fn reading(r: &RegisterInfo, result: Result<u64, String>, only: Option<&svd::Field>) -> RegisterReading {
    let mut out = RegisterReading { path: r.path.clone(), address: r.address as u32, size: r.size, value: None, skipped: None, error: None, fields: Vec::new() };
    match result {
        Ok(v) => {
            out.value = Some(v);
            out.fields = decode(r, v, only);
        }
        Err(e) => out.error = Some(e),
    }
    out
}

/// Host memory reads of several ranges in one batch, each with its own outcome
/// ([`aurix_core::adapter::Io::read_ops`]).
fn bus_reads(cx: &Cx, ranges: &[(u32, usize)]) -> Result<Vec<Result<Vec<u8>, String>>, String> {
    let rs = cx.with(|c| c.io().map_err(|e| e.to_string())?.read_ops(ranges).map_err(|e| e.to_string()))?;
    Ok(rs
        .into_iter()
        .zip(ranges)
        .map(|(r, &(a, _))| aurix_core::adapter::check(&r, a as u64, false).map(|_| r.data).map_err(|e| e.to_string()))
        .collect())
}

/// Reads registers (address, bytes), adjacent ones in one access; the accesses to host memory
/// all in one batch. A failed access is retried register by register (those in host memory in
/// one batch again), so one unreadable register does not fail its neighbours.
fn read_registers(api: &CoreApi, regs: Vec<(u32, usize)>) -> Result<Vec<Result<Vec<u8>, String>>, ApiError> {
    api.call(move |cx| {
        // Runs of adjacent registers, as index ranges into `regs`.
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut i = 0;
        while i < regs.len() {
            let (start, mut end) = (regs[i].0, regs[i].0 as usize + regs[i].1);
            let mut j = i + 1;
            while j < regs.len() && regs[j].0 as usize == end && end + regs[j].1 - start as usize <= MAX_RUN {
                end += regs[j].1;
                j += 1;
            }
            runs.push((i, j));
            i = j;
        }
        let span = |(i, j): (usize, usize)| (regs[i].0, (regs[j - 1].0 + regs[j - 1].1 as u32 - regs[i].0) as usize);
        let mut out: Vec<Option<Result<Vec<u8>, String>>> = vec![None; regs.len()];
        // Registers whose run failed, to read one by one.
        let mut retry_bus: Vec<usize> = Vec::new();
        let mut bus_runs: Vec<(usize, usize)> = Vec::new();
        for &run in &runs {
            let (start, len) = span(run);
            match route(cx, start, len) {
                Ok(None) => bus_runs.push(run),
                // Served by a core of its own (HSM, SCR): read as before, run by run.
                Ok(Some(_)) | Err(_) => match read_routed(cx, start, len) {
                    Ok(bytes) => {
                        for k in run.0..run.1 {
                            let at = (regs[k].0 - start) as usize;
                            out[k] = Some(Ok(bytes[at..at + regs[k].1].to_vec()));
                        }
                    }
                    Err(e) if run.1 - run.0 == 1 => out[run.0] = Some(Err(e)),
                    Err(_) => {
                        for k in run.0..run.1 {
                            out[k] = Some(read_routed(cx, regs[k].0, regs[k].1));
                        }
                    }
                },
            }
        }
        if !bus_runs.is_empty() {
            let ranges: Vec<(u32, usize)> = bus_runs.iter().map(|&r| span(r)).collect();
            match bus_reads(cx, &ranges) {
                Ok(results) => {
                    for ((&run, &(start, _)), res) in bus_runs.iter().zip(&ranges).zip(results) {
                        match res {
                            Ok(bytes) => {
                                for k in run.0..run.1 {
                                    let at = (regs[k].0 - start) as usize;
                                    out[k] = Some(Ok(bytes[at..at + regs[k].1].to_vec()));
                                }
                            }
                            Err(e) if run.1 - run.0 == 1 => out[run.0] = Some(Err(e)),
                            Err(_) => retry_bus.extend(run.0..run.1),
                        }
                    }
                }
                Err(e) => {
                    for &run in &bus_runs {
                        for k in run.0..run.1 {
                            out[k] = Some(Err(e.clone()));
                        }
                    }
                }
            }
        }
        if !retry_bus.is_empty() {
            let ranges: Vec<(u32, usize)> = retry_bus.iter().map(|&k| regs[k]).collect();
            match bus_reads(cx, &ranges) {
                Ok(results) => {
                    for (&k, res) in retry_bus.iter().zip(results) {
                        out[k] = Some(res);
                    }
                }
                Err(e) => {
                    for &k in &retry_bus {
                        out[k] = Some(Err(e.clone()));
                    }
                }
            }
        }
        Ok(out.into_iter().map(|r| r.unwrap_or_else(|| Err("not read".into()))).collect())
    })
}

pub fn read_peripheral(api: &CoreApi, req: ReadPeripheralRequest) -> Result<PeripheralValues, ApiError> {
    let l = loaded(api)?;
    let (_, regs, field) = resolve(&l, &req.path)?;
    let (offset, limit) = page(req.offset, req.limit);
    let regs: Vec<&RegisterInfo> = regs.iter().skip(offset).take(limit).collect::<Vec<_>>();
    let wanted: Vec<usize> = (0..regs.len()).filter(|&i| req.force || skip_reason(regs[i]).is_none()).collect();
    let results = read_registers(api, wanted.iter().map(|&i| (regs[i].address as u32, regs[i].bytes())).collect())?;
    let mut results = wanted.iter().copied().zip(results).peekable();
    let mut out = Vec::with_capacity(regs.len());
    for (i, r) in regs.iter().enumerate() {
        match results.peek() {
            Some((k, _)) if *k == i => {
                let (_, res) = results.next().unwrap();
                out.push(reading(r, res.map(|b| value_of(&b, r.size)), field));
            }
            _ => {
                let mut x = reading(r, Err(String::new()), field);
                x.error = None;
                x.skipped = skip_reason(r);
                out.push(x);
            }
        }
    }
    let total = l.model.find(&req.path).map(|n| l.model.registers(&n).len()).unwrap_or(0);
    Ok(PeripheralValues { path: req.path, total: total as u32, offset: offset as u32, registers: out })
}

// --- Writing -----------------------------------------------------------------------------------

/// The register value with the fields other than `except` whose writing acts
/// (`modifiedWriteValues`) set to the value that leaves them as they are.
fn neutral(r: &RegisterInfo, value: u64, except: Option<&svd::Field>) -> u64 {
    let mut v = value;
    for f in r.register.fields.iter().filter(|f| except.is_none_or(|e| !std::ptr::eq(e, *f))) {
        let m = mask(f.width) << f.offset.min(63);
        match f.modified_write.or(r.register.modified_write) {
            Some(ModifiedWrite::OneToClear | ModifiedWrite::OneToSet | ModifiedWrite::OneToToggle) => v &= !m,
            Some(ModifiedWrite::ZeroToClear | ModifiedWrite::ZeroToSet | ModifiedWrite::ZeroToToggle) => v |= m,
            _ => {}
        }
    }
    v
}

/// A value from JSON: a number, a decimal or `0x` string, or a name of the field's values.
fn parse_value(value: &Json, field: Option<&svd::Field>, path: &str) -> Result<u64, ApiError> {
    if let (Json::String(name), Some(f)) = (value, field) {
        let named = f.enumeration.iter().flat_map(|e| e.values.iter());
        if let Some(v) = named.clone().find(|v| *v.name == *name.trim()).or_else(|| named.clone().find(|v| v.name.eq_ignore_ascii_case(name.trim()))) {
            return Ok(v.value);
        }
    }
    match crate::variables::integer(value) {
        Some((x, _)) if (0..=u64::MAX as i128).contains(&x) => Ok(x as u64),
        _ => {
            let names: Vec<String> = field.iter().flat_map(|f| f.enumeration.iter()).flat_map(|e| e.values.iter()).map(|v| v.name.to_string()).take(16).collect();
            let hint = if names.is_empty() { String::new() } else { format!(" or one of {}", names.join(", ")) };
            Err(ApiError::bad_request(format!("`{path}`: {value} is not a non-negative integer{hint}")))
        }
    }
}

pub fn write_peripheral(api: &CoreApi, req: WritePeripheralRequest) -> Result<PeripheralWritten, ApiError> {
    let l = loaded(api)?;
    let node = l.model.find(&req.path).ok_or_else(|| not_found(&req.path, &l))?;
    let (r, field) = match node {
        Node::Register(r) => (r, None),
        Node::Field(r, f) => (r, Some(f)),
        _ => return Err(ApiError::bad_request(format!("`{}` is a peripheral or cluster: write one of its registers or fields", req.path))),
    };
    let path = match field {
        Some(f) => format!("{}.{}", r.path, f.name),
        None => r.path.clone(),
    };
    let writable = match field {
        Some(f) => f.access.unwrap_or(r.access) != Access::ReadOnly && r.access != Access::ReadOnly,
        None => r.access != Access::ReadOnly,
    };
    if !writable {
        return Err(ApiError::bad_request(format!("`{path}` is read-only")));
    }
    let v = parse_value(&req.value, field, &path)?;
    let width = field.map_or(r.size, |f| f.width);
    if v > mask(width) {
        return Err(ApiError::bad_request(format!("`{path}`: {v:#x} does not fit {width} bits")));
    }
    let (addr, n) = (r.address as u32, r.bytes());
    let readable = skip_reason(&r).is_none();
    let mut note = None;
    let value = match field {
        None => v,
        Some(f) => {
            let current = if readable {
                let b = read_registers(api, vec![(addr, n)])?.pop().unwrap().map_err(|e| ApiError::new(ErrorCode::Memory, format!("`{}`: {e}", r.path)))?;
                value_of(&b, r.size)
            } else {
                note = Some(format!("`{}` cannot be read without a side effect, so its other fields were written as 0", r.path));
                0
            };
            let m = mask(f.width) << f.offset.min(63);
            neutral(&r, (current & !m) | ((v << f.offset.min(63)) & m), Some(f))
        }
    };
    let data = value.to_le_bytes()[..n].to_vec();
    let what = path.clone();
    let refused = move |e: String| {
        ApiError::new(
            ErrorCode::Memory,
            format!("`{what}`: the write was refused ({e}). Registers protected by ENDINIT or Safety ENDINIT refuse writes without the unlock sequence, which this operation does not do"),
        )
    };
    let (written_by, back) = api.call(move |cx| {
        let t = write_routed(cx, addr, &data).map_err(refused)?;
        emit(Event::MemoryChanged { target: t, addr, len: data.len() as u32 });
        Ok((t, readable.then(|| read_routed(cx, addr, data.len()))))
    })?;
    let _ = written_by;
    let readback = back.map(|b| reading(&r, b.map(|b| value_of(&b, r.size)), None));
    // Bits that should read as written: the field's, or the register's plain read-write fields.
    if let Some(got) = readback.as_ref().and_then(|x| x.value) {
        let plain = |f: &svd::Field| f.access.unwrap_or(r.access) == Access::ReadWrite && f.modified_write.or(r.register.modified_write).is_none();
        let check = match field {
            Some(f) if plain(f) => mask(f.width) << f.offset.min(63),
            Some(_) => 0,
            None if r.register.fields.is_empty() && r.access == Access::ReadWrite && r.register.modified_write.is_none() => mask(r.size),
            None => r.register.fields.iter().filter(|f| plain(f)).fold(0, |m, f| m | (mask(f.width) << f.offset.min(63))),
        };
        if (got ^ value) & check != 0 {
            note = Some(format!(
                "`{path}` reads back {got:#x} after writing {value:#x}: the value did not stick. Registers protected by ENDINIT or Safety ENDINIT ignore writes without the unlock sequence; some bits may be read-only in this mode"
            ));
        }
    }
    Ok(PeripheralWritten { path, address: addr, written: value, readback, note })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_to_clear_neighbours_stay_untouched() {
        let xml = r#"<device><peripherals><peripheral><name>P</name><baseAddress>0</baseAddress><registers>
          <register><name>R</name><addressOffset>0</addressOffset><fields>
            <field><name>EN</name><bitOffset>0</bitOffset><bitWidth>1</bitWidth></field>
            <field><name>W1C</name><bitOffset>1</bitOffset><bitWidth>1</bitWidth><modifiedWriteValues>oneToClear</modifiedWriteValues></field>
            <field><name>W0C</name><bitOffset>2</bitOffset><bitWidth>1</bitWidth><modifiedWriteValues>zeroToClear</modifiedWriteValues></field>
          </fields></register></registers></peripheral></peripherals></device>"#;
        let d = svd::parse(xml, &[]).unwrap();
        let Some(Node::Field(r, en)) = d.find("P.R.EN") else { panic!() };
        // Read 0b011 (EN=1, W1C pending, W0C=0); writing EN=1 must not write 1 to W1C nor 0 to W0C.
        assert_eq!(neutral(&r, 0b011, Some(en)), 0b101);
        let Some(Node::Field(_, w1c)) = d.find("P.R.W1C") else { panic!() };
        assert_eq!(neutral(&r, 0b010, Some(w1c)), 0b110, "the field written keeps its own bit");
    }

    #[test]
    fn values_by_name_or_number() {
        let xml = r#"<device><peripherals><peripheral><name>P</name><baseAddress>0</baseAddress><registers>
          <register><name>R</name><addressOffset>0</addressOffset><fields>
            <field><name>M</name><bitOffset>0</bitOffset><bitWidth>2</bitWidth><enumeratedValues>
              <enumeratedValue><name>RUN</name><value>2</value></enumeratedValue></enumeratedValues></field>
          </fields></register></registers></peripheral></peripherals></device>"#;
        let d = svd::parse(xml, &[]).unwrap();
        let Some(Node::Field(_, m)) = d.find("P.R.M") else { panic!() };
        assert_eq!(parse_value(&Json::from("RUN"), Some(m), "P.R.M").unwrap(), 2);
        assert_eq!(parse_value(&Json::from("run"), Some(m), "P.R.M").unwrap(), 2);
        assert_eq!(parse_value(&Json::from("0x3"), Some(m), "P.R.M").unwrap(), 3);
        assert_eq!(parse_value(&Json::from(1), Some(m), "P.R.M").unwrap(), 1);
        let e = parse_value(&Json::from("STOP"), Some(m), "P.R.M").unwrap_err();
        assert!(e.message.contains("RUN"), "{}", e.message);
    }
}
