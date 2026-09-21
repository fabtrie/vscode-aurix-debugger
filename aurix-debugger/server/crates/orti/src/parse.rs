//! The ORTI grammar.
//!
//! ```text
//! file      := version? implementation* object*
//! version   := "VERSION" "{" ( ident "=" str ("," str)* ";" )* "}" ";"
//! impl      := "IMPLEMENTATION" ident "{" objdecl* "}" ";"
//! objdecl   := ident "{" attrdecl* "}" "," str ";"
//! attrdecl  := "TOTRACE"? ( "ENUM" str "[" enums "]" | "CTYPE" str | "STRING" )
//!              ident ( "[" "]" )? "," str ";"
//! enums     := enum ( "," enum )*                       -- a trailing comma is tolerated
//! enum      := str ( ":" ident )? "=" ( int | str )
//! object    := ident ident? "{" ( ident ( "[" int "]" )? "=" ( str | int ) ";" )* "}" ";"
//! ```
//!
//! The parser is forgiving in the places generators differ - an unnamed OS object, a trailing
//! comma, a missing `;` after a closing brace - and strict about the places where being wrong
//! would silently produce wrong values, such as the `CTYPE`/`ENUM`/`STRING` keyword that decides
//! whether an attribute holds an expression or prose.

use crate::lex::{lex, line_col, Tok, Token};
use crate::model::*;

#[derive(Clone, Debug, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.column, self.message)
    }
}

impl std::error::Error for ParseError {}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    pos: usize,
}

impl Parser<'_> {
    fn err<T>(&self, message: impl Into<String>) -> Result<T, ParseError> {
        let at = self.toks.get(self.pos).map_or(self.src.len(), |t| t.pos);
        let (line, column) = line_col(self.src, at);
        Err(ParseError { message: message.into(), line, column })
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.tok)
    }

    fn is_punct(&self, c: char) -> bool {
        matches!(self.peek(), Some(Tok::Punct(x)) if *x == c)
    }

    fn eat_punct(&mut self, c: char) -> bool {
        let yes = self.is_punct(c);
        if yes {
            self.pos += 1;
        }
        yes
    }

    fn expect_punct(&mut self, c: char) -> Result<(), ParseError> {
        if self.eat_punct(c) {
            Ok(())
        } else {
            self.err(format!("expected '{c}'"))
        }
    }

    /// Consumes a specific keyword if it is next.
    fn eat_word(&mut self, w: &str) -> bool {
        let yes = matches!(self.peek(), Some(Tok::Ident(x)) if x == w);
        if yes {
            self.pos += 1;
        }
        yes
    }

    fn ident(&mut self) -> Result<String, ParseError> {
        match self.peek() {
            Some(Tok::Ident(n)) => {
                let n = n.clone();
                self.pos += 1;
                Ok(n)
            }
            _ => self.err("expected a name"),
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        match self.peek() {
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.pos += 1;
                Ok(s)
            }
            _ => self.err("expected a quoted string"),
        }
    }

    fn int(&mut self) -> Result<i64, ParseError> {
        match self.peek() {
            Some(Tok::Int(v)) => {
                let v = *v;
                self.pos += 1;
                Ok(v)
            }
            _ => self.err("expected a number"),
        }
    }

    /// A `;` that generators sometimes leave off after a closing brace.
    fn optional_semi(&mut self) {
        while self.eat_punct(';') {}
    }

    fn version(&mut self) -> Result<Version, ParseError> {
        let mut v = Version::default();
        self.expect_punct('{')?;
        while !self.is_punct('}') {
            let name = self.ident()?;
            self.expect_punct('=')?;
            let mut values = vec![self.string()?];
            while self.eat_punct(',') {
                values.push(self.string()?);
            }
            self.expect_punct(';')?;
            match name.as_str() {
                "KOIL" => v.koil = values.first().cloned(),
                "OSSEMANTICS" => {
                    v.semantics = values.first().cloned();
                    v.semantics_version = values.get(1).cloned();
                }
                _ => v.other.push((name, values)),
            }
        }
        self.expect_punct('}')?;
        self.optional_semi();
        Ok(v)
    }

    fn enum_entry(&mut self) -> Result<EnumEntry, ParseError> {
        let name = self.string()?;
        // `"IdleTask" : IdleTask = "&OsCfg_..."` - the enumerator points at another object, which
        // is what makes a value in a table clickable.
        let link = if self.eat_punct(':') { Some(self.ident()?) } else { None };
        self.expect_punct('=')?;
        let value = match self.peek() {
            Some(Tok::Int(v)) => {
                let v = *v;
                self.pos += 1;
                EnumValue::Const(v)
            }
            Some(Tok::Str(_)) => EnumValue::Expr(self.string()?),
            _ => return self.err("expected a number or a quoted expression"),
        };
        Ok(EnumEntry { name, link, value })
    }

    fn attr_decl(&mut self) -> Result<AttrDecl, ParseError> {
        let totrace = self.eat_word("TOTRACE");
        let (kind, ctype, enums) = if self.eat_word("ENUM") {
            let ctype = self.string()?;
            self.expect_punct('[')?;
            let mut enums = Vec::new();
            while !self.is_punct(']') {
                enums.push(self.enum_entry()?);
                if !self.eat_punct(',') {
                    break;
                }
            }
            self.expect_punct(']')?;
            (AttrKind::Enum, Some(ctype), enums)
        } else if self.eat_word("CTYPE") {
            (AttrKind::CType, Some(self.string()?), Vec::new())
        } else if self.eat_word("STRING") {
            (AttrKind::String, None, Vec::new())
        } else {
            return self.err("expected CTYPE, ENUM or STRING");
        };

        let name = self.ident()?;
        // `NAME[]`: the SMP extension's per-core array.
        let indexed = if self.eat_punct('[') {
            self.expect_punct(']')?;
            true
        } else {
            false
        };
        self.expect_punct(',')?;
        let desc = self.string()?;
        self.expect_punct(';')?;
        Ok(AttrDecl { name, desc, kind, ctype, indexed, totrace, enums })
    }

    fn object_decl(&mut self) -> Result<ObjectDecl, ParseError> {
        let name = self.ident()?;
        self.expect_punct('{')?;
        let mut attrs = Vec::new();
        while !self.is_punct('}') {
            attrs.push(self.attr_decl()?);
        }
        self.expect_punct('}')?;
        // `}, "Stacks";` - the description is what a panel should call the collection.
        let desc = if self.eat_punct(',') { self.string()? } else { String::new() };
        self.optional_semi();
        Ok(ObjectDecl { name, desc, attrs })
    }

    fn implementation(&mut self, out: &mut OrtiFile) -> Result<(), ParseError> {
        out.implementation = self.ident()?;
        self.expect_punct('{')?;
        while !self.is_punct('}') {
            out.decls.push(self.object_decl()?);
        }
        self.expect_punct('}')?;
        self.optional_semi();
        Ok(())
    }

    fn object(&mut self, type_name: String) -> Result<Object, ParseError> {
        // The instance name is optional: ORTI allows a bare `OS { ... }`.
        let name = if self.is_punct('{') { String::new() } else { self.ident()? };
        self.expect_punct('{')?;
        let mut attrs = Vec::new();
        while !self.is_punct('}') {
            let name = self.ident()?;
            let index = if self.eat_punct('[') {
                let i = self.int()?;
                self.expect_punct(']')?;
                if i < 0 {
                    return self.err("a core index cannot be negative");
                }
                Some(i as u32)
            } else {
                None
            };
            self.expect_punct('=')?;
            // Values are quoted, but a generator writing a bare ordinal should not fail the file.
            let value = match self.peek() {
                Some(Tok::Str(_)) => self.string()?,
                Some(Tok::Int(_)) => self.int()?.to_string(),
                _ => return self.err("expected a quoted value"),
            };
            self.expect_punct(';')?;
            attrs.push(AttrValue { name, index, value });
        }
        self.expect_punct('}')?;
        self.optional_semi();
        Ok(Object { type_name, name, attrs })
    }
}

/// Parses an ORTI file.
pub fn parse(src: &str) -> Result<OrtiFile, ParseError> {
    let toks = lex(src).map_err(|e| {
        let (line, column) = line_col(src, e.pos);
        ParseError { message: e.message, line, column }
    })?;
    let mut p = Parser { src, toks, pos: 0 };
    let mut out = OrtiFile::default();

    while p.pos < p.toks.len() {
        let word = p.ident()?;
        match word.as_str() {
            "VERSION" => out.version = p.version()?,
            "IMPLEMENTATION" => p.implementation(&mut out)?,
            // Anything else at the top level is an object instance of a declared type.
            _ => {
                let obj = p.object(word)?;
                out.objects.push(obj);
            }
        }
    }
    Ok(out)
}
