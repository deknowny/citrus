//! Syntax tree of `.ci` files.

use super::Span;

// Spans of every item are kept for execution events (docs/design/language.md).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum Item {
    Use {
        path: String,
        span: Span,
    },
    Let {
        name: String,
        value: Expr,
        span: Span,
    },
    Fn {
        name: String,
        params: Vec<(String, Option<Expr>)>,
        body: Body,
        span: Span,
    },
    Field {
        name: String,
        value: Expr,
        span: Span,
    },
    Block {
        kind: String,
        label: Option<Expr>,
        items: Vec<Item>,
        span: Span,
    },
    For {
        var: String,
        iter: Expr,
        items: Vec<Item>,
        span: Span,
    },
    If {
        cond: Expr,
        then: Vec<Item>,
        otherwise: Vec<Item>,
        span: Span,
    },
}

/// `{ let …; expr }` bodies of functions and `if` expressions.
#[derive(Debug, Clone)]
pub struct Body {
    pub lets: Vec<(String, Expr, Span)>,
    pub value: Box<Expr>,
}

#[derive(Debug, Clone)]
pub enum StrPart {
    Lit(String),
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub enum Expr {
    None(Span),
    Bool(bool, Span),
    Int(i64, Span),
    Duration(u64, Span),
    Str(Vec<StrPart>, Span),
    Name(String, Span),
    List(Vec<Expr>, Span),
    Map(Vec<(String, Expr)>, Span),
    Comprehension {
        value: Box<Expr>,
        var: String,
        iter: Box<Expr>,
        cond: Option<Box<Expr>>,
        span: Span,
    },
    Field(Box<Expr>, String, Span),
    Index(Box<Expr>, Box<Expr>, Span),
    Call {
        callee: Box<Expr>,
        args: Vec<(Option<String>, Expr)>,
        span: Span,
    },
    Unary(&'static str, Box<Expr>, Span),
    Binary(&'static str, Box<Expr>, Box<Expr>, Span),
    If {
        cond: Box<Expr>,
        then: Body,
        otherwise: Body,
        span: Span,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::None(span)
            | Expr::Bool(_, span)
            | Expr::Int(_, span)
            | Expr::Duration(_, span)
            | Expr::Str(_, span)
            | Expr::Name(_, span)
            | Expr::List(_, span)
            | Expr::Map(_, span)
            | Expr::Comprehension { span, .. }
            | Expr::Field(_, _, span)
            | Expr::Index(_, _, span)
            | Expr::Call { span, .. }
            | Expr::Unary(_, _, span)
            | Expr::Binary(_, _, _, span)
            | Expr::If { span, .. } => *span,
        }
    }
}
