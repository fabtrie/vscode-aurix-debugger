//! What an ORTI file says, once parsed.
//!
//! The model is deliberately generic: nothing here knows that `TASK` or `STACK` are OSEK object
//! types. An ORTI file describes its own object types in the declaration section, and generators
//! invent their own - this file's MICROSAR OS declares `vs_ISR`, `vs_SPINLOCK`, `vs_EVENT`,
//! `vs_SCHEDULETABLE` and `vs_OS_CONFIG` next to the standard ones. A reader that hardcodes the
//! OSEK set drops half of what the generator exported.

/// The `VERSION` block.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Version {
    /// `KOIL = "2.3"` - the version of the OS description language.
    pub koil: Option<String>,
    /// `OSSEMANTICS = "ORTI", "2.3"` - the semantics name and its version.
    pub semantics: Option<String>,
    pub semantics_version: Option<String>,
    /// Any other `NAME = "a", "b";` line, kept in file order.
    pub other: Vec<(String, Vec<String>)>,
}

impl Version {
    /// Whether the file claims the (unofficial) ORTI 2.3 semantics, where the OS attributes may
    /// be arrays indexed by logical core. Indexed attributes are accepted regardless - the flag
    /// is worth reporting, not worth enforcing.
    pub fn is_2_3(&self) -> bool {
        self.semantics_version.as_deref() == Some("2.3")
    }
}

/// How an attribute's value is to be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrKind {
    /// `CTYPE "t" NAME` - the value is a C expression of that type.
    CType,
    /// `ENUM "t" [ ... ] NAME` - a C expression whose result is looked up in [`AttrDecl::enums`].
    Enum,
    /// `STRING NAME` - the value is literal text, **not** an expression. Confusing these is the
    /// easiest way to get an ORTI reader wrong: `ACTION = "SET_EVENT task ev"` and
    /// `PRIORITY = "STANDARD: 4"` are prose, while `STATE = "Cfg_Dyn.State"` is code, and only
    /// the declaration says which is which.
    String,
}

/// One enumerator of an `ENUM` attribute.
#[derive(Clone, Debug, PartialEq)]
pub struct EnumEntry {
    /// The text to display. **Not unique**: this file has `"Not evaluated"` and `"-"` twice in
    /// the same list, so an enum must be keyed by value, never by name.
    pub name: String,
    /// `"name" : object = ...` - the object this enumerator points at, for follow-the-link.
    pub link: Option<String>,
    pub value: EnumValue,
}

/// The right-hand side of an enumerator: either an ordinal or an expression to evaluate.
#[derive(Clone, Debug, PartialEq)]
pub enum EnumValue {
    /// `"OS_CORE_ID_0" = 0`
    Const(i64),
    /// `"IdleTask_OsCore0" = "&OsCfg_Trace_IdleTask_OsCore0"`. Often a plain constant in quotes
    /// (`"GetApplicationID" = "0x02"`), so try an integer literal before reaching for the
    /// evaluator.
    Expr(String),
}

/// One attribute of an object type, as declared in the `IMPLEMENTATION` section.
#[derive(Clone, Debug, PartialEq)]
pub struct AttrDecl {
    pub name: String,
    /// The trailing description string, for column headers.
    pub desc: String,
    pub kind: AttrKind,
    /// The C type named after `CTYPE` / `ENUM`, e.g. `"void *"`, `"unsigned char"`.
    pub ctype: Option<String>,
    /// Declared as `NAME[]`: the SMP extension's per-core array. Values then arrive as
    /// `NAME[0]`, `NAME[1]`, one per logical core.
    pub indexed: bool,
    /// `TOTRACE` - the attribute is meant to be recorded by a trace, not only read.
    pub totrace: bool,
    /// Empty unless `kind` is [`AttrKind::Enum`].
    pub enums: Vec<EnumEntry>,
}

impl AttrDecl {
    /// The display text for a value, or `None` when no enumerator matches (a bit set, or a value
    /// the target should not be holding).
    pub fn enum_name(&self, value: i64) -> Option<&str> {
        self.enums.iter().find(|e| e.value == EnumValue::Const(value)).map(|e| e.name.as_str())
    }

    /// The object an enumerator links to, by ordinal.
    pub fn enum_link(&self, value: i64) -> Option<&str> {
        self.enums.iter().find(|e| e.value == EnumValue::Const(value)).and_then(|e| e.link.as_deref())
    }
}

/// One object type, as declared in the `IMPLEMENTATION` section.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectDecl {
    /// `OS`, `TASK`, `vs_SPINLOCK`, ...
    pub name: String,
    /// The trailing description string, e.g. `"Stacks"` - a good panel title.
    pub desc: String,
    pub attrs: Vec<AttrDecl>,
}

impl ObjectDecl {
    pub fn attr(&self, name: &str) -> Option<&AttrDecl> {
        self.attrs.iter().find(|a| a.name == name)
    }
}

/// One attribute value of one object instance.
#[derive(Clone, Debug, PartialEq)]
pub struct AttrValue {
    pub name: String,
    /// `RUNNINGTASK[1]` - the logical core, for attributes declared `NAME[]`.
    pub index: Option<u32>,
    /// The raw text between the quotes: a C expression, or literal prose for a
    /// [`AttrKind::String`] attribute.
    pub value: String,
}

/// One object instance from the information section.
#[derive(Clone, Debug, PartialEq)]
pub struct Object {
    /// The object type, matching an [`ObjectDecl::name`].
    pub type_name: String,
    /// The instance name. The OS object may be unnamed, in which case this is empty.
    pub name: String,
    pub attrs: Vec<AttrValue>,
}

impl Object {
    /// A non-indexed attribute's raw value.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|a| a.name == name && a.index.is_none()).map(|a| a.value.as_str())
    }

    /// An indexed attribute's raw value for one logical core.
    pub fn attr_at(&self, name: &str, index: u32) -> Option<&str> {
        self.attrs.iter().find(|a| a.name == name && a.index == Some(index)).map(|a| a.value.as_str())
    }
}

/// A parsed ORTI file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OrtiFile {
    pub version: Version,
    /// The name after `IMPLEMENTATION`, e.g. `Vector_ORTI`.
    pub implementation: String,
    /// Object type declarations, in file order.
    pub decls: Vec<ObjectDecl>,
    /// Object instances, in file order.
    pub objects: Vec<Object>,
}

impl OrtiFile {
    pub fn decl(&self, type_name: &str) -> Option<&ObjectDecl> {
        self.decls.iter().find(|d| d.name == type_name)
    }

    /// Every instance of one object type, in file order.
    pub fn objects_of<'a>(&'a self, type_name: &'a str) -> impl Iterator<Item = &'a Object> {
        self.objects.iter().filter(move |o| o.type_name == type_name)
    }

    /// The single `OS` object, if the file has one.
    pub fn os(&self) -> Option<&Object> {
        self.objects.iter().find(|o| o.type_name == "OS")
    }

    /// How many logical cores the file describes.
    ///
    /// `vs_SMP_NUMCPU` when MICROSAR wrote it, otherwise the widest index used by any indexed OS
    /// attribute, otherwise one. A single-core ORTI 2.2 file has no indices at all and answers 1.
    pub fn core_count(&self) -> u32 {
        let Some(os) = self.os() else { return 1 };
        if let Some(n) = os.attr("vs_SMP_NUMCPU").and_then(|v| v.trim().parse::<u32>().ok()) {
            if n > 0 {
                return n;
            }
        }
        os.attrs.iter().filter_map(|a| a.index).max().map_or(1, |m| m + 1)
    }
}
