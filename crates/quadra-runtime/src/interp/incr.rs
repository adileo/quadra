//! Which records a rule must be re-evaluated on after a transaction's writes.
//!
//! In memory (no source) every record is re-evaluated: that is the reference behaviour. With
//! storage on demand the dependency paths of the rule are walked backwards from the records
//! and ledger rows written in the current transaction.

use super::deps::{analyze, Deps, Step};
use super::*;
use std::collections::BTreeSet;

/// A record written (or created) in the current transaction.
pub(super) struct Written {
    pub id: Id,
    pub ent: String,
    pub fields: HashSet<String>,
    pub created: bool,
}

enum Node {
    Rec(Id),
    Row(HashMap<String, V>),
}

fn base_name(k: &str) -> &str {
    k.split(['@', '#']).next().unwrap_or(k)
}

fn ref_ids(v: &V) -> Vec<Id> {
    match v {
        V::Ref { id, .. } => vec![*id],
        V::Set(items) | V::List(items) => items.iter().filter_map(|i| i.as_ref_id()).collect(),
        _ => Vec::new(),
    }
}

impl<'m> Interp<'m> {
    /// Dependencies of a rule, computed once.
    pub(super) fn deps(&mut self, key: &str, root: &str, exprs: &[&Expr], stmts: &[&[Stmt]]) -> Rc<Deps> {
        if let Some(d) = self.deps_cache.get(key) {
            return d.clone();
        }
        let d = Rc::new(analyze(self.m, root, exprs, stmts));
        self.deps_cache.insert(key.to_string(), d.clone());
        d
    }

    pub(super) fn is_a(&self, ent: &str, target: &str) -> bool {
        ent == target || self.m.entity(ent).map_or(false, |e| self.m.implements(e, target))
    }

    /// Concrete entities behind a name (a trait's implementors, or itself).
    fn concrete(&self, ent: &str) -> Vec<String> {
        match self.m.entity(ent) {
            Some(e) if e.kind == EntityKind::Trait => self.m.implementors(ent).iter().map(|e| e.name.clone()).collect(),
            _ => vec![ent.to_string()],
        }
    }

    pub(super) fn written(&mut self) -> Vec<Written> {
        let tx = self.store.tx;
        let mut out = Vec::new();
        for id in self.store.dirty.clone() {
            let Some(r) = self.store.records.get(&id) else {
                continue;
            };
            let fields = r.history.iter().filter(|(t, _, _)| *t == tx).map(|(_, f, _)| base_name(f).to_string()).collect();
            out.push(Written { id, ent: r.ent.to_string(), fields, created: r.created_tx == tx });
        }
        out
    }

    /// Records of `ent` a rule must be evaluated on now. Every record in memory.
    pub(super) fn candidates(&mut self, ent: &str, deps: &Deps) -> Vec<Id> {
        self.candidates_with(ent, deps, true).unwrap_or_else(|| self.ids(ent, None))
    }

    /// Like `candidates`, for a deploy going through the records batch by batch: a record
    /// written here is a candidate only for the rules that read what was written, and a rule
    /// that reads a written entity as a whole (or cannot be followed) gives None instead of
    /// every record (the deploy evaluates it once on every record, after the batches).
    pub fn candidates_strict(&mut self, ent: &str, deps: &Deps) -> Option<Vec<Id>> {
        self.candidates_with(ent, deps, false)
    }

    fn candidates_with(&mut self, ent: &str, deps: &Deps, own: bool) -> Option<Vec<Id>> {
        if !self.store.lazy() || (self.sweep && deps.time) {
            return None;
        }
        let written = self.written();
        let tx = self.store.tx;
        let rows: Vec<(String, HashMap<String, V>)> = self.store.rows.iter().filter(|r| r.tx == tx).map(|r| (r.ledger.to_string(), r.cols.clone())).collect();
        if written.is_empty() && rows.is_empty() {
            return Some(Vec::new());
        }
        // (a recursion over a tree is followed by the incremental formulas only: `rollup.rs`)
        let global_hit = deps.opaque || !deps.recursive.is_empty() || written.iter().any(|w| deps.globals.iter().any(|g| self.is_a(&w.ent, g))) || rows.iter().any(|(l, _)| deps.globals.contains(l));
        if global_hit {
            return None;
        }
        let mut out: BTreeSet<Id> = BTreeSet::new();
        // the rule's own records that changed
        if own {
            for w in &written {
                if self.is_a(&w.ent, ent) {
                    out.insert(w.id);
                }
            }
        }
        for (path, rent, field) in &deps.reads {
            let mut nodes: Vec<Node> = Vec::new();
            for w in &written {
                if self.is_a(&w.ent, rent) && (w.created || w.fields.contains(field)) {
                    nodes.push(Node::Rec(w.id));
                }
            }
            for (l, cols) in &rows {
                if l == rent {
                    nodes.push(Node::Row(cols.clone()));
                }
            }
            if nodes.is_empty() {
                continue;
            }
            for step in path.iter().rev() {
                nodes = self.step_back(step, nodes);
                if nodes.is_empty() {
                    break;
                }
            }
            for n in nodes {
                if let Node::Rec(id) = n {
                    out.insert(id);
                }
            }
        }
        Some(
            out.into_iter()
                .filter(|id| {
                    self.store.exists(*id, None) && {
                        let e = self.ent_of(*id);
                        self.is_a(&e, ent)
                    }
                })
                .collect(),
        )
    }

    /// The records from which `path` leads to record `id` (walked backwards).
    pub(super) fn back_from(&mut self, path: &[Step], id: Id) -> Vec<Id> {
        let mut nodes = vec![Node::Rec(id)];
        for step in path.iter().rev() {
            nodes = self.step_back(step, nodes);
            if nodes.is_empty() {
                break;
            }
        }
        nodes.into_iter().filter_map(|n| if let Node::Rec(id) = n { Some(id) } else { None }).collect()
    }

    /// From records at the end of `step` to the records at its start.
    fn step_back(&mut self, step: &Step, nodes: Vec<Node>) -> Vec<Node> {
        let mut out: BTreeSet<Id> = BTreeSet::new();
        match step {
            Step::Ref { ent, field } => {
                for n in nodes {
                    let Node::Rec(b) = n else { continue };
                    for a in self.concrete(ent) {
                        let target = V::Ref { ent: Rc::from(""), id: b };
                        match self.store.find(&a, field, &target) {
                            Some(ids) => out.extend(ids),
                            None => {
                                for id in self.ids(&a, None) {
                                    if self.store.read(id, field, None).map_or(false, |v| ref_ids(&v).contains(&b)) {
                                        out.insert(id);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Step::Inv { back, .. } => {
                let tx = self.store.tx;
                for n in nodes {
                    match n {
                        Node::Rec(c) => {
                            if let Some(v) = self.store.read(c, back, None) {
                                out.extend(ref_ids(&v));
                            }
                            // the value before this transaction (a record moved to another parent)
                            if let Some(r) = self.store.records.get(&c) {
                                if let Some((_, _, v)) = r.history.iter().rev().find(|(t, f, _)| *t < tx && f == back) {
                                    out.extend(ref_ids(v));
                                }
                            }
                        }
                        Node::Row(cols) => {
                            if let Some(v) = cols.get(back) {
                                out.extend(ref_ids(v));
                            }
                        }
                    }
                }
            }
        }
        out.into_iter().map(Node::Rec).collect()
    }

    /// Rule-specific dependency sets.
    pub(super) fn transition_deps(&mut self, e: &'m Entity, fi: usize, ti: usize) -> Rc<Deps> {
        let t = &e.flows[fi].transitions[ti];
        let exprs: Vec<&Expr> = t.when.iter().chain(t.unless.iter()).collect();
        self.deps(&format!("tr:{}:{fi}:{ti}", e.name), &e.name, &exprs, &[])
    }

    pub(super) fn post_deps(&mut self, e: &'m Entity, pi: usize) -> Rc<Deps> {
        let p = &e.posts[pi];
        let exprs: Vec<&Expr> = p.when.iter().collect();
        self.deps(&format!("post:{}:{pi}", e.name), &e.name, &exprs, &[&p.body])
    }

    pub fn record_checks_deps(&mut self, e: &'m Entity) -> Rc<Deps> {
        let mut exprs: Vec<&Expr> = Vec::new();
        for c in self.m.all_checks(e) {
            exprs.push(&c.cond);
            exprs.push(&c.message);
        }
        for f in self.m.all_fields(e) {
            if let Some(r) = &f.required_when {
                exprs.push(r);
            }
            match &f.where_ {
                Some(Where::Compare(_, x)) | Some(Where::Expr(x)) => exprs.push(x),
                None => {}
            }
        }
        let key = format!("checks:{}", e.name);
        if let Some(d) = self.deps_cache.get(&key) {
            return d.clone();
        }
        let mut d = (*self.deps(&key, &e.name, &exprs, &[])).clone();
        // the rules of the `record` values it holds: the holder is re-checked when it is
        // written; anything else they read (other records) counts as a global read
        let mut seen: Vec<String> = Vec::new();
        let mut stack: Vec<&Entity> = vec![e];
        while let Some(owner) = stack.pop() {
            for f in self.m.all_fields(owner) {
                let Some(re) = record_type(self.m, f.ty.as_ref()) else {
                    continue;
                };
                if seen.contains(&re.name) {
                    continue;
                }
                seen.push(re.name.clone());
                stack.push(re);
                let mut rexprs: Vec<&Expr> = Vec::new();
                for c in self.m.all_checks(re) {
                    rexprs.push(&c.cond);
                    rexprs.push(&c.message);
                }
                for rf in self.m.all_fields(re) {
                    if let Some(r) = &rf.required_when {
                        rexprs.push(r);
                    }
                    match &rf.where_ {
                        Some(Where::Compare(_, x)) | Some(Where::Expr(x)) => rexprs.push(x),
                        None => {}
                    }
                }
                let rd = analyze(self.m, &re.name, &rexprs, &[]);
                for (path, ent, _) in rd.reads {
                    if !(path.is_empty() && ent == re.name) {
                        d.globals.insert(ent);
                    }
                }
                d.globals.extend(rd.globals);
                d.opaque |= rd.opaque;
                d.time |= rd.time;
            }
        }
        let d = Rc::new(d);
        self.deps_cache.insert(key, d.clone());
        d
    }

    pub(super) fn frozen_deps(&mut self, e: &'m Entity) -> Rc<Deps> {
        let fr = e.frozen.as_ref().map(|f| &f.when);
        let exprs: Vec<&Expr> = fr.into_iter().collect();
        self.deps(&format!("frozen:{}", e.name), &e.name, &exprs, &[])
    }

    pub fn formula_deps(&mut self, ent: &str, field: &str) -> Rc<Deps> {
        let key = format!("formula:{ent}:{field}");
        if let Some(d) = self.deps_cache.get(&key) {
            return d.clone();
        }
        let d = Rc::new(super::deps::analyze_formula(self.m, ent, field));
        self.deps_cache.insert(key, d.clone());
        d
    }
}

/// The `record` type of a field (`Address`, `Address?`, `[Address]`).
fn record_type<'m>(m: &'m Model, t: Option<&Ty>) -> Option<&'m Entity> {
    let name = match t? {
        Ty::List(i) | Ty::Set(i) => return record_type(m, Some(i)),
        Ty::Optional(i) => return record_type(m, Some(i)),
        Ty::Path { segments, .. } if segments.len() == 1 => &segments[0],
        _ => return None,
    };
    m.entity(name).filter(|e| e.kind == EntityKind::Record)
}
