//! The `awk` builtin: a hand-written subset interpreter.
//!
//! No Rust awk crate compiles for wasm32-wasip2 (frawk/zawk hard-require cranelift/LLVM JIT
//! backends), so this is a from-scratch lexer + recursive-descent parser + tree-walking evaluator
//! for the slice of awk that covers real one-liners:
//!
//! - `-F fs`, `-v var=val`; fields `$0..$NF`; `NR NF FS OFS`; user variables
//! - `pattern { action }` with `/regex/` patterns, comparison/boolean expressions, `BEGIN`/`END`
//! - `print` (OFS-joined) and `printf` (subset of %-conversions)
//! - arithmetic (`+ - * / %`), string concatenation (juxtaposition), comparisons, `~`/`!~`,
//!   `&& || !`, assignments (`= += -= *= /= %=`), `++`/`--`, `length(...)`
//!
//! Deliberately absent (parse errors say so honestly): arrays, user functions, control flow
//! (`if`/`while`/`for`), `getline`, field assignment. awk's number/string duck-typing follows the
//! usual rule: compare numerically when both sides look numeric, else as strings.

use std::collections::HashMap;
use std::io::{Read, Write};

type AwkResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Regex(String),
    Ident(String),
    Dollar,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Semi,
    Newline,
    Comma,
    Assign,
    OpAssign(BinOp), // += -= *= /= %=
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Incr,
    Decr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Match,
    NotMatch,
    And,
    Or,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// Whether a `/` at this point starts a regex (vs. division) — the classic awk heuristic: a regex
/// can only follow an operator/opener, never a complete operand.
fn regex_can_follow(prev: Option<&Tok>) -> bool {
    !matches!(
        prev,
        Some(Tok::Num(_) | Tok::Str(_) | Tok::Ident(_) | Tok::RParen | Tok::Regex(_))
    )
}

#[allow(clippy::too_many_lines, clippy::similar_names)] // one lexer dispatch loop; `tok`/`toks` are intentional
fn lex(src: &str) -> AwkResult<Vec<Tok>> {
    let mut toks = Vec::new();
    let mut chars = src.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\r' => {
                chars.next();
            }
            '\\' => {
                // Line continuation.
                chars.next();
                if chars.peek() == Some(&'\n') {
                    chars.next();
                } else {
                    return Err("stray backslash".into());
                }
            }
            '#' => {
                while let Some(&c) = chars.peek() {
                    if c == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            '\n' => {
                chars.next();
                toks.push(Tok::Newline);
            }
            '{' => {
                chars.next();
                toks.push(Tok::LBrace);
            }
            '}' => {
                chars.next();
                toks.push(Tok::RBrace);
            }
            '(' => {
                chars.next();
                toks.push(Tok::LParen);
            }
            ')' => {
                chars.next();
                toks.push(Tok::RParen);
            }
            ';' => {
                chars.next();
                toks.push(Tok::Semi);
            }
            ',' => {
                chars.next();
                toks.push(Tok::Comma);
            }
            '$' => {
                chars.next();
                toks.push(Tok::Dollar);
            }
            '"' => {
                chars.next();
                let mut s = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('\\') => s.push('\\'),
                            Some('"') => s.push('"'),
                            Some('/') => s.push('/'),
                            Some(other) => s.push(other),
                            None => return Err("unterminated string".into()),
                        },
                        Some(other) => s.push(other),
                        None => return Err("unterminated string".into()),
                    }
                }
                toks.push(Tok::Str(s));
            }
            '/' if regex_can_follow(toks.last()) => {
                chars.next();
                let mut re = String::new();
                loop {
                    match chars.next() {
                        Some('/') => break,
                        Some('\\') => match chars.next() {
                            Some('/') => re.push('/'),
                            Some(other) => {
                                re.push('\\');
                                re.push(other);
                            }
                            None => return Err("unterminated regex".into()),
                        },
                        Some(other) => re.push(other),
                        None => return Err("unterminated regex".into()),
                    }
                }
                toks.push(Tok::Regex(re));
            }
            '0'..='9' | '.' => {
                let mut num = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_digit() || c == '.' {
                        num.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                toks.push(Tok::Num(num.parse().map_err(|_| format!("bad number '{num}'"))?));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut id = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        id.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                toks.push(Tok::Ident(id));
            }
            _ => {
                chars.next();
                let two = |chars: &mut std::iter::Peekable<std::str::Chars>, next: char| {
                    if chars.peek() == Some(&next) {
                        chars.next();
                        true
                    } else {
                        false
                    }
                };
                let tok = match c {
                    '+' if two(&mut chars, '+') => Tok::Incr,
                    '+' if two(&mut chars, '=') => Tok::OpAssign(BinOp::Add),
                    '+' => Tok::Plus,
                    '-' if two(&mut chars, '-') => Tok::Decr,
                    '-' if two(&mut chars, '=') => Tok::OpAssign(BinOp::Sub),
                    '-' => Tok::Minus,
                    '*' if two(&mut chars, '=') => Tok::OpAssign(BinOp::Mul),
                    '*' => Tok::Star,
                    '/' if two(&mut chars, '=') => Tok::OpAssign(BinOp::Div),
                    '/' => Tok::Slash,
                    '%' if two(&mut chars, '=') => Tok::OpAssign(BinOp::Mod),
                    '%' => Tok::Percent,
                    '=' if two(&mut chars, '=') => Tok::Eq,
                    '=' => Tok::Assign,
                    '!' if two(&mut chars, '=') => Tok::Ne,
                    '!' if two(&mut chars, '~') => Tok::NotMatch,
                    '!' => Tok::Not,
                    '<' if two(&mut chars, '=') => Tok::Le,
                    '<' => Tok::Lt,
                    '>' if two(&mut chars, '=') => Tok::Ge,
                    '>' => Tok::Gt,
                    '~' => Tok::Match,
                    '&' if two(&mut chars, '&') => Tok::And,
                    '|' if two(&mut chars, '|') => Tok::Or,
                    other => return Err(format!("unexpected character '{other}'").into()),
                };
                toks.push(tok);
            }
        }
    }
    Ok(toks)
}

// ---------------------------------------------------------------------------
// AST + parser
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Expr {
    Num(f64),
    Str(String),
    /// A `/re/` literal: as a standalone value it matches against `$0` (1/0).
    Regex(String),
    Var(String),
    Field(Box<Expr>),
    Assign(String, Option<BinOp>, Box<Expr>),
    IncrDecr {
        name: String,
        delta: f64,
        prefix: bool,
    },
    Not(Box<Expr>),
    Neg(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    MatchRe {
        lhs: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Concat(Box<Expr>, Box<Expr>),
    Length(Option<Box<Expr>>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug)]
enum Stmt {
    Print(Vec<Expr>),
    Printf(Vec<Expr>),
    Expr(Expr),
}

#[derive(Clone, Debug)]
enum Pattern {
    Begin,
    End,
    Always,
    Expr(Expr),
}

#[derive(Clone, Debug)]
struct Rule {
    pattern: Pattern,
    stmts: Vec<Stmt>,
}

const UNSUPPORTED_KEYWORDS: &[&str] = &[
    "if", "else", "while", "for", "do", "function", "func", "getline", "delete", "in", "next",
    "exit", "return", "split", "sub", "gsub", "substr", "index", "sprintf",
];

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn peek2(&self) -> Option<&Tok> {
        self.toks.get(self.pos + 1)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.peek() == Some(tok) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: &Tok) -> AwkResult<()> {
        if self.eat(tok) {
            Ok(())
        } else {
            Err(format!("expected {tok:?}, found {:?}", self.peek()).into())
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(Tok::Newline)) {
            self.pos += 1;
        }
    }

    fn parse_program(&mut self) -> AwkResult<Vec<Rule>> {
        let mut rules = Vec::new();
        loop {
            self.skip_newlines();
            let Some(tok) = self.peek() else { break };
            let pattern = match tok {
                Tok::Ident(id) if id == "BEGIN" => {
                    self.pos += 1;
                    Pattern::Begin
                }
                Tok::Ident(id) if id == "END" => {
                    self.pos += 1;
                    Pattern::End
                }
                Tok::LBrace => Pattern::Always,
                _ => Pattern::Expr(self.parse_expr()?),
            };
            self.skip_newlines();
            let stmts = if self.peek() == Some(&Tok::LBrace) {
                self.parse_block()?
            } else {
                match pattern {
                    Pattern::Begin | Pattern::End => {
                        return Err("BEGIN/END require an action block".into())
                    }
                    // A bare pattern means `{ print }`.
                    _ => vec![Stmt::Print(Vec::new())],
                }
            };
            rules.push(Rule { pattern, stmts });
        }
        Ok(rules)
    }

    fn parse_block(&mut self) -> AwkResult<Vec<Stmt>> {
        self.expect(&Tok::LBrace)?;
        let mut stmts = Vec::new();
        loop {
            while matches!(self.peek(), Some(Tok::Semi | Tok::Newline)) {
                self.pos += 1;
            }
            if self.eat(&Tok::RBrace) {
                return Ok(stmts);
            }
            if self.peek().is_none() {
                return Err("unterminated action block".into());
            }
            stmts.push(self.parse_stmt()?);
        }
    }

    fn parse_stmt(&mut self) -> AwkResult<Stmt> {
        if let Some(Tok::Ident(id)) = self.peek() {
            let id = id.clone();
            if UNSUPPORTED_KEYWORDS.contains(&id.as_str()) {
                return Err(format!("'{id}' is not supported in this awk subset").into());
            }
            if id == "print" {
                self.pos += 1;
                return Ok(Stmt::Print(self.parse_expr_list()?));
            }
            if id == "printf" {
                self.pos += 1;
                let args = self.parse_expr_list()?;
                if args.is_empty() {
                    return Err("printf requires a format argument".into());
                }
                return Ok(Stmt::Printf(args));
            }
        }
        Ok(Stmt::Expr(self.parse_expr()?))
    }

    /// Comma-separated expressions, ending at `;`, `}`, or newline. Empty is allowed (`print`).
    fn parse_expr_list(&mut self) -> AwkResult<Vec<Expr>> {
        let mut exprs = Vec::new();
        if matches!(
            self.peek(),
            None | Some(Tok::Semi | Tok::RBrace | Tok::Newline)
        ) {
            return Ok(exprs);
        }
        loop {
            exprs.push(self.parse_expr()?);
            if !self.eat(&Tok::Comma) {
                return Ok(exprs);
            }
            self.skip_newlines();
        }
    }

    fn parse_expr(&mut self) -> AwkResult<Expr> {
        // Assignment: IDENT (=|+=|-=|*=|/=|%=) expr — right-associative via recursion.
        if let (Some(Tok::Ident(name)), Some(op)) = (self.peek(), self.peek2()) {
            let name = name.clone();
            let op = match op {
                Tok::Assign => Some(None),
                Tok::OpAssign(b) => Some(Some(*b)),
                _ => None,
            };
            if let Some(op) = op {
                if UNSUPPORTED_KEYWORDS.contains(&name.as_str()) {
                    return Err(format!("'{name}' is not supported in this awk subset").into());
                }
                self.pos += 2;
                let rhs = self.parse_expr()?;
                return Ok(Expr::Assign(name, op, Box::new(rhs)));
            }
        }
        self.parse_or()
    }

    fn parse_or(&mut self) -> AwkResult<Expr> {
        let mut lhs = self.parse_and()?;
        while self.eat(&Tok::Or) {
            self.skip_newlines();
            let rhs = self.parse_and()?;
            lhs = Expr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> AwkResult<Expr> {
        let mut lhs = self.parse_match()?;
        while self.eat(&Tok::And) {
            self.skip_newlines();
            let rhs = self.parse_match()?;
            lhs = Expr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_match(&mut self) -> AwkResult<Expr> {
        let mut lhs = self.parse_cmp()?;
        loop {
            let negated = match self.peek() {
                Some(Tok::Match) => false,
                Some(Tok::NotMatch) => true,
                _ => return Ok(lhs),
            };
            self.pos += 1;
            let pattern = self.parse_cmp()?;
            lhs = Expr::MatchRe {
                lhs: Box::new(lhs),
                pattern: Box::new(pattern),
                negated,
            };
        }
    }

    fn parse_cmp(&mut self) -> AwkResult<Expr> {
        let lhs = self.parse_concat()?;
        let op = match self.peek() {
            Some(Tok::Eq) => CmpOp::Eq,
            Some(Tok::Ne) => CmpOp::Ne,
            Some(Tok::Lt) => CmpOp::Lt,
            Some(Tok::Le) => CmpOp::Le,
            Some(Tok::Gt) => CmpOp::Gt,
            Some(Tok::Ge) => CmpOp::Ge,
            _ => return Ok(lhs),
        };
        self.pos += 1;
        let rhs = self.parse_concat()?;
        Ok(Expr::Cmp(op, Box::new(lhs), Box::new(rhs)))
    }

    /// String concatenation by juxtaposition: `$1 " " $2`.
    fn parse_concat(&mut self) -> AwkResult<Expr> {
        let mut lhs = self.parse_additive()?;
        while self.starts_operand() {
            let rhs = self.parse_additive()?;
            lhs = Expr::Concat(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// Whether the next token can begin an operand (for concat juxtaposition). `-`/`+` are NOT
    /// included — they bind as binary operators in `parse_additive`.
    fn starts_operand(&self) -> bool {
        match self.peek() {
            Some(Tok::Num(_) | Tok::Str(_) | Tok::Dollar | Tok::LParen | Tok::Not | Tok::Incr | Tok::Decr) => true,
            Some(Tok::Ident(id)) => !UNSUPPORTED_KEYWORDS.contains(&id.as_str()),
            _ => false,
        }
    }

    fn parse_additive(&mut self) -> AwkResult<Expr> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => BinOp::Add,
                Some(Tok::Minus) => BinOp::Sub,
                _ => return Ok(lhs),
            };
            self.pos += 1;
            let rhs = self.parse_mul()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
    }

    fn parse_mul(&mut self) -> AwkResult<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => BinOp::Mul,
                Some(Tok::Slash) => BinOp::Div,
                Some(Tok::Percent) => BinOp::Mod,
                _ => return Ok(lhs),
            };
            self.pos += 1;
            let rhs = self.parse_unary()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
    }

    fn parse_unary(&mut self) -> AwkResult<Expr> {
        match self.peek() {
            Some(Tok::Not) => {
                self.pos += 1;
                Ok(Expr::Not(Box::new(self.parse_unary()?)))
            }
            Some(Tok::Minus) => {
                self.pos += 1;
                Ok(Expr::Neg(Box::new(self.parse_unary()?)))
            }
            Some(Tok::Plus) => {
                self.pos += 1;
                self.parse_unary()
            }
            Some(Tok::Incr | Tok::Decr) => {
                let delta = if self.peek() == Some(&Tok::Incr) { 1.0 } else { -1.0 };
                self.pos += 1;
                match self.next() {
                    Some(Tok::Ident(name)) => Ok(Expr::IncrDecr {
                        name,
                        delta,
                        prefix: true,
                    }),
                    other => Err(format!("expected variable after ++/--, found {other:?}").into()),
                }
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> AwkResult<Expr> {
        let primary = self.parse_primary()?;
        if let Expr::Var(name) = &primary {
            let delta = match self.peek() {
                Some(Tok::Incr) => Some(1.0),
                Some(Tok::Decr) => Some(-1.0),
                _ => None,
            };
            if let Some(delta) = delta {
                self.pos += 1;
                return Ok(Expr::IncrDecr {
                    name: name.clone(),
                    delta,
                    prefix: false,
                });
            }
        }
        Ok(primary)
    }

    fn parse_primary(&mut self) -> AwkResult<Expr> {
        match self.next() {
            Some(Tok::Num(n)) => Ok(Expr::Num(n)),
            Some(Tok::Str(s)) => Ok(Expr::Str(s)),
            Some(Tok::Regex(re)) => Ok(Expr::Regex(re)),
            Some(Tok::Dollar) => {
                let idx = self.parse_primary()?;
                Ok(Expr::Field(Box::new(idx)))
            }
            Some(Tok::LParen) => {
                let e = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Some(Tok::Ident(id)) => {
                if UNSUPPORTED_KEYWORDS.contains(&id.as_str()) {
                    return Err(format!("'{id}' is not supported in this awk subset").into());
                }
                if id == "length" {
                    if self.eat(&Tok::LParen) {
                        if self.eat(&Tok::RParen) {
                            return Ok(Expr::Length(None));
                        }
                        let arg = self.parse_expr()?;
                        self.expect(&Tok::RParen)?;
                        return Ok(Expr::Length(Some(Box::new(arg))));
                    }
                    return Ok(Expr::Length(None));
                }
                if self.peek() == Some(&Tok::LParen) {
                    return Err(format!("call to unsupported function '{id}'").into());
                }
                Ok(Expr::Var(id))
            }
            other => Err(format!("unexpected token {other:?}").into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Values + evaluator
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Value {
    Num(f64),
    Str(String),
}

/// awk's default number rendering: integers print bare, others get a short decimal form.
#[allow(clippy::float_cmp, clippy::cast_possible_truncation)] // exact integrality test; `n as i64` is guarded by the abs()<1e16 check
fn fmt_num(n: f64) -> String {
    if n == n.trunc() && n.abs() < 1e16 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn str_to_num(s: &str) -> f64 {
    // awk parses the longest numeric prefix ("3x" → 3, "abc" → 0).
    let t = s.trim_start();
    let mut end = 0;
    let bytes = t.as_bytes();
    let mut seen_digit = false;
    let mut seen_dot = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'+' | b'-' if i == 0 => end = i + 1,
            b'0'..=b'9' => {
                seen_digit = true;
                end = i + 1;
            }
            b'.' if !seen_dot => {
                seen_dot = true;
                end = i + 1;
            }
            _ => break,
        }
    }
    if !seen_digit {
        return 0.0;
    }
    t[..end].parse().unwrap_or(0.0)
}

/// A string that is *entirely* numeric — the both-sides-numeric test for comparisons.
fn looks_numeric(s: &str) -> bool {
    !s.trim().is_empty() && s.trim().parse::<f64>().is_ok()
}

impl Value {
    fn num(&self) -> f64 {
        match self {
            Value::Num(n) => *n,
            Value::Str(s) => str_to_num(s),
        }
    }

    fn string(&self) -> String {
        match self {
            Value::Num(n) => fmt_num(*n),
            Value::Str(s) => s.clone(),
        }
    }

    fn truthy(&self) -> bool {
        match self {
            Value::Num(n) => *n != 0.0,
            Value::Str(s) => !s.is_empty(),
        }
    }
}

struct Env {
    vars: HashMap<String, Value>,
    /// `fields[0]` is `$0`; `fields[i]` is `$i`.
    fields: Vec<String>,
    nr: usize,
    fs: String,
    ofs: String,
    regex_cache: HashMap<String, regex::Regex>,
}

impl Env {
    fn new(fs: String) -> Self {
        Self {
            vars: HashMap::new(),
            fields: vec![String::new()],
            nr: 0,
            fs,
            ofs: " ".to_string(),
            regex_cache: HashMap::new(),
        }
    }

    fn set_record(&mut self, line: &str) {
        let split: Vec<String> = if self.fs == " " {
            line.split_whitespace().map(String::from).collect()
        } else if self.fs.chars().count() == 1 {
            let c = self.fs.chars().next().unwrap();
            line.split(c).map(String::from).collect()
        } else {
            match self.compile(&self.fs.clone()) {
                Ok(re) => re.split(line).map(String::from).collect(),
                Err(_) => vec![line.to_string()],
            }
        };
        self.fields = std::iter::once(line.to_string()).chain(split).collect();
    }

    fn nf(&self) -> usize {
        self.fields.len().saturating_sub(1)
    }

    fn field(&self, idx: usize) -> String {
        self.fields.get(idx).cloned().unwrap_or_default()
    }

    fn compile(&mut self, pattern: &str) -> AwkResult<regex::Regex> {
        if let Some(re) = self.regex_cache.get(pattern) {
            return Ok(re.clone());
        }
        let re = regex::Regex::new(pattern)?;
        self.regex_cache.insert(pattern.to_string(), re.clone());
        Ok(re)
    }

    #[allow(clippy::cast_precision_loss)] // NR/NF are small line/field counts; f64 is awk's only number type
    fn get_var(&self, name: &str) -> Value {
        match name {
            "NR" => Value::Num(self.nr as f64),
            "NF" => Value::Num(self.nf() as f64),
            "FS" => Value::Str(self.fs.clone()),
            "OFS" => Value::Str(self.ofs.clone()),
            _ => self
                .vars
                .get(name)
                .cloned()
                .unwrap_or(Value::Str(String::new())),
        }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // NR is a non-negative counter
    fn set_var(&mut self, name: &str, value: Value) {
        match name {
            "FS" => self.fs = value.string(),
            "OFS" => self.ofs = value.string(),
            "NR" => self.nr = value.num() as usize,
            _ => {
                self.vars.insert(name.to_string(), value);
            }
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)] // field index is guarded >= 0; char counts are small
fn eval(expr: &Expr, env: &mut Env) -> AwkResult<Value> {
    Ok(match expr {
        Expr::Num(n) => Value::Num(*n),
        Expr::Str(s) => Value::Str(s.clone()),
        Expr::Regex(re) => {
            let re = env.compile(re)?;
            Value::Num(f64::from(re.is_match(&env.field(0))))
        }
        Expr::Var(name) => env.get_var(name),
        Expr::Field(idx) => {
            let idx = eval(idx, env)?.num();
            if idx < 0.0 {
                return Err("negative field index".into());
            }
            Value::Str(env.field(idx as usize))
        }
        Expr::Assign(name, op, rhs) => {
            let rhs = eval(rhs, env)?;
            let value = match op {
                None => rhs,
                Some(op) => Value::Num(arith(*op, env.get_var(name).num(), rhs.num())),
            };
            env.set_var(name, value.clone());
            value
        }
        Expr::IncrDecr {
            name,
            delta,
            prefix,
        } => {
            let old = env.get_var(name).num();
            let new = old + delta;
            env.set_var(name, Value::Num(new));
            Value::Num(if *prefix { new } else { old })
        }
        Expr::Not(e) => Value::Num(f64::from(!eval(e, env)?.truthy())),
        Expr::Neg(e) => Value::Num(-eval(e, env)?.num()),
        Expr::Bin(op, lhs, rhs) => {
            Value::Num(arith(*op, eval(lhs, env)?.num(), eval(rhs, env)?.num()))
        }
        Expr::Cmp(op, lhs, rhs) => {
            let l = eval(lhs, env)?;
            let r = eval(rhs, env)?;
            // Numeric comparison when both sides look numeric; else string comparison.
            let numeric = match (&l, &r) {
                (Value::Num(_), Value::Num(_)) => true,
                (Value::Num(_), Value::Str(s)) | (Value::Str(s), Value::Num(_)) => {
                    looks_numeric(s)
                }
                (Value::Str(a), Value::Str(b)) => looks_numeric(a) && looks_numeric(b),
            };
            let ord = if numeric {
                l.num().partial_cmp(&r.num())
            } else {
                Some(l.string().cmp(&r.string()))
            };
            let Some(ord) = ord else {
                return Ok(Value::Num(0.0)); // NaN comparisons are false
            };
            let result = match op {
                CmpOp::Eq => ord.is_eq(),
                CmpOp::Ne => ord.is_ne(),
                CmpOp::Lt => ord.is_lt(),
                CmpOp::Le => ord.is_le(),
                CmpOp::Gt => ord.is_gt(),
                CmpOp::Ge => ord.is_ge(),
            };
            Value::Num(f64::from(result))
        }
        Expr::MatchRe {
            lhs,
            pattern,
            negated,
        } => {
            let text = eval(lhs, env)?.string();
            // `/re/` on the right of `~` is the regex itself, not a match-against-$0.
            let pattern = match pattern.as_ref() {
                Expr::Regex(re) => re.clone(),
                other => eval(other, env)?.string(),
            };
            let re = env.compile(&pattern)?;
            Value::Num(f64::from(re.is_match(&text) != *negated))
        }
        Expr::And(lhs, rhs) => Value::Num(f64::from(
            eval(lhs, env)?.truthy() && eval(rhs, env)?.truthy(),
        )),
        Expr::Or(lhs, rhs) => Value::Num(f64::from(
            eval(lhs, env)?.truthy() || eval(rhs, env)?.truthy(),
        )),
        Expr::Concat(lhs, rhs) => {
            let mut s = eval(lhs, env)?.string();
            s.push_str(&eval(rhs, env)?.string());
            Value::Str(s)
        }
        Expr::Length(arg) => {
            let s = match arg {
                Some(e) => eval(e, env)?.string(),
                None => env.field(0),
            };
            Value::Num(s.chars().count() as f64)
        }
    })
}

fn arith(op: BinOp, l: f64, r: f64) -> f64 {
    match op {
        BinOp::Add => l + r,
        BinOp::Sub => l - r,
        BinOp::Mul => l * r,
        BinOp::Div => l / r,
        BinOp::Mod => l % r,
    }
}

fn exec_stmts(stmts: &[Stmt], env: &mut Env, out: &mut dyn Write) -> AwkResult<()> {
    for stmt in stmts {
        match stmt {
            Stmt::Print(exprs) => {
                let rendered = if exprs.is_empty() {
                    env.field(0)
                } else {
                    let mut parts = Vec::with_capacity(exprs.len());
                    for e in exprs {
                        parts.push(eval(e, env)?.string());
                    }
                    parts.join(&env.ofs)
                };
                out.write_all(rendered.as_bytes())?;
                out.write_all(b"\n")?;
            }
            Stmt::Printf(args) => {
                let format = eval(&args[0], env)?.string();
                let mut values = Vec::with_capacity(args.len() - 1);
                for e in &args[1..] {
                    values.push(eval(e, env)?);
                }
                let rendered = format_printf(&format, &values)?;
                out.write_all(rendered.as_bytes())?;
            }
            Stmt::Expr(e) => {
                eval(e, env)?;
            }
        }
    }
    Ok(())
}

/// A subset printf: `%[-0][width][.prec](d|i|f|g|e|s|x|o|c|%)` plus `\n`/`\t` escapes.
#[allow(clippy::cast_possible_truncation, clippy::similar_names)] // %d/%x/%o intentionally truncate the f64 to an integer; pad_len/pad_char share the pad_ prefix
fn format_printf(format: &str, values: &[Value]) -> AwkResult<String> {
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    let mut next_value = values.iter();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') | None => out.push('\\'),
                Some(other) => out.push(other),
            },
            '%' => {
                if chars.peek() == Some(&'%') {
                    chars.next();
                    out.push('%');
                    continue;
                }
                let mut left = false;
                let mut zero = false;
                while let Some(&f) = chars.peek() {
                    match f {
                        '-' => left = true,
                        '0' => zero = true,
                        _ => break,
                    }
                    chars.next();
                }
                let mut width = String::new();
                while chars.peek().is_some_and(char::is_ascii_digit) {
                    width.push(chars.next().unwrap());
                }
                let width: Option<usize> = width.parse().ok();
                let mut prec: Option<usize> = None;
                if chars.peek() == Some(&'.') {
                    chars.next();
                    let mut p = String::new();
                    while chars.peek().is_some_and(char::is_ascii_digit) {
                        p.push(chars.next().unwrap());
                    }
                    prec = Some(p.parse().unwrap_or(0));
                }
                let conv = chars.next().ok_or("incomplete % conversion")?;
                let value = next_value
                    .next()
                    .ok_or("not enough arguments for format string")?;
                let text = match conv {
                    'd' | 'i' => format!("{}", value.num() as i64),
                    'f' => format!("{:.*}", prec.unwrap_or(6), value.num()),
                    'e' => format!("{:.*e}", prec.unwrap_or(6), value.num()),
                    'g' => fmt_num(value.num()),
                    'x' => format!("{:x}", value.num() as i64),
                    'o' => format!("{:o}", value.num() as i64),
                    'c' => value.string().chars().next().map(String::from).unwrap_or_default(),
                    's' => {
                        let s = value.string();
                        match prec {
                            Some(p) => s.chars().take(p).collect(),
                            None => s,
                        }
                    }
                    other => return Err(format!("unsupported conversion '%{other}'").into()),
                };
                let padded = match width {
                    Some(w) if text.chars().count() < w => {
                        let pad_len = w - text.chars().count();
                        let pad_char = if zero && !left { '0' } else { ' ' };
                        let pad: String = std::iter::repeat_n(pad_char, pad_len).collect();
                        if left {
                            format!("{text}{pad}")
                        } else {
                            format!("{pad}{text}")
                        }
                    }
                    _ => text,
                };
                out.push_str(&padded);
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// The `run_tool`-shaped entry point (registered via `text_builtin!` in texttools).
#[allow(clippy::similar_names)] // argv/args/arg, rules/rule, files/file are conventional
pub(crate) fn run_awk(
    argv: &[String],
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    _err: &mut dyn Write,
) -> Result<i32, Box<dyn std::error::Error>> {
    let args = &argv[1..];
    let mut fs = " ".to_string();
    let mut presets: Vec<(String, String)> = Vec::new();
    let mut program: Option<String> = None;
    let mut files: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-F" => {
                let f = iter.next().ok_or("option requires an argument -- 'F'")?;
                // `-F '\t'` arrives as the two chars `\` `t`.
                fs = match f.as_str() {
                    r"\t" => "\t".to_string(),
                    other => other.to_string(),
                };
            }
            "-v" => {
                let kv = iter.next().ok_or("option requires an argument -- 'v'")?;
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| format!("invalid -v assignment '{kv}'"))?;
                presets.push((k.to_string(), v.to_string()));
            }
            f if f.starts_with("-F") && f.len() > 2 => {
                fs = f[2..].to_string();
            }
            f if f.starts_with('-') && f.len() > 1 => {
                return Err(format!("unknown option '{f}'").into());
            }
            operand => {
                if program.is_none() {
                    program = Some(operand.to_string());
                } else {
                    files.push(operand.to_string());
                }
            }
        }
    }
    let Some(program) = program else {
        return Err("usage: awk [-F fs] [-v var=val] 'program' [FILE...]".into());
    };

    let toks = lex(&program)?;
    let rules = Parser { toks, pos: 0 }.parse_program()?;

    let mut env = Env::new(fs);
    for (k, v) in presets {
        env.set_var(&k, Value::Str(v));
    }

    for rule in &rules {
        if matches!(rule.pattern, Pattern::Begin) {
            exec_stmts(&rule.stmts, &mut env, out)?;
        }
    }

    // GNU behavior: a program with only BEGIN rules never reads input.
    let wants_input = rules
        .iter()
        .any(|r| !matches!(r.pattern, Pattern::Begin));
    if wants_input {
        let mut text = String::new();
        if files.is_empty() {
            stdin.read_to_string(&mut text)?;
        } else {
            for file in &files {
                text.push_str(&std::fs::read_to_string(file)?);
            }
        }
        for line in text.split_inclusive('\n') {
            let line = line.strip_suffix('\n').unwrap_or(line);
            env.nr += 1;
            env.set_record(line);
            for rule in &rules {
                let fire = match &rule.pattern {
                    Pattern::Begin | Pattern::End => false,
                    Pattern::Always => true,
                    Pattern::Expr(e) => eval(e, &mut env)?.truthy(),
                };
                if fire {
                    exec_stmts(&rule.stmts, &mut env, out)?;
                }
            }
        }
    }

    for rule in &rules {
        if matches!(rule.pattern, Pattern::End) {
            exec_stmts(&rule.stmts, &mut env, out)?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn awk(args: &[&str], input: &str) -> String {
        let argv: Vec<String> = std::iter::once("awk")
            .chain(args.iter().copied())
            .map(String::from)
            .collect();
        let mut stdin = input.as_bytes();
        let mut out = Vec::new();
        let mut err = Vec::new();
        run_awk(&argv, &mut stdin, &mut out, &mut err).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn awk_err(args: &[&str]) -> String {
        let argv: Vec<String> = std::iter::once("awk")
            .chain(args.iter().copied())
            .map(String::from)
            .collect();
        let mut stdin = "".as_bytes();
        let mut out = Vec::new();
        let mut err = Vec::new();
        run_awk(&argv, &mut stdin, &mut out, &mut err)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn field_selection_and_fs() {
        assert_eq!(awk(&["{print $2}"], "a b c\nd e f\n"), "b\ne\n");
        assert_eq!(awk(&["{print $NF}"], "a b c\n"), "c\n");
        assert_eq!(awk(&["-F", ":", "{print $1}"], "root:x:0\n"), "root\n");
        assert_eq!(awk(&["{print $0}"], "a  b\n"), "a  b\n");
        // Out-of-range fields are empty.
        assert_eq!(awk(&["{print $9}"], "a\n"), "\n");
    }

    #[test]
    fn patterns_regex_and_comparison() {
        assert_eq!(awk(&["/err/"], "ok\nerr 1\nerr 2\n"), "err 1\nerr 2\n");
        assert_eq!(
            awk(&["-F", ":", "$3 > 10 {print $1}"], "a:x:5\nb:x:50\n"),
            "b\n"
        );
        assert_eq!(awk(&["$1 == \"go\" {print $2}"], "stop x\ngo y\n"), "y\n");
        assert_eq!(awk(&["$0 ~ /a.c/"], "abc\nxyz\n"), "abc\n");
        assert_eq!(awk(&["$1 !~ /^#/"], "#c\nkeep\n"), "keep\n");
        assert_eq!(awk(&["NR == 2"], "a\nb\nc\n"), "b\n");
    }

    #[test]
    fn begin_end_and_accumulators() {
        assert_eq!(awk(&["{s += $1} END {print s}"], "1\n2\n3\n"), "6\n");
        assert_eq!(awk(&["/x/ {n++} END {print n}"], "x\ny\nx\n"), "2\n");
        assert_eq!(
            awk(&["BEGIN {print \"start\"} {print} END {print \"done\"}"], "m\n"),
            "start\nm\ndone\n"
        );
        // BEGIN-only programs read no input.
        assert_eq!(awk(&["BEGIN {print 1 + 2}"], ""), "3\n");
    }

    #[test]
    fn expressions_concat_arith_vars() {
        assert_eq!(awk(&["{print $2, $1}"], "a b\n"), "b a\n");
        assert_eq!(awk(&["{print $1 \"-\" $2}"], "a b\n"), "a-b\n");
        assert_eq!(awk(&["BEGIN {print 7 % 3, 2 * 3, 10 / 4}"], ""), "1 6 2.5\n");
        assert_eq!(awk(&["-v", "x=5", "BEGIN {print x + 1}"], ""), "6\n");
        assert_eq!(awk(&["BEGIN {OFS=\"|\"; print 1, 2}"], ""), "1|2\n");
        assert_eq!(awk(&["{print length($1), length}"], "abc de\n"), "3 6\n");
        assert_eq!(awk(&["BEGIN {n = 1; n += 2; print n++; print n}"], ""), "3\n4\n");
    }

    #[test]
    fn printf_subset() {
        assert_eq!(awk(&["BEGIN {printf \"%d-%s\\n\", 42, \"x\"}"], ""), "42-x\n");
        assert_eq!(awk(&["BEGIN {printf \"%5d|\\n\", 42}"], ""), "   42|\n");
        assert_eq!(awk(&["BEGIN {printf \"%-5s|\\n\", \"ab\"}"], ""), "ab   |\n");
        assert_eq!(awk(&["BEGIN {printf \"%.2f\\n\", 3.14159}"], ""), "3.14\n");
        assert_eq!(awk(&["BEGIN {printf \"%05d\\n\", 42}"], ""), "00042\n");
    }

    #[test]
    fn numeric_vs_string_comparison() {
        // Numeric-looking strings compare numerically ("10" > "9").
        assert_eq!(awk(&["$1 > $2 {print \"yes\"}"], "10 9\n"), "yes\n");
        // Non-numeric strings compare lexically.
        assert_eq!(awk(&["$1 < $2 {print \"yes\"}"], "abc abd\n"), "yes\n");
    }

    #[test]
    fn unsupported_surface_is_honest() {
        assert!(awk_err(&["{ for (i in x) print }"]).contains("not supported"));
        assert!(awk_err(&["{ if ($1) print }"]).contains("not supported"));
        assert!(awk_err(&["{ split($0, a) }"]).contains("not supported"));
        assert!(awk_err(&["{ foo($1) }"]).contains("unsupported function"));
    }
}
