//! Syntax tree of language v2.

// Spans and docs of every node are kept for errors and execution events.
#![allow(dead_code)]

use crate::lang::Span;

/// A type as written: `str`, `list<glob>`, `Option<int>`, `Release`.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeExpr {
    pub name: String,
    pub args: Vec<TypeExpr>,
    pub span: Span,
}

/// `#[name]`, `#[name(arg, key = arg)]`.
#[derive(Debug, Clone)]
pub struct Attr {
    pub name: String,
    pub args: Vec<(Option<String>, Expr)>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FnDecl {
    pub name: String,
    pub is_const: bool,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub body: Block,
    pub span: Span,
}

/// A release step or rollback: an optional `Release` parameter and a body.
#[derive(Debug, Clone)]
pub struct StepDecl {
    pub name: String,
    pub attrs: Vec<Attr>,
    pub doc: Option<String>,
    pub param: Option<Param>,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ItemKind {
    Const {
        ty: Option<TypeExpr>,
        value: Expr,
    },
    Fn(FnDecl),
    Struct {
        fields: Vec<(String, TypeExpr)>,
    },
    Group {
        items: Vec<Item>,
    },
    Check {
        body: Block,
    },
    Task {
        body: Block,
    },
    Environment,
    /// A profile checks may belong to (`#[env(…)]` for its checks).
    Profile,
    /// A resource checks need; `start`/`ready` bodies when Citrus starts it.
    Service {
        start: Option<Block>,
        ready: Option<Block>,
    },
    /// Something built from the repository (an image), for environments.
    Artifact,
    Release {
        steps: Vec<StepDecl>,
        rollback: Option<StepDecl>,
    },
}

#[derive(Debug, Clone)]
pub struct Item {
    pub name: String,
    pub name_span: Span,
    pub doc: Option<String>,
    pub attrs: Vec<Attr>,
    pub kind: ItemKind,
    pub span: Span,
}

impl Item {
    pub fn attr(&self, name: &str) -> Option<&Attr> {
        self.attrs.iter().find(|attr| attr.name == name)
    }
}

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    /// The value of the block: its last expression without `;`.
    pub tail: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Let {
        name: String,
        mutable: bool,
        ty: Option<TypeExpr>,
        value: Expr,
        span: Span,
    },
    Assign {
        name: String,
        value: Expr,
        span: Span,
    },
    Assert {
        cond: Expr,
        message: Option<Expr>,
        span: Span,
    },
    Return {
        value: Option<Expr>,
        span: Span,
    },
    For {
        var: String,
        iter: Expr,
        body: Block,
        span: Span,
    },
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub enum StrPart {
    Lit(String),
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Wild(Span),
    Bind(String, Span),
    Lit(Expr),
    /// `Some(p)`, `Ok(p)`, `Err(p)`, `None`.
    Variant(String, Option<Box<Pattern>>, Span),
}

/// A piece of one word of a command line: text or `{expr}`.
#[derive(Debug, Clone)]
pub enum CmdPiece {
    Lit(String),
    Expr(Expr),
}

/// One argument of `run!`/`cmd!`: pieces joined into one argument, or a
/// list spread into several (`{flags...}`).
#[derive(Debug, Clone)]
pub enum CmdWord {
    Word(Vec<CmdPiece>),
    Splat(Expr),
}

impl CmdWord {
    /// The word when it is plain text.
    pub fn literal(&self) -> Option<String> {
        match self {
            CmdWord::Word(pieces) => pieces
                .iter()
                .map(|piece| match piece {
                    CmdPiece::Lit(text) => Some(text.clone()),
                    CmdPiece::Expr(_) => None,
                })
                .collect(),
            CmdWord::Splat(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Expr {
    Unit(Span),
    Bool(bool, Span),
    Int(i64, Span),
    Duration(u64, Span),
    Str(Vec<StrPart>, Span),
    /// `name` or `std::fs::read`.
    Path(Vec<String>, Span),
    List(Vec<Expr>, Span),
    StructLit {
        name: String,
        fields: Vec<(String, Expr)>,
        span: Span,
    },
    Field(Box<Expr>, String, Span),
    Index(Box<Expr>, Box<Expr>, Span),
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        span: Span,
    },
    Method {
        receiver: Box<Expr>,
        name: String,
        args: Vec<Expr>,
        span: Span,
    },
    Unary(&'static str, Box<Expr>, Span),
    Binary(&'static str, Box<Expr>, Box<Expr>, Span),
    Try(Box<Expr>, Span),
    If {
        cond: Box<Expr>,
        then: Block,
        otherwise: Option<Box<Expr>>,
        span: Span,
    },
    Match {
        value: Box<Expr>,
        arms: Vec<(Pattern, Expr)>,
        span: Span,
    },
    Block(Block),
    /// `run!("…")` (runs it) or `cmd!("…")` (a `Command`), split into words
    /// when the file loads; leading `KEY=value` words are its environment.
    Command {
        run: bool,
        env: Vec<(String, CmdWord)>,
        words: Vec<CmdWord>,
        span: Span,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Unit(span)
            | Expr::Bool(_, span)
            | Expr::Int(_, span)
            | Expr::Duration(_, span)
            | Expr::Str(_, span)
            | Expr::Path(_, span)
            | Expr::List(_, span)
            | Expr::StructLit { span, .. }
            | Expr::Field(_, _, span)
            | Expr::Index(_, _, span)
            | Expr::Call { span, .. }
            | Expr::Method { span, .. }
            | Expr::Unary(_, _, span)
            | Expr::Binary(_, _, _, span)
            | Expr::Try(_, span)
            | Expr::If { span, .. }
            | Expr::Match { span, .. }
            | Expr::Command { span, .. } => *span,
            Expr::Block(block) => block.span,
        }
    }
}

/// A parsed project: its files' items and inner attributes.
#[derive(Debug, Clone, Default)]
pub struct Program {
    pub inner: Vec<Attr>,
    pub items: Vec<Item>,
}
