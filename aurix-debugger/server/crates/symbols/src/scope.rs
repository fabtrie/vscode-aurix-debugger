//! Local variables of a function at a PC, and where they live.
//!
//! Locals come from DWARF (a DIE per variable, located by its DWARF expression) or from a table
//! that a symbol source without DWARF fills ([`TableLocal`]: SDCC's `.cdb` for the SCR, and any
//! other format later). Both are a [`VarRef`]; [`Symbols::locals_at`], [`Symbols::var_location`],
//! [`Symbols::local_type`] and [`Symbols::local_name`] treat them alike, so frontends need not know
//! where a local came from.

use gimli::{AttributeValue, Operation};

use crate::types::{attr_int, TypeId};
use crate::{Die, DieRef, Symbols, R};

/// A local variable or parameter: a DWARF entry or an entry of [`Symbols::table_locals`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VarRef {
    Die(DieRef),
    Table(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Local {
    pub var: VarRef,
    pub name: String,
    pub ty: TypeId,
    pub param: bool,
}

/// Where a table local lives, independent of a debug format. Register numbers are DWARF numbers
/// of the core (the numbering of the frame's registers, see [`Regs`]).
#[derive(Clone, Debug, PartialEq)]
pub enum LocDesc {
    Memory(u32),
    /// The value's bytes from registers, least significant first: each `(register, bytes)` gives
    /// the low `bytes` bytes of that register. One piece covering the value is the register itself.
    Registers(Vec<(u16, u8)>),
    /// A known value, not stored anywhere.
    Value(Vec<u8>),
    /// No storage (optimized away).
    OptimizedOut,
}

/// A local variable or parameter from a symbol source without DWARF.
#[derive(Clone, Debug, PartialEq)]
pub struct TableLocal {
    /// The PC range it is visible in: its function, or a smaller range when the source knows it.
    pub low: u32,
    pub high: u32,
    pub name: String,
    pub ty: TypeId,
    pub param: bool,
    pub loc: LocDesc,
}

/// Where a variable lives.
#[derive(Clone, Debug, PartialEq)]
pub enum Loc {
    Memory(u32),
    Register(u16),
    /// Known value, not stored anywhere (constant, DW_OP_stack_value).
    Value(Vec<u8>),
    OptimizedOut,
}

/// Register values of a frame by DWARF number (D0-D15 = 0-15, A0-A15 = 16-31).
pub type Regs<'a> = &'a dyn Fn(u16) -> Option<u32>;
pub type MemRead<'a> = &'a mut dyn FnMut(u32, usize) -> Option<Vec<u8>>;

impl Symbols {
    fn die_contains(&self, unit: &gimli::Unit<R>, e: &Die, pc: u32) -> bool {
        if let Some(AttributeValue::Addr(low)) = e.attr_value(gimli::DW_AT_low_pc) {
            let high = match e.attr_value(gimli::DW_AT_high_pc) {
                Some(AttributeValue::Addr(h)) => h,
                Some(v) => low + v.udata_value().unwrap_or(0),
                None => low + 1,
            };
            return (pc as u64) >= low && (pc as u64) < high;
        }
        if let Some(v) = e.attr_value(gimli::DW_AT_ranges) {
            if let Ok(Some(mut it)) = self.dwarf.attr_ranges(unit, v) {
                while let Ok(Some(r)) = it.next() {
                    if (pc as u64) >= r.begin && (pc as u64) < r.end {
                        return true;
                    }
                }
                return false;
            }
        }
        true
    }

    /// Parameters and local variables visible at `pc`, outermost scope first
    /// (a later entry shadows an earlier one with the same name): from DWARF, then from the table.
    pub fn locals_at(&self, pc: u32) -> Vec<Local> {
        let mut out = self.dwarf_locals_at(pc);
        out.extend(self.table_locals.iter().enumerate().filter(|(_, l)| pc >= l.low && pc < l.high).map(|(i, l)| Local {
            var: VarRef::Table(i),
            name: l.name.clone(),
            ty: l.ty.clone(),
            param: l.param,
        }));
        out
    }

    /// The type of a local.
    pub fn local_type(&self, v: VarRef) -> TypeId {
        match v {
            VarRef::Die(d) => self.var_type(d),
            VarRef::Table(i) => self.table_locals.get(i).map_or(TypeId::Void, |l| l.ty.clone()),
        }
    }

    /// The name of a local.
    pub fn local_name(&self, v: VarRef) -> Option<String> {
        match v {
            VarRef::Die(d) => self.die_name(d),
            VarRef::Table(i) => self.table_locals.get(i).map(|l| l.name.clone()),
        }
    }

    fn dwarf_locals_at(&self, pc: u32) -> Vec<Local> {
        let Some(f) = self.function_at(pc) else { return Vec::new() };
        if f.unit == usize::MAX {
            return Vec::new();
        }
        let Some(u) = self.unit(f.unit) else { return Vec::new() };
        let Ok(mut cur) = u.entries_at_offset(f.die) else { return Vec::new() };
        let mut out = Vec::new();
        let mut base = None;
        let mut skip: Option<isize> = None;
        while let Ok(Some(e)) = cur.next_dfs() {
            let depth = e.depth();
            let b = *base.get_or_insert(depth);
            if depth <= b && e.offset() != f.die {
                break;
            }
            if let Some(s) = skip {
                if depth > s {
                    continue;
                }
                skip = None;
            }
            let here = DieRef { unit: f.unit, off: e.offset().0 };
            match e.tag() {
                gimli::DW_TAG_lexical_block => {
                    if !self.die_contains(u, e, pc) {
                        skip = Some(depth);
                    }
                }
                gimli::DW_TAG_subprogram | gimli::DW_TAG_inlined_subroutine if e.offset() != f.die => skip = Some(depth),
                gimli::DW_TAG_formal_parameter | gimli::DW_TAG_variable => {
                    if matches!(e.attr_value(gimli::DW_AT_declaration), Some(AttributeValue::Flag(true))) {
                        continue;
                    }
                    let Some(name) = self.attr_str(u, e.attr_value(gimli::DW_AT_name)) else { continue };
                    let ty = self.ref_attr(here, e, gimli::DW_AT_type).map(TypeId::Die).unwrap_or(TypeId::Void);
                    out.push(Local { var: VarRef::Die(here), name, ty, param: e.tag() == gimli::DW_TAG_formal_parameter });
                }
                _ => {}
            }
        }
        out
    }

    /// Frame base (DW_AT_frame_base) of the function containing `pc`.
    pub fn frame_base(&self, pc: u32, regs: Regs, mem: MemRead) -> Option<u32> {
        let f = self.function_at(pc)?;
        let (u, e) = self.entry(f.die_ref())?;
        let v = e.attr_value(gimli::DW_AT_frame_base)?;
        let expr = self.location_expr(u, v, pc).ok()??;
        match eval_location(expr, u.encoding(), regs, None, mem).ok()? {
            Loc::Memory(a) => Some(a),
            Loc::Register(r) => regs(r),
            _ => None,
        }
    }

    /// The expression of a location attribute that applies at `pc`
    /// (Ok(None): not available at this PC).
    fn location_expr(&self, u: &gimli::Unit<R>, v: AttributeValue<R>, pc: u32) -> Result<Option<gimli::Expression<R>>, String> {
        match v {
            AttributeValue::Exprloc(x) => Ok(Some(x)),
            AttributeValue::Block(b) => Ok(Some(gimli::Expression(b))),
            other => {
                let mut it = self.dwarf.attr_locations(u, other).map_err(|e| e.to_string())?.ok_or("unsupported location attribute")?;
                while let Some(l) = it.next().map_err(|e| e.to_string())? {
                    if (pc as u64) >= l.range.begin && (pc as u64) < l.range.end {
                        return Ok(Some(l.data));
                    }
                }
                Ok(None)
            }
        }
    }

    /// Location of a local variable or parameter at `pc`.
    pub fn var_location(&self, var: VarRef, pc: u32, regs: Regs, mem: MemRead) -> Result<Loc, String> {
        match var {
            VarRef::Die(d) => self.die_location(d, pc, regs, mem),
            VarRef::Table(i) => {
                let l = self.table_locals.get(i).ok_or("invalid variable reference")?;
                let size = self.type_info(&l.ty).size as usize;
                Ok(table_location(&l.loc, size, regs))
            }
        }
    }

    /// Location of a variable or parameter DIE at `pc`.
    fn die_location(&self, var: DieRef, pc: u32, regs: Regs, mem: MemRead) -> Result<Loc, String> {
        let (u, e) = self.entry(var).ok_or("invalid variable reference")?;
        if let Some(cv) = e.attr_value(gimli::DW_AT_const_value) {
            return Ok(match cv {
                AttributeValue::Block(b) => Loc::Value(b.slice().to_vec()),
                other => match attr_int(&other) {
                    Some(x) => Loc::Value(x.to_le_bytes().to_vec()),
                    None => Loc::OptimizedOut,
                },
            });
        }
        let Some(v) = e.attr_value(gimli::DW_AT_location) else { return Ok(Loc::OptimizedOut) };
        let Some(expr) = self.location_expr(u, v, pc)? else { return Ok(Loc::OptimizedOut) };
        let needs_fb = {
            let mut ops = expr.operations(u.encoding());
            let mut found = false;
            while let Ok(Some(op)) = ops.next() {
                found |= matches!(op, Operation::FrameOffset { .. });
            }
            found
        };
        let fb = if needs_fb { self.frame_base(pc, regs, mem) } else { None };
        eval_location(expr, u.encoding(), regs, fb, mem)
    }
}

/// A table location as a [`Loc`]: one register holding the whole value stays a register (so it
/// can be written); pieces from several registers are read into a value.
pub fn table_location(loc: &LocDesc, size: usize, regs: Regs) -> Loc {
    match loc {
        LocDesc::Memory(a) => Loc::Memory(*a),
        LocDesc::Value(b) => Loc::Value(b.clone()),
        LocDesc::OptimizedOut => Loc::OptimizedOut,
        LocDesc::Registers(p) if p.len() == 1 && p[0].1 as usize >= size => Loc::Register(p[0].0),
        LocDesc::Registers(p) => {
            let mut b = Vec::with_capacity(size);
            for &(r, n) in p {
                match regs(r) {
                    Some(v) => b.extend_from_slice(&v.to_le_bytes()[..(n as usize).min(4)]),
                    None => return Loc::OptimizedOut,
                }
            }
            b.resize(size, 0);
            Loc::Value(b)
        }
    }
}

/// Evaluates a DWARF location description.
pub fn eval_location(expr: gimli::Expression<R>, enc: gimli::Encoding, regs: Regs, frame_base: Option<u32>, mem: MemRead) -> Result<Loc, String> {
    let mut stack: Vec<u64> = Vec::new();
    let mut ops = expr.operations(enc);
    fn pop(stack: &mut Vec<u64>) -> Result<u64, String> {
        stack.pop().ok_or_else(|| "DWARF expression stack underflow".to_string())
    }
    while let Some(op) = ops.next().map_err(|e| e.to_string())? {
        match op {
            Operation::Address { address } => stack.push(address),
            // A register location; pieces after it are not supported.
            Operation::Register { register } => return Ok(Loc::Register(register.0)),
            Operation::RegisterOffset { register, offset, .. } => {
                let v = regs(register.0).ok_or_else(|| format!("register r{} not available in this frame", register.0))?;
                stack.push((v as i64).wrapping_add(offset) as u64);
            }
            Operation::FrameOffset { offset } => {
                let fb = frame_base.ok_or("frame base not available")?;
                stack.push((fb as i64).wrapping_add(offset) as u64);
            }
            Operation::PlusConstant { value } => {
                let t = pop(&mut stack)?;
                stack.push(t.wrapping_add(value));
            }
            Operation::UnsignedConstant { value } => stack.push(value),
            Operation::SignedConstant { value } => stack.push(value as u64),
            Operation::Plus | Operation::Minus | Operation::Mul | Operation::And | Operation::Or | Operation::Xor | Operation::Shl | Operation::Shr => {
                let b = pop(&mut stack)?;
                let a = pop(&mut stack)?;
                stack.push(match op {
                    Operation::Plus => a.wrapping_add(b),
                    Operation::Minus => a.wrapping_sub(b),
                    Operation::Mul => a.wrapping_mul(b),
                    Operation::And => a & b,
                    Operation::Or => a | b,
                    Operation::Xor => a ^ b,
                    Operation::Shl => a.wrapping_shl(b as u32),
                    _ => a.wrapping_shr(b as u32),
                });
            }
            Operation::Neg => {
                let a = pop(&mut stack)?;
                stack.push((a as i64).wrapping_neg() as u64);
            }
            Operation::Not => {
                let a = pop(&mut stack)?;
                stack.push(!a);
            }
            Operation::Deref { size, .. } => {
                let a = pop(&mut stack)?;
                let n = if size == 0 { 4 } else { (size as usize).min(8) };
                let b = mem(a as u32, n).ok_or_else(|| format!("cannot read memory at 0x{a:08x}"))?;
                let mut w = [0u8; 8];
                w[..n.min(b.len())].copy_from_slice(&b[..n.min(b.len())]);
                stack.push(u64::from_le_bytes(w));
            }
            Operation::StackValue => return Ok(Loc::Value(pop(&mut stack)?.to_le_bytes().to_vec())),
            Operation::ImplicitValue { data } => return Ok(Loc::Value(data.slice().to_vec())),
            Operation::Piece { .. } => break,
            Operation::Nop => {}
            other => return Err(format!("unsupported DWARF operation {other:?}")),
        }
    }
    stack.pop().map(|a| Loc::Memory(a as u32)).ok_or_else(|| "empty location expression".into())
}
