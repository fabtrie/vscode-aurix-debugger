//! Turning a DAQ point specification into an address the core can sample, and its bytes back
//! into a JSON value.
//!
//! Both happen on an API thread, never on the core thread: resolution walks DWARF and may take a
//! while, and decoding runs once per delivered sample. The [`Decoder`] a point gets is **owned**
//! and self-contained, so the sampling path never locks the symbol store.

use std::sync::Arc;

use serde_json::{json, Value};

use aurix_core::daq::{PointCfg, SampleMode, Space};
use aurix_core::{ops, ServerHandle};
use symbols::expr::{Eval, Place, SymbolRef, Target};
use symbols::types::TypeClass;

/// How a point's bytes become a JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Decoder {
    Uint { size: u8, bits: Option<(u32, u32)> },
    Int { size: u8, bits: Option<(u32, u32)> },
    Float(u8),
    Bool,
    /// Enumerator names by value; short enough for a linear scan.
    Enum { size: u8, signed: bool, names: Vec<(i64, String)> },
    /// Aggregates, unknown types and anything the client asked for raw.
    Hex,
}

impl Decoder {
    /// The `encoding` reported to the client.
    pub fn name(&self) -> &'static str {
        match self {
            Decoder::Uint { .. } => "uint",
            Decoder::Int { .. } => "int",
            Decoder::Float(_) => "float",
            Decoder::Bool => "bool",
            Decoder::Enum { .. } => "enum",
            Decoder::Hex => "hex",
        }
    }

    /// The bytes that take part in change detection, for a bit field: without this a change to a
    /// neighbouring field in the same storage word would look like a change to this point.
    pub fn cmp_mask(&self, len: u32) -> Option<Vec<u8>> {
        let (shift, bits) = match self {
            Decoder::Uint { bits: Some(b), .. } | Decoder::Int { bits: Some(b), .. } => *b,
            _ => return None,
        };
        let mask = field_mask(shift, bits);
        Some((0..len).map(|i| (mask >> (8 * i.min(7))) as u8).collect())
    }

    pub fn decode(&self, b: &[u8]) -> Value {
        match self {
            Decoder::Hex => json!(hex(b)),
            Decoder::Bool => json!(b.iter().any(|&x| x != 0)),
            Decoder::Float(4) => float(f32::from_le_bytes(word(b)) as f64),
            Decoder::Float(_) => float(f64::from_le_bytes(dword(b))),
            Decoder::Uint { size, bits } => json!(extract(b, *size, *bits)),
            Decoder::Int { size, bits } => json!(sign_extend(extract(b, *size, *bits), width(*size, *bits))),
            Decoder::Enum { size, signed, names } => {
                let raw = extract(b, *size, None);
                let v = if *signed { sign_extend(raw, width(*size, None)) } else { raw as i64 };
                match names.iter().find(|(n, _)| *n == v) {
                    Some((_, name)) => json!(name),
                    // Not an enumerator: a bit set, or a value the target should not hold.
                    None => json!(v),
                }
            }
        }
    }
}

fn field_mask(shift: u32, bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        ((1u64 << bits) - 1) << shift.min(63)
    }
}

fn width(size: u8, bits: Option<(u32, u32)>) -> u32 {
    match bits {
        Some((_, n)) => n,
        None => (size as u32 * 8).min(64),
    }
}

fn le_u64(b: &[u8]) -> u64 {
    let mut v = 0u64;
    for (i, &x) in b.iter().take(8).enumerate() {
        v |= (x as u64) << (8 * i);
    }
    v
}

fn extract(b: &[u8], size: u8, bits: Option<(u32, u32)>) -> u64 {
    let raw = le_u64(&b[..b.len().min(size as usize).min(8)]);
    match bits {
        Some((shift, n)) => (raw >> shift.min(63)) & if n >= 64 { u64::MAX } else { (1u64 << n) - 1 },
        None => raw,
    }
}

fn sign_extend(v: u64, bits: u32) -> i64 {
    if bits == 0 || bits >= 64 {
        return v as i64;
    }
    let sign = 1u64 << (bits - 1);
    ((v ^ sign).wrapping_sub(sign)) as i64
}

fn word(b: &[u8]) -> [u8; 4] {
    let mut w = [0u8; 4];
    let n = b.len().min(4);
    w[..n].copy_from_slice(&b[..n]);
    w
}

fn dword(b: &[u8]) -> [u8; 8] {
    let mut w = [0u8; 8];
    let n = b.len().min(8);
    w[..n].copy_from_slice(&b[..n]);
    w
}

/// JSON has no NaN or infinity, and `serde_json` turns them into `null`, which a client cannot
/// tell from "no value". An uninitialised float in RAM is very often NaN, so name them instead.
fn float(v: f64) -> Value {
    if v.is_nan() {
        json!("nan")
    } else if v == f64::INFINITY {
        json!("inf")
    } else if v == f64::NEG_INFINITY {
        json!("-inf")
    } else {
        json!(v)
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A point as the client asked for it.
#[derive(Clone, Debug)]
pub struct Spec {
    pub expression: Option<String>,
    pub address: Option<u32>,
    pub size: Option<u32>,
    pub interval_us: u32,
    pub target: Option<String>,
    pub name: Option<String>,
    pub mode: SampleMode,
    pub resend_after_us: u32,
    /// `decode: false` streams raw hex instead of a typed value.
    pub decode: bool,
}

/// A point after resolution: either something the core can sample, or the reason it cannot.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub name: String,
    pub expression: Option<String>,
    pub target: Option<String>,
    pub address: Option<u32>,
    pub size: Option<u32>,
    pub bits: Option<(u32, u32)>,
    pub type_name: Option<String>,
    pub decoder: Decoder,
    pub requested_interval_us: u32,
    pub cfg: Option<PointCfg>,
    pub error: Option<String>,
}

impl Resolved {
    fn failed(spec: &Spec, name: String, err: String) -> Resolved {
        Resolved {
            name,
            expression: spec.expression.clone(),
            target: spec.target.clone(),
            address: None,
            size: None,
            bits: None,
            type_name: None,
            decoder: Decoder::Hex,
            requested_interval_us: spec.interval_us,
            cfg: None,
            error: Some(err),
        }
    }
}

/// Evaluation context for a DAQ point: globals only.
///
/// `pc` and `reg` deliberately return `None`. A frame local lives in a register and only exists
/// while the target is halted, so it can never be sampled; this turns that into a clear error at
/// subscribe time instead of a surprise later.
struct DaqTarget<'a> {
    handle: &'a ServerHandle,
    core: usize,
}

impl Target for DaqTarget<'_> {
    fn pc(&self) -> Option<u32> {
        None
    }
    fn reg(&self, _: u16) -> Option<u32> {
        None
    }
    fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
        // Only at resolve time, to follow an index or a pointer (`buf[i]`, `p->x`).
        let t = self.core;
        self.handle
            .call(move |cx| ops::read_memory_as(cx, t, addr, len).map_err(|e| e.to_string()))
            .unwrap_or_else(|| Err("server stopped".into()))
    }
    fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
        None
    }
}

/// Picks the decoder for a resolved value, from its DWARF type.
fn decoder_for(syms: &symbols::Symbols, ty: &symbols::types::TypeId, bits: Option<(u32, u32)>) -> (Decoder, u32, Option<String>) {
    let info = syms.type_info(ty);
    let size = info.size.max(1);
    let name = info.name.clone();
    let s = size.min(8) as u8;
    let d = match info.class {
        // TypeClass has no Boolean: the name is the only way to tell a C99 _Bool apart.
        TypeClass::Cardinal if size == 1 && matches!(name.as_deref(), Some("_Bool") | Some("bool")) => Decoder::Bool,
        TypeClass::Cardinal | TypeClass::Pointer if size <= 8 => Decoder::Uint { size: s, bits },
        TypeClass::Integer if size <= 8 => Decoder::Int { size: s, bits },
        TypeClass::Real if size == 4 || size == 8 => Decoder::Float(s),
        TypeClass::Enumeration if size <= 8 => {
            let signed = syms.type_info(&info.resolved).base.map(|b| syms.type_info(&b).class) == Some(TypeClass::Integer);
            Decoder::Enum { size: s, signed, names: syms.enumerators(ty).into_iter().map(|(_, n, v)| (v, n)).collect() }
        }
        _ => Decoder::Hex,
    };
    (d, size, name)
}

/// Resolves one point against the symbols of `core` (if any).
pub fn resolve(handle: &ServerHandle, set: Option<&Arc<tcf::SymbolSet>>, core: usize, target: Option<&str>, spec: &Spec) -> Resolved {
    let space = match target {
        Some(_) => Space::Core(core),
        None => Space::Host,
    };
    let fallback = spec.name.clone();

    // A raw address needs no symbols at all.
    if let Some(addr) = spec.address {
        let name = fallback.unwrap_or_else(|| format!("0x{addr:08x}"));
        let Some(size) = spec.size else {
            return Resolved::failed(spec, name, "`address` needs `size`".into());
        };
        return Resolved {
            name,
            expression: None,
            target: spec.target.clone(),
            address: Some(addr),
            size: Some(size),
            bits: None,
            type_name: None,
            decoder: Decoder::Hex,
            requested_interval_us: spec.interval_us,
            cfg: Some(PointCfg {
                addr,
                len: size,
                space,
                interval_us: spec.interval_us,
                mode: spec.mode,
                cmp_mask: None,
                resend_after_us: spec.resend_after_us,
            }),
            error: None,
        };
    }

    let Some(expr) = spec.expression.clone() else {
        return Resolved::failed(spec, fallback.unwrap_or_default(), "give either `expression` or `address` with `size`".into());
    };
    let name = fallback.unwrap_or_else(|| expr.clone());
    let Some(set) = set else {
        return Resolved::failed(spec, name, "no symbols loaded for this target (use `load_symbols`, or start a VS Code debug session)".into());
    };

    // Try each ELF of the set, primary first.
    let mut last = String::from("unknown symbol");
    for (_, syms) in &set.elfs {
        let mut t = DaqTarget { handle, core };
        let v = match Eval::new(syms, &mut t).evaluate(&expr) {
            Ok(v) => v,
            Err(e) => {
                last = e;
                continue;
            }
        };
        let (addr, storage, bits) = match v.place {
            Place::Memory(a) => (a, None, None),
            Place::Bits { addr, storage, shift, bits } => (addr, Some(storage), Some((shift, bits))),
            Place::Register(_) => {
                return Resolved::failed(spec, name, format!("`{expr}` is a register value: DAQ can only sample things with a fixed address"))
            }
            Place::None => return Resolved::failed(spec, name, format!("`{expr}` has no address")),
        };
        let (mut decoder, ty_size, type_name) = decoder_for(syms, &v.ty, bits);
        let size = spec.size.or(storage).unwrap_or(ty_size).max(1);
        if !spec.decode || size > 8 {
            decoder = Decoder::Hex;
        }
        let cmp_mask = decoder.cmp_mask(size);
        return Resolved {
            name,
            expression: Some(expr),
            target: spec.target.clone(),
            address: Some(addr),
            size: Some(size),
            bits,
            type_name,
            requested_interval_us: spec.interval_us,
            cfg: Some(PointCfg {
                addr,
                len: size,
                space,
                interval_us: spec.interval_us,
                mode: spec.mode,
                cmp_mask,
                resend_after_us: spec.resend_after_us,
            }),
            decoder,
            error: None,
        };
    }
    Resolved::failed(spec, name, format!("`{expr}`: {last}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_are_decoded_with_their_width_and_sign() {
        let u32d = Decoder::Uint { size: 4, bits: None };
        assert_eq!(u32d.decode(&[0xd2, 0x04, 0x00, 0x00]), json!(1234u64));
        let i32d = Decoder::Int { size: 4, bits: None };
        assert_eq!(i32d.decode(&[0xff, 0xff, 0xff, 0xff]), json!(-1i64));
        let i8d = Decoder::Int { size: 1, bits: None };
        assert_eq!(i8d.decode(&[0x80]), json!(-128i64));
        let u8d = Decoder::Uint { size: 1, bits: None };
        assert_eq!(u8d.decode(&[0x80]), json!(128u64));
    }

    #[test]
    fn a_bit_field_is_shifted_and_masked() {
        // Bits 4..7 of a 32-bit word.
        let d = Decoder::Uint { size: 4, bits: Some((4, 3)) };
        assert_eq!(d.decode(&[0x50, 0x00, 0x00, 0x00]), json!(5u64));
        // Signed bit fields sign-extend from their own width, not the storage width.
        let d = Decoder::Int { size: 4, bits: Some((4, 3)) };
        assert_eq!(d.decode(&[0x50, 0x00, 0x00, 0x00]), json!(-3i64));
    }

    #[test]
    fn a_bit_field_mask_covers_only_its_own_bits() {
        let d = Decoder::Uint { size: 4, bits: Some((4, 3)) };
        assert_eq!(d.cmp_mask(4), Some(vec![0x70, 0x00, 0x00, 0x00]));
        // A whole-word point compares every byte, so it needs no mask.
        assert_eq!(Decoder::Uint { size: 4, bits: None }.cmp_mask(4), None);
    }

    #[test]
    fn floats_keep_nan_and_infinity_distinguishable_from_null() {
        let d = Decoder::Float(4);
        assert_eq!(d.decode(&21.5f32.to_le_bytes()), json!(21.5));
        assert_eq!(d.decode(&f32::NAN.to_le_bytes()), json!("nan"));
        assert_eq!(d.decode(&f32::INFINITY.to_le_bytes()), json!("inf"));
        assert_eq!(d.decode(&f32::NEG_INFINITY.to_le_bytes()), json!("-inf"));
        assert_eq!(Decoder::Float(8).decode(&(-0.25f64).to_le_bytes()), json!(-0.25));
    }

    #[test]
    fn bools_and_enums_read_as_names() {
        assert_eq!(Decoder::Bool.decode(&[0]), json!(false));
        assert_eq!(Decoder::Bool.decode(&[1]), json!(true));
        let e = Decoder::Enum { size: 4, signed: false, names: vec![(0, "IDLE".into()), (2, "RUN".into())] };
        assert_eq!(e.decode(&[2, 0, 0, 0]), json!("RUN"));
        // A value that matches no enumerator stays a number rather than becoming a lie.
        assert_eq!(e.decode(&[7, 0, 0, 0]), json!(7i64));
    }

    #[test]
    fn aggregates_fall_back_to_hex() {
        assert_eq!(Decoder::Hex.decode(&[0x0a, 0x1b, 0x2c]), json!("0a1b2c"));
        assert_eq!(Decoder::Hex.name(), "hex");
    }
}
