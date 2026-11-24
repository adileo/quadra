//! Incremental formulas (the notes on scaling, phase 3): materialized formulas whose stored value is
//! read back instead of computing them, kept up to date by every commit.
//!
//! A formula is *incremental* when `quadra_analysis::deps::incremental` says every value it
//! reads is reached by a path the dependency analysis follows (no clock, no whole entity, no
//! ledger, nothing that depends on who reads it). Its column then holds its value at every
//! commit, and a record loaded from the database carries it (`Record::formulas`). While a
//! transaction runs, the value goes stale when something it depends on is written: the writes
//! of the transaction (`Store::wlog`) are followed back along the formula's dependency paths
//! (as `candidates` does at commit), incrementally. A stale value is computed again:
//!
//! - by delta when the formula is a rollup over the record's children (`lines.sum(l => …)`,
//!   `orders.where(o => …).count()`, `.min`/`.max`): the stored value plus what the changed
//!   children contribute now minus what they contributed before the transaction (their old
//!   contributions are evaluated on the values before it: stored fields from the history,
//!   formulas from their stored values). A `min`/`max` whose extreme may have left falls back;
//! - otherwise the interpreter's way, which reads the stored values of what did not change.
//!
//! Computed values are kept while the working set stays as it is (`Store::epoch`), so a
//! rollup read by a check and again by the commit is computed once.

use super::deps::{analyze_lambda, Deps, Step};
use super::*;
use std::collections::HashSet;

/// Values of incremental formulas read from their columns, computed by delta, and computed the
/// interpreter's way, by this process (benchmarks and tests report them).
pub static STORED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static DELTAS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static COMPUTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn count(c: &std::sync::atomic::AtomicU64) {
    c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[derive(Default)]
pub(super) struct Inc<'m> {
    /// (entity, field) → incremental
    trusted: HashMap<(String, String), bool>,
    trusted_ptr: HashMap<(usize, usize), bool>,
    /// the rollups recognized among them
    rollups: HashMap<(String, String), Option<Rc<Rollup<'m>>>>,
    /// the sums of a ledger's rows posted to the record recognized among them
    ledger_rollups: HashMap<(String, String), Option<Rc<quadra_analysis::deps::LedgerRollup<'m>>>>,
    /// stale sets by key: the records whose value (or contribution) may have changed
    stale: HashMap<String, Stale>,
    /// records reached back along a path from a written record, in this generation of writes
    stepped: HashMap<(Vec<Step>, Id), Rc<Vec<Id>>>,
    stepped_at: (u64, u64),
    /// values of incremental formulas computed while the working set stays as it is
    fresh: HashMap<(Id, &'m str), V>,
    fresh_at: (u64, u64),
    /// evaluating as of before the transaction (a rollup's old contributions)
    pub old_view: bool,
}

struct Stale {
    /// (store uid, write log generation)
    at: (u64, u64),
    /// entries of the write log already followed
    cursor: usize,
    ids: HashSet<Id>,
}

/// A formula of the form `coll.where(p).….agg(f)` over an inverse collection of the record.
pub(super) struct Rollup<'m> {
    child: String,
    back: String,
    filters: Vec<(&'m str, &'m Expr)>,
    agg: Agg<'m>,
    /// the aggregate call (its typed zero)
    span: Span,
    /// what a child's contribution depends on, from the child
    deps: Rc<Deps>,
}

enum Agg<'m> {
    Sum(&'m str, &'m Expr),
    Count,
    Min(&'m str, &'m Expr),
    Max(&'m str, &'m Expr),
}

fn base_name(k: &str) -> &str {
    k.split(['@', '#']).next().unwrap_or(k)
}

fn holds(v: &Option<V>, id: Id) -> bool {
    match v {
        Some(V::Ref { id: x, .. }) => *x == id,
        Some(V::Set(items)) | Some(V::List(items)) => items.iter().any(|i| i.as_ref_id() == Some(id)),
        _ => false,
    }
}

/// Same currency or unit (sums by delta only add values of one).
fn same_unit(a: &V, b: &V) -> bool {
    match (a, b) {
        (V::Money { cur: x, .. }, V::Money { cur: y, .. }) => x == y,
        (V::Qty { unit: x, .. }, V::Qty { unit: y, .. }) => x == y,
        (V::Price { cur: x, per: p, .. }, V::Price { cur: y, per: q, .. }) => x == y && p == q,
        (V::Int(_), V::Int(_)) => true,
        (V::Dec(_) | V::Int(_), V::Dec(_) | V::Int(_)) => true,
        (V::Pct(_), V::Pct(_)) => true,
        (V::Duration(_), V::Duration(_)) => true,
        _ => false,
    }
}

/// The parameter and body of a lambda argument.
fn lambda(e: &Expr) -> Option<(&str, &Expr)> {
    match &e.kind {
        ExprKind::Lambda { params, body } if params.len() == 1 => Some((params[0].as_str(), body)),
        _ => None,
    }
}

/// Whether a lambda's body reads only its parameter (and constants): not the record the
/// formula belongs to, which would make every child's contribution change with it.
fn only_param(m: &Model, owner: &Entity, param: &str, e: &Expr) -> bool {
    let mut ok = true;
    fn walk(m: &Model, owner: &Entity, bound: &mut Vec<String>, e: &Expr, ok: &mut bool) {
        if !*ok {
            return;
        }
        match &e.kind {
            ExprKind::Name(n) => {
                if !bound.iter().any(|b| b == n) && (m.find_field(owner, n).is_some() || crate::query::IMPLICIT.contains(&n.as_str()) || n == "me" || n == "current") {
                    *ok = false;
                }
            }
            ExprKind::SelfRef | ExprKind::Block(_) | ExprKind::Match { .. } | ExprKind::As { .. } | ExprKind::Error => *ok = false,
            ExprKind::Lit(_) | ExprKind::Quantity { .. } => {}
            ExprKind::FString(parts) => {
                for p in parts {
                    if let FPart::Expr(x, _) = p {
                        walk(m, owner, bound, x, ok);
                    }
                }
            }
            ExprKind::Field { base, .. } => walk(m, owner, bound, base, ok),
            ExprKind::Call { callee, args } => {
                match &callee.kind {
                    ExprKind::Field { base, .. } => walk(m, owner, bound, base, ok),
                    _ => *ok = false,
                }
                for a in args {
                    walk(m, owner, bound, &a.value, ok);
                }
            }
            ExprKind::Index { base, index } => {
                walk(m, owner, bound, base, ok);
                walk(m, owner, bound, index, ok);
            }
            ExprKind::Unary { expr, .. } => walk(m, owner, bound, expr, ok),
            ExprKind::Binary { lhs, rhs, .. } => {
                walk(m, owner, bound, lhs, ok);
                walk(m, owner, bound, rhs, ok);
            }
            ExprKind::If { cond, then, otherwise } => {
                walk(m, owner, bound, cond, ok);
                walk(m, owner, bound, then, ok);
                walk(m, owner, bound, otherwise, ok);
            }
            ExprKind::Lambda { params, body } => {
                let n = bound.len();
                bound.extend(params.iter().cloned());
                walk(m, owner, bound, body, ok);
                bound.truncate(n);
            }
            ExprKind::Tuple(items) => items.iter().for_each(|(_, x)| walk(m, owner, bound, x, ok)),
            ExprKind::List(items) => items.iter().for_each(|a| walk(m, owner, bound, &a.value, ok)),
            ExprKind::Set(items) => items.iter().for_each(|x| walk(m, owner, bound, x, ok)),
            ExprKind::When(arms) => {
                for (c, v) in arms {
                    if let Some(c) = c {
                        walk(m, owner, bound, c, ok);
                    }
                    walk(m, owner, bound, v, ok);
                }
            }
            ExprKind::Fail(x) => walk(m, owner, bound, x, ok),
        }
    }
    walk(m, owner, &mut vec![param.to_string()], e, &mut ok);
    ok
}

impl<'m> Interp<'m> {
    /// `trusted` of a field of an entity of the model, without allocating (the hot path).
    pub(super) fn trusted_field(&mut self, e: &'m Entity, f: &'m Field) -> bool {
        let key = (e as *const Entity as usize, f as *const Field as usize);
        if let Some(t) = self.inc.trusted_ptr.get(&key) {
            return *t;
        }
        let t = self.trusted(&e.name, &f.name);
        self.inc.trusted_ptr.insert(key, t);
        t
    }

    /// Whether `ent.field` is an incremental formula: kept up to date by every commit when it
    /// is materialized (a record loaded from the database then holds its stored value).
    pub(super) fn trusted(&mut self, ent: &str, field: &str) -> bool {
        let key = (ent.to_string(), field.to_string());
        if let Some(t) = self.inc.trusted.get(&key) {
            return *t;
        }
        let t = quadra_analysis::deps::incremental(self.m, ent, field).is_ok();
        self.inc.trusted.insert(key, t);
        t
    }

    /// Whether reading formula `field` of record `id` (of `ent`) reads a stored value: an
    /// incremental formula whose value the record was loaded with (the API then does not load
    /// what the formula reads).
    pub fn has_stored(&mut self, ent: &str, id: Id, field: &str) -> bool {
        self.store.lazy() && self.trusted(ent, field) && self.store.records.get(&id).is_some_and(|r| r.formulas.get(field).is_some_and(|v| !v.is_none()))
    }

    /// A new transaction, or one rolled back: nothing computed for the previous writes holds.
    pub(super) fn reset_incremental(&mut self) {
        self.inc.stale.clear();
        self.inc.stepped.clear();
        self.inc.fresh.clear();
    }

    /// Records reached back along `path` from record `id` (cached for this generation of writes).
    fn back_along(&mut self, path: &[Step], id: Id) -> Rc<Vec<Id>> {
        let at = (self.store.uid, self.store.wlog_gen);
        if self.inc.stepped_at != at {
            self.inc.stepped.clear();
            self.inc.stepped_at = at;
        }
        let key = (path.to_vec(), id);
        if let Some(v) = self.inc.stepped.get(&key) {
            return v.clone();
        }
        let v = Rc::new(self.back_from(path, id));
        self.inc.stepped.insert(key, v.clone());
        v
    }

    /// The records of `ent` whose value of what `deps` describes may have changed with the
    /// writes of the transaction so far (`key` names the stale set).
    fn stale_set(&mut self, key: &str, ent: &str, deps: &Deps) -> HashSet<Id> {
        let at = (self.store.uid, self.store.wlog_gen);
        let mut s = match self.inc.stale.remove(key) {
            Some(s) if s.at == at => s,
            _ => Stale { at, cursor: 0, ids: HashSet::new() },
        };
        let mut added: Vec<Id> = Vec::new();
        while s.cursor < self.store.wlog.len() {
            let (wid, wfield) = self.store.wlog[s.cursor].clone();
            s.cursor += 1;
            let went = self.ent_of(wid);
            // the record's own fields (a record created or moved, or a read of its own field)
            if self.is_a(&went, ent) && s.ids.insert(wid) {
                added.push(wid);
            }
            let field = base_name(&wfield);
            for (path, rent, rfield) in &deps.reads {
                if path.is_empty() || !self.is_a(&went, rent) || !(field.is_empty() || field == rfield) {
                    continue;
                }
                let found = self.back_along(path, wid);
                for x in found.iter() {
                    if s.ids.insert(*x) {
                        added.push(*x);
                    }
                }
            }
        }
        // a formula defined by recursion over a tree: the records whose value reads a stale
        // value, transitively (the descendants of a category moved in the tree)
        while let Some(x) = added.pop() {
            for path in &deps.recursive {
                let found = self.back_along(path, x);
                for y in found.iter() {
                    if s.ids.insert(*y) {
                        added.push(*y);
                    }
                }
            }
        }
        let ids = s.ids.clone();
        self.inc.stale.insert(key.to_string(), s);
        ids
    }

    /// `quadra_analysis::deps::ledger_rollup` of a formula (cached).
    fn ledger_rollup(&mut self, ent: &str, field: &str) -> Option<Rc<quadra_analysis::deps::LedgerRollup<'m>>> {
        let key = (ent.to_string(), field.to_string());
        if let Some(r) = self.inc.ledger_rollups.get(&key) {
            return r.clone();
        }
        let r = quadra_analysis::deps::ledger_rollup(self.m, ent, field).map(Rc::new);
        self.inc.ledger_rollups.insert(key, r.clone());
        r
    }

    /// The records of `ent` written or created in the transaction (their own fields).
    fn own_written(&mut self, ent: &str) -> HashSet<Id> {
        if self.store.wlog.is_empty() {
            return HashSet::new();
        }
        self.stale_set(&format!("own:{ent}"), ent, &Deps::default())
    }

    /// The rows of the ledger of `lr` posted in the transaction to record `id`.
    fn posted_to(&self, lr: &quadra_analysis::deps::LedgerRollup<'m>, id: Id) -> Vec<Id> {
        let tx = self.store.tx;
        self.store.rows.iter().filter(|r| r.tx == tx && *r.ledger == *lr.ledger && holds(&r.cols.get(&lr.key).cloned(), id)).map(|r| r.id).collect()
    }

    /// Whether the stored value of `ent.field` on record `id` may be stale.
    fn is_stale(&mut self, ent: &str, field: &str, id: Id) -> bool {
        if !self.store.thawed.is_empty() && self.thawed_of(ent).contains(&id) {
            return true;
        }
        if let Some(lr) = self.ledger_rollup(ent, field) {
            // rows posted to the record (what the records written so far post, as a read of
            // the ledger posts it first), or a stored field of the record written
            self.post_written();
            return !self.posted_to(&lr, id).is_empty() || self.own_written(ent).contains(&id);
        }
        if self.store.wlog.is_empty() {
            return false;
        }
        let deps = self.formula_deps(ent, field);
        self.stale_set(&format!("f:{ent}.{field}"), ent, &deps).contains(&id)
    }

    /// The records of `ent` whose stored `field` may be stale (the records a commit computes
    /// it on), with the records unfrozen in the transaction.
    pub(super) fn stale_of(&mut self, ent: &str, field: &str) -> Vec<Id> {
        let mut ids: Vec<Id> = match self.ledger_rollup(ent, field) {
            Some(lr) => {
                // the records the rows of the transaction are posted to, those written
                let tx = self.store.tx;
                let mut ids: Vec<Id> = self.store.rows.iter().filter(|r| r.tx == tx && *r.ledger == *lr.ledger).filter_map(|r| r.cols.get(&lr.key).and_then(|v| v.as_ref_id())).collect();
                ids.extend(self.own_written(ent));
                ids
            }
            None => {
                let deps = self.formula_deps(ent, field);
                self.stale_set(&format!("f:{ent}.{field}"), ent, &deps).into_iter().collect()
            }
        };
        ids.extend(self.thawed_of(ent));
        ids.sort();
        ids.dedup();
        ids.into_iter()
            .filter(|id| {
                self.store.exists(*id, None) && {
                    let e = self.ent_of(*id);
                    self.is_a(&e, ent)
                }
            })
            .collect()
    }

    fn fresh_valid(&mut self) {
        let at = (self.store.uid, self.store.epoch);
        if self.inc.fresh_at != at {
            self.inc.fresh.clear();
            self.inc.fresh_at = at;
        }
    }

    /// Keep the value of an incremental formula computed now (until the working set changes).
    pub(super) fn remember(&mut self, id: Id, field: &'m str, v: &V) {
        if !self.store.lazy() || self.store.uid == 0 {
            return;
        }
        count(&COMPUTED);
        self.fresh_valid();
        self.inc.fresh.insert((id, field), v.clone());
    }

    /// The value of the incremental formula `f` of record `id` without computing it from what it
    /// reads: its stored value while nothing it depends on was written, else by delta when it is
    /// a rollup. None: compute it.
    pub(super) fn stored_formula(&mut self, ent: &str, f: &'m Field, id: Id, frozen: bool) -> R<Option<V>> {
        if !self.store.lazy() || self.store.uid == 0 {
            return Ok(None);
        }
        self.fresh_valid();
        if let Some(v) = self.inc.fresh.get(&(id, f.name.as_str())) {
            return Ok(Some(v.clone()));
        }
        // (an empty column: no value, or the formula failed: computed, which tells them apart)
        let Some(stored) = self.store.records.get(&id).and_then(|r| r.formulas.get(&f.name)).filter(|v| !v.is_none()).cloned() else {
            return Ok(None);
        };
        // a record frozen before this transaction reads the world as of then: its value holds
        if frozen || !self.is_stale(ent, &f.name, id) {
            if !self.store.untracked {
                self.store.reads.formulas.insert(id);
            }
            count(&STORED);
            return Ok(Some(stored));
        }
        if let Some(v) = self.delta(ent, f, id, &stored)? {
            count(&DELTAS);
            if !self.store.untracked {
                self.store.reads.formulas.insert(id);
            }
            self.remember(id, f.name.as_str(), &v);
            return Ok(Some(v));
        }
        Ok(None)
    }

    /// A formula's value as before the transaction (a rollup's old contribution): its stored
    /// value, or an error when there is none.
    pub(super) fn old_formula(&mut self, ent: &str, f: &Field, id: Id) -> R<V> {
        if !self.trusted(ent, &f.name) {
            return fail("no value before the transaction");
        }
        match self.store.records.get(&id).and_then(|r| r.formulas.get(&f.name)).filter(|v| !v.is_none()).cloned() {
            Some(v) => {
                if !self.store.untracked {
                    self.store.reads.formulas.insert(id);
                }
                Ok(v)
            }
            None => fail("no value before the transaction"),
        }
    }

    /// After a commit: the values computed are the database's.
    pub(super) fn after_commit(&mut self) {
        if !self.store.lazy() {
            return;
        }
        let vals: Vec<((Id, String), V)> = self.store.materialized.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for ((id, field), v) in vals {
            if let Some(r) = self.store.records.get_mut(&id) {
                r.formulas.insert(field, v);
            }
        }
    }

    // ---- checks ---------------------------------------------------------------------------

    /// Among the records of `e` a commit re-checks (`ids`), those that need only some of their
    /// `check`s: not written in the transaction, reached only through what those checks read
    /// (each check's own dependencies, followed back from the writes as the formulas' are),
    /// not through the rules of their fields, and none of them a `warn` (warnings are computed
    /// all together). Their selection; the others get every rule.
    pub(super) fn narrow_checks(&mut self, e: &'m Entity, ids: &[Id]) -> HashMap<Id, RuleSel> {
        let mut out = HashMap::new();
        if !self.store.lazy() || self.store.uid == 0 || ids.len() < 2 || !self.store.thawed.is_empty() {
            return out;
        }
        let m = self.m;
        let checks = m.all_checks(e);
        if checks.len() < 2 || checks.iter().any(|c| c.warn) {
            return out;
        }
        // the rules of the fields (`required when`, `where`, record values): one group
        let mut exprs: Vec<&Expr> = Vec::new();
        for f in m.all_fields(e) {
            if let Some(r) = &f.required_when {
                exprs.push(r);
            }
            match &f.where_ {
                Some(Where::Compare(_, x)) | Some(Where::Expr(x)) => exprs.push(x),
                None => {}
            }
            if f.formula.is_none() && ty_names_inner_pub(f.ty.as_ref()).and_then(|n| m.entity(&n)).is_some_and(|x| x.kind == EntityKind::Record) {
                return out;
            }
        }
        let fields = self.deps(&format!("fieldrules:{}", e.name), &e.name, &exprs, &[]);
        let unfollowed = |d: &Deps| d.opaque || d.time || !d.globals.is_empty() || !d.recursive.is_empty();
        if unfollowed(&fields) {
            return out;
        }
        let hit_fields = self.stale_set(&format!("fr:{}", e.name), &e.name, &fields);
        let mut per: Vec<HashSet<Id>> = Vec::new();
        for (i, c) in checks.iter().enumerate() {
            let d = self.deps(&format!("check:{}:{i}", e.name), &e.name, &[&c.cond, &c.message], &[]);
            if unfollowed(&d) {
                return out;
            }
            per.push(self.stale_set(&format!("c:{}:{i}", e.name), &e.name, &d));
        }
        let tx = self.store.tx;
        for id in ids {
            let own = self.store.dirty.contains(id) || self.store.records.get(id).is_some_and(|r| r.created_tx == tx || r.history.iter().any(|(t, _, _)| *t == tx));
            if own || hit_fields.contains(id) {
                continue;
            }
            let sel: std::collections::BTreeSet<usize> = per.iter().enumerate().filter(|(_, set)| set.contains(id)).map(|(i, _)| i).collect();
            if sel.is_empty() || sel.len() == checks.len() {
                continue;
            }
            out.insert(*id, RuleSel { checks: sel, ..Default::default() });
        }
        out
    }

    // ---- rollups ------------------------------------------------------------------------

    fn rollup(&mut self, ent: &str, field: &str) -> Option<Rc<Rollup<'m>>> {
        let key = (ent.to_string(), field.to_string());
        if let Some(r) = self.inc.rollups.get(&key) {
            return r.clone();
        }
        let r = self.recognize(ent, field).map(Rc::new);
        self.inc.rollups.insert(key, r.clone());
        r
    }

    fn recognize(&self, ent: &str, field: &str) -> Option<Rollup<'m>> {
        let m = self.m;
        let owner = m.entity(ent)?;
        let e = m.find_field(owner, field)?.formula.as_ref()?;
        let ExprKind::Call { callee, args } = &e.kind else {
            return None;
        };
        let ExprKind::Field { base, name, optional: false } = &callee.kind else {
            return None;
        };
        let lam = match args.as_slice() {
            [] => None,
            [a] if a.name.is_none() && !a.spread => Some(lambda(&a.value)?),
            _ => return None,
        };
        let mut filters: Vec<(&'m str, &'m Expr)> = Vec::new();
        let mut b: &'m Expr = base;
        let coll = loop {
            match &b.kind {
                ExprKind::Call { callee, args } => {
                    let ExprKind::Field { base, name, optional: false } = &callee.kind else {
                        return None;
                    };
                    let [a] = args.as_slice() else { return None };
                    if name != "where" || a.name.is_some() || a.spread {
                        return None;
                    }
                    filters.push(lambda(&a.value)?);
                    b = base;
                }
                ExprKind::Name(n) => break n.as_str(),
                ExprKind::Field { base, name, optional: false } if matches!(base.kind, ExprKind::SelfRef) => break name.as_str(),
                _ => return None,
            }
        };
        filters.reverse();
        let agg = match (name.as_str(), lam) {
            ("sum", Some((p, x))) => Agg::Sum(p, x),
            ("min", Some((p, x))) => Agg::Min(p, x),
            ("max", Some((p, x))) => Agg::Max(p, x),
            ("count", None) => Agg::Count,
            ("count", Some(l)) => {
                filters.push(l);
                Agg::Count
            }
            _ => return None,
        };
        // an inverse collection of the record (not `through`)
        let cf = m.find_field(owner, coll)?;
        if cf.formula.is_some() || cf.through.is_some() {
            return None;
        }
        let inner = match cf.ty.as_ref()? {
            Ty::List(i) | Ty::Set(i) => i.as_ref(),
            _ => return None,
        };
        let Ty::Path { segments, .. } = inner else {
            return None;
        };
        let child = m.entity(&segments[0]).filter(|c| c.kind == EntityKind::Entity && !c.is_ledger)?;
        let back = cf.via.clone().or_else(|| {
            child
                .fields
                .iter()
                .find(|x| {
                    let n = ty_names(x.ty.as_ref());
                    n.as_deref() == Some(owner.name.as_str()) || n.map_or(false, |n| owner.traits.contains(&n))
                })
                .map(|x| x.name.clone())
        })?;
        let bf = m.find_field(child, &back)?;
        if bf.formula.is_some() || bf.per_company {
            return None;
        }
        let mut lambdas: Vec<(&str, &Expr)> = filters.clone();
        match &agg {
            Agg::Sum(p, x) | Agg::Min(p, x) | Agg::Max(p, x) => lambdas.push((p, x)),
            Agg::Count => {}
        }
        let mut deps = Deps::default();
        for (p, x) in &lambdas {
            if !only_param(m, owner, p, x) {
                return None;
            }
            let d = analyze_lambda(m, &child.name, p, x);
            if d.opaque || d.time || !d.globals.is_empty() || d.volatile.is_some() {
                return None;
            }
            deps.reads.extend(d.reads);
        }
        deps.reads.insert((Vec::new(), child.name.clone(), back.clone()));
        Some(Rollup { child: child.name.clone(), back, filters, agg, span: e.span, deps: Rc::new(deps) })
    }

    /// The extreme of what the children of `id` other than `changed` contribute (their stored
    /// values hold), in SQL: the first row ordered by the value, as the grid orders a field of
    /// a number, an amount of one currency, a quantity of one unit or a date (none first when
    /// descending: a child without a value makes the interpreter fail, so then None). Some(None):
    /// no other child contributes. None: SQL cannot tell.
    fn extreme_of_others(&mut self, r: &Rollup<'m>, id: Id, changed: &[Id], max: bool) -> R<Option<Option<V>>> {
        let (Agg::Min(p, x) | Agg::Max(p, x)) = &r.agg else {
            return Ok(None);
        };
        // the value is a field of the child
        let ExprKind::Field { base, name, optional: false } = &x.kind else {
            return Ok(None);
        };
        if !matches!(&base.kind, ExprKind::Name(n) if n == p) {
            return Ok(None);
        }
        let m = self.m;
        let Some(f) = m.entity(&r.child).and_then(|c| m.find_field(c, name)) else {
            return Ok(None);
        };
        let fixed = |t: Option<&Ty>| match t {
            Some(Ty::Optional(i)) => {
                matches!(i.as_ref(), Ty::Path { segments, args, .. } if segments.len() == 1 && args.iter().all(|a| matches!(a, Ty::Path { segments, .. } if segments.len() == 1 && m.entity(&segments[0]).is_none() && segments[0] != "self")))
            }
            Some(Ty::Path { segments, args, .. }) => segments.len() == 1 && args.iter().all(|a| matches!(a, Ty::Path { segments, .. } if segments.len() == 1 && m.entity(&segments[0]).is_none() && segments[0] != "self")),
            _ => false,
        };
        let kind = match f.ty.as_ref() {
            Some(Ty::Optional(i)) => ty_names(Some(i)),
            t => ty_names(t),
        };
        let ok = match kind.as_deref() {
            Some("Int" | "Decimal" | "Date") => true,
            Some("Money" | "Qty") => fixed(f.ty.as_ref()),
            _ => false,
        };
        if !ok {
            return Ok(None);
        }
        let binders: Vec<crate::query::Binder> = r.filters.iter().map(|(p, _)| crate::query::Binder::Param(p.to_string())).collect();
        let mut spec = Spec::new(&r.child);
        spec.conds = r.filters.iter().zip(binders.iter()).map(|((_, e), b)| crate::query::Cond { binder: b, expr: e, negate: false }).collect();
        spec.refs = vec![(r.back.clone(), id)];
        let mut ctx = Ctx::default();
        // none first: the descending order; the extreme: the order of the aggregate
        let mut desc_spec = spec.clone();
        desc_spec.order = vec![crate::query::Key::Grid { path: vec![name.clone()], desc: true }];
        desc_spec.id_desc = true;
        let Some(rows) = self.sql_rows_excluding(&desc_spec, &mut ctx, changed, 1)? else {
            return Ok(None);
        };
        let Some((top, keys)) = rows.first().cloned() else {
            return Ok(Some(None));
        };
        if keys.first().cloned().flatten().is_none() {
            return Ok(None);
        }
        let pick = if max {
            top
        } else {
            let mut asc = spec.clone();
            asc.order = vec![crate::query::Key::Grid { path: vec![name.clone()], desc: false }];
            let Some(rows) = self.sql_rows_excluding(&asc, &mut ctx, changed, 1)? else {
                return Ok(None);
            };
            let Some((low, _)) = rows.first().cloned() else {
                return Ok(None);
            };
            low
        };
        let rec = V::Ref { ent: Rc::from(r.child.as_str()), id: pick };
        let v = self.get(&rec, name, None, None)?;
        Ok(if v.is_none() { None } else { Some(Some(v)) })
    }

    /// The value of a child's contribution (None: it does not contribute; none: no value),
    /// now or before the transaction. An error stops the delta.
    fn contribution(&mut self, r: &Rollup<'m>, c: Id, old: bool) -> R<Option<V>> {
        let rec = V::Ref { ent: Rc::from(r.child.as_str()), id: c };
        let saved = self.inc.old_view;
        self.inc.old_view = old;
        let out = (|| {
            for (p, x) in &r.filters {
                let mut ctx = Ctx::default();
                ctx.bind(p, rec.clone());
                if !self.eval(x, &mut ctx)?.truthy() {
                    return Ok(None);
                }
            }
            let (p, x) = match &r.agg {
                Agg::Count => return Ok(Some(V::Int(1))),
                Agg::Sum(p, x) | Agg::Min(p, x) | Agg::Max(p, x) => (*p, *x),
            };
            let mut ctx = Ctx::default();
            ctx.bind(p, rec.clone());
            // (none: a sum skips it; a min or max fails on it, which the caller sees)
            Ok(Some(self.eval(x, &mut ctx)?))
        })();
        self.inc.old_view = saved;
        out
    }

    /// The new value of the sum (or count) of a ledger's rows posted to record `id`: its
    /// stored value and the rows the transaction posted to it (rows are never changed or
    /// removed). None (compute it the interpreter's way): a stored field of the record was
    /// written (the conditions may read it), a sum of another currency or unit.
    fn ledger_delta(&mut self, lr: &quadra_analysis::deps::LedgerRollup<'m>, ent: &str, f: &'m Field, id: Id, old: &V) -> R<Option<V>> {
        if old.is_none() || !self.store.thawed.is_empty() || self.own_written(ent).contains(&id) {
            return Ok(None);
        }
        let this = V::Ref { ent: Rc::from(ent), id };
        let ledger: Rc<str> = Rc::from(lr.ledger.as_str());
        let mut add: Vec<V> = Vec::new();
        'rows: for row in self.posted_to(lr, id) {
            let rv = V::Ref { ent: ledger.clone(), id: row };
            for (p, x) in &lr.filters {
                let mut ctx = Ctx::for_record(&this, ent);
                ctx.bind(p, rv.clone());
                match self.eval(x, &mut ctx) {
                    Ok(v) if v.truthy() => {}
                    Ok(_) => continue 'rows,
                    Err(_) => return Ok(None),
                }
            }
            match &lr.sum {
                None => add.push(V::Int(1)),
                Some((p, x)) => {
                    let mut ctx = Ctx::for_record(&this, ent);
                    ctx.bind(p, rv.clone());
                    match self.eval(x, &mut ctx) {
                        Ok(v) => add.push(v),
                        Err(_) => return Ok(None),
                    }
                }
            }
        }
        if lr.sum.is_none() {
            let V::Int(n) = old else { return Ok(None) };
            return Ok(Some(V::Int(n + add.len() as i64)));
        }
        let mut acc = old.clone();
        for v in add.into_iter().filter(|v| !v.is_none()) {
            if !same_unit(&acc, &v) {
                return Ok(None);
            }
            acc = match self.add_sub(true, acc, v) {
                Ok(x) => x,
                Err(_) => return Ok(None),
            };
        }
        let v = acc.normalized();
        let v = self.zero_for(lr.span, v, Some(&this), None);
        Ok(Some(self.zero_for(f.span, v, Some(&this), None)))
    }

    /// The new value of a rollup from its stored value and the children whose contribution
    /// changed (None: compute it the interpreter's way).
    fn delta(&mut self, ent: &str, f: &'m Field, id: Id, old: &V) -> R<Option<V>> {
        if let Some(lr) = self.ledger_rollup(ent, &f.name) {
            return self.ledger_delta(&lr, ent, f, id, old);
        }
        let Some(r) = self.rollup(ent, &f.name) else {
            return Ok(None);
        };
        // records unfrozen here read the current world again instead of the one of their
        // freezing: what they contribute changed without a write
        if old.is_none() || !self.store.thawed.is_empty() {
            return Ok(None);
        }
        let mut changed: Vec<Id> = self.stale_set(&format!("r:{ent}.{}", f.name), &r.child, &r.deps).into_iter().collect();
        changed.sort();
        let changed_children = changed.clone();
        let tx = self.store.tx;
        let mut add: Vec<V> = Vec::new();
        let mut sub: Vec<V> = Vec::new();
        for c in changed {
            if !self.store.exists(c, None) {
                continue;
            }
            let ce = self.ent_of(c);
            if !self.is_a(&ce, &r.child) {
                continue;
            }
            let now = self.store.read(c, &r.back, None);
            let before = self.store.records.get(&c).and_then(|x| x.before(tx, &r.back));
            if holds(&now, id) {
                match self.contribution(&r, c, false) {
                    Ok(Some(v)) => add.push(v),
                    Ok(None) => {}
                    Err(_) => return Ok(None),
                }
            }
            if holds(&before, id) {
                match self.contribution(&r, c, true) {
                    Ok(Some(v)) => sub.push(v),
                    Ok(None) => {}
                    Err(_) => return Ok(None),
                }
            }
        }
        let this = V::Ref { ent: Rc::from(ent), id };
        let v = match &r.agg {
            Agg::Count => {
                let V::Int(n) = old else { return Ok(None) };
                V::Int(n + add.len() as i64 - sub.len() as i64)
            }
            Agg::Sum(..) => {
                let mut acc = old.clone();
                for (adding, vals) in [(true, add), (false, sub)] {
                    for v in vals.into_iter().filter(|v| !v.is_none()) {
                        if !same_unit(&acc, &v) {
                            return Ok(None);
                        }
                        acc = match self.add_sub(adding, acc, v) {
                            Ok(x) => x,
                            Err(_) => return Ok(None),
                        };
                    }
                }
                let v = acc.normalized();
                let v = self.zero_for(r.span, v, Some(&this), None);
                self.zero_for(f.span, v, Some(&this), None)
            }
            Agg::Min(..) | Agg::Max(..) => {
                let max = matches!(r.agg, Agg::Max(..));
                if add.iter().chain(sub.iter()).any(|v| v.is_none()) {
                    return Ok(None);
                }
                // an extreme that may have left (a tie included): the extreme of the other
                // children, one indexed query, with what the changed ones contribute now
                let mut left = false;
                for s in &sub {
                    match self.compare(s, old) {
                        Ok(0) => left = true,
                        Ok(_) => {}
                        Err(_) => return Ok(None),
                    }
                }
                let mut best = old.clone();
                if left {
                    let Ok(Some(rest)) = self.extreme_of_others(&r, id, &changed_children, max) else {
                        return Ok(None);
                    };
                    if add.is_empty() && rest.is_none() {
                        return Ok(Some(V::None));
                    }
                    best = match rest {
                        Some(v) => v,
                        None => add[0].clone(),
                    };
                }
                for a in add {
                    match self.compare(&a, &best) {
                        Ok(c) if (max && c > 0) || (!max && c < 0) => best = a,
                        Ok(_) => {}
                        Err(_) => return Ok(None),
                    }
                }
                best
            }
        };
        Ok(Some(v))
    }
}
