//! Recursive-descent parser for `.ci` files.

use super::ast::{Body, Expr, Item, StrPart};
use super::lexer::{Part, Tok, Token, lex};
use super::{Error, Span};

const KEYWORDS: [&str; 12] = [
    "let", "fn", "for", "in", "if", "else", "use", "true", "false", "none", "and", "or",
];

#[derive(Debug)]
pub struct File {
    pub items: Vec<Item>,
}

pub fn parse_file(file: usize, source: &str) -> Result<File, Error> {
    let mut parser = Parser {
        tokens: lex(file, source, 0)?,
        index: 0,
        file,
    };
    parser.header()?;
    let items = parser.items(true)?;
    parser.expect_eof()?;
    Ok(File { items })
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
    file: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.tokens[self.index].tok
    }

    fn peek_at(&self, offset: usize) -> &Tok {
        &self.tokens[(self.index + offset).min(self.tokens.len() - 1)].tok
    }

    fn span(&self) -> Span {
        self.tokens[self.index].span
    }

    fn bump(&mut self) -> Token {
        let token = self.tokens[self.index].clone();
        if self.index < self.tokens.len() - 1 {
            self.index += 1;
        }
        token
    }

    fn is_sym(&self, symbol: &str) -> bool {
        matches!(self.peek(), Tok::Sym(found) if *found == symbol)
    }

    fn is_word(&self, word: &str) -> bool {
        matches!(self.peek(), Tok::Ident(found) if found == word)
    }

    fn eat_sym(&mut self, symbol: &str) -> bool {
        if self.is_sym(symbol) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_sym(&mut self, symbol: &str) -> Result<Span, Error> {
        if self.is_sym(symbol) {
            return Ok(self.bump().span);
        }
        Err(Error::at(
            self.span(),
            format!("expected `{symbol}`, found {}", describe(self.peek())),
        ))
    }

    fn expect_word(&mut self, word: &str) -> Result<Span, Error> {
        if self.is_word(word) {
            return Ok(self.bump().span);
        }
        Err(Error::at(
            self.span(),
            format!("expected `{word}`, found {}", describe(self.peek())),
        ))
    }

    fn name(&mut self) -> Result<(String, Span), Error> {
        match self.peek().clone() {
            Tok::Ident(name) if !KEYWORDS.contains(&name.as_str()) => Ok((name, self.bump().span)),
            other => Err(Error::at(
                self.span(),
                format!("expected a name, found {}", describe(&other)),
            )),
        }
    }

    fn expect_eof(&self) -> Result<(), Error> {
        match self.peek() {
            Tok::Eof => Ok(()),
            other => Err(Error::at(
                self.span(),
                format!("unexpected {}", describe(other)),
            )),
        }
    }

    fn header(&mut self) -> Result<u32, Error> {
        if !self.is_word("citrus") {
            return Err(
                Error::at(self.span(), "a .ci file starts with its language version")
                    .help("add `citrus 1` as the first line"),
            );
        }
        self.bump();
        match self.peek().clone() {
            Tok::Int(1) => {
                self.bump();
                Ok(1)
            }
            Tok::Int(other) => Err(Error::at(
                self.span(),
                format!("language version {other} is not supported by this Citrus"),
            )
            .help("this Citrus reads `citrus 1`; upgrade Citrus for newer files")),
            other => Err(Error::at(
                self.span(),
                format!("expected the language version, found {}", describe(&other)),
            )),
        }
    }

    /// Items until `}` (or end of file at the top level).
    fn items(&mut self, top: bool) -> Result<Vec<Item>, Error> {
        let mut items = Vec::new();
        loop {
            while self.eat_sym(",") || self.eat_sym(";") {}
            if matches!(self.peek(), Tok::Eof) || self.is_sym("}") {
                return Ok(items);
            }
            items.push(self.item(top)?);
        }
    }

    fn item(&mut self, top: bool) -> Result<Item, Error> {
        let start = self.span();
        if self.is_word("use") {
            if !top {
                return Err(Error::at(start, "`use` belongs at the top of a file"));
            }
            self.bump();
            let Tok::Str(parts) = self.peek().clone() else {
                return Err(Error::at(
                    self.span(),
                    "expected a file path in quotes after `use`",
                ));
            };
            let span = self.bump().span;
            let path = literal(&parts)
                .ok_or_else(|| Error::at(span, "a `use` path cannot contain `{…}`"))?;
            return Ok(Item::Use {
                path,
                span: join(start, span),
            });
        }
        if self.is_word("let") {
            self.bump();
            let (name, _) = self.name()?;
            self.expect_sym("=")?;
            let value = self.expr()?;
            return Ok(Item::Let {
                span: join(start, value.span()),
                name,
                value,
            });
        }
        if self.is_word("fn") {
            self.bump();
            let (name, _) = self.name()?;
            self.expect_sym("(")?;
            let mut params = Vec::new();
            while !self.is_sym(")") {
                let (param, _) = self.name()?;
                let default = if self.eat_sym("=") {
                    Some(self.expr()?)
                } else {
                    None
                };
                params.push((param, default));
                if !self.eat_sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
            let body = self.body()?;
            return Ok(Item::Fn {
                name,
                params,
                span: join(start, body.value.span()),
                body,
            });
        }
        if self.is_word("for") {
            self.bump();
            let (var, _) = self.name()?;
            self.expect_word("in")?;
            let iter = self.expr()?;
            self.expect_sym("{")?;
            let items = self.items(false)?;
            let end = self.expect_sym("}")?;
            return Ok(Item::For {
                var,
                iter,
                items,
                span: join(start, end),
            });
        }
        if self.is_word("if") {
            self.bump();
            let cond = self.expr()?;
            self.expect_sym("{")?;
            let then = self.items(false)?;
            let mut end = self.expect_sym("}")?;
            let mut otherwise = Vec::new();
            if self.is_word("else") {
                self.bump();
                self.expect_sym("{")?;
                otherwise = self.items(false)?;
                end = self.expect_sym("}")?;
            }
            return Ok(Item::If {
                cond,
                then,
                otherwise,
                span: join(start, end),
            });
        }
        let (name, name_span) = self.name()?;
        if self.eat_sym("=") {
            let value = self.expr()?;
            return Ok(Item::Field {
                span: join(name_span, value.span()),
                name,
                value,
            });
        }
        let label = match self.peek() {
            Tok::Str(_) => Some(self.primary()?),
            Tok::Sym("{") => None,
            other => {
                return Err(Error::at(
                    self.span(),
                    format!(
                        "expected `=` or a block after `{name}`, found {}",
                        describe(other)
                    ),
                )
                .help(format!(
                    "a field is `{name} = value`; a block is `{name} \"label\" {{ … }}`"
                )));
            }
        };
        self.expect_sym("{")?;
        let items = self.items(false)?;
        let end = self.expect_sym("}")?;
        Ok(Item::Block {
            kind: name,
            label,
            items,
            span: join(name_span, end),
        })
    }

    fn body(&mut self) -> Result<Body, Error> {
        self.expect_sym("{")?;
        let mut lets = Vec::new();
        while self.is_word("let") {
            let start = self.bump().span;
            let (name, _) = self.name()?;
            self.expect_sym("=")?;
            let value = self.expr()?;
            lets.push((name, value.clone(), join(start, value.span())));
            while self.eat_sym(";") || self.eat_sym(",") {}
        }
        let value = self.expr()?;
        self.expect_sym("}")?;
        Ok(Body {
            lets,
            value: Box::new(value),
        })
    }

    pub fn expr(&mut self) -> Result<Expr, Error> {
        self.binary(0)
    }

    fn binary(&mut self, level: usize) -> Result<Expr, Error> {
        const LEVELS: [&[&str]; 6] = [
            &["or"],
            &["and"],
            &["==", "!=", "<", "<=", ">", ">=", "in"],
            &["??"],
            &["+", "-"],
            &["*", "/", "%"],
        ];
        if level == LEVELS.len() {
            return self.unary();
        }
        let mut left = self.binary(level + 1)?;
        loop {
            let operator = match self.peek() {
                Tok::Sym(symbol) if LEVELS[level].contains(symbol) => *symbol,
                Tok::Ident(word) if LEVELS[level].contains(&word.as_str()) => LEVELS[level]
                    .iter()
                    .copied()
                    .find(|op| *op == word)
                    .unwrap_or("?"),
                _ => return Ok(left),
            };
            self.bump();
            let right = self.binary(level + 1)?;
            let span = join(left.span(), right.span());
            left = Expr::Binary(operator, Box::new(left), Box::new(right), span);
        }
    }

    fn unary(&mut self) -> Result<Expr, Error> {
        let start = self.span();
        if self.is_word("not") || self.is_sym("!") {
            self.bump();
            let value = self.unary()?;
            return Ok(Expr::Unary(
                "not",
                Box::new(value.clone()),
                join(start, value.span()),
            ));
        }
        if self.eat_sym("-") {
            let value = self.unary()?;
            return Ok(Expr::Unary(
                "-",
                Box::new(value.clone()),
                join(start, value.span()),
            ));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, Error> {
        let mut value = self.primary()?;
        loop {
            if self.eat_sym(".") {
                let (field, span) = self.name()?;
                value = Expr::Field(Box::new(value.clone()), field, join(value.span(), span));
            } else if self.is_sym("(") {
                self.bump();
                let mut args = Vec::new();
                while !self.is_sym(")") {
                    let named = match (self.peek().clone(), self.peek_at(1).clone()) {
                        (Tok::Ident(name), Tok::Sym(":")) => {
                            self.bump();
                            self.bump();
                            Some(name)
                        }
                        _ => None,
                    };
                    args.push((named, self.expr()?));
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                let end = self.expect_sym(")")?;
                value = Expr::Call {
                    span: join(value.span(), end),
                    callee: Box::new(value),
                    args,
                };
            } else if self.is_sym("[") {
                self.bump();
                let index = self.expr()?;
                let end = self.expect_sym("]")?;
                value = Expr::Index(
                    Box::new(value.clone()),
                    Box::new(index),
                    join(value.span(), end),
                );
            } else {
                return Ok(value);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, Error> {
        let token = self.bump();
        let span = token.span;
        Ok(match token.tok {
            Tok::Int(value) => Expr::Int(value, span),
            Tok::Duration(value) => Expr::Duration(value, span),
            Tok::Str(parts) => {
                let mut out = Vec::new();
                for part in parts {
                    match part {
                        Part::Lit(text) => out.push(StrPart::Lit(text)),
                        Part::Hole(source, offset) => {
                            let hint = |error: Error| {
                                if error.help.is_some() {
                                    error
                                } else {
                                    error.help("`{` in a string starts a value like `{name}`; write `{{` and `}}` for literal braces")
                                }
                            };
                            let mut inner = Parser {
                                tokens: lex(self.file, &source, offset).map_err(hint)?,
                                index: 0,
                                file: self.file,
                            };
                            let expr = inner.expr().map_err(hint)?;
                            inner.expect_eof().map_err(hint)?;
                            out.push(StrPart::Expr(expr));
                        }
                    }
                }
                Expr::Str(out, span)
            }
            Tok::Ident(word) => match word.as_str() {
                "true" => Expr::Bool(true, span),
                "false" => Expr::Bool(false, span),
                "none" => Expr::None(span),
                "if" => {
                    let cond = self.expr()?;
                    let then = self.body()?;
                    self.expect_word("else").map_err(|error| {
                        error.help("an `if` that produces a value needs an `else`")
                    })?;
                    let otherwise = if self.is_word("if") {
                        let nested = self.primary()?;
                        Body {
                            lets: Vec::new(),
                            value: Box::new(nested),
                        }
                    } else {
                        self.body()?
                    };
                    let end = otherwise.value.span();
                    Expr::If {
                        cond: Box::new(cond),
                        then,
                        otherwise,
                        span: join(span, end),
                    }
                }
                word if KEYWORDS.contains(&word) => {
                    return Err(Error::at(span, format!("`{word}` cannot start a value")));
                }
                _ => Expr::Name(word, span),
            },
            Tok::Sym("(") => {
                let value = self.expr()?;
                self.expect_sym(")")?;
                value
            }
            Tok::Sym("[") => {
                let mut items = Vec::new();
                while !self.is_sym("]") {
                    let value = self.expr()?;
                    if items.is_empty() && self.is_word("for") {
                        self.bump();
                        let (var, _) = self.name()?;
                        self.expect_word("in")?;
                        let iter = self.expr()?;
                        let cond = if self.is_word("if") {
                            self.bump();
                            Some(Box::new(self.expr()?))
                        } else {
                            None
                        };
                        let end = self.expect_sym("]")?;
                        return Ok(Expr::Comprehension {
                            value: Box::new(value),
                            var,
                            iter: Box::new(iter),
                            cond,
                            span: join(span, end),
                        });
                    }
                    items.push(value);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                let end = self.expect_sym("]")?;
                Expr::List(items, join(span, end))
            }
            Tok::Sym("{") => {
                let mut entries = Vec::new();
                while !self.is_sym("}") {
                    let key = match self.bump().tok {
                        Tok::Ident(name) => name,
                        Tok::Str(parts) => literal(&parts).ok_or_else(|| {
                            Error::at(
                                self.tokens[self.index - 1].span,
                                "a map key cannot contain `{…}`",
                            )
                        })?,
                        other => {
                            return Err(Error::at(
                                self.tokens[self.index - 1].span,
                                format!("expected a map key, found {}", describe(&other)),
                            ));
                        }
                    };
                    self.expect_sym(":")
                        .map_err(|error| error.help("map entries are `key: value`"))?;
                    entries.push((key, self.expr()?));
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                let end = self.expect_sym("}")?;
                Expr::Map(entries, join(span, end))
            }
            other => {
                return Err(Error::at(
                    span,
                    format!("expected a value, found {}", describe(&other)),
                ));
            }
        })
    }
}

fn literal(parts: &[Part]) -> Option<String> {
    parts
        .iter()
        .map(|part| {
            if let Part::Lit(text) = part {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect()
}

fn join(start: Span, end: Span) -> Span {
    Span {
        file: start.file,
        start: start.start,
        end: end.end.max(start.start),
    }
}

fn describe(tok: &Tok) -> String {
    match tok {
        Tok::Ident(name) => format!("`{name}`"),
        Tok::Str(_) => "a string".into(),
        Tok::Int(value) => format!("`{value}`"),
        Tok::Duration(_) => "a duration".into(),
        Tok::Sym(symbol) => format!("`{symbol}`"),
        Tok::Eof => "the end of the file".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_declarations_loops_and_expressions() {
        let file = parse_file(
            0,
            "citrus 1\nuse \"ci/more.ci\"\nlet names = [n for n in [\"a\", \"b\"] if n != \"c\"]\n\
             fn image(name, tag = \"x\") { let base = \"r\"\n \"{base}/{name}:{tag}\" }\n\
             for name in names {\n  check \"test-{name}\" { owns = [\"src/{name}/**\"], run = make(\"t\", JOBS: 2), cache = true }\n}\n\
             project { base = \"origin/main\" }\n",
        )
        .unwrap();
        assert_eq!(file.items.len(), 5);
        assert!(
            matches!(&file.items[3], Item::For { items, .. } if matches!(&items[0], Item::Block { kind, .. } if kind == "check"))
        );
    }

    #[test]
    fn version_header_is_required_and_checked() {
        assert!(
            parse_file(0, "check \"a\" {}")
                .unwrap_err()
                .help
                .unwrap()
                .contains("citrus 1")
        );
        assert!(
            parse_file(0, "citrus 2\n")
                .unwrap_err()
                .message
                .contains("not supported")
        );
    }

    #[test]
    fn errors_name_what_was_expected() {
        let error = parse_file(0, "citrus 1\ncheck \"a\" { owns [\"x\"] }").unwrap_err();
        assert!(
            error.message.contains("expected `=` or a block"),
            "{}",
            error.message
        );
        let error = parse_file(0, "citrus 1\nlet x = if true { 1 }").unwrap_err();
        assert!(error.help.unwrap().contains("else"));
    }
}
