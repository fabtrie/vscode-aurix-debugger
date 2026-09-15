//! C types from DWARF: classification in TCF terms, members, enumerators.

use gimli::{AttributeValue, Operation};

use crate::{DieRef, Symbols};

/// TCF `TypeClass` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TypeClass {
    Unknown = 0,
    Cardinal = 1,
    Integer = 2,
    Real = 3,
    Pointer = 4,
    Array = 5,
    Composite = 6,
    Enumeration = 7,
    Function = 8,
}

impl TypeClass {
    pub fn from_u8(v: u8) -> Option<TypeClass> {
        Some(match v {
            0 => TypeClass::Unknown,
            1 => TypeClass::Cardinal,
            2 => TypeClass::Integer,
            3 => TypeClass::Real,
            4 => TypeClass::Pointer,
            5 => TypeClass::Array,
            6 => TypeClass::Composite,
            7 => TypeClass::Enumeration,
            8 => TypeClass::Function,
            _ => return None,
        })
    }
}

/// A type: a DWARF DIE or a type synthesized by expressions.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeId {
    Die(DieRef),
    /// Dimension `n` of a (multi-dimensional) array type DIE.
    ArrayDim(DieRef, u8),
    Void,
    Base(TypeClass, u8),
    PointerTo(Box<TypeId>),
}

impl TypeId {
    /// Stable textual ID (used as TCF symbol ID).
    pub fn to_id(&self) -> String {
        match self {
            TypeId::Die(d) => format!("T{}.{:x}", d.unit, d.off),
            TypeId::ArrayDim(d, n) => format!("A{}.{:x}.{}", d.unit, d.off, n),
            TypeId::Void => "V".into(),
            TypeId::Base(c, s) => format!("B{}.{}", *c as u8, s),
            TypeId::PointerTo(t) => format!("P{}", t.to_id()),
        }
    }

    pub fn parse(s: &str) -> Option<TypeId> {
        let rest = s.get(1..)?;
        match s.as_bytes().first()? {
            b'T' => {
                let (u, o) = rest.split_once('.')?;
                Some(TypeId::Die(DieRef { unit: u.parse().ok()?, off: usize::from_str_radix(o, 16).ok()? }))
            }
            b'A' => {
                let mut it = rest.split('.');
                let unit = it.next()?.parse().ok()?;
                let off = usize::from_str_radix(it.next()?, 16).ok()?;
                let n = it.next()?.parse().ok()?;
                Some(TypeId::ArrayDim(DieRef { unit, off }, n))
            }
            b'V' if rest.is_empty() => Some(TypeId::Void),
            b'B' => {
                let (c, sz) = rest.split_once('.')?;
                Some(TypeId::Base(TypeClass::from_u8(c.parse().ok()?)?, sz.parse().ok()?))
            }
            b'P' => Some(TypeId::PointerTo(Box::new(TypeId::parse(rest)?))),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeInfo {
    pub name: Option<String>,
    pub class: TypeClass,
    pub size: u32,
    /// Pointer target, array element or enumeration's underlying type.
    pub base: Option<TypeId>,
    /// Number of elements of an array.
    pub length: Option<u32>,
    /// The type without typedefs and qualifiers.
    pub resolved: TypeId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MemberInfo {
    pub die: DieRef,
    pub name: Option<String>,
    pub ty: TypeId,
    /// Byte offset of the member (bit fields: of their storage unit).
    pub offset: u32,
    pub size: u32,
    /// Bit fields: (shift of the least significant bit in the storage unit, bit count).
    pub bits: Option<(u32, u32)>,
}

const MAX_TYPE_DEPTH: u32 = 24;

fn base_name(c: TypeClass, size: u8) -> String {
    match (c, size) {
        (TypeClass::Integer, 1) => "signed char",
        (TypeClass::Cardinal, 1) => "unsigned char",
        (TypeClass::Integer, 2) => "short",
        (TypeClass::Cardinal, 2) => "unsigned short",
        (TypeClass::Integer, 4) => "int",
        (TypeClass::Cardinal, 4) => "unsigned int",
        (TypeClass::Integer, 8) => "long long",
        (TypeClass::Cardinal, 8) => "unsigned long long",
        (TypeClass::Real, 4) => "float",
        (TypeClass::Real, 8) => "double",
        _ => "<unknown>",
    }
    .to_string()
}

/// Value of a constant DWARF expression such as `DW_OP_plus_uconst n`.
fn const_expr(expr: gimli::Expression<crate::R>, enc: gimli::Encoding) -> Option<u64> {
    let mut ops = expr.operations(enc);
    let mut acc = 0u64;
    while let Ok(Some(op)) = ops.next() {
        match op {
            Operation::PlusConstant { value } => acc = acc.wrapping_add(value),
            Operation::UnsignedConstant { value } => acc = value,
            Operation::SignedConstant { value } => acc = value as u64,
            _ => return None,
        }
    }
    Some(acc)
}

pub(crate) fn attr_int(v: &AttributeValue<crate::R>) -> Option<i64> {
    match *v {
        AttributeValue::Sdata(x) => Some(x),
        AttributeValue::Udata(x) => Some(x as i64),
        AttributeValue::Data1(x) => Some(x as i64),
        AttributeValue::Data2(x) => Some(x as i64),
        AttributeValue::Data4(x) => Some(x as i64),
        AttributeValue::Data8(x) => Some(x as i64),
        _ => None,
    }
}

impl Symbols {
    pub fn type_info(&self, t: &TypeId) -> TypeInfo {
        self.type_info_at(t, 0)
    }

    fn unknown_type(t: &TypeId) -> TypeInfo {
        TypeInfo { name: None, class: TypeClass::Unknown, size: 0, base: None, length: None, resolved: t.clone() }
    }

    fn type_info_at(&self, t: &TypeId, depth: u32) -> TypeInfo {
        if depth > MAX_TYPE_DEPTH {
            return Self::unknown_type(t);
        }
        match t {
            TypeId::Void => TypeInfo { name: Some("void".into()), ..Self::unknown_type(t) },
            TypeId::Base(c, s) => {
                TypeInfo { name: Some(base_name(*c, *s)), class: *c, size: *s as u32, base: None, length: None, resolved: t.clone() }
            }
            TypeId::PointerTo(inner) => {
                let bi = self.type_info_at(inner, depth + 1);
                TypeInfo {
                    name: Some(format!("{}*", bi.name.unwrap_or_default())),
                    class: TypeClass::Pointer,
                    size: 4,
                    base: Some((**inner).clone()),
                    length: None,
                    resolved: t.clone(),
                }
            }
            TypeId::ArrayDim(d, n) => self.array_info(*d, *n, depth),
            TypeId::Die(d) => self.die_type_info(*d, depth),
        }
    }

    fn die_type_info(&self, d: DieRef, depth: u32) -> TypeInfo {
        let t = TypeId::Die(d);
        let Some((u, e)) = self.entry(d) else { return Self::unknown_type(&t) };
        let name = self.attr_str(u, e.attr_value(gimli::DW_AT_name)).filter(|n| !n.is_empty());
        let size = e.attr_value(gimli::DW_AT_byte_size).and_then(|v| v.udata_value()).unwrap_or(0) as u32;
        let inner = self.ref_attr(d, &e, gimli::DW_AT_type).map(TypeId::Die).unwrap_or(TypeId::Void);
        match e.tag() {
            gimli::DW_TAG_base_type => {
                let class = match e.attr_value(gimli::DW_AT_encoding) {
                    Some(AttributeValue::Encoding(gimli::DW_ATE_signed)) | Some(AttributeValue::Encoding(gimli::DW_ATE_signed_char)) => {
                        TypeClass::Integer
                    }
                    Some(AttributeValue::Encoding(gimli::DW_ATE_unsigned))
                    | Some(AttributeValue::Encoding(gimli::DW_ATE_unsigned_char))
                    | Some(AttributeValue::Encoding(gimli::DW_ATE_boolean)) => TypeClass::Cardinal,
                    Some(AttributeValue::Encoding(gimli::DW_ATE_float)) => TypeClass::Real,
                    _ => TypeClass::Unknown,
                };
                TypeInfo { name, class, size, base: None, length: None, resolved: t }
            }
            gimli::DW_TAG_typedef => {
                let mut i = self.type_info_at(&inner, depth + 1);
                i.name = name.or(i.name);
                i
            }
            gimli::DW_TAG_const_type | gimli::DW_TAG_volatile_type | gimli::DW_TAG_restrict_type => {
                let mut i = self.type_info_at(&inner, depth + 1);
                let q = match e.tag() {
                    gimli::DW_TAG_const_type => "const ",
                    gimli::DW_TAG_volatile_type => "volatile ",
                    _ => "",
                };
                i.name = i.name.map(|n| format!("{q}{n}"));
                i
            }
            gimli::DW_TAG_pointer_type | gimli::DW_TAG_reference_type => {
                let bi = self.type_info_at(&inner, depth + 1);
                TypeInfo {
                    name: Some(format!("{}*", bi.name.unwrap_or_default())),
                    class: TypeClass::Pointer,
                    size: if size == 0 { 4 } else { size },
                    base: Some(inner),
                    length: None,
                    resolved: t,
                }
            }
            gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type | gimli::DW_TAG_union_type => {
                let kw = if e.tag() == gimli::DW_TAG_union_type { "union" } else { "struct" };
                TypeInfo {
                    name: Some(name.map(|n| format!("{kw} {n}")).unwrap_or_else(|| format!("{kw} {{...}}"))),
                    class: TypeClass::Composite,
                    size,
                    base: None,
                    length: None,
                    resolved: t,
                }
            }
            gimli::DW_TAG_enumeration_type => {
                let base = if inner == TypeId::Void {
                    let negative = self.enumerators(&t).iter().any(|(_, _, v)| *v < 0);
                    TypeId::Base(if negative { TypeClass::Integer } else { TypeClass::Cardinal }, size.clamp(1, 8) as u8)
                } else {
                    inner
                };
                TypeInfo {
                    name: Some(name.map(|n| format!("enum {n}")).unwrap_or_else(|| "enum {...}".into())),
                    class: TypeClass::Enumeration,
                    size,
                    base: Some(base),
                    length: None,
                    resolved: t,
                }
            }
            gimli::DW_TAG_array_type => self.array_info(d, 0, depth),
            gimli::DW_TAG_subroutine_type | gimli::DW_TAG_subprogram => TypeInfo {
                name: Some(name.unwrap_or_else(|| "function".into())),
                class: TypeClass::Function,
                size: 0,
                base: None,
                length: None,
                resolved: t,
            },
            _ => TypeInfo { name, ..Self::unknown_type(&t) },
        }
    }

    /// Element counts of an array DIE's dimensions (None: unknown/flexible).
    fn array_dims(&self, d: DieRef) -> Vec<Option<u32>> {
        let dims: Vec<Option<u32>> = self
            .children(d)
            .iter()
            .filter(|(_, c)| c.tag() == gimli::DW_TAG_subrange_type)
            .map(|(_, c)| {
                if let Some(n) = c.attr_value(gimli::DW_AT_count).as_ref().and_then(attr_int) {
                    return u32::try_from(n).ok();
                }
                let ub = c.attr_value(gimli::DW_AT_upper_bound).as_ref().and_then(attr_int)?;
                let lb = c.attr_value(gimli::DW_AT_lower_bound).as_ref().and_then(attr_int).unwrap_or(0);
                if ub == u32::MAX as i64 || ub < lb - 1 {
                    return None;
                }
                u32::try_from(ub - lb + 1).ok()
            })
            .collect();
        if dims.is_empty() {
            vec![None]
        } else {
            dims
        }
    }

    fn array_info(&self, d: DieRef, n: u8, depth: u32) -> TypeInfo {
        let Some((_, e)) = self.entry(d) else { return Self::unknown_type(&TypeId::ArrayDim(d, n)) };
        let elem = self.ref_attr(d, &e, gimli::DW_AT_type).map(TypeId::Die).unwrap_or(TypeId::Void);
        let dims = self.array_dims(d);
        let n = (n as usize).min(dims.len() - 1);
        let base = if n + 1 < dims.len() { TypeId::ArrayDim(d, (n + 1) as u8) } else { elem };
        let bi = self.type_info_at(&base, depth + 1);
        let length = dims[n];
        TypeInfo {
            name: Some(format!("{}[{}]", bi.name.clone().unwrap_or_default(), length.map(|l| l.to_string()).unwrap_or_default())),
            class: TypeClass::Array,
            size: length.unwrap_or(0).saturating_mul(bi.size),
            base: Some(base),
            length,
            resolved: TypeId::ArrayDim(d, n as u8),
        }
    }

    /// Type of a variable, parameter or member DIE.
    pub fn var_type(&self, d: DieRef) -> TypeId {
        self.entry(d).and_then(|(_, e)| self.ref_attr(d, &e, gimli::DW_AT_type)).map(TypeId::Die).unwrap_or(TypeId::Void)
    }

    /// Members of a struct/union type (in declaration order).
    pub fn members(&self, t: &TypeId) -> Vec<MemberInfo> {
        let info = self.type_info(t);
        let TypeId::Die(d) = info.resolved else { return Vec::new() };
        if info.class != TypeClass::Composite {
            return Vec::new();
        }
        self.children(d)
            .into_iter()
            .filter(|(_, e)| e.tag() == gimli::DW_TAG_member || e.tag() == gimli::DW_TAG_inheritance)
            .filter_map(|(r, _)| self.member_info(r))
            .collect()
    }

    pub fn member_info(&self, m: DieRef) -> Option<MemberInfo> {
        let (u, e) = self.entry(m)?;
        let name = self.attr_str(u, e.attr_value(gimli::DW_AT_name)).filter(|n| !n.is_empty());
        let ty = self.ref_attr(m, &e, gimli::DW_AT_type).map(TypeId::Die).unwrap_or(TypeId::Void);
        let tsize = self.type_info(&ty).size;
        let mut offset = match e.attr_value(gimli::DW_AT_data_member_location) {
            None => 0,
            Some(AttributeValue::LocationListsRef(o)) => o.0 as u32,
            Some(v) => match attr_int(&v) {
                Some(x) => x as u32,
                None => v.exprloc_value().and_then(|x| const_expr(x, u.encoding())).unwrap_or(0) as u32,
            },
        };
        let mut size = tsize;
        let mut bits = None;
        if let Some(bit_size) = e.attr_value(gimli::DW_AT_bit_size).as_ref().and_then(attr_int) {
            let bit_size = bit_size as u32;
            let storage = e.attr_value(gimli::DW_AT_byte_size).and_then(|v| v.udata_value()).map(|x| x as u32).unwrap_or(tsize).max(1);
            if let Some(bo) = e.attr_value(gimli::DW_AT_bit_offset).as_ref().and_then(attr_int) {
                // DWARF 2/3: bit offset from the most significant bit of the storage unit.
                bits = Some(((storage * 8).saturating_sub(bo as u32 + bit_size), bit_size));
                size = storage;
            } else if let Some(dbo) = e.attr_value(gimli::DW_AT_data_bit_offset).as_ref().and_then(attr_int) {
                let unit_bits = storage * 8;
                offset = (dbo as u32 / unit_bits) * storage;
                bits = Some((dbo as u32 % unit_bits, bit_size));
                size = storage;
            }
        }
        Some(MemberInfo { die: m, name, ty, offset, size, bits })
    }

    /// A member by name, looking into anonymous struct/union members.
    pub fn find_member(&self, t: &TypeId, name: &str) -> Option<MemberInfo> {
        let members = self.members(t);
        if let Some(m) = members.iter().find(|m| m.name.as_deref() == Some(name)) {
            return Some(m.clone());
        }
        members.iter().filter(|m| m.name.is_none()).find_map(|anon| {
            let mut inner = self.find_member(&anon.ty, name)?;
            inner.offset += anon.offset;
            Some(inner)
        })
    }

    /// Enumerators of an enumeration type: (DIE, name, value).
    pub fn enumerators(&self, t: &TypeId) -> Vec<(DieRef, String, i64)> {
        let resolved = match t {
            TypeId::Die(d) => {
                // Resolve typedefs without recursing through type_info (which calls us).
                let mut cur = *d;
                let mut guard = 0;
                loop {
                    let Some((_, e)) = self.entry(cur) else { return Vec::new() };
                    match e.tag() {
                        gimli::DW_TAG_enumeration_type => break cur,
                        gimli::DW_TAG_typedef | gimli::DW_TAG_const_type | gimli::DW_TAG_volatile_type if guard < MAX_TYPE_DEPTH => {
                            let Some(next) = self.ref_attr(cur, &e, gimli::DW_AT_type) else { return Vec::new() };
                            cur = next;
                            guard += 1;
                        }
                        _ => return Vec::new(),
                    }
                }
            }
            _ => return Vec::new(),
        };
        let Some(u) = self.unit(resolved.unit) else { return Vec::new() };
        self.children(resolved)
            .into_iter()
            .filter(|(_, e)| e.tag() == gimli::DW_TAG_enumerator)
            .filter_map(|(r, e)| {
                let name = self.attr_str(u, e.attr_value(gimli::DW_AT_name))?;
                let value = e.attr_value(gimli::DW_AT_const_value).as_ref().and_then(attr_int)?;
                Some((r, name, value))
            })
            .collect()
    }

    pub fn enumerator(&self, e: DieRef) -> Option<(String, i64)> {
        let (u, d) = self.entry(e)?;
        let name = self.attr_str(u, d.attr_value(gimli::DW_AT_name))?;
        let value = d.attr_value(gimli::DW_AT_const_value).as_ref().and_then(attr_int)?;
        Some((name, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_ids_round_trip() {
        let d = DieRef { unit: 21, off: 0x3803 };
        for t in [
            TypeId::Die(d),
            TypeId::ArrayDim(d, 2),
            TypeId::Void,
            TypeId::Base(TypeClass::Cardinal, 4),
            TypeId::PointerTo(Box::new(TypeId::PointerTo(Box::new(TypeId::Die(d))))),
        ] {
            assert_eq!(TypeId::parse(&t.to_id()), Some(t.clone()), "{}", t.to_id());
        }
        assert_eq!(TypeId::parse(""), None);
        assert_eq!(TypeId::parse("X1"), None);
    }
}
