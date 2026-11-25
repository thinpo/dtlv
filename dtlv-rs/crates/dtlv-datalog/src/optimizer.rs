//! Query planning: cost-based ordering of datom clauses, plus `explain` and `analyze`.
//!
//! Datom clauses form a conjunction, so their order does not change the result. The planner reorders each
//! contiguous run of them greedily, cheapest access first, given the variables bound so far. Every other
//! clause keeps its position, because predicates, functions, and negation depend on what precedes them.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use dtlv_codec::{encode_av_prefix, Value};
use dtlv_kv::ReadTxn;

use crate::parser::{parse_query_inputs, Clause, ExprTerm, FtsArg};
use crate::{seed_envs, Conn, DatalogError, QueryInput};

/// Entries counted per index range when estimating; larger ranges are reported at this bound.
const COUNT_CAP: u64 = 10_000;
/// Cost assumed for a scan that no index can narrow.
const SCAN_COST: u64 = 1_000_000;

/// How a datom clause reaches its matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Entity and value are both known: one EAV lookup.
    EavPoint,
    /// Entity known, value free: the entity's EAV prefix.
    EavPrefix,
    /// Indexed attribute and value: an AVE range.
    AveRange,
    /// Nothing narrows the search: scan EAV and filter by attribute.
    EavScan,
    /// A clause that is not a datom pattern (predicate, function, negation, rule, full-text, idoc).
    Other,
}

/// One clause of a query plan, in evaluation order.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanStep {
    pub clause: String,
    pub access: Access,
    /// Estimated matches for this clause, given the variables bound before it.
    pub estimated_rows: u64,
}

/// Measured execution of the plan's prefix up to and including one clause.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzeStep {
    pub clause: String,
    /// Bindings after this clause.
    pub rows: u64,
    /// Time to evaluate the plan from the start through this clause.
    pub elapsed: Duration,
}

fn term_label(t: &ExprTerm) -> String {
    match t {
        ExprTerm::Var(v) => v.clone(),
        ExprTerm::Wildcard => "_".into(),
        ExprTerm::Val(v) => format!("{v:?}"),
    }
}

fn label(c: &Clause) -> String {
    let s = match c {
        Clause::Datom(e, a, v) => format!("[{} {} {}]", term_label(e), a, term_label(v)),
        other => format!("{other:?}"),
    };
    if s.len() > 96 {
        format!("{}...", &s[..96])
    } else {
        s
    }
}

fn term_vars(t: &ExprTerm, out: &mut HashSet<String>) {
    if let ExprTerm::Var(v) = t {
        out.insert(v.clone());
    }
}

/// Every variable a clause mentions. Used to track what is bound after it.
fn clause_vars(c: &Clause, out: &mut HashSet<String>) {
    match c {
        Clause::Datom(e, _, v) => {
            term_vars(e, out);
            term_vars(v, out);
        }
        Clause::Pred(_, a, b) => {
            term_vars(a, out);
            term_vars(b, out);
        }
        Clause::Fulltext(arg, e) => {
            out.insert(e.clone());
            if let FtsArg::Var(v) = arg {
                out.insert(v.clone());
            }
        }
        Clause::IdocMatch(_, preds, vars) => {
            for (_, t) in preds {
                term_vars(t, out);
            }
            for v in vars.iter().flatten() {
                out.insert(v.clone());
            }
        }
        Clause::Fn(_, args, _) => {
            for a in args {
                term_vars(a, out);
            }
        }
        Clause::Or(branches) | Clause::OrJoin(_, branches) => {
            for c in branches.iter().flatten() {
                clause_vars(c, out);
            }
        }
        Clause::Not(inner) | Clause::NotJoin(_, inner) => {
            for c in inner {
                clause_vars(c, out);
            }
        }
        Clause::RuleCall(_, args) => {
            for a in args {
                term_vars(a, out);
            }
        }
        Clause::Nested(_, args, outs) => {
            for a in args {
                term_vars(a, out);
            }
            for v in outs.iter().flatten() {
                out.insert(v.clone());
            }
        }
    }
}

impl Conn {
    /// Number of AVE entries for `attr = val`, counted up to `COUNT_CAP`.
    fn count_capped(&self, rtxn: &ReadTxn<'_>, aid: u32, val: &Value) -> Result<u64, DatalogError> {
        let mut pref = Vec::with_capacity(32);
        encode_av_prefix(aid, val, &mut pref);
        let mut n = 0;
        for item in self.ave.prefix_iter(rtxn, &pref)? {
            item?;
            n += 1;
            if n >= COUNT_CAP {
                break;
            }
        }
        Ok(n)
    }

    /// Estimated cost and access path of a datom clause under the current bindings.
    fn datom_cost(&self, rtxn: &ReadTxn<'_>, e: &ExprTerm, a: &str, v: &ExprTerm, bound: &HashSet<String>) -> Result<(u64, Access), DatalogError> {
        let known = |t: &ExprTerm| match t {
            ExprTerm::Val(_) => true,
            ExprTerm::Var(x) => bound.contains(x),
            ExprTerm::Wildcard => false,
        };
        let (e_known, v_known) = (known(e), known(v));
        let aid = match self.attr_id(a) {
            Ok(aid) => aid,
            // Unknown attributes fail at evaluation; plan them first so the error surfaces early.
            Err(_) => return Ok((1, Access::EavPoint)),
        };
        let indexed = self.attrs.get(&aid).is_some_and(|s| s.index);
        Ok(match (e_known, v_known) {
            (true, true) => (1, Access::EavPoint),
            (true, false) => (2, Access::EavPrefix),
            (false, true) if indexed => match v {
                ExprTerm::Val(val) => (self.count_capped(rtxn, aid, val)?.max(1), Access::AveRange),
                _ => (10, Access::AveRange),
            },
            _ => (SCAN_COST, Access::EavScan),
        })
    }

    /// Reorders the datom clauses inside each contiguous run, cheapest first, and leaves every other clause in place.
    pub(crate) fn plan_clauses(&self, rtxn: &ReadTxn<'_>, bound: &HashSet<String>, clauses: Vec<Clause>) -> Result<Vec<Clause>, DatalogError> {
        let mut bound = bound.clone();
        let mut out = Vec::with_capacity(clauses.len());
        let mut run: Vec<Clause> = Vec::new();
        for c in clauses {
            if matches!(c, Clause::Datom(..)) {
                run.push(c);
                continue;
            }
            self.order_run(rtxn, std::mem::take(&mut run), &mut bound, &mut out)?;
            clause_vars(&c, &mut bound);
            out.push(c);
        }
        self.order_run(rtxn, run, &mut bound, &mut out)?;
        Ok(out)
    }

    fn order_run(&self, rtxn: &ReadTxn<'_>, mut run: Vec<Clause>, bound: &mut HashSet<String>, out: &mut Vec<Clause>) -> Result<(), DatalogError> {
        while !run.is_empty() {
            let mut best: Option<(usize, u64)> = None;
            for (i, c) in run.iter().enumerate() {
                let Clause::Datom(e, a, v) = c else { unreachable!("runs hold datom clauses only") };
                let (cost, _) = self.datom_cost(rtxn, e, a, v, bound)?;
                if best.is_none_or(|(_, b)| cost < b) {
                    best = Some((i, cost));
                }
            }
            let (i, _) = best.expect("run is non-empty");
            let c = run.remove(i);
            clause_vars(&c, bound);
            out.push(c);
        }
        Ok(())
    }

    /// The plan for a query: clauses in evaluation order, with their access paths and estimates.
    pub fn explain(&self, rtxn: &ReadTxn<'_>, query: &str, inputs: &[QueryInput]) -> Result<Vec<PlanStep>, DatalogError> {
        let parsed = parse_query_inputs(query, inputs)?;
        let bound: HashSet<String> = parsed.in_vars.iter().cloned().collect();
        let clauses = self.plan_clauses(rtxn, &bound, parsed.clauses)?;
        let mut bound = bound;
        let mut steps = Vec::with_capacity(clauses.len());
        for c in &clauses {
            let (estimated_rows, access) = match c {
                Clause::Datom(e, a, v) => self.datom_cost(rtxn, e, a, v, &bound)?,
                _ => (0, Access::Other),
            };
            steps.push(PlanStep { clause: label(c), access, estimated_rows });
            clause_vars(c, &mut bound);
        }
        Ok(steps)
    }

    /// Runs the planned query one prefix at a time, reporting rows and cumulative time after each clause.
    pub fn analyze(&self, rtxn: &ReadTxn<'_>, query: &str, inputs: &[QueryInput]) -> Result<Vec<AnalyzeStep>, DatalogError> {
        let parsed = parse_query_inputs(query, inputs)?;
        let seed = seed_envs(&parsed.in_bindings, inputs)?;
        let bound: HashSet<String> = parsed.in_vars.iter().cloned().collect();
        let clauses = self.plan_clauses(rtxn, &bound, parsed.clauses)?;
        let mut steps = Vec::with_capacity(clauses.len());
        for end in 1..=clauses.len() {
            let start = Instant::now();
            let envs = self.eval_clauses(rtxn, &clauses[..end], seed.clone(), &HashMap::new())?;
            steps.push(AnalyzeStep { clause: label(&clauses[end - 1]), rows: envs.len() as u64, elapsed: start.elapsed() });
        }
        Ok(steps)
    }
}

