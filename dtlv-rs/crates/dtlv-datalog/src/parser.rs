use std::collections::HashMap;
use dtlv_codec::Value;
use dtlv_search::Query as FtsQuery;

use crate::PathSeg;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("unexpected token: {0}")]
    UnexpectedToken(String),
    #[error("invalid number: {0}")]
    InvalidNumber(String),
    #[error("invalid query syntax: {0}")]
    InvalidQuery(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Edn {
    Keyword(String),
    Symbol(String),
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Vec(Vec<Edn>),
    List(Vec<Edn>),
    Map(Vec<(Edn, Edn)>),
}

fn tokenize_edn(input: &str) -> Result<Vec<String>, ParseError> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() || c == ',' {
            i += 1;
            continue;
        }
        if c == ';' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if matches!(c, '[' | ']' | '(' | ')' | '{' | '}') {
            tokens.push(c.to_string());
            i += 1;
            continue;
        }
        if c == '"' {
            let mut s = String::from("\"");
            i += 1;
            let mut closed = false;
            while i < chars.len() {
                let ch = chars[i];
                if ch == '\\' && i + 1 < chars.len() {
                    s.push(chars[i + 1]);
                    i += 2;
                } else if ch == '"' {
                    s.push('"');
                    i += 1;
                    closed = true;
                    break;
                } else {
                    s.push(ch);
                    i += 1;
                }
            }
            if !closed {
                return Err(ParseError::UnexpectedEof);
            }
            tokens.push(s);
            continue;
        }
        let mut atom = String::new();
        while i < chars.len()
            && !chars[i].is_whitespace()
            && chars[i] != ','
            && !matches!(chars[i], '[' | ']' | '(' | ')' | '{' | '}')
        {
            atom.push(chars[i]);
            i += 1;
        }
        tokens.push(atom);
    }
    Ok(tokens)
}

fn parse_edn_tokens(tokens: &[String], pos: &mut usize) -> Result<Edn, ParseError> {
    if *pos >= tokens.len() {
        return Err(ParseError::UnexpectedEof);
    }
    let tok = &tokens[*pos];
    match tok.as_str() {
        "[" => {
            *pos += 1;
            let mut items = Vec::new();
            while *pos < tokens.len() && tokens[*pos] != "]" {
                items.push(parse_edn_tokens(tokens, pos)?);
            }
            if *pos >= tokens.len() {
                return Err(ParseError::UnexpectedEof);
            }
            *pos += 1;
            Ok(Edn::Vec(items))
        }
        "(" => {
            *pos += 1;
            let mut items = Vec::new();
            while *pos < tokens.len() && tokens[*pos] != ")" {
                items.push(parse_edn_tokens(tokens, pos)?);
            }
            if *pos >= tokens.len() {
                return Err(ParseError::UnexpectedEof);
            }
            *pos += 1;
            Ok(Edn::List(items))
        }
        "{" => {
            *pos += 1;
            let mut pairs = Vec::new();
            while *pos < tokens.len() && tokens[*pos] != "}" {
                let k = parse_edn_tokens(tokens, pos)?;
                let v = parse_edn_tokens(tokens, pos)?;
                pairs.push((k, v));
            }
            if *pos >= tokens.len() {
                return Err(ParseError::UnexpectedEof);
            }
            *pos += 1;
            Ok(Edn::Map(pairs))
        }
        "]" | ")" | "}" => Err(ParseError::UnexpectedToken(tok.clone())),
        s if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 => {
            *pos += 1;
            Ok(Edn::Str(s[1..s.len() - 1].to_owned()))
        }
        s if s.starts_with(':') => {
            *pos += 1;
            Ok(Edn::Keyword(s.to_owned()))
        }
        "true" => {
            *pos += 1;
            Ok(Edn::Bool(true))
        }
        "false" => {
            *pos += 1;
            Ok(Edn::Bool(false))
        }
        "#inst" => {
            *pos += 1;
            if *pos >= tokens.len() {
                return Err(ParseError::UnexpectedEof);
            }
            let s = &tokens[*pos];
            if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
                *pos += 1;
                let inner = &s[1..s.len() - 1];
                let ms = parse_iso8601_utc_ms(inner)
                    .ok_or_else(|| ParseError::InvalidQuery(format!("invalid #inst: {}", inner)))?;
                Ok(Edn::Int(ms))
            } else {
                Err(ParseError::UnexpectedToken(s.clone()))
            }
        }
        s => {
            *pos += 1;
            if let Ok(n) = s.parse::<i64>() {
                Ok(Edn::Int(n))
            } else if let Ok(f) = s.parse::<f64>() {
                Ok(Edn::Float(f))
            } else {
                Ok(Edn::Symbol(s.to_owned()))
            }
        }
    }
}

pub fn parse_edn(input: &str) -> Result<Edn, ParseError> {
    let tokens = tokenize_edn(input)?;
    let mut pos = 0;
    parse_edn_tokens(&tokens, &mut pos)
}

#[derive(Debug, Clone, PartialEq)]
pub enum FindElem {
    Var(String),
    Agg(AggOp, String),
    Pull(String, Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggOp {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindSpec {
    Rel,
    Scalar,
    Coll,
    Tuple,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprTerm {
    Var(String),
    Wildcard,
    Val(Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Le,
    Lt,
    Ge,
    Gt,
    Eq,
    Ne,
    /// Carries the escape character (`!` unless `{:escape "c"}` is given).
    Like(char),
    NotLike(char),
}

/// Output binding of a function clause: `[(f ...) ?x]`, `[(f ...) [?a ?b]]`, or a bare filter `[(f ...)]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FnOut {
    None,
    Var(String),
    /// One entry per tuple position; `None` is an ignored (`_`) position.
    Tuple(Vec<Option<String>>),
    /// `[?x ...]`: one result row per element of a collection.
    Coll(String),
    /// `[[?a ?b]]`: one result row per tuple; `None` is an ignored (`_`) position.
    Rel(Vec<Option<String>>),
}

/// One `:in` input binding: `?x`, `[?x ...]`, `[?a ?b]`, or `[[?a ?b] ...]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InBinding {
    Scalar(String),
    Coll(String),
    Tuple(Vec<String>),
    Rel(Vec<String>),
}

impl InBinding {
    pub fn vars(&self) -> Vec<String> {
        match self {
            InBinding::Scalar(v) | InBinding::Coll(v) => vec![v.clone()],
            InBinding::Tuple(vs) | InBinding::Rel(vs) => vs.clone(),
        }
    }
}

/// A runtime value supplied for an `:in` binding.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryInput {
    Scalar(Value),
    Coll(Vec<Value>),
    Tuple(Vec<Value>),
    Rel(Vec<Vec<Value>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderKey {
    Var(String),
    Col(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderSpec {
    pub key: OrderKey,
    pub desc: bool,
}

/// Full-text query argument: a literal query tree, or a variable bound to a string at run time.
#[derive(Debug, Clone)]
pub enum FtsArg {
    Static(FtsQuery),
    Var(String),
}

#[derive(Debug, Clone)]
pub enum Clause {
    Datom(ExprTerm, String, ExprTerm),
    Pred(CmpOp, ExprTerm, ExprTerm),
    Fulltext(FtsArg, String), // (query, entity_var)
    /// `[(idoc-match $ [:attr] {pattern}) [[?e ?a ?v]]]`: attribute (or all idoc attributes), predicates,
    /// and the `?e ?a ?v` output variables (`None` for `_`).
    IdocMatch(Option<String>, Vec<(Vec<PathSeg>, ExprTerm)>, [Option<String>; 3]),
    Or(Vec<Vec<Clause>>),
    /// `(or-join [?join ...] branch ...)`: branches may bind only the join vars into the outer scope.
    OrJoin(Vec<String>, Vec<Vec<Clause>>),
    Not(Vec<Clause>),
    /// `(not-join [?join ...] clause ...)`: clauses see only the join vars from the outer scope.
    NotJoin(Vec<String>, Vec<Clause>),
    RuleCall(String, Vec<ExprTerm>),
    /// Built-in function call: `[(get-else $ ?e :a 0) ?v]` or `[(missing? $ ?e :a)]`.
    Fn(String, Vec<ExprTerm>, FnOut),
    /// `[(q [:find ...] $ args...) [[?a ?b]]]`: a nested query whose result columns bind positionally to
    /// the output variables (`None` for `_`). Its `:in` inputs are the args, in order.
    Nested(Box<ParsedQuery>, Vec<ExprTerm>, Vec<Option<String>>),
}

#[derive(Debug, Clone)]
pub struct ParsedQuery {
    pub find: Vec<FindElem>,
    pub find_spec: FindSpec,
    pub in_bindings: Vec<InBinding>,
    /// Flattened variables of `in_bindings`, in declaration order.
    pub in_vars: Vec<String>,
    pub clauses: Vec<Clause>,
    pub order_by: Vec<OrderSpec>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub return_keys: Option<Vec<String>>,
    /// `:with` variables: kept in the set of rows aggregated, but not returned.
    pub with_vars: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RuleDef {
    pub name: String,
    pub params: Vec<String>,
    pub body: Vec<Clause>,
}

fn edn_to_term(edn: &Edn, in_map: Option<&HashMap<String, Value>>) -> Result<ExprTerm, ParseError> {
    match edn {
        Edn::Symbol(s) if s == "_" => Ok(ExprTerm::Wildcard),
        Edn::Symbol(s) if s.starts_with('?') => {
            if let Some(m) = in_map {
                if let Some(v) = m.get(s) {
                    return Ok(ExprTerm::Val(v.clone()));
                }
            }
            Ok(ExprTerm::Var(s.clone()))
        }
        Edn::Str(s) => Ok(ExprTerm::Val(Value::Str(s.clone()))),
        Edn::Keyword(k) => Ok(ExprTerm::Val(Value::Keyword(k.clone()))),
        Edn::Int(i) => Ok(ExprTerm::Val(Value::I64(*i))),
        Edn::Float(f) => Ok(ExprTerm::Val(Value::F64(*f))),
        Edn::Bool(b) => Ok(ExprTerm::Val(Value::Bool(*b))),
        // Literal collection, e.g. `["Smith" "Doe"]` as a function argument.
        Edn::Vec(items) => {
            let vals = items
                .iter()
                .map(|i| match edn_to_term(i, in_map)? {
                    ExprTerm::Val(v) => Ok(v),
                    _ => Err(ParseError::InvalidQuery("collection literals may contain only constants".into())),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ExprTerm::Val(Value::Tuple(vals)))
        }
        other => Err(ParseError::InvalidQuery(format!("unexpected term: {:?}", other))),
    }
}

/// Datalevin's LIKE escape character when none is given.
pub const DEFAULT_ESCAPE: char = '!';

/// Pattern check shared with the matcher: an escape must precede `%`, `_`, or itself.
pub fn like_pattern_valid(pattern: &str, escape: char) -> bool {
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == escape {
            match chars.next() {
                None => {}
                Some(n) if n == '%' || n == '_' || n == escape => {}
                Some(_) => return false,
            }
        }
    }
    true
}

fn parse_escape(opts: &Edn) -> Result<char, ParseError> {
    if let Edn::Map(pairs) = opts {
        if let [(Edn::Keyword(k), Edn::Str(v))] = pairs.as_slice() {
            if k == ":escape" {
                let mut it = v.chars();
                if let (Some(c), None) = (it.next(), it.next()) {
                    return Ok(c);
                }
            }
        }
    }
    Err(ParseError::InvalidQuery("like options must be {:escape \"c\"}".into()))
}

/// Converts a full-text query literal: a string, `{:phrase "..."}`, `{:term "..."}`, or `[:and|:or|:not ...]`.
fn edn_to_fts(edn: &Edn) -> Result<FtsQuery, ParseError> {
    match edn {
        Edn::Str(s) => Ok(FtsQuery::Text(s.clone())),
        Edn::Map(pairs) if pairs.len() == 1 => match (&pairs[0].0, &pairs[0].1) {
            (Edn::Keyword(k), Edn::Str(s)) if k == ":phrase" => Ok(FtsQuery::Phrase(s.clone())),
            (Edn::Keyword(k), Edn::Str(s)) if k == ":term" => Ok(FtsQuery::Term(s.clone())),
            _ => Err(ParseError::InvalidQuery("full-text map must be {:phrase \"...\"} or {:term \"...\"}".into())),
        },
        Edn::Vec(items) if !items.is_empty() => {
            let op = match &items[0] {
                Edn::Keyword(k) => k.as_str(),
                _ => return Err(ParseError::InvalidQuery("full-text vector must start with :and, :or, :not, :prefix, or :fuzzy".into())),
            };
            let children = match op {
                ":prefix" | ":fuzzy" => Vec::new(),
                _ => items[1..].iter().map(edn_to_fts).collect::<Result<Vec<_>, _>>()?,
            };
            match op {
                ":prefix" => match &items[1..] {
                    [Edn::Str(s)] => Ok(FtsQuery::Prefix(s.clone())),
                    _ => Err(ParseError::InvalidQuery("[:prefix] takes one string".into())),
                },
                ":fuzzy" => match &items[1..] {
                    [Edn::Str(s)] => Ok(FtsQuery::Fuzzy(s.clone(), 1)),
                    [Edn::Str(s), Edn::Int(n)] if (0..=2).contains(n) => Ok(FtsQuery::Fuzzy(s.clone(), *n as u8)),
                    _ => Err(ParseError::InvalidQuery("[:fuzzy] takes a string and an optional distance 0..=2".into())),
                },
                ":and" => Ok(FtsQuery::And(children)),
                ":or" => Ok(FtsQuery::Or(children)),
                ":not" if children.len() == 1 => Ok(FtsQuery::Not(Box::new(children.into_iter().next().unwrap()))),
                _ => Err(ParseError::InvalidQuery(format!("unknown full-text operator {op}"))),
            }
        }
        other => Err(ParseError::InvalidQuery(format!("invalid full-text query: {other:?}"))),
    }
}

/// `[(q <query> $ args...) <out>]`. Output is a relation `[[?a ?b ...]]`, or a single-column collection `[?a ...]`.
fn parse_nested(inner: &[Edn], items: &[Edn]) -> Result<Clause, ParseError> {
    let query = inner.get(1).ok_or_else(|| ParseError::InvalidQuery("q needs a query".into()))?;
    let args: Vec<ExprTerm> = inner[2..]
        .iter()
        .filter(|e| !matches!(e, Edn::Symbol(s) if s == "$"))
        .map(|e| edn_to_term(e, None))
        .collect::<Result<_, _>>()?;
    let var_or_none = |e: &Edn| match e {
        Edn::Symbol(s) if s == "_" => Ok(None),
        Edn::Symbol(s) if s.starts_with('?') => Ok(Some(s.clone())),
        _ => Err(ParseError::InvalidQuery("nested query output must be ?vars or _".into())),
    };
    let out: Vec<Option<String>> = match items.get(1) {
        Some(Edn::Vec(rows)) if rows.len() == 1 => match &rows[0] {
            Edn::Vec(cols) => cols.iter().map(var_or_none).collect::<Result<_, _>>()?,
            _ => return Err(ParseError::InvalidQuery("nested query output must be [[?a ?b]]".into())),
        },
        Some(Edn::Vec(cols)) if cols.len() == 2 && matches!(cols[1], Edn::Symbol(ref s) if s == "...") => {
            vec![var_or_none(&cols[0])?]
        }
        _ => return Err(ParseError::InvalidQuery("nested query output must be [[?a ?b]] or [?a ...]".into())),
    };
    let parsed = parse_query_edn(query.clone(), None)?;
    if parsed.find.len() != out.len() {
        return Err(ParseError::InvalidQuery(format!(
            "nested query returns {} columns but {} outputs are bound",
            parsed.find.len(),
            out.len()
        )));
    }
    Ok(Clause::Nested(Box::new(parsed), args, out))
}

/// `[(idoc-match $ [:attr] {pattern}) [[?e ?a ?v]]]`.
fn parse_idoc_match(inner: &[Edn], items: &[Edn], in_map: Option<&HashMap<String, Value>>) -> Result<Clause, ParseError> {
    let args: Vec<&Edn> = inner[1..].iter().filter(|e| !matches!(e, Edn::Symbol(s) if s == "$")).collect();
    let (attr, pattern) = match args.as_slice() {
        [pattern] => (None, *pattern),
        [Edn::Keyword(a), pattern] => (Some(a.clone()), *pattern),
        _ => return Err(ParseError::InvalidQuery("idoc-match takes an optional attribute and a pattern".into())),
    };
    fn walk(p: &Edn, path: &mut Vec<PathSeg>, out: &mut Vec<(Vec<PathSeg>, ExprTerm)>, in_map: Option<&HashMap<String, Value>>) -> Result<(), ParseError> {
        match p {
            Edn::Map(pairs) => {
                for (k, v) in pairs {
                    let seg = match k {
                        Edn::Keyword(k) => PathSeg::Key(k.trim_start_matches(':').to_owned()),
                        Edn::Str(s) => PathSeg::Key(s.clone()),
                        Edn::Int(i) => PathSeg::Index(*i as u32),
                        _ => return Err(ParseError::InvalidQuery("idoc pattern keys must be keywords or strings".into())),
                    };
                    path.push(seg);
                    walk(v, path, out, in_map)?;
                    path.pop();
                }
                Ok(())
            }
            scalar => {
                out.push((path.clone(), edn_to_term(scalar, in_map)?));
                Ok(())
            }
        }
    }
    let mut preds = Vec::new();
    walk(pattern, &mut Vec::new(), &mut preds, in_map)?;

    let out_vars = match items.get(1) {
        Some(Edn::Vec(rows)) if rows.len() == 1 => match &rows[0] {
            Edn::Vec(cols) if cols.len() == 3 => {
                let mut vars = [None, None, None];
                for (i, c) in cols.iter().enumerate() {
                    vars[i] = match c {
                        Edn::Symbol(s) if s == "_" => None,
                        Edn::Symbol(s) if s.starts_with('?') => Some(s.clone()),
                        _ => return Err(ParseError::InvalidQuery("idoc-match output must be ?vars or _".into())),
                    };
                }
                vars
            }
            _ => return Err(ParseError::InvalidQuery("idoc-match output must be [[?e ?a ?v]]".into())),
        },
        _ => return Err(ParseError::InvalidQuery("idoc-match output must be [[?e ?a ?v]]".into())),
    };
    Ok(Clause::IdocMatch(attr, preds, out_vars))
}

/// Function-call clause: `(f args...)` with an optional output binding item.
fn parse_fn_clause(inner: &[Edn], items: &[Edn], in_map: Option<&HashMap<String, Value>>) -> Result<Clause, ParseError> {
    let name = match &inner[0] {
        Edn::Symbol(s) => s.clone(),
        _ => return Err(ParseError::InvalidQuery("function name must be a symbol".into())),
    };
    // Names outside KNOWN_FNS may be user functions registered on the connection; they are resolved at evaluation.
    // Datalevin built-ins take the database as their first argument; the sole database is `$`.
    let args_edn = match inner.get(1) {
        Some(Edn::Symbol(s)) if s == "$" => &inner[2..],
        _ => &inner[1..],
    };
    let args = args_edn
        .iter()
        .map(|a| edn_to_term(a, in_map))
        .collect::<Result<Vec<_>, _>>()?;
    let out = match items.get(1) {
        None => FnOut::None,
        Some(Edn::Symbol(s)) if s == "_" => FnOut::None,
        Some(Edn::Symbol(s)) if s.starts_with('?') => FnOut::Var(s.clone()),
        Some(Edn::Vec(vs)) => {
            let var_or_none = |v: &Edn| match v {
                Edn::Symbol(s) if s.starts_with('?') => Ok(Some(s.clone())),
                Edn::Symbol(s) if s == "_" => Ok(None),
                _ => Err(ParseError::InvalidQuery("tuple binding must list ?vars or _".into())),
            };
            match vs.as_slice() {
                [Edn::Symbol(s), d] if s.starts_with('?') && matches!(d, Edn::Symbol(e) if e == "...") => {
                    FnOut::Coll(s.clone())
                }
                [Edn::Vec(row)] if !row.is_empty() => FnOut::Rel(row.iter().map(var_or_none).collect::<Result<_, _>>()?),
                _ => FnOut::Tuple(vs.iter().map(var_or_none).collect::<Result<_, _>>()?),
            }
        }
        Some(other) => return Err(ParseError::InvalidQuery(format!("invalid function output: {:?}", other))),
    };
    if items.len() > 2 {
        return Err(ParseError::InvalidQuery("function clause has too many elements".into()));
    }
    Ok(Clause::Fn(name, args, out))
}

/// Parses the `[?v ...]` join-variable list of `or-join` / `not-join`.
fn join_vars(edn: &Edn) -> Result<Vec<String>, ParseError> {
    match edn {
        Edn::Vec(items) => items
            .iter()
            .map(|i| match i {
                Edn::Symbol(s) if s.starts_with('?') => Ok(s.clone()),
                _ => Err(ParseError::InvalidQuery("join variables must be ?vars".into())),
            })
            .collect(),
        _ => Err(ParseError::InvalidQuery("join variables must be a vector".into())),
    }
}

/// Parse one `:in` item: `?x`, `[?x ...]`, `[?a ?b]`, or `[[?a ?b] ...]`.
fn parse_in_binding(edn: &Edn) -> Result<Option<InBinding>, ParseError> {
    fn vars_of(items: &[Edn]) -> Result<Vec<String>, ParseError> {
        items
            .iter()
            .map(|i| match i {
                Edn::Symbol(s) if s.starts_with('?') => Ok(s.clone()),
                _ => Err(ParseError::InvalidQuery("bindings must be ?vars".into())),
            })
            .collect()
    }
    let dots = Edn::Symbol("...".into());
    Ok(Some(match edn {
        // Database and rules inputs are not data bindings.
        Edn::Symbol(s) if s == "$" || s == "%" => return Ok(None),
        Edn::Symbol(s) if s.starts_with('?') => InBinding::Scalar(s.clone()),
        Edn::Vec(items) => match items.as_slice() {
            [v, d] if *d == dots => {
                let mut vs = vars_of(std::slice::from_ref(v))?;
                InBinding::Coll(vs.remove(0))
            }
            [Edn::Vec(row), d] if *d == dots => InBinding::Rel(vars_of(row)?),
            [Edn::Vec(row)] => InBinding::Rel(vars_of(row)?),
            other => InBinding::Tuple(vars_of(other)?),
        },
        other => return Err(ParseError::InvalidQuery(format!("invalid :in binding: {:?}", other))),
    }))
}

fn parse_clause(edn: &Edn, in_map: Option<&HashMap<String, Value>>) -> Result<Clause, ParseError> {
    match edn {
        Edn::Vec(items) if !items.is_empty() => {
            // Check if predicate/function form: [(<= ?a ?b)] or [(fulltext $ ?q) ...]
            if let Edn::List(inner) = &items[0] {
                if inner.is_empty() {
                    return Err(ParseError::InvalidQuery("empty expression list".into()));
                }
                if let Edn::Symbol(op) = &inner[0] {
                    if op == "q" {
                        return parse_nested(inner, items);
                    }
                    if op == "idoc-match" {
                        return parse_idoc_match(inner, items, in_map);
                    }
                    if op == "fulltext" {
                        // [(fulltext $ ?q) [[?e _ ?name]]] or [(fulltext $ ?q) ?e]
                        if inner.len() < 3 || items.len() < 2 {
                            return Err(ParseError::InvalidQuery("invalid fulltext clause".into()));
                        }
                        let q_arg = match &inner[2] {
                            Edn::Symbol(s) if s.starts_with('?') => match in_map.and_then(|m| m.get(s)) {
                                Some(v) => FtsArg::Static(FtsQuery::Text(
                                    v.as_str().unwrap_or_default().to_owned(),
                                )),
                                None => FtsArg::Var(s.clone()),
                            },
                            other => FtsArg::Static(edn_to_fts(other)?),
                        };
                        let e_var = match &items[1] {
                            Edn::Symbol(s) if s.starts_with('?') => s.clone(),
                            Edn::Vec(outer) => {
                                // [[?e _ ?name]]
                                if let Some(Edn::Vec(inner_v)) = outer.first() {
                                    if let Some(Edn::Symbol(s)) = inner_v.first() {
                                        s.clone()
                                    } else {
                                        return Err(ParseError::InvalidQuery("expected ?e in fulltext".into()));
                                    }
                                } else {
                                    return Err(ParseError::InvalidQuery("expected [[?e ...]] in fulltext".into()));
                                }
                            }
                            _ => return Err(ParseError::InvalidQuery("invalid fulltext binding".into())),
                        };
                        return Ok(Clause::Fulltext(q_arg, e_var));
                    }

                    let cmp = match op.as_str() {
                        "<=" => CmpOp::Le,
                        "<" => CmpOp::Lt,
                        ">=" => CmpOp::Ge,
                        ">" => CmpOp::Gt,
                        "=" => CmpOp::Eq,
                        "!=" | "not=" => CmpOp::Ne,
                        "like" => CmpOp::Like(DEFAULT_ESCAPE),
                        "not-like" => CmpOp::NotLike(DEFAULT_ESCAPE),
                        _ => return parse_fn_clause(inner, items, in_map),
                    };
                    // `like` and `not-like` take an optional {:escape "c"} options map.
                    let (cmp, expected_args) = match (cmp, inner.get(3)) {
                        (CmpOp::Like(_), Some(opts)) => (CmpOp::Like(parse_escape(opts)?), 4),
                        (CmpOp::NotLike(_), Some(opts)) => (CmpOp::NotLike(parse_escape(opts)?), 4),
                        _ => (cmp, 3),
                    };
                    if inner.len() != expected_args {
                        return Err(ParseError::InvalidQuery("predicate requires 2 args".into()));
                    }
                    let lhs = edn_to_term(&inner[1], in_map)?;
                    let rhs = edn_to_term(&inner[2], in_map)?;
                    if let (CmpOp::Like(esc) | CmpOp::NotLike(esc), ExprTerm::Val(Value::Str(pat))) = (cmp, &rhs) {
                        if !like_pattern_valid(pat, esc) {
                            return Err(ParseError::InvalidQuery(format!("invalid like pattern: {pat}")));
                        }
                    }
                    return Ok(Clause::Pred(cmp, lhs, rhs));
                }
            }

            // Datom pattern: [e :attr v] or [e :attr]
            if items.len() < 2 || items.len() > 3 {
                return Err(ParseError::InvalidQuery("datom pattern must have 2 or 3 elements".into()));
            }
            let e_term = edn_to_term(&items[0], in_map)?;
            let attr = match &items[1] {
                Edn::Keyword(k) => k.clone(),
                _ => return Err(ParseError::InvalidQuery("attribute must be a keyword".into())),
            };
            let v_term = if items.len() == 3 {
                edn_to_term(&items[2], in_map)?
            } else {
                ExprTerm::Wildcard
            };
            Ok(Clause::Datom(e_term, attr, v_term))
        }
        Edn::List(items) if !items.is_empty() => {
            let head = match &items[0] {
                Edn::Symbol(s) => s.as_str(),
                _ => return Err(ParseError::InvalidQuery("list clause must start with symbol".into())),
            };
            match head {
                "or" => {
                    let mut branches = Vec::new();
                    for b in &items[1..] {
                        match b {
                            Edn::List(sub) if matches!(sub.first(), Some(Edn::Symbol(s)) if s == "and") => {
                                let mut cs = Vec::new();
                                for c in &sub[1..] {
                                    cs.push(parse_clause(c, in_map)?);
                                }
                                branches.push(cs);
                            }
                            other => branches.push(vec![parse_clause(other, in_map)?]),
                        }
                    }
                    Ok(Clause::Or(branches))
                }
                "or-join" => {
                    // (or-join [?vars...] branch1 branch2 ...)
                    if items.len() < 3 {
                        return Err(ParseError::InvalidQuery("or-join requires join vars and branches".into()));
                    }
                    let mut branches = Vec::new();
                    for b in &items[2..] {
                        match b {
                            Edn::List(sub) if matches!(sub.first(), Some(Edn::Symbol(s)) if s == "and") => {
                                let mut cs = Vec::new();
                                for c in &sub[1..] {
                                    cs.push(parse_clause(c, in_map)?);
                                }
                                branches.push(cs);
                            }
                            other => branches.push(vec![parse_clause(other, in_map)?]),
                        }
                    }
                    Ok(Clause::OrJoin(join_vars(&items[1])?, branches))
                }
                "not" => {
                    let mut cs = Vec::new();
                    for c in &items[1..] {
                        cs.push(parse_clause(c, in_map)?);
                    }
                    Ok(Clause::Not(cs))
                }
                "not-join" => {
                    // (not-join [?vars...] clause1 ...)
                    if items.len() < 3 {
                        return Err(ParseError::InvalidQuery("not-join requires join vars and clauses".into()));
                    }
                    let mut cs = Vec::new();
                    for c in &items[2..] {
                        cs.push(parse_clause(c, in_map)?);
                    }
                    Ok(Clause::NotJoin(join_vars(&items[1])?, cs))
                }
                rule_name => {
                    let mut args = Vec::new();
                    for a in &items[1..] {
                        args.push(edn_to_term(a, in_map)?);
                    }
                    Ok(Clause::RuleCall(rule_name.to_owned(), args))
                }
            }
        }
        other => Err(ParseError::InvalidQuery(format!("invalid clause: {:?}", other))),
    }
}

fn parse_find_elem(edn: &Edn) -> Result<FindElem, ParseError> {
    match edn {
        Edn::Symbol(s) if s.starts_with('?') => Ok(FindElem::Var(s.clone())),
        Edn::List(items) if items.len() == 3 && matches!(&items[0], Edn::Symbol(s) if s == "pull") => {
            let var = match &items[1] {
                Edn::Symbol(s) if s.starts_with('?') => s.clone(),
                _ => return Err(ParseError::InvalidQuery("pull target must be a ?var".into())),
            };
            let attrs = match &items[2] {
                Edn::Vec(v) => v
                    .iter()
                    .map(|e| match e {
                        Edn::Keyword(k) => Ok(k.clone()),
                        Edn::Symbol(s) if s == "*" => Ok("*".into()),
                        _ => Err(ParseError::InvalidQuery("pull pattern expects keywords or *".into())),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                _ => return Err(ParseError::InvalidQuery("pull pattern must be a vector".into())),
            };
            Ok(FindElem::Pull(var, attrs))
        }
        Edn::List(items) if items.len() == 2 => {
            let op = match &items[0] {
                Edn::Symbol(s) => match s.as_str() {
                    "count" => AggOp::Count,
                    "sum" => AggOp::Sum,
                    "min" => AggOp::Min,
                    "max" => AggOp::Max,
                    "avg" => AggOp::Avg,
                    _ => return Err(ParseError::InvalidQuery(format!("unknown aggregate: {}", s))),
                },
                _ => return Err(ParseError::InvalidQuery("invalid aggregate".into())),
            };
            let var = match &items[1] {
                Edn::Symbol(s) if s.starts_with('?') => s.clone(),
                _ => return Err(ParseError::InvalidQuery("aggregate argument must be a variable".into())),
            };
            Ok(FindElem::Agg(op, var))
        }
        other => Err(ParseError::InvalidQuery(format!("invalid :find element: {:?}", other))),
    }
}

fn parse_order_by(edn: &Edn) -> Result<Vec<OrderSpec>, ParseError> {
    let items = match edn {
        Edn::Vec(v) => v,
        _ => return Err(ParseError::InvalidQuery(":order-by must be a vector".into())),
    };
    let mut specs = Vec::new();
    for item in items {
        match item {
            Edn::Vec(pair) if !pair.is_empty() => {
                let key = match &pair[0] {
                    Edn::Symbol(s) if s.starts_with('?') => OrderKey::Var(s.clone()),
                    Edn::Int(idx) if *idx >= 0 => OrderKey::Col(*idx as usize),
                    _ => return Err(ParseError::InvalidQuery("invalid :order-by key".into())),
                };
                let desc = match pair.get(1) {
                    Some(Edn::Keyword(k)) if k == ":desc" => true,
                    Some(Edn::Keyword(k)) if k == ":asc" => false,
                    None => false,
                    _ => return Err(ParseError::InvalidQuery("invalid :order-by direction".into())),
                };
                specs.push(OrderSpec { key, desc });
            }
            Edn::Symbol(s) if s.starts_with('?') => {
                specs.push(OrderSpec {
                    key: OrderKey::Var(s.clone()),
                    desc: false,
                });
            }
            _ => return Err(ParseError::InvalidQuery("invalid :order-by entry".into())),
        }
    }
    Ok(specs)
}

fn parse_find_items(items: &[Edn], find: &mut Vec<FindElem>, find_spec: &mut FindSpec) -> Result<(), ParseError> {
    if items.len() == 2 && items[1] == Edn::Symbol("...".into()) {
        *find_spec = FindSpec::Coll;
        find.push(parse_find_elem(&items[0])?);
        return Ok(());
    }
    for item in items {
        if let Edn::Symbol(s) = item {
            if s == "." {
                *find_spec = FindSpec::Scalar;
                continue;
            }
        }
        if let Edn::Vec(v) = item {
            if v.last() == Some(&Edn::Symbol("...".into())) && v.len() == 2 {
                *find_spec = FindSpec::Coll;
                find.push(parse_find_elem(&v[0])?);
            } else {
                *find_spec = FindSpec::Tuple;
                for elem in v {
                    find.push(parse_find_elem(elem)?);
                }
            }
        } else {
            find.push(parse_find_elem(item)?);
        }
    }
    Ok(())
}

fn parse_query_impl(input: &str, inputs: Option<&[QueryInput]>) -> Result<ParsedQuery, ParseError> {
    let edn = parse_edn(input)?;
    parse_query_edn(edn, inputs)
}

fn parse_query_edn(edn: Edn, inputs: Option<&[QueryInput]>) -> Result<ParsedQuery, ParseError> {
    let mut find = Vec::new();
    let mut find_spec = FindSpec::Rel;
    let mut in_bindings: Vec<InBinding> = Vec::new();
    let mut where_edns = Vec::new();
    let mut order_by = Vec::new();
    let mut limit = None;
    let mut offset = None;
    let mut return_keys = None;
    let mut with_vars: Vec<String> = Vec::new();

    match edn {
        Edn::Vec(items) => {
            let mut section = "";
            let mut find_edns = Vec::new();
            let mut i = 0;
            while i < items.len() {
                if let Edn::Keyword(kw) = &items[i] {
                    if matches!(
                        kw.as_str(),
                        ":find" | ":in" | ":where" | ":order-by" | ":limit" | ":offset" | ":keys" | ":strs" | ":syms" | ":with"
                    ) {
                        section = match kw.as_str() {
                            ":find" => "find",
                            ":with" => "with",
                            ":in" => "in",
                            ":where" => "where",
                            ":order-by" => "order-by",
                            ":limit" => "limit",
                            ":offset" => "offset",
                            ":keys" | ":strs" | ":syms" => "keys",
                            _ => unreachable!(),
                        };
                        i += 1;
                        continue;
                    }
                }
                match section {
                    "find" => find_edns.push(items[i].clone()),
                    "in" => {
                        if let Some(b) = parse_in_binding(&items[i])? {
                            in_bindings.push(b);
                        }
                    }
                    "where" => where_edns.push(items[i].clone()),
                    "with" => {
                        if let Edn::Symbol(s) = &items[i] {
                            with_vars.push(s.clone());
                        }
                    }
                    "order-by" => order_by = parse_order_by(&items[i])?,
                    "limit" => {
                        if let Edn::Int(n) = items[i] {
                            limit = Some(n);
                        }
                    }
                    "offset" => {
                        if let Edn::Int(n) = items[i] {
                            offset = Some(n);
                        }
                    }
                    "keys" => {
                        if let Edn::Symbol(s) = &items[i] {
                            return_keys.get_or_insert_with(Vec::new).push(s.clone());
                        }
                    }
                    _ => return Err(ParseError::InvalidQuery("unexpected token before :find".into())),
                }
                i += 1;
            }
            parse_find_items(&find_edns, &mut find, &mut find_spec)?;
        }
        Edn::Map(pairs) => {
            for (k, v) in pairs {
                let kw = match k {
                    Edn::Keyword(s) => s,
                    _ => return Err(ParseError::InvalidQuery("map query keys must be keywords".into())),
                };
                match kw.as_str() {
                    ":find" => {
                        if let Edn::Vec(items) = v {
                            parse_find_items(&items, &mut find, &mut find_spec)?;
                        }
                    }
                    ":in" => {
                        if let Edn::Vec(items) = v {
                            for item in items {
                                if let Some(b) = parse_in_binding(&item)? {
                                    in_bindings.push(b);
                                }
                            }
                        }
                    }
                    ":where" => {
                        if let Edn::Vec(items) = v {
                            where_edns = items;
                        }
                    }
                    ":order-by" => order_by = parse_order_by(&v)?,
                    ":with" => {
                        if let Edn::Vec(items) = v {
                            for item in items {
                                if let Edn::Symbol(s) = item {
                                    with_vars.push(s);
                                }
                            }
                        }
                    }
                    ":limit" => {
                        if let Edn::Int(n) = v {
                            limit = Some(n);
                        } else if let Edn::Vec(items) = v {
                            if let Some(Edn::Int(n)) = items.first() {
                                limit = Some(*n);
                            }
                        }
                    }
                    ":offset" => {
                        if let Edn::Int(n) = v {
                            offset = Some(n);
                        } else if let Edn::Vec(items) = v {
                            if let Some(Edn::Int(n)) = items.first() {
                                offset = Some(*n);
                            }
                        }
                    }
                    ":keys" | ":strs" | ":syms" => {
                        if let Edn::Vec(items) = v {
                            let mut ks = Vec::new();
                            for item in items {
                                if let Edn::Symbol(s) = item {
                                    ks.push(s);
                                }
                            }
                            return_keys = Some(ks);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => return Err(ParseError::InvalidQuery("query must be a vector or map".into())),
    }

    if find.is_empty() {
        return Err(ParseError::InvalidQuery("missing :find clause".into()));
    }

    let in_vars: Vec<String> = in_bindings.iter().flat_map(|b| b.vars()).collect();
    // Scalars are substituted into the clauses at parse time; other shapes are seeded at execution.
    let in_map = if let Some(vals) = inputs {
        if in_bindings.len() != vals.len() {
            return Err(ParseError::InvalidQuery(format!(
                "expected {} :in arguments, got {}",
                in_bindings.len(),
                vals.len()
            )));
        }
        let mut m = HashMap::new();
        for (b, v) in in_bindings.iter().zip(vals.iter()) {
            if let (InBinding::Scalar(name), QueryInput::Scalar(val)) = (b, v) {
                m.insert(name.clone(), val.clone());
            }
        }
        Some(m)
    } else {
        None
    };

    let mut clauses = Vec::with_capacity(where_edns.len());
    for ce in &where_edns {
        clauses.push(parse_clause(ce, in_map.as_ref())?);
    }

    Ok(ParsedQuery {
        find,
        find_spec,
        in_bindings,
        in_vars,
        clauses,
        order_by,
        limit,
        offset,
        return_keys,
        with_vars,
    })
}

pub fn parse_query_template(input: &str) -> Result<ParsedQuery, ParseError> {
    parse_query_impl(input, None)
}

pub fn parse_query(input: &str, inputs: &[Value]) -> Result<ParsedQuery, ParseError> {
    let typed: Vec<QueryInput> = inputs.iter().cloned().map(QueryInput::Scalar).collect();
    parse_query_impl(input, Some(&typed))
}

pub fn parse_query_inputs(input: &str, inputs: &[QueryInput]) -> Result<ParsedQuery, ParseError> {
    parse_query_impl(input, Some(inputs))
}

pub fn parse_rules(input: &str) -> Result<Vec<RuleDef>, ParseError> {
    let edn = parse_edn(input)?;
    let rule_vecs = match edn {
        Edn::Vec(v) => v,
        _ => return Err(ParseError::InvalidQuery("rules must be a vector".into())),
    };
    let mut rules = Vec::with_capacity(rule_vecs.len());
    for rv in rule_vecs {
        let items = match rv {
            Edn::Vec(v) if !v.is_empty() => v,
            _ => return Err(ParseError::InvalidQuery("each rule must be a non-empty vector".into())),
        };
        let head = match &items[0] {
            Edn::List(l) if !l.is_empty() => l,
            _ => return Err(ParseError::InvalidQuery("rule head must be a list".into())),
        };
        let name = match &head[0] {
            Edn::Symbol(s) => s.clone(),
            _ => return Err(ParseError::InvalidQuery("rule name must be a symbol".into())),
        };
        let mut params = Vec::new();
        for p in &head[1..] {
            match p {
                Edn::Symbol(s) if s.starts_with('?') => params.push(s.clone()),
                _ => return Err(ParseError::InvalidQuery("rule parameters must be ?vars".into())),
            }
        }
        let mut body = Vec::new();
        for c in &items[1..] {
            body.push(parse_clause(c, None)?);
        }
        rules.push(RuleDef { name, params, body });
    }
    Ok(rules)
}

/// Parse ISO-8601 UTC timestamp like `2026-11-24T06:00:00Z` into epoch milliseconds.
pub fn parse_iso8601_utc_ms(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date_part, time_part) = s.split_once('T')?;
    let mut d = date_part.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time_part.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let sec: i64 = t.next()?.parse().ok()?;

    // Howard Hinnant's civil-from-days inverse (days since 1970-01-01)
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + hour * 3600 + min * 60 + sec;
    Some(secs * 1000)
}
