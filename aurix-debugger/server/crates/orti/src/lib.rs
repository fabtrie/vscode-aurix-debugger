//! ORTI files: the OS awareness description an AUTOSAR/OSEK generator emits beside the ELF.
//!
//! An ORTI file has two halves. The `IMPLEMENTATION` section *declares* object types and their
//! attributes - what a debugger should show and how to interpret it. The information section then
//! gives one entry per object, where every attribute value is a C expression to evaluate against
//! the target, or literal prose. Nothing about the OS is built in: the file describes itself.
//!
//! ```
//! let f = orti::parse(r#"
//!     VERSION { KOIL = "2.3"; OSSEMANTICS = "ORTI", "2.3"; };
//!     IMPLEMENTATION Vector_ORTI {
//!       OS {
//!         CTYPE "int" vs_SMP_NUMCPU, "Number of CPUs";
//!         ENUM "void *" ["Idle" = 0, "Main" = 1] RUNNINGTASK[], "Running Task";
//!       }, "OS";
//!     };
//!     OS TC37x {
//!       vs_SMP_NUMCPU = "2";
//!       RUNNINGTASK[0] = "OsCfg_Trace_OsCore0_Dyn.CurrentTask";
//!       RUNNINGTASK[1] = "OsCfg_Trace_OsCore1_Dyn.CurrentTask";
//!     };
//! "#).unwrap();
//! assert_eq!(f.core_count(), 2);
//! assert_eq!(f.os().unwrap().attr_at("RUNNINGTASK", 1), Some("OsCfg_Trace_OsCore1_Dyn.CurrentTask"));
//! assert_eq!(f.decl("OS").unwrap().attr("RUNNINGTASK").unwrap().enum_name(1), Some("Main"));
//! ```
//!
//! # Versions
//!
//! ORTI 2.1 and 2.2 are the published OSEK/VDX specification. **2.3 is not published**: it is
//! Lauterbach's unofficial SMP extension, which makes the `OS` attributes arrays indexed by
//! logical core - `RUNNINGTASK[0]`, `RUNNINGTASK[1]` - so that one file can describe a multicore
//! system. Vector's generator writes `vs_SMP_NUMCPU` and `vs_COREID[]` alongside, which is what
//! lets a reader map ORTI core indices onto debug targets. AUTOSAR ARTI is the official
//! successor; this crate does not implement it.
//!
//! The parser accepts indexed attributes whatever the declared version says, because the index is
//! unambiguous and a generator getting the `VERSION` block wrong should not cost the user their
//! second core.

mod awareness;
mod lex;
mod model;
mod parse;
pub mod resolve;
pub mod stack;

pub use awareness::{Awareness, PlannedObject};
pub use lex::{line_col, LexError};
pub use model::{AttrDecl, AttrKind, AttrValue, EnumEntry, EnumValue, Object, ObjectDecl, OrtiFile, Version};
pub use parse::{parse, ParseError};
pub use resolve::{plan, value, value_live, Attribute, Plan, Read, Shown, Snapshot};
pub use stack::{measure as measure_stacks, Direction, StackUsage};

#[cfg(test)]
mod tests;
