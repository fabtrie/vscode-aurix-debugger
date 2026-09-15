//! Dumps DIE subtrees with their attributes and location lists, following
//! type references, for exploring compiler-specific DWARF.
//!
//! Usage: dwarf_dump <elf> <what>...
//!   <function name> | v:<global name> | g<decimal .debug_info offset> | <unit>:0x<unit offset>

use std::collections::HashSet;
use std::path::Path;

use gimli::{AttributeValue, UnitOffset};
use symbols::Symbols;

const MAX_FOLLOW: usize = 40;

fn unit_of_global(s: &Symbols, goff: usize) -> (usize, usize) {
    let u = s.units.iter().rposition(|u| u.header.debug_info_offset().is_some_and(|o| o.0 <= goff)).unwrap();
    (u, goff - s.units[u].header.debug_info_offset().unwrap().0)
}

fn global_of(s: &Symbols, unit: usize, off: usize) -> usize {
    s.units[unit].header.debug_info_offset().unwrap().0 + off
}

fn ops_text(e: &gimli::Expression<symbols::R>, enc: gimli::Encoding) -> String {
    let mut ops = e.operations(enc);
    let mut out = Vec::new();
    while let Ok(Some(op)) = ops.next() {
        out.push(format!("{op:?}"));
    }
    out.join(", ")
}

/// Dumps one DIE subtree; returns referenced DIEs as global offsets.
fn dump(s: &Symbols, unit_idx: usize, off: usize) -> Vec<usize> {
    let mut refs = Vec::new();
    let unit = s.dwarf.unit(s.units[unit_idx].header).unwrap();
    let mut cursor = unit.entries_at_offset(UnitOffset(off)).unwrap();
    let mut base = None;
    let mut lines = 0;
    while let Some(entry) = cursor.next_dfs().unwrap() {
        let depth = entry.depth();
        let base_depth = *base.get_or_insert(depth);
        if depth < base_depth || (depth == base_depth && entry.offset().0 != off) {
            break;
        }
        lines += 1;
        if lines > 60 {
            println!("  ...");
            break;
        }
        let indent = "  ".repeat((depth - base_depth) as usize);
        println!("{indent}<g{} u{}:{:#x}> {}", global_of(s, unit_idx, entry.offset().0), unit_idx, entry.offset().0, entry.tag());
        for attr in entry.attrs() {
            let v = attr.value();
            let text = match v {
                AttributeValue::DebugStrRef(_) | AttributeValue::String(_) => {
                    s.dwarf.attr_string(&unit, v).map(|x| format!("{:?}", x.to_string_lossy())).unwrap_or_default()
                }
                AttributeValue::Exprloc(ref e) => format!("expr [{}]", ops_text(e, unit.encoding())),
                AttributeValue::DebugInfoRef(r) => {
                    refs.push(r.0);
                    format!("g{}", r.0)
                }
                AttributeValue::UnitRef(r) => {
                    refs.push(global_of(s, unit_idx, r.0));
                    format!("g{}", global_of(s, unit_idx, r.0))
                }
                _ => format!("{v:?}"),
            };
            println!("{indent}    {} = {text}", attr.name());
            if attr.name() == gimli::DW_AT_location || attr.name() == gimli::DW_AT_frame_base {
                if let Ok(Some(mut locs)) = s.dwarf.attr_locations(&unit, v) {
                    while let Ok(Some(l)) = locs.next() {
                        println!("{indent}      [{:#x}, {:#x}) {}", l.range.begin, l.range.end, ops_text(&l.data, unit.encoding()));
                    }
                }
            }
        }
    }
    refs
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let s = Symbols::load(Path::new(&args[1])).unwrap();
    for what in &args[2..] {
        println!("################ {what}");
        let (unit_idx, off) = if let Some((u, o)) = what.split_once(":0x") {
            (u.parse::<usize>().unwrap(), usize::from_str_radix(o, 16).unwrap())
        } else if let Some(name) = what.strip_prefix("v:") {
            let g = s.globals_named(name).into_iter().next().expect("global not found");
            println!("global {} @ {:#x}", g.name, g.addr);
            (g.unit, g.die.0)
        } else if let Some(g) = what.strip_prefix('g').and_then(|x| x.parse::<usize>().ok()) {
            unit_of_global(&s, g)
        } else {
            let f = s.functions_named(what).into_iter().next().expect("function not found");
            println!("function {} {:#x}..{:#x}", f.name, f.low, f.high);
            (f.unit, f.die.0)
        };
        let mut seen = HashSet::new();
        seen.insert(global_of(&s, unit_idx, off));
        let mut queue = dump(&s, unit_idx, off);
        while let Some(g) = queue.pop() {
            if seen.len() >= MAX_FOLLOW || !seen.insert(g) {
                continue;
            }
            let (u, o) = unit_of_global(&s, g);
            queue.extend(dump(&s, u, o));
        }
    }
}
