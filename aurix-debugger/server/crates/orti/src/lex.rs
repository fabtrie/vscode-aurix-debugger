//! Tokens of the ORTI language.
//!
//! The grammar is small enough that the lexer is the interesting half: the only real subtleties
//! are that strings carry C expressions (so their contents are kept verbatim, escapes and all)
//! and that comments appear between any two tokens, including inside enumerator lists.

/// Byte offset into the source, for error messages.
pub type Pos = usize;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    /// An identifier or keyword: `TASK`, `ENUM`, `vs_SMP_NUMCPU`.
    Ident(String),
    /// A quoted string, unescaped.
    Str(String),
    /// An integer literal, decimal or `0x` hex, with an optional sign.
    Int(i64),
    Punct(char),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub pos: Pos,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LexError {
    pub message: String,
    pub pos: Pos,
}

const PUNCTS: &[char] = &['{', '}', '[', ']', '(', ')', ',', ';', '=', ':'];

/// Skips whitespace and comments, returning the offset of the next real character.
fn skip_trivia(b: &[u8], mut i: usize) -> Result<usize, LexError> {
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b[i..].starts_with(b"/*") {
            let start = i;
            i += 2;
            loop {
                if i + 1 >= b.len() {
                    return Err(LexError { message: "unterminated /* comment".into(), pos: start });
                }
                if b[i] == b'*' && b[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        return Ok(i);
    }
}

/// A string literal, from the opening quote. Backslash escapes are honoured so that a quote can
/// appear inside an expression, but everything else is kept as written: the contents are C source
/// for the expression evaluator, not text for a human.
fn string(b: &[u8], i: &mut usize) -> Result<String, LexError> {
    let start = *i;
    *i += 1;
    let mut out = String::new();
    loop {
        let Some(&c) = b.get(*i) else {
            return Err(LexError { message: "unterminated string".into(), pos: start });
        };
        match c {
            b'"' => {
                *i += 1;
                return Ok(out);
            }
            b'\\' => {
                let Some(&e) = b.get(*i + 1) else {
                    return Err(LexError { message: "unterminated string".into(), pos: start });
                };
                out.push(match e {
                    b'n' => '\n',
                    b't' => '\t',
                    b'r' => '\r',
                    other => other as char,
                });
                *i += 2;
            }
            _ => {
                out.push(c as char);
                *i += 1;
            }
        }
    }
}

fn integer(b: &[u8], i: &mut usize) -> Result<i64, LexError> {
    let start = *i;
    let neg = b[*i] == b'-';
    if neg {
        *i += 1;
    }
    let (radix, digits_at) = if b[*i..].starts_with(b"0x") || b[*i..].starts_with(b"0X") { (16, *i + 2) } else { (10, *i) };
    *i = digits_at;
    while *i < b.len() && (b[*i] as char).is_digit(radix) {
        *i += 1;
    }
    if *i == digits_at {
        return Err(LexError { message: "expected a number".into(), pos: start });
    }
    let text = std::str::from_utf8(&b[digits_at..*i]).unwrap_or_default();
    // Hex constants up to 0xFFFFFFFFFFFFFFFF are written in ORTI files as unsigned; wrap them
    // into i64 rather than failing, since only the bit pattern matters for an enumerator.
    let mag = i64::from_str_radix(text, radix).or_else(|_| u64::from_str_radix(text, radix).map(|v| v as i64));
    let Ok(mag) = mag else {
        return Err(LexError { message: format!("number out of range: {text}"), pos: start });
    };
    // Suffixes (`0x10U`, `20L`) are legal C and harmless here.
    while *i < b.len() && matches!(b[*i], b'u' | b'U' | b'l' | b'L') {
        *i += 1;
    }
    Ok(if neg { -mag } else { mag })
}

pub fn lex(src: &str) -> Result<Vec<Token>, LexError> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        i = skip_trivia(b, i)?;
        let Some(&c) = b.get(i) else { return Ok(out) };
        let pos = i;
        let tok = match c {
            b'"' => Tok::Str(string(b, &mut i)?),
            b'0'..=b'9' => Tok::Int(integer(b, &mut i)?),
            // A '-' only starts a number here; ORTI has no arithmetic outside quoted expressions.
            b'-' if b.get(i + 1).is_some_and(|d| d.is_ascii_digit()) => Tok::Int(integer(b, &mut i)?),
            c if c == b'_' || c.is_ascii_alphabetic() => {
                let start = i;
                while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                Tok::Ident(src[start..i].to_string())
            }
            c if PUNCTS.contains(&(c as char)) => {
                i += 1;
                Tok::Punct(c as char)
            }
            other => return Err(LexError { message: format!("unexpected character '{}'", other as char), pos }),
        };
        out.push(Token { tok, pos });
    }
}

/// Line and column (both 1-based) of a byte offset, for error messages.
pub fn line_col(src: &str, pos: Pos) -> (usize, usize) {
    let upto = &src[..pos.min(src.len())];
    let line = upto.bytes().filter(|&c| c == b'\n').count() + 1;
    let col = upto.rfind('\n').map_or(upto.len(), |nl| upto.len() - nl - 1) + 1;
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_the_pieces_an_orti_file_is_made_of() {
        let toks = lex(r#"/* c */ TASK Idle { STATE = "a ? b : 0xFF"; };"#).unwrap();
        let kinds: Vec<&Tok> = toks.iter().map(|t| &t.tok).collect();
        assert_eq!(
            kinds,
            vec![
                &Tok::Ident("TASK".into()),
                &Tok::Ident("Idle".into()),
                &Tok::Punct('{'),
                &Tok::Ident("STATE".into()),
                &Tok::Punct('='),
                &Tok::Str("a ? b : 0xFF".into()),
                &Tok::Punct(';'),
                &Tok::Punct('}'),
                &Tok::Punct(';'),
            ]
        );
        // Numbers as ORTI writes them, including the full-width mask used for "no lock".
        let nums = lex("0 255 0xFF 4294967295 -1 0x10U").unwrap();
        let ints: Vec<&Tok> = nums.iter().map(|t| &t.tok).collect();
        assert_eq!(ints, vec![&Tok::Int(0), &Tok::Int(255), &Tok::Int(0xFF), &Tok::Int(4294967295), &Tok::Int(-1), &Tok::Int(16)]);
        // A comment may sit between any two tokens, and the last one need not end a line.
        assert_eq!(lex("a /* x */ , /* y */ b").unwrap().len(), 3);
        assert_eq!(lex("// only a comment").unwrap().len(), 0);
        assert!(lex("\"no end").is_err());
        assert!(lex("/* no end").is_err());
        assert_eq!(line_col("a\nbc\nd", 4), (2, 3));
    }

    #[test]
    fn keeps_expression_text_verbatim() {
        // The contents of a value string are C source; nothing in it may be normalised away.
        let src = r#"X = "~((Os_Hal_AddressType)0)";"#;
        let toks = lex(src).unwrap();
        assert_eq!(toks[2].tok, Tok::Str("~((Os_Hal_AddressType)0)".into()));
    }
}
