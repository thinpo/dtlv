use std::collections::HashMap;
use dtlv_codec::Value;
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
    Like,
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

#[derive(Debug, Clone)]
pub enum Clause {
    Datom(ExprTerm, String, ExprTerm),
    Pred(CmpOp, ExprTerm, ExprTerm),
    Fulltext(ExprTerm, String), // (query_term, entity_var)
    Or(Vec<Vec<Clause>>),
    Not(Vec<Clause>),
    RuleCall(String, Vec<ExprTerm>),
}

#[derive(Debug, Clone)]
pub struct ParsedQuery {
    pub find: Vec<FindElem>,
    pub find_spec: FindSpec,
    pub in_vars: Vec<String>,
    pub clauses: Vec<Clause>,
    pub order_by: Vec<OrderSpec>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub return_keys: Option<Vec<String>>,
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
        other => Err(ParseError::InvalidQuery(format!("unexpected term: {:?}", other))),
    }
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
                    if op == "fulltext" {
                        // [(fulltext $ ?q) [[?e _ ?name]]] or [(fulltext $ ?q) ?e]
                        if inner.len() < 3 || items.len() < 2 {
                            return Err(ParseError::InvalidQuery("invalid fulltext clause".into()));
                        }
                        let q_term = edn_to_term(&inner[2], in_map)?;
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
                        return Ok(Clause::Fulltext(q_term, e_var));
                    }

                    let cmp = match op.as_str() {
                        "<=" => CmpOp::Le,
                        "<" => CmpOp::Lt,
                        ">=" => CmpOp::Ge,
                        ">" => CmpOp::Gt,
                        "=" => CmpOp::Eq,
                        "!=" | "not=" => CmpOp::Ne,
                        "like" => CmpOp::Like,
                        _ => return Err(ParseError::InvalidQuery(format!("unknown predicate: {}", op))),
                    };
                    if inner.len() != 3 {
                        return Err(ParseError::InvalidQuery("predicate requires 2 args".into()));
                    }
                    let lhs = edn_to_term(&inner[1], in_map)?;
                    let rhs = edn_to_term(&inner[2], in_map)?;
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
                    Ok(Clause::Or(branches))
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
                    Ok(Clause::Not(cs))
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

fn parse_query_impl(input: &str, inputs: Option<&[Value]>) -> Result<ParsedQuery, ParseError> {
    let edn = parse_edn(input)?;
    let mut find = Vec::new();
    let mut find_spec = FindSpec::Rel;
    let mut in_vars = Vec::new();
    let mut where_edns = Vec::new();
    let mut order_by = Vec::new();
    let mut limit = None;
    let mut offset = None;
    let mut return_keys = None;

    match edn {
        Edn::Vec(items) => {
            let mut section = "";
            let mut find_edns = Vec::new();
            let mut i = 0;
            while i < items.len() {
                if let Edn::Keyword(kw) = &items[i] {
                    if matches!(
                        kw.as_str(),
                        ":find" | ":in" | ":where" | ":order-by" | ":limit" | ":offset" | ":keys" | ":strs" | ":syms"
                    ) {
                        section = match kw.as_str() {
                            ":find" => "find",
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
                        if let Edn::Symbol(s) = &items[i] {
                            if s != "$" && s != "%" && s.starts_with('?') {
                                in_vars.push(s.clone());
                            }
                        }
                    }
                    "where" => where_edns.push(items[i].clone()),
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
                                if let Edn::Symbol(s) = item {
                                    if s != "$" && s != "%" && s.starts_with('?') {
                                        in_vars.push(s);
                                    }
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

    let in_map = if let Some(vals) = inputs {
        if in_vars.len() != vals.len() {
            return Err(ParseError::InvalidQuery(format!(
                "expected {} :in arguments, got {}",
                in_vars.len(),
                vals.len()
            )));
        }
        Some(in_vars.iter().cloned().zip(vals.iter().cloned()).collect::<HashMap<_, _>>())
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
        in_vars,
        clauses,
        order_by,
        limit,
        offset,
        return_keys,
    })
}

pub fn parse_query_template(input: &str) -> Result<ParsedQuery, ParseError> {
    parse_query_impl(input, None)
}

pub fn parse_query(input: &str, inputs: &[Value]) -> Result<ParsedQuery, ParseError> {
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
