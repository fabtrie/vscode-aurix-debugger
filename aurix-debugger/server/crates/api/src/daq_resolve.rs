//! Turning a DAQ point specification into an address the core can sample, and its bytes back
//! into a JSON value.
//!
//! Both happen on an API thread, never on the core thread: resolution walks DWARF and may take a
//! while, and decoding runs once per delivered sample. The [`Decoder`] a point gets is **owned**
//! and self-contained, so the sampling path never locks the symbol store.

use serde_json::{json, Value};

use aurix_core::daq::{PointCfg, SampleMode, Space};
use symbols::expr::Place;
use symbols::types::TypeClass;

/// How a point's bytes become a JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Decoder {
    Uint { size: u8, bits: Option<(u32, u32)> },
    Int { size: u8, bits: Option<(u32, u32)> },
    Float(u8),
    Bool { bits: Option<(u32, u32)> },
    /// Enumerator names by value; short enough for a linear scan.
    Enum { size: u8, signed: bool, bits: Option<(u32, u32)>, names: Vec<(i64, String)> },
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
            Decoder::Bool { .. } => "bool",
            Decoder::Enum { .. } => "enum",
            Decoder::Hex => "hex",
        }
    }

    /// The bytes that take part in change detection, for a bit field: without this a change to a
    /// neighbouring field in the same storage word would look like a change to this point.
    pub fn cmp_mask(&self, len: u32) -> Option<Vec<u8>> {
        let (shift, bits) = match self {
            Decoder::Uint { bits: Some(b), .. }
            | Decoder::Int { bits: Some(b), .. }
            | Decoder::Bool { bits: Some(b) }
            | Decoder::Enum { bits: Some(b), .. } => *b,
            _ => return None,
        };
        let mask = field_mask(shift, bits);
        Some((0..len).map(|i| (mask >> (8 * i.min(7))) as u8).collect())
    }

    pub fn decode(&self, b: &[u8]) -> Value {
        match self {
            Decoder::Hex => json!(hex(b)),
            Decoder::Bool { bits: None } => json!(b.iter().any(|&x| x != 0)),
            Decoder::Bool { bits } => json!(extract(b, b.len().min(8) as u8, *bits) != 0),
            Decoder::Float(4) => float(f32::from_le_bytes(word(b)) as f64),
            Decoder::Float(_) => float(f64::from_le_bytes(dword(b))),
            Decoder::Uint { size, bits } => json!(extract(b, *size, *bits)),
            Decoder::Int { size, bits } => json!(sign_extend(extract(b, *size, *bits), width(*size, *bits))),
            Decoder::Enum { size, signed, bits, names } => {
                let raw = extract(b, *size, *bits);
                let v = if *signed { sign_extend(raw, width(*size, *bits)) } else { raw as i64 };
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
    pub fn failed(spec: &Spec, name: String, err: String) -> Resolved {
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

/// Picks the decoder for a resolved value, from its DWARF type.
pub fn decoder_for(syms: &symbols::Symbols, ty: &symbols::types::TypeId, bits: Option<(u32, u32)>) -> (Decoder, u32, Option<String>) {
    let info = syms.type_info(ty);
    let size = info.size.max(1);
    let name = info.name.clone();
    let s = size.min(8) as u8;
    let d = match info.class {
        // TypeClass has no Boolean: the name is the only way to tell a C99 _Bool apart.
        TypeClass::Cardinal if size == 1 && matches!(name.as_deref(), Some("_Bool") | Some("bool")) => Decoder::Bool { bits },
        TypeClass::Cardinal | TypeClass::Pointer if size <= 8 => Decoder::Uint { size: s, bits },
        TypeClass::Integer if size <= 8 => Decoder::Int { size: s, bits },
        TypeClass::Real if size == 4 || size == 8 => Decoder::Float(s),
        TypeClass::Enumeration if size <= 8 => {
            let signed = syms.type_info(&info.resolved).base.map(|b| syms.type_info(&b).class) == Some(TypeClass::Integer);
            Decoder::Enum { size: s, signed, bits, names: syms.enumerators(ty).into_iter().map(|(_, n, v)| (v, n)).collect() }
        }
        _ => Decoder::Hex,
    };
    (d, size, name)
}

/// Resolves one point, with the symbols of `target` (the core the point names, if any).
///
/// Names are evaluated by [`tcf::evaluate_global`], as `evaluate` does: a point on an expression
/// samples what `evaluate` reads, `@N` and the core-local rules included. A frame local lives in a
/// register and only exists while the target is halted, so it can never be sampled; without a
/// stack frame it does not resolve, which turns that into a clear error at subscribe time instead
/// of a surprise later.
///
/// Core-local RAM is translated to its core's global DSPR/PSPR address and read over the system
/// bus, so such a point shares the batch with all others. So is everything else a TriCore sees:
/// its view differs from the system bus only in that alias. Only the HSM and MCS channels have
/// memory of their own, so only their points are read through the core.
pub fn resolve(session: &tcf::Shared, target: Option<&tcf::CoreInfo>, spec: &Spec) -> Resolved {
    let fallback = spec.name.clone();

    // A raw address needs no symbols at all; a TriCore target only decides the local alias.
    if let Some(given) = spec.address {
        let name = fallback.unwrap_or_else(|| format!("0x{given:08x}"));
        let Some(size) = spec.size else {
            return Resolved::failed(spec, name, "`address` needs `size`".into());
        };
        let (addr, space) = match target {
            Some(c) if c.tricore() => (aurix_core::tricore::global_address(c.coreid, given), Space::Host),
            Some(c) => (given, Space::Core(c.idx)),
            None => (given, Space::Host),
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
    let ctx = target.map(|c| c.id.clone()).unwrap_or_default();
    if session.symbols_for(&ctx).is_none_or(|s| s.elfs.is_empty()) {
        return Resolved::failed(spec, name, message(&expr, tcf::GlobalError::NoSymbols));
    }
    let gv = match tcf::evaluate_global(session, &ctx, &expr) {
        Ok(gv) => gv,
        Err(e) => return Resolved::failed(spec, name, message(&expr, e)),
    };
    let v = &gv.value;
    let (local, storage, bits) = match v.place {
        Place::Memory(a) if v.bytes.is_none() => (a, None, None),
        Place::Bits { addr, storage, shift, bits } if v.bytes.is_none() => (addr, Some(storage), Some((shift, bits))),
        Place::Register(_) => return Resolved::failed(spec, name, format!("`{expr}` is a register value: DAQ can only sample things with a fixed address")),
        _ => return Resolved::failed(spec, name, format!("`{expr}` has no address")),
    };
    let core = gv.scope_core();
    let (addr, space) = if core.tricore() {
        match gv.global_address(session, local) {
            Ok(a) => (a, Space::Host),
            Err(e) => return Resolved::failed(spec, name, message(&expr, e)),
        }
    } else {
        (local, Space::Core(core.idx))
    };
    let syms = &gv.elf.1;
    let (mut decoder, ty_size, type_name) = decoder_for(syms, &v.ty, bits);
    let size = spec.size.or(storage).unwrap_or(ty_size).max(1);
    if !spec.decode || size > 8 {
        decoder = Decoder::Hex;
    }
    let cmp_mask = decoder.cmp_mask(size);
    Resolved {
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
    }
}

/// The `error` of a point whose expression `expr` does not resolve.
fn message(expr: &str, e: tcf::GlobalError) -> String {
    match e {
        tcf::GlobalError::NoSymbols => "no symbols loaded for this target (use `load_symbols`, or start a VS Code debug session)".into(),
        tcf::GlobalError::Expression(m) | tcf::GlobalError::Memory(m) => format!("`{expr}`: {m}"),
        // These name the expression themselves.
        tcf::GlobalError::NoTarget(m) | tcf::GlobalError::Core(m) => m,
    }
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
        assert_eq!(Decoder::Bool { bits: None }.decode(&[0]), json!(false));
        assert_eq!(Decoder::Bool { bits: None }.decode(&[1]), json!(true));
        let e = Decoder::Enum { size: 4, signed: false, bits: None, names: vec![(0, "IDLE".into()), (2, "RUN".into())] };
        assert_eq!(e.decode(&[2, 0, 0, 0]), json!("RUN"));
        // A value that matches no enumerator stays a number rather than becoming a lie.
        assert_eq!(e.decode(&[7, 0, 0, 0]), json!(7i64));
    }

    #[test]
    fn enum_and_bool_bit_fields_read_only_their_own_bits() {
        // Bits 4..5 hold RUN (2); the neighbouring bits are set and must not leak in.
        let e = Decoder::Enum { size: 4, signed: false, bits: Some((4, 2)), names: vec![(0, "IDLE".into()), (2, "RUN".into())] };
        assert_eq!(e.decode(&[0xef, 0xff, 0x00, 0x00]), json!("RUN"));
        assert_eq!(e.cmp_mask(4), Some(vec![0x30, 0x00, 0x00, 0x00]));
        let b = Decoder::Bool { bits: Some((9, 1)) };
        assert_eq!(b.decode(&[0xff, 0xfd, 0x00, 0x00]), json!(false));
        assert_eq!(b.decode(&[0x00, 0x02, 0x00, 0x00]), json!(true));
    }

    #[test]
    fn aggregates_fall_back_to_hex() {
        assert_eq!(Decoder::Hex.decode(&[0x0a, 0x1b, 0x2c]), json!("0a1b2c"));
        assert_eq!(Decoder::Hex.name(), "hex");
    }
}
