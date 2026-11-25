//! Magic-set rewriting for demand-driven evaluation of rules.
//!
//! A call `p(c, ?x)` with some constant arguments only needs the tuples of `p` that agree with those constants,
//! and, transitively, only the tuples of the rules it depends on that feed them. The rewrite produces:
//!
//! * adorned copies `p#bf` of the rules, one per binding pattern reached from the call (`b` bound, `f` free);
//! * magic predicates `m#p#bf` holding the bindings each adorned rule is asked for, seeded by the call's
//!   constants and extended through rule bodies left to right;
//!
//! so bottom-up evaluation of the rewritten program derives only facts relevant to the call.
//!
//! Only conjunctive bodies are rewritten: datom, predicate, function, full-text, idoc, and positive rule
//! calls. A rule with `or`, `not`, or a nested query makes the rewrite decline, and the caller evaluates the
//! original program.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::optimizer::clause_vars;
use crate::parser::{CmpOp, Clause, ExprTerm, RuleDef};

/// Rules to evaluate for one rewritten call, and the adorned name to call instead.
pub struct Rewritten {
    pub rules: HashMap<String, Vec<RuleDef>>,
    pub goal: String,
}

fn bits(a: &[bool]) -> String {
    a.iter().map(|&b| if b { 'b' } else { 'f' }).collect()
}

fn adorned(name: &str, a: &[bool]) -> String {
    format!("{name}#{}", bits(a))
}

fn magic(name: &str, a: &[bool]) -> String {
    format!("m#{name}#{}", bits(a))
}

fn conjunctive(body: &[Clause]) -> bool {
    body.iter().all(|c| {
        matches!(
            c,
            Clause::Datom(..) | Clause::Pred(..) | Clause::Fn(..) | Clause::Fulltext(..) | Clause::IdocMatch(..) | Clause::RuleCall(..)
        )
    })
}

fn term_bound(t: &ExprTerm, bound: &HashSet<String>) -> bool {
    match t {
        ExprTerm::Val(_) => true,
        ExprTerm::Var(v) => bound.contains(v),
        ExprTerm::Wildcard => false,
    }
}

/// Rewrites a call to `goal` whose arguments are `args`. `None` when no argument is constant, when the goal is
/// not a rule, or when a reachable rule has a body the rewrite does not handle.
pub fn rewrite(rule_map: &HashMap<String, Vec<RuleDef>>, goal: &str, args: &[ExprTerm]) -> Option<Rewritten> {
    let goal_adorn: Vec<bool> = args.iter().map(|a| matches!(a, ExprTerm::Val(_))).collect();
    if !goal_adorn.iter().any(|&b| b) || !rule_map.contains_key(goal) {
        return None;
    }
    let mut out: HashMap<String, Vec<RuleDef>> = HashMap::new();
    let mut seen: HashSet<(String, Vec<bool>)> = HashSet::new();
    let mut queue: VecDeque<(String, Vec<bool>)> = VecDeque::new();
    seen.insert((goal.to_owned(), goal_adorn.clone()));
    queue.push_back((goal.to_owned(), goal_adorn.clone()));

    // Seed: the call's constants, as the single tuple of the goal's magic predicate.
    let bound_positions: Vec<usize> = (0..args.len()).filter(|&i| goal_adorn[i]).collect();
    let seed_params: Vec<String> = bound_positions.iter().map(|i| format!("?__seed{i}")).collect();
    let seed_body: Vec<Clause> = bound_positions
        .iter()
        .zip(&seed_params)
        .map(|(&i, p)| Clause::Pred(CmpOp::Eq, ExprTerm::Var(p.clone()), args[i].clone()))
        .collect();
    out.entry(magic(goal, &goal_adorn)).or_default().push(RuleDef {
        name: magic(goal, &goal_adorn),
        params: seed_params,
        body: seed_body,
    });

    while let Some((pred, a)) = queue.pop_front() {
        // Every adorned rule's body starts with its magic guard. With no bound arguments the guard is the
        // zero-argument predicate, which holds exactly one (empty) tuple.
        if !a.iter().any(|&b| b) {
            out.entry(magic(&pred, &a)).or_default().push(RuleDef {
                name: magic(&pred, &a),
                params: Vec::new(),
                body: Vec::new(),
            });
        }
        for def in rule_map.get(&pred)? {
            if !conjunctive(&def.body) {
                return None;
            }
            let mut bound: HashSet<String> = def
                .params
                .iter()
                .enumerate()
                .filter(|(i, _)| a[*i])
                .map(|(_, p)| p.clone())
                .collect();
            let head_bound: Vec<String> = def.params.iter().enumerate().filter(|(i, _)| a[*i]).map(|(_, p)| p.clone()).collect();
            let guard = Clause::RuleCall(magic(&pred, &a), head_bound.iter().map(|v| ExprTerm::Var(v.clone())).collect());
            let mut new_body = vec![guard];
            for c in &def.body {
                match c {
                    Clause::RuleCall(q, qargs) if rule_map.contains_key(q) => {
                        let qa: Vec<bool> = qargs.iter().map(|t| term_bound(t, &bound)).collect();
                        if qa.iter().any(|&b| b) {
                            // The magic rule for q: the bindings this point of the body can offer it.
                            let bound_args: Vec<&ExprTerm> = qargs.iter().zip(qa.iter()).filter(|(_, b)| **b).map(|(t, _)| t).collect();
                            let mh: Vec<String> = (0..bound_args.len()).map(|k| format!("?__mh{k}")).collect();
                            let mut mbody = new_body.clone();
                            for (p, t) in mh.iter().zip(&bound_args) {
                                mbody.push(Clause::Pred(CmpOp::Eq, ExprTerm::Var(p.clone()), (*t).clone()));
                            }
                            out.entry(magic(q, &qa)).or_default().push(RuleDef {
                                name: magic(q, &qa),
                                params: mh,
                                body: mbody,
                            });
                        }
                        if seen.insert((q.clone(), qa.clone())) {
                            queue.push_back((q.clone(), qa.clone()));
                        }
                        new_body.push(Clause::RuleCall(adorned(q, &qa), qargs.clone()));
                    }
                    other => new_body.push(other.clone()),
                }
                clause_vars(c, &mut bound);
            }
            out.entry(adorned(&pred, &a)).or_default().push(RuleDef {
                name: adorned(&pred, &a),
                params: def.params.clone(),
                body: new_body,
            });
        }
    }
    Some(Rewritten { rules: out, goal: adorned(goal, &goal_adorn) })
}

/// Names of rules a clause list calls, directly or inside `or`/`not`/nested queries.
pub fn called_rules(clauses: &[Clause], out: &mut HashSet<String>) {
    for c in clauses {
        match c {
            Clause::RuleCall(n, _) => {
                out.insert(n.clone());
            }
            Clause::Or(bs) | Clause::OrJoin(_, bs) => {
                for b in bs {
                    called_rules(b, out);
                }
            }
            Clause::Not(inner) | Clause::NotJoin(_, inner) => called_rules(inner, out),
            Clause::Nested(q, _, _) => called_rules(&q.clauses, out),
            _ => {}
        }
    }
}

/// The rules reachable from `names` through rule bodies.
pub fn reachable(rule_map: &HashMap<String, Vec<RuleDef>>, names: HashSet<String>) -> HashSet<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = names.into_iter().filter(|n| rule_map.contains_key(n)).collect();
    while let Some(n) = stack.pop() {
        if !seen.insert(n.clone()) {
            continue;
        }
        let mut callees = HashSet::new();
        for def in &rule_map[&n] {
            called_rules(&def.body, &mut callees);
        }
        stack.extend(callees.into_iter().filter(|c| rule_map.contains_key(c) && !seen.contains(c)));
    }
    seen
}
