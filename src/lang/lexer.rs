//! Tokens of `.ci` files, each with its byte span.

use super::{Error, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    /// Literal text and `{expr}` holes; holes keep the absolute offset of their source.
    Str(Vec<Part>),
    Int(i64),
    /// Seconds.
    Duration(u64),
    Sym(&'static str),
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

const SYMBOLS: [&str; 27] = [
    "=>", "==", "!=", "<=", ">=", "??", "(", ")", "[", "]", "{", "}", ",", ":", "=", ".", "+", "-",
    "*", "/", "%", "<", ">", "!", ";", "|", "&",
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
        if c == b'#' {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        let start = index;
        if c.is_ascii_alphabetic() || c == b'_' {
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric()
                    || bytes[index] == b'_'
                    || bytes[index] == b'-')
            {
                index += 1;
            }
            // A trailing '-' belongs to an operator, not the identifier.
            while index > start + 1 && bytes[index - 1] == b'-' {
                index -= 1;
            }
            tokens.push(Token {
                tok: Tok::Ident(source[start..index].to_owned()),
                span: span(start, index),
            });
            continue;
        }
        if c.is_ascii_digit() {
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            let value: i64 = source[start..index]
                .parse()
                .map_err(|_| Error::at(span(start, index), "number too large"))?;
            let unit_start = index;
            while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
                index += 1;
            }
            let unit = &source[unit_start..index];
            let tok = match unit {
                "" => Tok::Int(value),
                "s" => Tok::Duration(value as u64),
                "m" => Tok::Duration(value as u64 * 60),
                "h" => Tok::Duration(value as u64 * 3600),
                "d" => Tok::Duration(value as u64 * 86400),
                other => {
                    return Err(Error::at(
                        span(unit_start, index),
                        format!("unknown duration unit `{other}`"),
                    )
                    .help("use s, m, h or d, e.g. `30s`"));
                }
            };
            tokens.push(Token {
                tok,
                span: span(start, index),
            });
            continue;
        }
        if c == b'"' {
            let triple = source[index..].starts_with("\"\"\"");
            let (parts, end) = if triple {
                string(source, index + 3, true, file, base)?
            } else {
                string(source, index + 1, false, file, base)?
            };
            tokens.push(Token {
                tok: Tok::Str(parts),
                span: span(start, end),
            });
            index = end;
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
        index += symbol.len();
        tokens.push(Token {
            tok: Tok::Sym(symbol),
            span: span(start, index),
        });
    }
    tokens.push(Token {
        tok: Tok::Eof,
        span: span(bytes.len(), bytes.len()),
    });
    Ok(tokens)
}

/// A string body from `from` (after the opening quotes); returns parts and the end offset.
fn string(
    source: &str,
    from: usize,
    triple: bool,
    file: usize,
    base: usize,
) -> Result<(Vec<Part>, usize), Error> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut index = from;
    let bytes = source.as_bytes();
    loop {
        if index >= bytes.len() {
            return Err(Error::at(
                Span {
                    file,
                    start: base + from - 1,
                    end: base + index,
                },
                "string is not closed",
            ));
        }
        if triple && source[index..].starts_with("\"\"\"") {
            parts.push(Part::Lit(std::mem::take(&mut text)));
            let parts = if triple { dedent(parts) } else { parts };
            return Ok((parts, index + 3));
        }
        let c = source[index..].chars().next().unwrap_or_default();
        match c {
            '"' if !triple => {
                parts.push(Part::Lit(std::mem::take(&mut text)));
                return Ok((parts, index + 1));
            }
            '\n' if !triple => {
                return Err(Error::at(
                    Span {
                        file,
                        start: base + from - 1,
                        end: base + index,
                    },
                    "string is not closed on this line",
                )
                .help("use \"\"\"…\"\"\" for multi-line text"));
            }
            '\\' => {
                let next = source[index + 1..].chars().next().unwrap_or_default();
                text.push(match next {
                    'n' => '\n',
                    't' => '\t',
                    '"' => '"',
                    '\\' => '\\',
                    '{' => '{',
                    other => {
                        return Err(Error::at(
                            Span {
                                file,
                                start: base + index,
                                end: base + index + 2,
                            },
                            format!("unknown escape `\\{other}`"),
                        ));
                    }
                });
                index += 1 + next.len_utf8();
            }
            '{' if source[index..].starts_with("{{") => {
                text.push('{');
                index += 2;
            }
            '}' if source[index..].starts_with("}}") => {
                text.push('}');
                index += 2;
            }
            '{' => {
                parts.push(Part::Lit(std::mem::take(&mut text)));
                let mut depth = 1;
                let start = index + 1;
                let mut end = start;
                while end < bytes.len() && depth > 0 {
                    match bytes[end] {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        b'"' | b'\n' if depth > 0 => break,
                        _ => {}
                    }
                    if depth > 0 {
                        end += 1;
                    }
                }
                if depth != 0 {
                    return Err(Error::at(
                        Span {
                            file,
                            start: base + index,
                            end: base + end,
                        },
                        "`{` in a string is not closed",
                    )
                    .help("write `{{` for a literal brace"));
                }
                parts.push(Part::Hole(source[start..end].to_owned(), base + start));
                index = end + 1;
            }
            other => {
                text.push(other);
                index += other.len_utf8();
            }
        }
    }
}

/// Multi-line strings: drop the first newline and the common indentation.
fn dedent(parts: Vec<Part>) -> Vec<Part> {
    let joined: String = parts
        .iter()
        .map(|part| {
            if let Part::Lit(text) = part {
                text.as_str()
            } else {
                "\u{0}"
            }
        })
        .collect();
    let body = joined.strip_prefix('\n').unwrap_or(&joined);
    let indent = body
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    if indent == 0 && !joined.starts_with('\n') {
        return parts;
    }
    let mut first = true;
    parts
        .into_iter()
        .map(|part| match part {
            Part::Lit(text) => {
                let text = if first {
                    text.strip_prefix('\n').unwrap_or(&text).to_owned()
                } else {
                    text
                };
                first = false;
                let mut out = String::new();
                for (number, line) in text.split('\n').enumerate() {
                    if number > 0 {
                        out.push('\n');
                    }
                    out.push_str(
                        line.get(indent.min(line.len() - line.trim_start().len())..)
                            .unwrap_or(line),
                    );
                }
                Part::Lit(out)
            }
            hole => {
                first = false;
                hole
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<Tok> {
        lex(0, source, 0)
            .unwrap()
            .into_iter()
            .map(|token| token.tok)
            .collect()
    }

    #[test]
    fn lexes_names_numbers_durations_and_symbols() {
        assert_eq!(
            kinds("check \"a\" { timeout = 5m, n = 3 } # comment"),
            vec![
                Tok::Ident("check".into()),
                Tok::Str(vec![Part::Lit("a".into())]),
                Tok::Sym("{"),
                Tok::Ident("timeout".into()),
                Tok::Sym("="),
                Tok::Duration(300),
                Tok::Sym(","),
                Tok::Ident("n".into()),
                Tok::Sym("="),
                Tok::Int(3),
                Tok::Sym("}"),
                Tok::Eof
            ]
        );
        assert_eq!(kinds("test-api - 1")[0], Tok::Ident("test-api".into()));
    }

    #[test]
    fn strings_have_holes_with_absolute_offsets() {
        let tokens = lex(0, "x = \"mt3s-{name}-{{lit}}\"", 0).unwrap();
        let Tok::Str(parts) = &tokens[2].tok else {
            panic!()
        };
        assert_eq!(
            parts,
            &vec![
                Part::Lit("mt3s-".into()),
                Part::Hole("name".into(), 11),
                Part::Lit("-{lit}".into())
            ]
        );
    }

    #[test]
    fn errors_point_at_the_problem() {
        let error = lex(0, "a = 5x", 0).unwrap_err();
        assert!(error.message.contains("duration unit"));
        assert_eq!((error.span.start, error.span.end), (5, 6));
        assert!(lex(0, "a = \"open", 0).is_err());
    }
}
