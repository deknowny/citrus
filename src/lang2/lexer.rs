//! Tokens of language v2 (docs/design/language-v2.md).

use crate::lang::{Error, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    /// Literal text and `{expr}` holes; a hole keeps the offset of its source.
    Str(Vec<Part>),
    Int(i64),
    /// Seconds.
    Duration(u64),
    Sym(&'static str),
    /// `/// text` above an item.
    Doc(String),
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Lit(String),
    Hole(String, usize),
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

// Longest first: `#![` before `#[`, `::` before `:`.
const SYMBOLS: [&str; 34] = [
    "#![", "#[", "::", "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "(", ")", "[", "]", "{",
    "}", ",", ":", ";", "=", ".", "+", "-", "*", "/", "%", "<", ">", "!", "?", "|", "&", "_",
];

pub fn lex(file: usize, source: &str, base: usize) -> Result<Vec<Token>, Error> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    let span = |start: usize, end: usize| Span {
        file,
        start: base + start,
        end: base + end,
    };
    while index < bytes.len() {
        let c = bytes[index];
        if c.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if source[index..].starts_with("//") {
            let start = index;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            let line = &source[start..index];
            if let Some(text) = line.strip_prefix("///")
                && !text.starts_with('/')
            {
                let text = text.strip_prefix(' ').unwrap_or(text).to_owned();
                tokens.push(Token {
                    tok: Tok::Doc(text),
                    span: span(start, index),
                });
            }
            continue;
        }
        if c == b'"' {
            let start = index;
            index += 1;
            let mut parts = Vec::new();
            let mut text = String::new();
            loop {
                let Some(&next) = bytes.get(index) else {
                    return Err(Error::at(span(start, index), "unterminated string"));
                };
                match next {
                    b'"' => {
                        index += 1;
                        break;
                    }
                    b'\\' => {
                        let escaped = bytes.get(index + 1).copied().unwrap_or(b'\\');
                        text.push(match escaped {
                            b'n' => '\n',
                            b't' => '\t',
                            other => other as char,
                        });
                        index += 2;
                    }
                    b'{' if bytes.get(index + 1) == Some(&b'{') => {
                        text.push('{');
                        index += 2;
                    }
                    b'}' if bytes.get(index + 1) == Some(&b'}') => {
                        text.push('}');
                        index += 2;
                    }
                    b'{' => {
                        let open = index + 1;
                        let mut depth = 1;
                        index += 1;
                        while index < bytes.len() && depth > 0 {
                            match bytes[index] {
                                b'{' => depth += 1,
                                b'}' => depth -= 1,
                                b'"' => {
                                    return Err(Error::at(
                                        span(index, index + 1),
                                        "a string inside `{…}` of a string: use a const",
                                    ));
                                }
                                _ => {}
                            }
                            index += 1;
                        }
                        if depth > 0 {
                            return Err(Error::at(span(start, index), "unclosed `{` in a string")
                                .help("write `{{` for a brace"));
                        }
                        if !text.is_empty() {
                            parts.push(Part::Lit(std::mem::take(&mut text)));
                        }
                        parts.push(Part::Hole(source[open..index - 1].to_owned(), base + open));
                    }
                    _ => {
                        let ch = source[index..].chars().next().unwrap_or('?');
                        text.push(ch);
                        index += ch.len_utf8();
                    }
                }
            }
            if !text.is_empty() || parts.is_empty() {
                parts.push(Part::Lit(text));
            }
            tokens.push(Token {
                tok: Tok::Str(parts),
                span: span(start, index),
            });
            continue;
        }
        if c.is_ascii_digit() {
            let start = index;
            while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'_') {
                index += 1;
            }
            let digits: String = source[start..index].chars().filter(|c| *c != '_').collect();
            let number: i64 = digits
                .parse()
                .map_err(|_| Error::at(span(start, index), "number too large"))?;
            let unit_start = index;
            while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
                index += 1;
            }
            let unit = &source[unit_start..index];
            let tok = match unit {
                "" => Tok::Int(number),
                "s" => Tok::Duration(number as u64),
                "m" => Tok::Duration(number as u64 * 60),
                "h" => Tok::Duration(number as u64 * 3600),
                other => {
                    return Err(
                        Error::at(span(start, index), format!("unknown unit `{other}`"))
                            .help("durations are written 30s, 5m or 2h"),
                    );
                }
            };
            tokens.push(Token {
                tok,
                span: span(start, index),
            });
            continue;
        }
        if c.is_ascii_alphabetic()
            || (c == b'_'
                && bytes
                    .get(index + 1)
                    .is_some_and(|n| n.is_ascii_alphanumeric() || *n == b'_'))
        {
            let start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            tokens.push(Token {
                tok: Tok::Ident(source[start..index].to_owned()),
                span: span(start, index),
            });
            continue;
        }
        let Some(symbol) = SYMBOLS
            .iter()
            .find(|symbol| source[index..].starts_with(**symbol))
        else {
            let ch = source[index..].chars().next().unwrap_or('?');
            return Err(Error::at(
                span(index, index + ch.len_utf8()),
                format!("unexpected character `{ch}`"),
            ));
        };
        tokens.push(Token {
            tok: Tok::Sym(symbol),
            span: span(index, index + symbol.len()),
        });
        index += symbol.len();
    }
    tokens.push(Token {
        tok: Tok::Eof,
        span: span(bytes.len(), bytes.len()),
    });
    Ok(tokens)
}
