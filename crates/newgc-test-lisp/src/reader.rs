//! S-expression reader.
//!
//! Parses a textual program into a tree of `Sexp` nodes. The
//! evaluator walks `Sexp`s directly; there's no intermediate
//! compilation pass.
//!
//! Syntax covered:
//!   - Integers: `42`, `-7`
//!   - Booleans: `#t`, `#f`
//!   - Nil: `()` or the symbol `nil`
//!   - Strings: `"hello world"` (no escapes beyond `\\` and `\"`)
//!   - Symbols: `+`, `cons`, `my-var`
//!   - Lists: `(a b c)`
//!   - Quote: `'x` shorthand for `(quote x)`
//!   - Line comments: `;` to end of line

use std::rc::Rc;

#[derive(Clone, Debug)]
pub enum Sexp {
    Number(i64),
    Bool(bool),
    Nil,
    String(Rc<String>),
    Symbol(Rc<String>),
    List(Vec<Sexp>),
}

impl Sexp {
    pub fn as_symbol(&self) -> Option<&str> {
        if let Sexp::Symbol(s) = self {
            Some(s.as_str())
        } else {
            None
        }
    }
}

pub struct Reader<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(src: &'a str) -> Self {
        Reader {
            src: src.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(b) if b.is_ascii_whitespace() => {
                    self.advance();
                }
                Some(b';') => {
                    while let Some(b) = self.peek() {
                        self.advance();
                        if b == b'\n' {
                            break;
                        }
                    }
                }
                _ => return,
            }
        }
    }

    /// Read one Sexp; returns None at EOF.
    pub fn read_one(&mut self) -> Result<Option<Sexp>, String> {
        self.skip_ws_and_comments();
        if self.peek().is_none() {
            return Ok(None);
        }
        Ok(Some(self.read_expr()?))
    }

    fn read_expr(&mut self) -> Result<Sexp, String> {
        self.skip_ws_and_comments();
        let b = self.peek().ok_or("unexpected EOF")?;
        match b {
            b'(' => self.read_list(),
            b')' => Err(format!("unexpected `)` at offset {}", self.pos)),
            b'"' => self.read_string(),
            b'\'' => {
                self.advance();
                let inner = self.read_expr()?;
                Ok(Sexp::List(vec![
                    Sexp::Symbol(Rc::new("quote".to_string())),
                    inner,
                ]))
            }
            b'#' => self.read_hash(),
            _ if b == b'-' || b.is_ascii_digit() => self.read_number_or_symbol(),
            _ => self.read_symbol(),
        }
    }

    fn read_list(&mut self) -> Result<Sexp, String> {
        debug_assert_eq!(self.peek(), Some(b'('));
        self.advance();
        let mut items = Vec::new();
        loop {
            self.skip_ws_and_comments();
            match self.peek() {
                Some(b')') => {
                    self.advance();
                    if items.is_empty() {
                        return Ok(Sexp::Nil);
                    }
                    return Ok(Sexp::List(items));
                }
                None => return Err("unterminated list".into()),
                _ => items.push(self.read_expr()?),
            }
        }
    }

    fn read_string(&mut self) -> Result<Sexp, String> {
        debug_assert_eq!(self.peek(), Some(b'"'));
        self.advance();
        let mut buf = Vec::new();
        loop {
            match self.advance() {
                None => return Err("unterminated string".into()),
                Some(b'"') => return Ok(Sexp::String(Rc::new(
                    String::from_utf8(buf).map_err(|e| e.to_string())?,
                ))),
                Some(b'\\') => match self.advance() {
                    Some(b'n') => buf.push(b'\n'),
                    Some(b't') => buf.push(b'\t'),
                    Some(b'\\') => buf.push(b'\\'),
                    Some(b'"') => buf.push(b'"'),
                    Some(c) => buf.push(c),
                    None => return Err("unterminated string escape".into()),
                },
                Some(c) => buf.push(c),
            }
        }
    }

    fn read_hash(&mut self) -> Result<Sexp, String> {
        debug_assert_eq!(self.peek(), Some(b'#'));
        self.advance();
        match self.advance() {
            Some(b't') => Ok(Sexp::Bool(true)),
            Some(b'f') => Ok(Sexp::Bool(false)),
            Some(c) => Err(format!("unknown # syntax: #{}", c as char)),
            None => Err("unterminated # syntax".into()),
        }
    }

    fn read_atom_bytes(&mut self) -> Vec<u8> {
        let mut buf = Vec::new();
        while let Some(b) = self.peek() {
            if b.is_ascii_whitespace() || b == b'(' || b == b')' || b == b';' {
                break;
            }
            buf.push(b);
            self.advance();
        }
        buf
    }

    fn read_number_or_symbol(&mut self) -> Result<Sexp, String> {
        let bytes = self.read_atom_bytes();
        let text = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
        if let Ok(n) = text.parse::<i64>() {
            Ok(Sexp::Number(n))
        } else {
            // Treat as symbol (e.g. `-` is a symbol, not -0).
            Ok(Sexp::Symbol(Rc::new(text.to_string())))
        }
    }

    fn read_symbol(&mut self) -> Result<Sexp, String> {
        let bytes = self.read_atom_bytes();
        if bytes.is_empty() {
            return Err(format!("empty token at offset {}", self.pos));
        }
        let text = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
        match text {
            "nil" => Ok(Sexp::Nil),
            _ => Ok(Sexp::Symbol(Rc::new(text.to_string()))),
        }
    }
}

pub fn read_all(src: &str) -> Result<Vec<Sexp>, String> {
    let mut reader = Reader::new(src);
    let mut out = Vec::new();
    while let Some(s) = reader.read_one()? {
        out.push(s);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_atoms() {
        assert!(matches!(read_all("42").unwrap()[0], Sexp::Number(42)));
        assert!(matches!(read_all("-7").unwrap()[0], Sexp::Number(-7)));
        assert!(matches!(read_all("#t").unwrap()[0], Sexp::Bool(true)));
        assert!(matches!(read_all("#f").unwrap()[0], Sexp::Bool(false)));
        assert!(matches!(read_all("nil").unwrap()[0], Sexp::Nil));
        assert!(matches!(read_all("()").unwrap()[0], Sexp::Nil));
    }

    #[test]
    fn read_simple_list() {
        let s = &read_all("(+ 1 2)").unwrap()[0];
        match s {
            Sexp::List(v) => {
                assert_eq!(v.len(), 3);
                assert_eq!(v[0].as_symbol(), Some("+"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn read_nested() {
        let s = &read_all("(let ((x 1) (y 2)) (+ x y))").unwrap()[0];
        match s {
            Sexp::List(v) => assert_eq!(v.len(), 3),
            _ => panic!(),
        }
    }

    #[test]
    fn read_string_with_escapes() {
        let r = &read_all(r#""hello\nworld""#).unwrap()[0];
        match r {
            Sexp::String(s) => assert_eq!(s.as_str(), "hello\nworld"),
            _ => panic!(),
        }
    }

    #[test]
    fn read_quote_shorthand() {
        let s = &read_all("'foo").unwrap()[0];
        match s {
            Sexp::List(v) => {
                assert_eq!(v.len(), 2);
                assert_eq!(v[0].as_symbol(), Some("quote"));
                assert_eq!(v[1].as_symbol(), Some("foo"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn read_comments_are_skipped() {
        let r = read_all("; this is a comment\n42").unwrap();
        assert_eq!(r.len(), 1);
        assert!(matches!(r[0], Sexp::Number(42)));
    }

    #[test]
    fn read_multiple_top_level() {
        let r = read_all("1 2 3 (cons 4 5)").unwrap();
        assert_eq!(r.len(), 4);
    }
}
