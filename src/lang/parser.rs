//! Recursive-descent parser of language v2.

use super::ast::*;
use super::lexer::{Part, Tok, Token, lex};
use crate::lang::{Error, Span};

pub struct Parser {
    tokens: Vec<Token>,
    index: usize,
    file: usize,
    /// In `if`/`match`/`for` heads `Name {` opens the block, not a struct.
    no_struct: bool,
    /// Where the name of the item being parsed is.
    name_span: Span,
}

type Parsed<T> = Result<T, Error>;

/// A command line's environment and words.
type CommandLine = (Vec<(String, CmdWord)>, Vec<CmdWord>);

const KEYWORDS: &[&str] = &[
    "const",
    "fn",
    "struct",
    "group",
    "check",
    "task",
    "release",
    "environment",
    "step",
    "rollback",
    "let",
    "mut",
    "if",
    "else",
    "for",
    "in",
    "match",
    "return",
    "assert",
    "true",
    "false",
];

pub fn parse_file(file: usize, source: &str, program: &mut Program) -> Parsed<()> {
    let tokens = lex(file, source, 0)?;
    let mut parser = Parser {
        tokens,
        index: 0,
        file,
        no_struct: false,
        name_span: Span::default(),
    };
    while parser.at_sym("#![") {
        program.inner.push(parser.attr("#![")?);
    }
    while !parser.at_eof() {
        // Project attributes may follow items, e.g. in a file of their own.
        if parser.at_sym("#![") {
            program.inner.push(parser.attr("#![")?);
            continue;
        }
        program.items.push(parser.item()?);
    }
    Ok(())
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.tokens[self.index].tok
    }

    fn peek_at(&self, ahead: usize) -> &Tok {
        &self.tokens[(self.index + ahead).min(self.tokens.len() - 1)].tok
    }

    fn span(&self) -> Span {
        self.tokens[self.index].span
    }

    fn previous_end(&self) -> Span {
        self.tokens[self.index.saturating_sub(1)].span
    }

    fn join(&self, start: Span) -> Span {
        Span {
            file: start.file,
            start: start.start,
            end: self.previous_end().end.max(start.start),
        }
    }

    fn bump(&mut self) -> Token {
        let token = self.tokens[self.index].clone();
        if self.index < self.tokens.len() - 1 {
            self.index += 1;
        }
        token
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }

    fn at_sym(&self, symbol: &str) -> bool {
        matches!(self.peek(), Tok::Sym(s) if *s == symbol)
    }

    fn at_word(&self, word: &str) -> bool {
        matches!(self.peek(), Tok::Ident(name) if name == word)
    }

    fn eat_sym(&mut self, symbol: &str) -> bool {
        if self.at_sym(symbol) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if self.at_word(word) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn describe(&self) -> String {
        match self.peek() {
            Tok::Ident(name) => format!("`{name}`"),
            Tok::Str(_) => "a string".into(),
            Tok::Int(_) => "a number".into(),
            Tok::Duration(_) => "a duration".into(),
            Tok::Sym(symbol) => format!("`{symbol}`"),
            Tok::Doc(_) => "a `///` comment".into(),
            Tok::Eof => "the end of the file".into(),
        }
    }

    fn expect_sym(&mut self, symbol: &str) -> Parsed<Span> {
        if self.at_sym(symbol) {
            Ok(self.bump().span)
        } else {
            Err(Error::at(
                self.span(),
                format!("expected `{symbol}`, found {}", self.describe()),
            ))
        }
    }

    fn ident(&mut self, what: &str) -> Parsed<(String, Span)> {
        match self.peek().clone() {
            Tok::Ident(name) if !KEYWORDS.contains(&name.as_str()) => {
                let span = self.bump().span;
                Ok((name, span))
            }
            _ => Err(Error::at(
                self.span(),
                format!("expected {what}, found {}", self.describe()),
            )),
        }
    }

    fn attr(&mut self, open: &str) -> Parsed<Attr> {
        let start = self.expect_sym(open)?;
        // Attribute names may be keywords: `#[environment(…)]`.
        let name = match self.peek().clone() {
            Tok::Ident(name) => {
                self.bump();
                name
            }
            _ => self.ident("an attribute name")?.0,
        };
        let mut args = Vec::new();
        if self.eat_sym("(") {
            while !self.at_sym(")") {
                let key = match (self.peek().clone(), self.peek_at(1)) {
                    (Tok::Ident(key), Tok::Sym("=")) => {
                        self.bump();
                        self.bump();
                        Some(key)
                    }
                    _ => None,
                };
                args.push((key, self.expr()?));
                if !self.eat_sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
        }
        self.expect_sym("]")?;
        Ok(Attr {
            name,
            args,
            span: self.join(start),
        })
    }

    fn item(&mut self) -> Parsed<Item> {
        let mut doc: Vec<String> = Vec::new();
        let mut attrs = Vec::new();
        let start = self.span();
        loop {
            match self.peek().clone() {
                Tok::Doc(text) => {
                    self.bump();
                    doc.push(text);
                }
                Tok::Sym("#[") => attrs.push(self.attr("#[")?),
                _ => break,
            }
        }
        let doc = (!doc.is_empty()).then(|| doc.join("\n"));
        let keyword = match self.peek().clone() {
            Tok::Ident(word) => word,
            _ => {
                return Err(Error::at(
                    self.span(),
                    format!(
                        "expected an item (check, group, fn, const, …), found {}",
                        self.describe()
                    ),
                ));
            }
        };
        let kind_start = self.span();
        let item_start = if attrs.is_empty() && doc.is_none() {
            kind_start
        } else {
            start
        };
        match keyword.as_str() {
            "const" if matches!(self.peek_at(1), Tok::Ident(word) if word == "fn") => {
                self.bump();
                let function = self.function(true, kind_start)?;
                Ok(self.finish(
                    function.name.clone(),
                    doc,
                    attrs,
                    ItemKind::Fn(function),
                    item_start,
                ))
            }
            "const" => {
                self.bump();
                let (name, name_span) = self.ident("a constant name")?;
                self.name_span = name_span;
                let ty = if self.eat_sym(":") {
                    Some(self.type_expr()?)
                } else {
                    None
                };
                self.expect_sym("=")?;
                let value = self.expr()?;
                self.expect_sym(";")?;
                Ok(self.finish(name, doc, attrs, ItemKind::Const { ty, value }, item_start))
            }
            "fn" => {
                let function = self.function(false, kind_start)?;
                Ok(self.finish(
                    function.name.clone(),
                    doc,
                    attrs,
                    ItemKind::Fn(function),
                    item_start,
                ))
            }
            "struct" => {
                self.bump();
                let (name, name_span) = self.ident("a struct name")?;
                self.name_span = name_span;
                self.expect_sym("{")?;
                let mut fields = Vec::new();
                while !self.at_sym("}") {
                    let (field, _) = self.ident("a field name")?;
                    self.expect_sym(":")?;
                    fields.push((field, self.type_expr()?));
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym("}")?;
                Ok(self.finish(name, doc, attrs, ItemKind::Struct { fields }, item_start))
            }
            "group" => {
                self.bump();
                let (name, name_span) = self.ident("a group name")?;
                self.name_span = name_span;
                self.expect_sym("{")?;
                let mut items = Vec::new();
                while !self.at_sym("}") && !self.at_eof() {
                    items.push(self.item()?);
                }
                self.expect_sym("}")?;
                Ok(self.finish(name, doc, attrs, ItemKind::Group { items }, item_start))
            }
            "check" | "task" => {
                self.bump();
                let (name, name_span) = self.ident(&format!("a {keyword} name"))?;
                self.name_span = name_span;
                let body = self.block()?;
                let kind = if keyword == "check" {
                    ItemKind::Check { body }
                } else {
                    ItemKind::Task { body }
                };
                Ok(self.finish(name, doc, attrs, kind, item_start))
            }
            "environment" | "profile" | "artifact" => {
                self.bump();
                let (name, name_span) = self.ident(&format!("a {keyword} name"))?;
                self.name_span = name_span;
                self.expect_sym(";")?;
                let kind = match keyword.as_str() {
                    "environment" => ItemKind::Environment,
                    "profile" => ItemKind::Profile,
                    _ => ItemKind::Artifact,
                };
                Ok(self.finish(name, doc, attrs, kind, item_start))
            }
            "service" => {
                self.bump();
                let (name, name_span) = self.ident("a service name")?;
                self.name_span = name_span;
                let (mut start, mut ready) = (None, None);
                if !self.eat_sym(";") {
                    self.expect_sym("{")?;
                    while !self.at_sym("}") && !self.at_eof() {
                        if self.eat_word("start") {
                            start = Some(self.block()?);
                        } else if self.eat_word("ready") {
                            ready = Some(self.block()?);
                        } else {
                            return Err(Error::at(self.span(), format!("expected `start {{ … }}` or `ready {{ … }}`, found {}", self.describe())));
                        }
                    }
                    self.expect_sym("}")?;
                }
                Ok(self.finish(name, doc, attrs, ItemKind::Service { start, ready }, item_start))
            }
            "release" => {
                self.bump();
                let (name, name_span) = self.ident("a release name")?;
                self.name_span = name_span;
                self.expect_sym("{")?;
                let mut steps = Vec::new();
                let mut rollback = None;
                while !self.at_sym("}") && !self.at_eof() {
                    let step = self.step()?;
                    if step.name == "rollback" {
                        rollback = Some(step);
                    } else {
                        steps.push(step);
                    }
                }
                self.expect_sym("}")?;
                Ok(self.finish(
                    name,
                    doc,
                    attrs,
                    ItemKind::Release { steps, rollback },
                    item_start,
                ))
            }
            other => Err(Error::at(
                kind_start,
                format!("expected an item (check, group, fn, const, …), found `{other}`"),
            )
            .help(if other == "let" {
                "a value shared by items is `const NAME = …;`"
            } else {
                "items: const, fn, struct, group, check, task, profile, service, artifact, environment, release"
            })),
        }
    }

    fn finish(
        &self,
        name: String,
        doc: Option<String>,
        attrs: Vec<Attr>,
        kind: ItemKind,
        start: Span,
    ) -> Item {
        Item {
            name_span: self.name_span,
            name,
            doc,
            attrs,
            kind,
            span: self.join(start),
        }
    }

    fn step(&mut self) -> Parsed<StepDecl> {
        let mut doc: Vec<String> = Vec::new();
        let mut attrs = Vec::new();
        let start = self.span();
        loop {
            match self.peek().clone() {
                Tok::Doc(text) => {
                    self.bump();
                    doc.push(text);
                }
                Tok::Sym("#[") => attrs.push(self.attr("#[")?),
                _ => break,
            }
        }
        let name = if self.eat_word("rollback") {
            "rollback".to_owned()
        } else if self.eat_word("step") {
            self.ident("a step name")?.0
        } else {
            return Err(Error::at(
                self.span(),
                format!("expected `step` or `rollback`, found {}", self.describe()),
            ));
        };
        let mut param = None;
        if self.eat_sym("(") {
            if !self.at_sym(")") {
                param = Some(self.param()?);
            }
            self.expect_sym(")")?;
        }
        let body = self.block()?;
        Ok(StepDecl {
            name,
            attrs,
            doc: (!doc.is_empty()).then(|| doc.join("\n")),
            param,
            body,
            span: self.join(start),
        })
    }

    fn param(&mut self) -> Parsed<Param> {
        let (name, span) = self.ident("a parameter name")?;
        self.expect_sym(":")?;
        let ty = self.type_expr()?;
        Ok(Param { name, ty, span })
    }

    fn function(&mut self, is_const: bool, start: Span) -> Parsed<FnDecl> {
        self.bump(); // fn
        let (name, name_span) = self.ident("a function name")?;
        self.name_span = name_span;
        self.expect_sym("(")?;
        let mut params = Vec::new();
        while !self.at_sym(")") {
            params.push(self.param()?);
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        let ret = if self.eat_sym("->") {
            Some(self.type_expr()?)
        } else {
            None
        };
        let body = self.block()?;
        Ok(FnDecl {
            name,
            is_const,
            params,
            ret,
            body,
            span: self.join(start),
        })
    }

    fn type_expr(&mut self) -> Parsed<TypeExpr> {
        let start = self.span();
        if self.eat_sym("(") {
            self.expect_sym(")")?;
            return Ok(TypeExpr {
                name: "()".into(),
                args: Vec::new(),
                span: self.join(start),
            });
        }
        let (name, _) = self.ident("a type")?;
        let mut args = Vec::new();
        if self.eat_sym("<") {
            loop {
                args.push(self.type_expr()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
            self.expect_sym(">")?;
        }
        Ok(TypeExpr {
            name,
            args,
            span: self.join(start),
        })
    }

    fn block(&mut self) -> Parsed<Block> {
        let start = self.expect_sym("{")?;
        let saved = self.no_struct;
        self.no_struct = false;
        let mut stmts = Vec::new();
        let mut tail = None;
        while !self.at_sym("}") && !self.at_eof() {
            if let Some(stmt) = self.stmt()? {
                stmts.push(stmt);
                continue;
            }
            let expr = self.expr()?;
            let block_like = matches!(expr, Expr::If { .. } | Expr::Match { .. } | Expr::Block(_));
            if self.eat_sym(";") {
                stmts.push(Stmt::Expr(expr));
            } else if self.at_sym("}") {
                tail = Some(Box::new(expr));
            } else if block_like {
                stmts.push(Stmt::Expr(expr));
            } else {
                return Err(Error::at(
                    self.span(),
                    format!("expected `;` or `}}`, found {}", self.describe()),
                ));
            }
        }
        self.expect_sym("}")?;
        self.no_struct = saved;
        Ok(Block {
            stmts,
            tail,
            span: self.join(start),
        })
    }

    /// A statement that is not an expression, or None.
    fn stmt(&mut self) -> Parsed<Option<Stmt>> {
        let start = self.span();
        if self.eat_word("let") {
            let mutable = self.eat_word("mut");
            let (name, _) = self.ident("a variable name")?;
            let ty = if self.eat_sym(":") {
                Some(self.type_expr()?)
            } else {
                None
            };
            self.expect_sym("=")?;
            let value = self.expr()?;
            self.expect_sym(";")?;
            return Ok(Some(Stmt::Let {
                name,
                mutable,
                ty,
                value,
                span: self.join(start),
            }));
        }
        if self.eat_word("assert") {
            let cond = self.expr()?;
            let message = if self.eat_sym(",") {
                Some(self.expr()?)
            } else {
                None
            };
            self.expect_sym(";")?;
            return Ok(Some(Stmt::Assert {
                cond,
                message,
                span: self.join(start),
            }));
        }
        if self.eat_word("return") {
            let value = if self.at_sym(";") {
                None
            } else {
                Some(self.expr()?)
            };
            self.expect_sym(";")?;
            return Ok(Some(Stmt::Return {
                value,
                span: self.join(start),
            }));
        }
        if self.eat_word("for") {
            let (var, _) = self.ident("a loop variable")?;
            if !self.eat_word("in") {
                return Err(Error::at(self.span(), "expected `in`"));
            }
            let iter = self.head_expr()?;
            let body = self.block()?;
            return Ok(Some(Stmt::For {
                var,
                iter,
                body,
                span: self.join(start),
            }));
        }
        if let (Tok::Ident(name), Tok::Sym("=")) = (self.peek().clone(), self.peek_at(1))
            && !KEYWORDS.contains(&name.as_str())
        {
            self.bump();
            self.bump();
            let value = self.expr()?;
            self.expect_sym(";")?;
            return Ok(Some(Stmt::Assign {
                name,
                value,
                span: self.join(start),
            }));
        }
        Ok(None)
    }

    /// An expression before a block (`if c {`): no struct literals.
    fn head_expr(&mut self) -> Parsed<Expr> {
        let saved = self.no_struct;
        self.no_struct = true;
        let expr = self.expr();
        self.no_struct = saved;
        expr
    }

    pub fn expr(&mut self) -> Parsed<Expr> {
        self.binary(0)
    }

    fn binary(&mut self, min: u8) -> Parsed<Expr> {
        let mut left = self.unary()?;
        loop {
            let (op, precedence) = match self.peek() {
                Tok::Sym("||") => ("||", 1),
                Tok::Sym("&&") => ("&&", 2),
                Tok::Sym("==") => ("==", 3),
                Tok::Sym("!=") => ("!=", 3),
                Tok::Sym("<") => ("<", 3),
                Tok::Sym("<=") => ("<=", 3),
                Tok::Sym(">") => (">", 3),
                Tok::Sym(">=") => (">=", 3),
                Tok::Sym("+") => ("+", 4),
                Tok::Sym("-") => ("-", 4),
                Tok::Sym("*") => ("*", 5),
                Tok::Sym("/") => ("/", 5),
                Tok::Sym("%") => ("%", 5),
                _ => break,
            };
            if precedence < min {
                break;
            }
            self.bump();
            let right = self.binary(precedence + 1)?;
            let span = Span {
                file: left.span().file,
                start: left.span().start,
                end: right.span().end,
            };
            left = Expr::Binary(op, Box::new(left), Box::new(right), span);
        }
        Ok(left)
    }

    fn unary(&mut self) -> Parsed<Expr> {
        let start = self.span();
        for op in ["!", "-"] {
            if self.eat_sym(op) {
                let inner = self.unary()?;
                return Ok(Expr::Unary(op, Box::new(inner), self.join(start)));
            }
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Parsed<Expr> {
        let start = self.span();
        let mut expr = self.primary()?;
        loop {
            if self.eat_sym("?") {
                expr = Expr::Try(Box::new(expr), self.join(start));
            } else if self.eat_sym(".") {
                let (name, _) = match self.peek().clone() {
                    Tok::Int(number) => {
                        let span = self.bump().span;
                        (number.to_string(), span)
                    }
                    _ => self.ident("a field or method name")?,
                };
                if self.eat_sym("(") {
                    let args = self.args()?;
                    expr = Expr::Method {
                        receiver: Box::new(expr),
                        name,
                        args,
                        span: self.join(start),
                    };
                } else {
                    expr = Expr::Field(Box::new(expr), name, self.join(start));
                }
            } else if self.at_sym("(") {
                self.bump();
                let args = self.args()?;
                expr = Expr::Call {
                    callee: Box::new(expr),
                    args,
                    span: self.join(start),
                };
            } else if self.eat_sym("[") {
                let index = self.expr()?;
                self.expect_sym("]")?;
                expr = Expr::Index(Box::new(expr), Box::new(index), self.join(start));
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn args(&mut self) -> Parsed<Vec<Expr>> {
        let saved = self.no_struct;
        self.no_struct = false;
        let mut args = Vec::new();
        while !self.at_sym(")") {
            if matches!(self.peek_at(1), Tok::Sym(":")) && matches!(self.peek(), Tok::Ident(_)) {
                return Err(
                    Error::at(self.span(), "functions take positional arguments")
                        .help("pass a struct, or use the builder methods of a std type"),
                );
            }
            args.push(self.expr()?);
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        self.no_struct = saved;
        Ok(args)
    }

    fn primary(&mut self) -> Parsed<Expr> {
        let start = self.span();
        match self.peek().clone() {
            Tok::Int(number) => {
                self.bump();
                Ok(Expr::Int(number, start))
            }
            Tok::Duration(seconds) => {
                self.bump();
                Ok(Expr::Duration(seconds, start))
            }
            Tok::Str(parts) => {
                self.bump();
                let mut out = Vec::new();
                for part in parts {
                    match part {
                        Part::Lit(text) => out.push(StrPart::Lit(text)),
                        Part::Hole(text, offset) => {
                            let tokens = lex(self.file, &text, offset)?;
                            let mut inner = Parser {
                                tokens,
                                index: 0,
                                file: self.file,
                                no_struct: false,
                                name_span: Span::default(),
                            };
                            let expr = inner.expr()?;
                            if !inner.at_eof() {
                                return Err(Error::at(
                                    inner.span(),
                                    "unexpected text inside `{…}`",
                                )
                                .help("`{name}`, `{value.field}` or `{call()}` put a value in; write `{{` for a brace"));
                            }
                            out.push(StrPart::Expr(expr));
                        }
                    }
                }
                Ok(Expr::Str(out, start))
            }
            Tok::Sym("(") => {
                self.bump();
                if self.eat_sym(")") {
                    return Ok(Expr::Unit(self.join(start)));
                }
                let saved = self.no_struct;
                self.no_struct = false;
                let inner = self.expr()?;
                self.no_struct = saved;
                self.expect_sym(")")?;
                Ok(inner)
            }
            Tok::Sym("[") => {
                self.bump();
                let saved = self.no_struct;
                self.no_struct = false;
                let mut items = Vec::new();
                while !self.at_sym("]") {
                    items.push(self.expr()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym("]")?;
                self.no_struct = saved;
                Ok(Expr::List(items, self.join(start)))
            }
            Tok::Sym("{") => Ok(Expr::Block(self.block()?)),
            Tok::Sym("|") => Err(Error::at(start, "Citrus has no closures")
                .help("loop with `for`, or call a named `fn`")),
            Tok::Ident(word) if word == "true" || word == "false" => {
                self.bump();
                Ok(Expr::Bool(word == "true", start))
            }
            Tok::Ident(word) if word == "if" => {
                self.bump();
                let cond = self.head_expr()?;
                let then = self.block()?;
                let otherwise = if self.eat_word("else") {
                    if self.at_word("if") {
                        Some(Box::new(self.primary()?))
                    } else {
                        Some(Box::new(Expr::Block(self.block()?)))
                    }
                } else {
                    None
                };
                Ok(Expr::If {
                    cond: Box::new(cond),
                    then,
                    otherwise,
                    span: self.join(start),
                })
            }
            Tok::Ident(word) if word == "match" => {
                self.bump();
                let value = self.head_expr()?;
                self.expect_sym("{")?;
                let mut arms = Vec::new();
                while !self.at_sym("}") {
                    let pattern = self.pattern()?;
                    self.expect_sym("=>")?;
                    let body = self.expr()?;
                    let block = matches!(body, Expr::Block(_));
                    arms.push((pattern, body));
                    if !self.eat_sym(",") && !block {
                        break;
                    }
                }
                self.expect_sym("}")?;
                Ok(Expr::Match {
                    value: Box::new(value),
                    arms,
                    span: self.join(start),
                })
            }
            Tok::Ident(word)
                if (word == "run" || word == "cmd") && matches!(self.peek_at(1), Tok::Sym("!")) =>
            {
                self.bump();
                self.bump();
                self.expect_sym("(")?;
                let Tok::Str(parts) = self.peek().clone() else {
                    return Err(Error::at(
                        self.span(),
                        format!("`{word}!` takes a command line in quotes"),
                    )
                    .help(format!("{word}!(\"cargo test --locked\")")));
                };
                let text_span = self.bump().span;
                self.expect_sym(")")?;
                let (env, words) = self.command_words(&parts, text_span)?;
                if words.is_empty() {
                    return Err(Error::at(text_span, "an empty command"));
                }
                Ok(Expr::Command {
                    run: word == "run",
                    env,
                    words,
                    span: self.join(start),
                })
            }
            Tok::Ident(word) if KEYWORDS.contains(&word.as_str()) => Err(Error::at(
                start,
                format!("`{word}` cannot start an expression here"),
            )),
            Tok::Ident(_) => {
                let mut segments = vec![self.ident("a name")?.0];
                while self.at_sym("::") {
                    self.bump();
                    segments.push(self.ident("a name after `::`")?.0);
                }
                let simple = segments.len() == 1;
                let capital = segments[0].starts_with(|c: char| c.is_ascii_uppercase());
                if simple && capital && !self.no_struct && self.at_sym("{") {
                    self.bump();
                    let mut fields = Vec::new();
                    while !self.at_sym("}") {
                        let (field, field_span) = self.ident("a field name")?;
                        let value = if self.eat_sym(":") {
                            self.expr()?
                        } else {
                            Expr::Path(vec![field.clone()], field_span)
                        };
                        fields.push((field, value));
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                    self.expect_sym("}")?;
                    return Ok(Expr::StructLit {
                        name: segments.remove(0),
                        fields,
                        span: self.join(start),
                    });
                }
                Ok(Expr::Path(segments, self.join(start)))
            }
            _ => Err(Error::at(
                start,
                format!("expected an expression, found {}", self.describe()),
            )),
        }
    }

    /// Split a command line into words: whitespace separates, `'…'` keeps
    /// spaces, `{x}` is part of the word it is in, `{xs...}` a whole word
    /// spread from a list. No shell: `|`, `>`, `*` are plain characters.
    fn command_words(&self, parts: &[Part], span: Span) -> Parsed<CommandLine> {
        let mut words: Vec<CmdWord> = Vec::new();
        let mut current: Vec<CmdPiece> = Vec::new();
        let mut text = String::new();
        let mut quoted = false;
        let mut splat_pending = false;
        let flush = |current: &mut Vec<CmdPiece>, text: &mut String, words: &mut Vec<CmdWord>| {
            if !text.is_empty() {
                current.push(CmdPiece::Lit(std::mem::take(text)));
            }
            if !current.is_empty() {
                words.push(CmdWord::Word(std::mem::take(current)));
            }
        };
        for part in parts {
            match part {
                Part::Lit(literal) => {
                    for ch in literal.chars() {
                        if splat_pending && !ch.is_whitespace() {
                            return Err(Error::at(span, "`{list...}` must be a word of its own")
                                .help("put a space after it"));
                        }
                        splat_pending = false;
                        match ch {
                            '\'' => quoted = !quoted,
                            c if c.is_whitespace() && !quoted => {
                                flush(&mut current, &mut text, &mut words)
                            }
                            c => text.push(c),
                        }
                    }
                }
                Part::Hole(source, offset) => {
                    if let Some(list) = source.trim().strip_suffix("...") {
                        if !text.is_empty() || !current.is_empty() || quoted {
                            return Err(Error::at(span, "`{list...}` must be a word of its own"));
                        }
                        let expr = self.hole_expr(list, *offset)?;
                        words.push(CmdWord::Splat(expr));
                        splat_pending = true;
                        continue;
                    }
                    if !text.is_empty() {
                        current.push(CmdPiece::Lit(std::mem::take(&mut text)));
                    }
                    current.push(CmdPiece::Expr(self.hole_expr(source, *offset)?));
                }
            }
        }
        if quoted {
            return Err(Error::at(span, "unclosed `'` in the command"));
        }
        flush(&mut current, &mut text, &mut words);
        // Leading `KEY=value` words set the environment, as in a shell.
        let mut env = Vec::new();
        while let Some(CmdWord::Word(pieces)) = words.first() {
            let Some(CmdPiece::Lit(first)) = pieces.first() else {
                break;
            };
            let Some((key, rest)) = first.split_once('=') else {
                break;
            };
            if key.is_empty()
                || !key
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                break;
            }
            let mut value = pieces[1..].to_vec();
            if !rest.is_empty() {
                value.insert(0, CmdPiece::Lit(rest.to_owned()));
            }
            env.push((key.to_owned(), CmdWord::Word(value)));
            words.remove(0);
        }
        Ok((env, words))
    }

    fn hole_expr(&self, source: &str, offset: usize) -> Parsed<Expr> {
        let tokens = lex(self.file, source, offset)?;
        let mut inner = Parser {
            tokens,
            index: 0,
            file: self.file,
            no_struct: false,
            name_span: Span::default(),
        };
        let expr = inner.expr()?;
        if !inner.at_eof() {
            return Err(Error::at(inner.span(), "unexpected text inside `{…}`")
                .help("`{name}` puts a value in; write `{{` for a brace"));
        }
        Ok(expr)
    }

    fn pattern(&mut self) -> Parsed<Pattern> {
        let start = self.span();
        match self.peek().clone() {
            Tok::Sym("_") => {
                self.bump();
                Ok(Pattern::Wild(start))
            }
            Tok::Str(_) | Tok::Int(_) | Tok::Sym("-") => Ok(Pattern::Lit(self.unary()?)),
            Tok::Ident(word) if word == "true" || word == "false" => {
                Ok(Pattern::Lit(self.primary()?))
            }
            Tok::Ident(word) if ["Some", "Ok", "Err", "None"].contains(&word.as_str()) => {
                self.bump();
                let inner = if self.eat_sym("(") {
                    let inner = self.pattern()?;
                    self.expect_sym(")")?;
                    Some(Box::new(inner))
                } else {
                    None
                };
                Ok(Pattern::Variant(word, inner, self.join(start)))
            }
            Tok::Ident(_) => {
                let (name, span) = self.ident("a pattern")?;
                Ok(Pattern::Bind(name, span))
            }
            _ => Err(Error::at(
                start,
                format!("expected a pattern, found {}", self.describe()),
            )),
        }
    }
}
