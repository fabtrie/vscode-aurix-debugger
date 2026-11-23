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
    /// An array of `n` elements of a type without a DIE (from SDCC debug tables).
    ArrayOf(Box<TypeId>, u32),
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
            TypeId::ArrayOf(t, n) => format!("Y{n}.{}", t.to_id()),
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
            b'Y' => {
                let (n, t) = rest.split_once('.')?;
                Some(TypeId::ArrayOf(Box::new(TypeId::parse(t)?), n.parse().ok()?))
            }
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

/// A member of a struct global, nested or not: the expression that names it, where it is, and
/// the global it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberPath {
    pub expression: String,
    pub address: u32,
    pub size: u32,
    pub global: usize,
    /// The member path of the member this one is in, `None` for a member of the global itself.
    pub parent: Option<usize>,
    /// The member's type, and (bit offset, bit size) of a bit field.
    pub ty: TypeId,
    pub bits: Option<(u32, u32)>,
}

/// Nesting depth and number of the member paths of an ELF, as a guard against huge programs.
pub const MEMBER_PATH_DEPTH: u32 = 8;
pub const MEMBER_PATHS_MAX: usize = 200_000;

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
            TypeId::ArrayOf(inner, n) => {
                let bi = self.type_info_at(inner, depth + 1);
                TypeInfo {
                    name: Some(format!("{}[{n}]", bi.name.unwrap_or_default())),
                    class: TypeClass::Array,
                    size: bi.size * n,
                    base: Some((**inner).clone()),
                    length: Some(*n),
                    resolved: t.clone(),
                }
            }
            TypeId::Die(d) => self.die_type_info(*d, depth),
        }
    }

    /// The definition of a struct/class/union/enum that DIE `d` only declares: the definition
    /// with the same (qualified) name and kind elsewhere, typically in another compilation unit.
    /// GCC writes a C++ class with virtual functions out in full only in the unit that has its
    /// vtable; C declares a struct in units that use it through pointers only.
    pub fn type_definition(&self, d: DieRef) -> Option<DieRef> {
        let (u, e) = self.entry(d)?;
        let kind = |tag| match tag {
            gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type => Some("struct"),
            gimli::DW_TAG_union_type => Some("union"),
            gimli::DW_TAG_enumeration_type => Some("enum"),
            _ => None,
        };
        let kw = kind(e.tag())?;
        let name = self.attr_str(u, e.attr_value(gimli::DW_AT_name)).filter(|n| !n.is_empty())?;
        let scoped = self.scoped_name(d).unwrap_or_else(|| name.clone());
        let key = if self.is_cpp(d) { scoped.clone() } else { format!("{kw} {name}") };
        // The index holds definitions only; a key can also be the tail of a longer C++ name.
        self.types_named(&key).iter().copied().find(|c| {
            let Some((cu, ce)) = self.entry(*c) else { return false };
            kind(ce.tag()) == Some(kw)
                && self.attr_str(cu, ce.attr_value(gimli::DW_AT_name)).as_deref() == Some(name.as_str())
                && self.scoped_name(*c).unwrap_or_else(|| name.clone()) == scoped
        })
    }

    fn die_type_info(&self, d: DieRef, depth: u32) -> TypeInfo {
        let t = TypeId::Die(d);
        let Some((u, e)) = self.entry(d) else { return Self::unknown_type(&t) };
        if matches!(e.attr_value(gimli::DW_AT_declaration), Some(AttributeValue::Flag(true))) {
            if let Some(def) = self.type_definition(d) {
                return self.die_type_info(def, depth + 1);
            }
        }
        let name = self.attr_str(u, e.attr_value(gimli::DW_AT_name)).filter(|n| !n.is_empty());
        // C++ names a type by its qualified name, without `struct`/`enum`.
        let cpp = self.is_cpp(d);
        let name = match self.scoped_name(d) {
            Some(q) if cpp => Some(q),
            _ => name,
        };
        let tagged = |kw: &str| match &name {
            Some(n) if cpp => n.clone(),
            Some(n) => format!("{kw} {n}"),
            None => format!("{kw} {{...}}"),
        };
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
                    gimli::DW_TAG_const_type => "const",
                    gimli::DW_TAG_volatile_type => "volatile",
                    _ => "",
                };
                // A qualified pointer (`int* const`) is not a pointer to a qualified type (`const int*`).
                let pointer = i.class == TypeClass::Pointer;
                i.name = i.name.map(|n| match q {
                    "" => n,
                    _ if pointer => format!("{n} {q}"),
                    _ => format!("{q} {n}"),
                });
                i
            }
            // A C++ reference is a pointer underneath; expressions read through it.
            gimli::DW_TAG_pointer_type | gimli::DW_TAG_reference_type | gimli::DW_TAG_rvalue_reference_type => {
                let bi = self.type_info_at(&inner, depth + 1);
                let sym = match e.tag() {
                    gimli::DW_TAG_reference_type => "&",
                    gimli::DW_TAG_rvalue_reference_type => "&&",
                    _ => "*",
                };
                TypeInfo {
                    name: Some(format!("{}{sym}", bi.name.unwrap_or_default())),
                    class: TypeClass::Pointer,
                    size: if size == 0 { 4 } else { size },
                    base: Some(inner),
                    length: None,
                    resolved: t,
                }
            }
            gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type | gimli::DW_TAG_union_type => {
                let kw = match e.tag() {
                    gimli::DW_TAG_union_type => "union",
                    gimli::DW_TAG_class_type => "class",
                    _ => "struct",
                };
                TypeInfo {
                    name: Some(tagged(kw)),
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
                    name: Some(tagged("enum")),
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

    /// Type of a variable, parameter or member DIE (C++: of the declaration a definition completes).
    pub fn var_type(&self, d: DieRef) -> TypeId {
        self.inherited_ref(d, gimli::DW_AT_type).map(TypeId::Die).unwrap_or(TypeId::Void)
    }

    /// The type DIE under typedefs and qualifiers.
    fn strip_typedefs(&self, t: &TypeId) -> Option<(DieRef, gimli::DwTag)> {
        let TypeId::Die(mut d) = t else { return None };
        for _ in 0..MAX_TYPE_DEPTH {
            let (_, e) = self.entry(d)?;
            match e.tag() {
                gimli::DW_TAG_typedef | gimli::DW_TAG_const_type | gimli::DW_TAG_volatile_type | gimli::DW_TAG_restrict_type => {
                    d = self.ref_attr(d, &e, gimli::DW_AT_type)?;
                }
                tag => return Some((d, tag)),
            }
        }
        None
    }

    /// Whether a type is a union (C++ names it without the keyword).
    pub fn is_union(&self, t: &TypeId) -> bool {
        self.strip_typedefs(t).is_some_and(|(_, tag)| tag == gimli::DW_TAG_union_type)
    }

    /// Whether a type is a C++ reference (`T&`, `T&&`), which expressions read through.
    pub fn is_reference(&self, t: &TypeId) -> bool {
        self.strip_typedefs(t).is_some_and(|(_, tag)| tag == gimli::DW_TAG_reference_type || tag == gimli::DW_TAG_rvalue_reference_type)
    }

    /// Members of a struct/union type (in declaration order); C++ base classes are unnamed
    /// members, static data members are left out.
    pub fn members(&self, t: &TypeId) -> Vec<MemberInfo> {
        let info = self.type_info(t);
        let TypeId::Die(d) = info.resolved else { return Vec::new() };
        if info.class != TypeClass::Composite {
            return Vec::new();
        }
        self.children(d)
            .into_iter()
            .filter(|(_, e)| e.tag() == gimli::DW_TAG_member || e.tag() == gimli::DW_TAG_inheritance)
            .filter(|(_, e)| !matches!(e.attr_value(gimli::DW_AT_declaration), Some(AttributeValue::Flag(true))))
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

    /// A C++ static data member of a class or of one of its base classes: the global that
    /// defines `Class::name`.
    pub fn static_member(&self, t: &TypeId, name: &str) -> Option<usize> {
        self.static_member_at(t, name, 0)
    }

    fn static_member_at(&self, t: &TypeId, name: &str, depth: u32) -> Option<usize> {
        let info = self.type_info(t);
        if info.class != TypeClass::Composite || depth > MAX_TYPE_DEPTH {
            return None;
        }
        let q = format!("{}::{name}", info.name?);
        let gi = self.global_indices(&q);
        let named: Vec<usize> = gi.iter().copied().filter(|&i| self.globals[i].name == q).collect();
        // A definition the linker dropped sits at address 0.
        if let Some(&i) = named.iter().find(|&&i| self.globals[i].addr != 0).or(named.first()) {
            return Some(i);
        }
        self.members(t).iter().filter(|m| m.name.is_none()).find_map(|base| self.static_member_at(&base.ty, name, depth + 1))
    }

    /// See [`Symbols::member_paths`]. Members of the globals an expression can name (a C
    /// identifier or a qualified C++ name), the one a lookup of a name picks where several have
    /// it; array elements and pointer targets are left out, base class and anonymous members
    /// are taken as the members they contain.
    pub(crate) fn build_member_paths(&self) -> Vec<MemberPath> {
        let mut out = Vec::new();
        for (i, g) in self.globals.iter().enumerate() {
            if g.addr == 0 || !crate::is_expression_name(&g.name) {
                continue;
            }
            let live = || self.global_indices(&g.name).iter().copied().filter(|&j| self.globals[j].addr != 0);
            if live().find(|&j| self.globals[j].external).or_else(|| live().next()) != Some(i) {
                continue;
            }
            let ty = self.global_type(i);
            if self.type_info(&ty).class == TypeClass::Composite {
                self.walk_members(&ty, &g.name, g.addr, i, None, 0, &mut out);
            }
            if out.len() >= MEMBER_PATHS_MAX {
                out.truncate(MEMBER_PATHS_MAX);
                break;
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_members(&self, ty: &TypeId, path: &str, addr: u32, global: usize, parent: Option<usize>, depth: u32, out: &mut Vec<MemberPath>) {
        if depth >= MEMBER_PATH_DEPTH || out.len() >= MEMBER_PATHS_MAX {
            return;
        }
        for m in self.members(ty) {
            let address = addr.wrapping_add(m.offset);
            match &m.name {
                None => self.walk_members(&m.ty, path, address, global, parent, depth + 1, out),
                Some(n) if n.starts_with("_vptr") => {}
                Some(n) => {
                    let expression = format!("{path}.{n}");
                    let nested = self.type_info(&m.ty).class == TypeClass::Composite;
                    let me = out.len();
                    out.push(MemberPath { expression: expression.clone(), address, size: m.size, global, parent, ty: m.ty.clone(), bits: m.bits });
                    if nested {
                        self.walk_members(&m.ty, &expression, address, global, Some(me), depth + 1, out);
                    }
                }
            }
        }
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
                        gimli::DW_TAG_enumeration_type => {
                            let declaration = matches!(e.attr_value(gimli::DW_AT_declaration), Some(AttributeValue::Flag(true)));
                            break if declaration { self.type_definition(cur).unwrap_or(cur) } else { cur };
                        }
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
