//! The ORTI file a session has loaded, and every attribute planned against the symbols.
//!
//! Planning is the expensive half - each attribute is parsed as a C expression and evaluated
//! against DWARF - so it happens once, when the file is loaded. A refresh afterwards is only the
//! reads the plans name.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use symbols::expr::Target;
use symbols::Symbols;

use crate::resolve::{plan, value_live, Attribute};

/// One object instance with its attributes planned.
pub struct PlannedObject {
    pub type_name: String,
    pub name: String,
    pub attrs: Vec<Attribute>,
}

/// The loaded ORTI file and everything derived from it.
pub struct Awareness {
    pub file: PathBuf,
    pub orti: crate::OrtiFile,
    pub objects: Vec<PlannedObject>,
    /// The ELF set the attribute expressions were resolved against.
    pub syms: Arc<Symbols>,
    /// Attributes that could not be planned, as `TYPE name.ATTR: reason`.
    pub errors: Vec<String>,
}

impl Awareness {
    /// Parses an ORTI file and plans every attribute against `syms`.
    ///
    /// `target` is read from while planning, to learn which branch of each conditional attribute
    /// is live and therefore which addresses to sample. A target that cannot be read is not an
    /// error: those attributes are recorded with their reason and planned again on first use, so
    /// loading an ORTI file before the board is powered still works.
    pub fn load(path: &Path, syms: Arc<Symbols>, target: &mut dyn Target) -> Result<Awareness, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let file = crate::parse(&text).map_err(|e| format!("{}:{e}", path.display()))?;

        let mut objects = Vec::with_capacity(file.objects.len());
        let mut errors = Vec::new();
        for obj in &file.objects {
            let Some(decl) = file.decl(&obj.type_name) else {
                errors.push(format!("{} {}: no declaration for this object type", obj.type_name, obj.name));
                continue;
            };
            let mut attrs = Vec::with_capacity(obj.attrs.len());
            for a in &obj.attrs {
                let Some(d) = decl.attr(&a.name) else {
                    errors.push(format!("{} {}.{}: not declared", obj.type_name, obj.name, a.name));
                    continue;
                };
                let planned = plan(&syms, target, d, a.index, &a.value);
                if let Some(e) = &planned.error {
                    errors.push(format!("{} {}.{}: {e}", obj.type_name, obj.name, a.name));
                }
                attrs.push(planned);
            }
            objects.push(PlannedObject { type_name: obj.type_name.clone(), name: obj.name.clone(), attrs });
        }
        Ok(Awareness { file: path.to_path_buf(), orti: file, objects, syms, errors })
    }

    pub fn objects_of<'a>(&'a self, type_name: &'a str) -> impl Iterator<Item = &'a PlannedObject> {
        self.objects.iter().filter(move |o| o.type_name == type_name)
    }

    /// Reads one attribute, planning it again first if the plan never worked.
    ///
    /// The retry is what makes loading an ORTI file against an unpowered board harmless: the
    /// attribute is planned properly the first time somebody actually looks at it.
    pub fn read(&self, decl_type: &str, attr: &Attribute, target: &mut dyn Target) -> Result<Vec<u8>, String> {
        if attr.error.is_none() {
            return value_live(&self.syms, target, attr);
        }
        let d = self.orti.decl(decl_type).and_then(|d| d.attr(&attr.name)).ok_or("no declaration")?;
        let fresh = plan(&self.syms, target, d, attr.index, &attr.raw);
        match fresh.error {
            Some(e) => Err(e),
            None => value_live(&self.syms, target, &fresh),
        }
    }

    /// Reads that a full refresh of one object type would make, ignoring coalescing. Reported so
    /// a caller can see what a panel costs before opening it.
    pub fn live_reads(&self, type_name: &str) -> u32 {
        self.objects_of(type_name).flat_map(|o| o.attrs.iter()).filter(|a| a.plan.is_live()).map(|a| a.plan.reads().len() as u32).sum()
    }
}
