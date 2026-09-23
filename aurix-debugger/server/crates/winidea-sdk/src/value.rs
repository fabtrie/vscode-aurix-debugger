//! `CValueType` — the SDK's universal tagged value.
//!
//! Every typed read (memory, registers, expressions, DAQ samples) comes back as
//! a `CValueType`: a value plus enough type information to interpret it. This
//! module converts one into a plain Rust [`Value`] at the boundary, so callers
//! never handle SDK objects.

use crate::WinIdea;
use crate::runtime::{self, Error, Owned};

/// A value read from the target, decoded according to its SDK type tag.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    /// An address, kept distinct from a plain integer.
    Address(u64),
    /// A struct/array/union, which the SDK only renders as text.
    Compound(String),
    /// The SDK could not evaluate the expression.
    Error(String),
    /// A value whose type tag matched none of the above; the SDK's own textual
    /// rendering, never a guess at the binary meaning.
    Text(String),
}

impl Value {
    /// The value as an `i64`, when it is integral.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Signed(v) => Some(*v),
            Self::Unsigned(v) => i64::try_from(*v).ok(),
            Self::Address(v) => i64::try_from(*v).ok(),
            _ => None,
        }
    }

    /// The value as an `f64`, when it is numeric.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Float(v) => Some(*v),
            Self::Signed(v) => Some(*v as f64),
            Self::Unsigned(v) => Some(*v as f64),
            _ => None,
        }
    }

    /// Whether this represents an SDK evaluation failure.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Signed(v) => write!(f, "{v}"),
            Self::Unsigned(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::Address(v) => write!(f, "{v:#x}"),
            Self::Compound(s) | Self::Text(s) => f.write_str(s),
            Self::Error(s) => write!(f, "<error: {s}>"),
        }
    }
}

/// Decode a borrowed `CValueType` handle into a [`Value`].
///
/// The type tag is queried in a deliberate order: error first (an errored value
/// carries no meaningful payload), then float, then the integer signedness
/// flags. `isTypeAddress` is checked before the integer cases because an address
/// also reports as unsigned.
///
/// # Safety
/// `ptr` must be a live `CValueType` handle.
pub(crate) unsafe fn decode(winidea: &WinIdea, ptr: *mut std::ffi::c_void) -> Result<Value, Error> {
    if ptr.is_null() {
        return Err(Error::NullHandle);
    }
    let sdk = winidea.raw();

    // Resolved BEFORE the closure: the closure returns `Option`, so `sym!`'s `?`
    // could not propagate a `MissingSymbol` out of it.
    let get_result = sym!(sdk, CValueType_getResult__SWIG_0);
    // SAFETY: the caller guarantees `ptr` is a live CValueType.
    let text = || unsafe { runtime::from_cstr(get_result(ptr)) };

    // SAFETY: as above; these are all simple tag queries on a live handle.
    let v = unsafe {
        if (sym!(sdk, CValueType_isError))(ptr) {
            Value::Error(text().unwrap_or_default())
        } else if (sym!(sdk, CValueType_isTypeCompound))(ptr) {
            Value::Compound(text().unwrap_or_default())
        } else if (sym!(sdk, CValueType_isTypeFloat))(ptr) {
            Value::Float((sym!(sdk, CValueType_getDouble))(ptr))
        } else if (sym!(sdk, CValueType_isTypeAddress))(ptr) {
            Value::Address((sym!(sdk, CValueType_getLong))(ptr) as u64)
        } else if (sym!(sdk, CValueType_isTypeSigned))(ptr) {
            Value::Signed((sym!(sdk, CValueType_getLong))(ptr))
        } else if (sym!(sdk, CValueType_isTypeUnsigned))(ptr) {
            Value::Unsigned((sym!(sdk, CValueType_getLong))(ptr) as u64)
        } else {
            // No tag matched. Fall back to the SDK's own rendering rather than
            // guessing how to interpret the bits.
            Value::Text(text().unwrap_or_default())
        }
    };
    runtime::check()?;
    Ok(v)
}

/// Decode a `CValueType` handle we own, releasing it afterwards.
///
/// # Safety
/// `ptr` must be null or a `CValueType` handle owned by the caller.
pub(crate) unsafe fn decode_owned(
    winidea: &WinIdea,
    ptr: *mut std::ffi::c_void,
) -> Result<Value, Error> {
    let sdk = winidea.raw();
    // SAFETY: guaranteed by the caller; delete_CValueType is its destructor.
    let owned = unsafe { Owned::adopt(winidea, ptr, sdk.delete_CValueType)? };
    // SAFETY: `owned.ptr` is live until `owned` drops at end of scope.
    unsafe { decode(winidea, owned.ptr) }
}

/// Build a `CValueType` to pass into the SDK, e.g. for a write.
///
/// The returned guard owns the handle and frees it on drop.
pub(crate) fn make_i64<'a>(winidea: &'a WinIdea, v: i64) -> Result<Owned<'a>, Error> {
    let sdk = winidea.raw();
    // SAFETY: the (int32 type, int64 value) constructor; 0 selects the SDK's
    // default integral type.
    let ptr = unsafe { (sym!(sdk, new_CValueType__SWIG_7))(0, v) };
    // SAFETY: `ptr` is a fresh CValueType.
    unsafe { Owned::adopt(winidea, ptr, sdk.delete_CValueType) }
}

/// Build a floating-point `CValueType`.
pub(crate) fn make_f64<'a>(winidea: &'a WinIdea, v: f64) -> Result<Owned<'a>, Error> {
    let sdk = winidea.raw();
    // SAFETY: the double constructor.
    let ptr = unsafe { (sym!(sdk, new_CValueType__SWIG_10))(v) };
    // SAFETY: `ptr` is a fresh CValueType.
    unsafe { Owned::adopt(winidea, ptr, sdk.delete_CValueType) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_conversions() {
        assert_eq!(Value::Signed(-5).as_i64(), Some(-5));
        assert_eq!(Value::Unsigned(5).as_i64(), Some(5));
        assert_eq!(Value::Address(0x1000).as_i64(), Some(0x1000));
        assert_eq!(Value::Float(1.5).as_i64(), None);

        assert_eq!(Value::Float(1.5).as_f64(), Some(1.5));
        assert_eq!(Value::Signed(3).as_f64(), Some(3.0));
        assert_eq!(Value::Compound("{}".into()).as_f64(), None);

        // u64 values above i64::MAX cannot be represented; report None rather
        // than silently wrapping to a negative number.
        assert_eq!(Value::Unsigned(u64::MAX).as_i64(), None);
    }

    #[test]
    fn display_formats_by_kind() {
        assert_eq!(Value::Signed(-5).to_string(), "-5");
        assert_eq!(Value::Address(0x20).to_string(), "0x20");
        assert_eq!(Value::Compound("{a=1}".into()).to_string(), "{a=1}");
        assert_eq!(Value::Error("nope".into()).to_string(), "<error: nope>");
        assert!(Value::Error("x".into()).is_error());
        assert!(!Value::Signed(0).is_error());
    }
}
