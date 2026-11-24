//! Queries over whole entities through the source's SQL (`crate::query`), with the
//! interpreter deciding what SQL cannot: the conditions it did not translate, the rows it
//! flagged, the records written in the working set (read-your-writes). Every answer is the one
//! the interpreter would give scanning the entity; `None` means "do it the interpreter's way".

use super::*;
use crate::query::{After, Agg, AggFn, AggOut, Answer, Binder, Cond, Frees, GroupKey, GroupOrder, GroupOut, Key, Perm, Request, RuleCond, TextSearch, What};

/// A query: the entity, its conditions and order.
#[derive(Clone)]
pub struct Spec<'a> {
    pub entity: &'a str,
    pub conds: Vec<Cond<'a>>,
    pub order: Vec<Key<'a>>,
    /// ties (and the order without keys) by id descending: `last`, `last_by`
    pub id_desc: bool,
    /// keep only the rows the user may read (the API)
    pub read_check: bool,
    /// the selection of the grid
    pub ids: Option<Vec<Id>>,
    /// rows whose reference (or set) `field` holds a record: the lines of a record
    pub refs: Vec<(String, Id)>,
    /// rows of a group: their key has this value
    pub keys: Vec<(GroupKey, Option<String>)>,
    /// rows matching a full-text search
    pub search: Option<TextSearch>,
}

impl<'a> Spec<'a> {
    pub fn new(entity: &'a str) -> Spec<'a> {
        Spec { entity, conds: Vec::new(), order: Vec::new(), id_desc: false, read_check: false, ids: None, refs: Vec::new(), keys: Vec::new(), search: None }
    }
}

/// The key of a row for a group, as the interpreter computes it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyVal {
    /// `query::group_value` (None: no value)
    Val(Option<String>),
    /// the path goes through a field or a record the user may not see
    Hidden,
}

/// A page of groups (see `What::Groups`).
pub struct Groups {
    pub groups: Vec<GroupOut>,
    /// how many groups
    pub total: u64,
}

/// A page of a query.
pub struct Page {
    pub ids: Vec<Id>,
    /// where the next page starts (None: no more rows)
    pub next: Option<After>,
    /// candidates SQL returned and how many of them the interpreter kept (for estimates)
    pub scanned: u64,
    pub kept: u64,
}

/// Rows read from SQL per round trip when the interpreter filters them further.
const BATCH: usize = 500;

impl<'m> Interp<'m> {
    /// `e` evaluated on a record as the expressions of a query see it: `self` and the record's
    /// members by name, current values (`where` of the API, ordering cells).
    pub fn eval_on(&mut self, rec: &V, ent: &str, e: &Expr) -> R<V> {
        let mut ctx = Ctx::for_record(rec, ent);
        self.eval(e, &mut ctx)
    }

    /// Whether every row of `ent` the user may read is readable without looking at the row (no
    /// condition in the rules that grant it, no conditional `deny`): then reading needs no check
    /// per row.
    pub fn read_unconditional(&mut self, ent: &str) -> bool {
        let Some(user) = self.user else { return true };
        if self.system > 0 {
            return true;
        }
        let roles = self.roles_of(user);
        let m = self.m;
        let own: Vec<&Rule> = roles.iter().filter_map(|r| m.role(r)).flat_map(|r| r.rules.iter()).collect();
        for rule in m.rules.iter().chain(own.iter().copied()) {
            if rule.effect == "deny" && !rule.history && Self::rule_matches(rule, "read", ent, None) {
                return false;
            }
        }
        if m.roles.is_empty() {
            return true;
        }
        own.iter().copied().chain(m.rules.iter()).any(|r| r.effect == "allow" && r.condition.is_none() && Self::rule_matches(r, "read", ent, None))
    }

    /// The row rules deciding `verb` on rows of `ent` for the current user, as the source can
    /// translate them (`check_permission` is the reference). None when nothing is checked (no
    /// user, code running as the system).
    pub fn perm_of(&mut self, verb: &str, ent: &str) -> Option<Perm<'m>> {
        let user = self.user?;
        if self.system > 0 {
            return None;
        }
        let roles = self.roles_of(user);
        Some(self.perm_build(verb, ent, &roles, 0))
    }

    fn perm_build(&self, verb: &str, ent: &str, roles: &[String], depth: usize) -> Perm<'m> {
        let m: &'m Model = self.m;
        let own: Vec<&'m Rule> = roles.iter().filter_map(|r| m.role(r)).flat_map(|r| r.rules.iter()).collect();
        // as `rule_condition`: a condition that is not a lambda always holds
        let cond = |r: &'m Rule| -> Option<RuleCond<'m>> {
            match r.condition.as_ref().map(|c| &c.kind) {
                Some(ExprKind::Lambda { params, body }) => Some(RuleCond { param: params.first().map(String::as_str).unwrap_or("_"), body: body.as_ref() }),
                _ => None,
            }
        };
        let deny = m.rules.iter().chain(own.iter().copied()).filter(|r| r.effect == "deny" && !r.history && Self::rule_matches(r, verb, ent, None)).map(cond).collect();
        let open = m.roles.is_empty();
        let mut perm = Perm { entity: ent.to_string(), deny, allow: Vec::new(), open, parent: None, too_deep: false };
        if open {
            return perm;
        }
        perm.allow = own.iter().copied().chain(m.rules.iter()).filter(|r| r.effect == "allow" && Self::rule_matches(r, verb, ent, None)).map(cond).collect();
        if let Some(pf) = m.entity(ent).and_then(|e| e.fields.iter().find(|f| f.parent)) {
            let parent_ent = ty_names(pf.ty.as_ref()).unwrap_or_default();
            let pverb = match verb {
                "read" => "read",
                "edit" | "create" => "edit",
                _ => "",
            };
            if !pverb.is_empty() && !parent_ent.is_empty() {
                if depth >= 8 {
                    perm.too_deep = true;
                } else {
                    perm.parent = Some((pf.name.clone(), Box::new(self.perm_build(pverb, &parent_ent, roles, depth + 1))));
                }
            }
        }
        perm
    }

    /// The values of the subexpressions of a permission's conditions that do not depend on the
    /// row, as `rule_condition` evaluates them (as the system, without the caller's locals).
    pub fn perm_frees(&mut self, perm: &Perm, f: &mut Frees) {
        let m = self.m;
        for (ent, c) in perm.conds() {
            let mut list = Vec::new();
            crate::query::free_exprs(m, ent, &Binder::Param(c.param.to_string()), c.body, &mut list);
            for e in list {
                if f.contains(e) {
                    continue;
                }
                self.system += 1;
                let v = self.eval(e, &mut Ctx::default());
                self.system -= 1;
                f.insert(
                    e,
                    match v {
                        Ok(v) => Ok(v),
                        Err(Flow::Fail(m)) => Err(m),
                        Err(_) => Err("not a value".to_string()),
                    },
                );
            }
        }
    }

    /// The permission a query of the API applies (rows the user may read), when it depends on
    /// the row.
    fn read_perm(&mut self, spec: &Spec) -> Option<Perm<'m>> {
        if spec.read_check && !self.read_unconditional(spec.entity) {
            self.perm_of("read", spec.entity)
        } else {
            None
        }
    }

    /// Whether `c` holds for `rec`, as the interpreter evaluates it (the lambda's parameter bound
    /// in the caller's context, or the record's members for a `This` condition).
    fn holds(&mut self, c: &Cond, rec: &V, ent: &str, ctx: &mut Ctx) -> R<bool> {
        let v = match c.binder {
            Binder::This => self.eval_on(rec, ent, c.expr)?,
            Binder::Param(p) => {
                ctx.scopes.push(Default::default());
                ctx.bind(p, rec.clone());
                let r = self.eval(c.expr, ctx);
                ctx.scopes.pop();
                r?
            }
        };
        Ok(v.truthy() != c.negate)
    }

    fn holds_all(&mut self, spec: &Spec, id: Id, ctx: &mut Ctx) -> R<bool> {
        let rec = V::Ref { ent: Rc::from(spec.entity), id };
        if spec.read_check && !self.can("read", spec.entity, None, Some(&rec)) {
            return Ok(false);
        }
        for (f, target) in &spec.refs {
            let v = self.store.read(id, f, None);
            if !crate::store::same(v.as_ref(), &V::Ref { ent: Rc::from(""), id: *target }) {
                return Ok(false);
            }
        }
        for (k, want) in &spec.keys {
            if self.group_key_of(&rec, spec.entity, k)? != KeyVal::Val(want.clone()) {
                return Ok(false);
            }
        }
        if let Some(s) = &spec.search {
            if !self.searched(&rec, s) {
                return Ok(false);
            }
        }
        for c in &spec.conds {
            if !self.holds(c, &rec, spec.entity, ctx)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether a record matches a full-text search (`query::text_matches` on its fields).
    pub fn searched(&mut self, rec: &V, s: &TextSearch) -> bool {
        let Some(id) = rec.as_ref_id() else {
            return false;
        };
        let texts: Vec<String> = s
            .fields
            .iter()
            .filter_map(|f| self.store.read(id, f, None))
            .filter_map(|v| match v {
                V::Text(t) => Some(t.to_string()),
                _ => None,
            })
            .collect();
        crate::query::text_matches(&s.words, &texts)
    }

    /// The key of a record for a group: its value at the end of the path (`query::group_value`),
    /// hidden when the path goes through a field or a record the user may not see.
    pub fn group_key_of(&mut self, rec: &V, ent: &str, key: &GroupKey) -> R<KeyVal> {
        let mut cur = rec.clone();
        let mut cur_ent = ent.to_string();
        for (k, seg) in key.path.iter().enumerate() {
            if self.hidden(&cur_ent, seg) {
                return Ok(KeyVal::Hidden);
            }
            let v = self.get(&cur, seg, None, None)?;
            if k + 1 == key.path.len() {
                return crate::query::group_value(&v, key.bucket).map(KeyVal::Val).map_err(Flow::Fail);
            }
            match &v {
                V::Ref { ent, .. } => {
                    if !self.can("read", ent, None, Some(&v)) {
                        return Ok(KeyVal::Hidden);
                    }
                    cur_ent = ent.to_string();
                    cur = v;
                }
                V::None => return Ok(KeyVal::Val(None)),
                _ => return Err(Flow::Fail(format!("`{seg}` is not a record"))),
            }
        }
        Ok(KeyVal::Val(None))
    }

    /// The values of the subexpressions that do not depend on the row.
    fn frees_of<'a>(&mut self, spec: &Spec<'a>, aggs: &[Agg<'a>], perm: Option<&Perm>, ctx: &mut Ctx) -> Frees {
        let m = self.m;
        let mut list: Vec<(&'a Expr, bool)> = Vec::new();
        let mut add = |b: &Binder, e: &'a Expr| {
            let mut out = Vec::new();
            crate::query::free_exprs(m, spec.entity, b, e, &mut out);
            list.extend(out.into_iter().map(|x| (x, *b == Binder::This)));
        };
        for c in &spec.conds {
            add(c.binder, c.expr);
        }
        for k in &spec.order {
            if let Key::Lang { binder, expr, .. } = k {
                add(binder, expr);
            }
        }
        for a in aggs {
            if let crate::query::AggOf::Expr { binder, expr } = &a.of {
                add(binder, expr);
            }
        }
        let mut f = Frees::default();
        for (e, this) in list {
            if f.contains(e) {
                continue;
            }
            let v = if this { self.eval(e, &mut Ctx::default()) } else { self.eval(e, ctx) };
            let v = match v {
                Ok(v) => Ok(v),
                Err(Flow::Fail(m)) => Err(m),
                Err(_) => Err("not a value".to_string()),
            };
            f.insert(e, v);
        }
        if let Some(p) = perm {
            self.perm_frees(p, &mut f);
        }
        f
    }

    /// The records of the working set that SQL must not read as committed: the query's own
    /// (left out of SQL and evaluated here), and the other entities written (what reads them
    /// is not translated).
    fn dirty_split(&mut self, ent: &str) -> (Vec<Id>, Vec<String>) {
        let mut own = Vec::new();
        let mut others: Vec<String> = Vec::new();
        for id in self.store.dirty.clone() {
            let Some(r) = self.store.records.get(&id) else {
                continue;
            };
            if *r.ent == *ent {
                own.push(id);
            } else if !others.iter().any(|o| **o == *r.ent) {
                others.push(r.ent.to_string());
            }
        }
        (own, others)
    }

    /// Can this query be asked to the source at all?
    fn sql_possible(&self, spec: &Spec, ctx: &Ctx) -> bool {
        self.store.lazy() && ctx.as_of.is_none() && self.m.entity(spec.entity).map_or(false, |e| e.kind == EntityKind::Entity && !e.is_ledger && !e.temporary)
    }

    /// Ask the source; None when the interpreter must answer (no SQL, or what SQL read changed
    /// after this working set's snapshot). Records what the answer relied on.
    fn ask(&mut self, spec: &Spec, frees: &Frees, perm: Option<&Perm>, exclude: &[Id], dirty: &[String], what: What) -> Option<Answer> {
        let tx = self.store.tx;
        let formulas = self.store.dirty.is_empty() && self.store.wlog.is_empty() && self.store.thawed.is_empty() && !self.store.rows.iter().any(|r| r.tx == tx);
        self.ask_with(spec, frees, perm, exclude, dirty, what, formulas)
    }

    /// The rows of `spec` in its order (at most `limit`), left out `exclude`, reading the
    /// incremental formulas' columns: the records whose stored values may be stale must be
    /// among `exclude` (a rollup's changed children). None: SQL cannot answer. Each row with
    /// its key values (None: no value).
    pub(super) fn sql_rows_excluding(&mut self, spec: &Spec, ctx: &mut Ctx, exclude: &[Id], limit: usize) -> R<Option<Vec<(Id, Vec<Option<String>>)>>> {
        if !self.sql_possible(spec, ctx) {
            return Ok(None);
        }
        let frees = self.frees_of(spec, &[], None, ctx);
        let Some(ans) = self.ask_with(spec, &frees, None, exclude, &[], What::Rows { after: None, limit, only_checked: false }, true) else {
            return Ok(None);
        };
        if !ans.residual.is_empty() || ans.rows.iter().any(|r| r.check) {
            return Ok(None);
        }
        Ok(Some(ans.rows.into_iter().map(|r| (r.id, r.keys)).collect()))
    }

    #[allow(clippy::too_many_arguments)]
    fn ask_with(&mut self, spec: &Spec, frees: &Frees, perm: Option<&Perm>, exclude: &[Id], dirty: &[String], what: What, formulas: bool) -> Option<Answer> {
        let src = self.store.src.clone()?;
        let req = Request {
            model: self.m,
            entity: spec.entity,
            conds: spec.conds.clone(),
            order: spec.order.clone(),
            id_desc: spec.id_desc,
            frees,
            exclude: exclude.to_vec(),
            dirty: dirty.to_vec(),
            ids: spec.ids.clone(),
            refs: spec.refs.clone(),
            perm,
            what,
            formulas,
            keys: spec.keys.clone(),
            search: spec.search.clone(),
        };
        let ans = src.borrow_mut().select(&req).ok()?;
        // SQL reads the latest committed rows; the working set is as of its snapshot
        if src.borrow_mut().changed_since(&ans.entities).unwrap_or(true) {
            return None;
        }
        if !self.store.untracked {
            self.store.reads.scans.insert(spec.entity.to_string());
            for (e, f) in &ans.fields {
                // a materialized formula is not in the history: its rows' changes are
                if self.field(e, f).is_some_and(|x| x.formula.is_some()) {
                    self.store.reads.formula_scans.insert(e.clone());
                } else {
                    self.store.reads.finds.insert((e.clone(), f.clone()));
                }
            }
        }
        Some(ans)
    }

    /// The rows SQL returns from `after`, filtered by the interpreter where SQL could not
    /// decide, until `limit` rows are kept or `max_scan` candidates were read (then the page is
    /// shorter and `next` says where to go on). The records of the working set's own writes
    /// are not included (see `overlay`).
    pub fn sql_page(&mut self, spec: &Spec, ctx: &mut Ctx, after: Option<After>, offset: usize, limit: usize, max_scan: usize) -> R<Option<Page>> {
        if !self.sql_possible(spec, ctx) {
            return Ok(None);
        }
        let perm = self.read_perm(spec);
        let frees = self.frees_of(spec, &[], perm.as_ref(), ctx);
        let (exclude, dirty) = self.dirty_split(spec.entity);
        // the row rules SQL could not apply: every row is checked here (known from the first answer)
        let mut permission = false;
        let mut cursor = after;
        let mut skip = offset;
        let mut page = Page { ids: Vec::new(), next: None, scanned: 0, kept: 0 };
        loop {
            let want = limit.saturating_add(skip).saturating_sub(page.ids.len()).max(1);
            let batch = if permission { want.max(50).min(BATCH) } else { want.min(5000) };
            let Some(ans) = self.ask(spec, &frees, perm.as_ref(), &exclude, &dirty, What::Rows { after: cursor.clone(), limit: batch, only_checked: false }) else {
                return Ok(None);
            };
            permission = perm.is_some() && !ans.perm_sql;
            let n = ans.rows.len();
            // the records the interpreter decides, loaded at once
            let check: Vec<Id> = ans.rows.iter().filter(|r| r.check || permission).map(|r| r.id).collect();
            self.store.prefetch_as(spec.entity, &check);
            for r in &ans.rows {
                page.scanned += 1;
                cursor = Some(After { keys: r.keys.clone(), id: r.id });
                let ok = if r.check || permission {
                    if r.check {
                        self.holds_all(spec, r.id, ctx)?
                    } else {
                        let rec = V::Ref { ent: Rc::from(spec.entity), id: r.id };
                        self.can("read", spec.entity, None, Some(&rec))
                    }
                } else {
                    true
                };
                if !ok {
                    continue;
                }
                page.kept += 1;
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                page.ids.push(r.id);
                if page.ids.len() >= limit {
                    page.next = cursor.clone();
                    return Ok(Some(page));
                }
            }
            if n < batch {
                // no more rows
                page.next = None;
                return Ok(Some(page));
            }
            if page.scanned as usize >= max_scan {
                page.next = cursor.clone();
                return Ok(Some(page));
            }
        }
    }

    /// The ids of the rows matching every condition of `spec`, in id order, when SQL decides
    /// them all (each condition translated; the rows it flags decided here): a deploy checking
    /// a new rule over every record with one query. None: SQL cannot (the rule is checked
    /// record by record).
    pub fn sql_matching(&mut self, spec: &Spec, ctx: &mut Ctx) -> R<Option<Vec<Id>>> {
        if !self.sql_possible(spec, ctx) || !self.store.dirty.is_empty() {
            return Ok(None);
        }
        let frees = self.frees_of(spec, &[], None, ctx);
        let mut out = Vec::new();
        let mut cursor: Option<After> = None;
        loop {
            let Some(ans) = self.ask(spec, &frees, None, &[], &[], What::Rows { after: cursor.clone(), limit: 20_000, only_checked: false }) else {
                return Ok(None);
            };
            if !ans.residual.is_empty() {
                return Ok(None);
            }
            let flagged: Vec<Id> = ans.rows.iter().filter(|r| r.check).map(|r| r.id).collect();
            self.store.prefetch_as(spec.entity, &flagged);
            let n = ans.rows.len();
            for r in &ans.rows {
                cursor = Some(After { keys: r.keys.clone(), id: r.id });
                if !r.check || self.holds_all(spec, r.id, ctx)? {
                    out.push(r.id);
                }
            }
            self.store.release(&flagged);
            if n < 20_000 {
                return Ok(Some(out));
            }
        }
    }

    /// How many rows (exact when at most `cap`, else an estimate: (count, estimated)).
    pub fn sql_count(&mut self, spec: &Spec, ctx: &mut Ctx, cap: Option<u64>) -> R<Option<(u64, bool)>> {
        if !self.sql_possible(spec, ctx) {
            return Ok(None);
        }
        let perm = self.read_perm(spec);
        let frees = self.frees_of(spec, &[], perm.as_ref(), ctx);
        let (exclude, dirty) = self.dirty_split(spec.entity);
        let Some(ans) = self.ask(spec, &frees, perm.as_ref(), &exclude, &dirty, What::Count { cap }) else {
            return Ok(None);
        };
        // the row rules SQL could not apply: every row is checked here
        let permission = perm.is_some() && !ans.perm_sql;
        let (n, flagged) = ans.count.unwrap_or((0, 0));
        if let Some((est, exact)) = ans.estimate {
            // too many to decide one by one: SQL's count (rows the interpreter would drop
            // included), estimated unless the database counted them all cheaply
            if exact && flagged == 0 && !permission && exclude.is_empty() {
                return Ok(Some((est, false)));
            }
            return Ok(Some((est, true)));
        }
        if flagged == 0 && !permission {
            let mut total = n;
            total += self.overlay(spec, ctx, &exclude)?.len() as u64;
            return Ok(Some((total, false)));
        }
        // the rows the interpreter decides, in batches
        let mut total = n - flagged;
        if permission {
            total = 0;
        }
        let mut cursor: Option<After> = None;
        loop {
            let Some(ans) = self.ask(spec, &frees, perm.as_ref(), &exclude, &dirty, What::Rows { after: cursor.clone(), limit: BATCH, only_checked: !permission }) else {
                return Ok(None);
            };
            let ids: Vec<Id> = ans.rows.iter().map(|r| r.id).collect();
            self.store.prefetch_as(spec.entity, &ids);
            for r in &ans.rows {
                cursor = Some(After { keys: r.keys.clone(), id: r.id });
                if (r.check && self.holds_all(spec, r.id, ctx)?) || (!r.check && self.can("read", spec.entity, None, Some(&V::Ref { ent: Rc::from(spec.entity), id: r.id }))) {
                    total += 1;
                }
            }
            if ans.rows.len() < BATCH {
                break;
            }
        }
        total += self.overlay(spec, ctx, &exclude)?.len() as u64;
        Ok(Some((total, false)))
    }

    /// The records of the query's entity written in the working set that match now (they are
    /// left out of SQL), in id order.
    fn overlay(&mut self, spec: &Spec, ctx: &mut Ctx, exclude: &[Id]) -> R<Vec<Id>> {
        let mut out = Vec::new();
        for &id in exclude {
            if let Some(ids) = &spec.ids {
                if !ids.contains(&id) {
                    continue;
                }
            }
            if self.store.exists(id, None) && self.holds_all(spec, id, ctx)? {
                out.push(id);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Every matching id in id order (SQL's and the working set's own records).
    pub fn sql_ids(&mut self, spec: &Spec, ctx: &mut Ctx) -> R<Option<Vec<Id>>> {
        if !spec.order.is_empty() || spec.id_desc {
            return Ok(None);
        }
        let Some(page) = self.sql_page(spec, ctx, None, 0, usize::MAX, usize::MAX)? else {
            return Ok(None);
        };
        let (exclude, _) = self.dirty_split(spec.entity);
        let mut ids = page.ids;
        ids.extend(self.overlay(spec, ctx, &exclude)?);
        ids.sort();
        ids.dedup();
        Ok(Some(ids))
    }

    /// The first matching id in the query's order (ties by id, descending with `id_desc`):
    /// `first`, `last`, `find`, `any`, `sort_by(…).first()`, `last_by`.
    pub fn sql_first(&mut self, spec: &Spec, ctx: &mut Ctx) -> R<Option<Option<Id>>> {
        let Some(page) = self.sql_page(spec, ctx, None, 0, 1, usize::MAX)? else {
            return Ok(None);
        };
        let sql_best = page.ids.first().copied();
        let (exclude, _) = self.dirty_split(spec.entity);
        let mine = self.overlay(spec, ctx, &exclude)?;
        if mine.is_empty() {
            return Ok(Some(sql_best));
        }
        let mut cands: Vec<Id> = mine;
        cands.extend(sql_best);
        // the interpreter's order: its keys, then the id
        let mut keyed: Vec<(Vec<V>, Id)> = Vec::new();
        for id in cands {
            let rec = V::Ref { ent: Rc::from(spec.entity), id };
            let mut k = Vec::new();
            for key in &spec.order {
                match key {
                    Key::Lang { binder, expr, .. } => {
                        let c = Cond { binder, expr, negate: false };
                        k.push(self.value_of(&c, &rec, spec.entity, ctx)?);
                    }
                    Key::Grid { .. } => return Ok(None),
                }
            }
            keyed.push((k, id));
        }
        let mut err = None;
        keyed.sort_by(|a, b| {
            for (i, key) in spec.order.iter().enumerate() {
                let desc = matches!(key, Key::Lang { desc: true, .. });
                match self.compare(&a.0[i], &b.0[i]) {
                    Ok(0) => continue,
                    Ok(c) => return if (c < 0) != desc { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater },
                    Err(e) => {
                        err = Some(e);
                        return std::cmp::Ordering::Equal;
                    }
                }
            }
            if spec.id_desc {
                b.1.cmp(&a.1)
            } else {
                a.1.cmp(&b.1)
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        Ok(Some(keyed.first().map(|(_, id)| *id)))
    }

    fn value_of(&mut self, c: &Cond, rec: &V, ent: &str, ctx: &mut Ctx) -> R<V> {
        match c.binder {
            Binder::This => self.eval_on(rec, ent, c.expr),
            Binder::Param(p) => {
                ctx.scopes.push(Default::default());
                ctx.bind(p, rec.clone());
                let r = self.eval(c.expr, ctx);
                ctx.scopes.pop();
                r
            }
        }
    }

    /// Aggregates in SQL: None when a row needs the interpreter (then the caller computes them
    /// on the rows).
    pub fn sql_aggs(&mut self, spec: &Spec, ctx: &mut Ctx, aggs: &[Agg]) -> R<Option<(u64, Vec<AggOut>)>> {
        if !self.sql_possible(spec, ctx) {
            return Ok(None);
        }
        let perm = self.read_perm(spec);
        let frees = self.frees_of(spec, aggs, perm.as_ref(), ctx);
        let (exclude, dirty) = self.dirty_split(spec.entity);
        if !exclude.is_empty() {
            return Ok(None);
        }
        let Some(ans) = self.ask(spec, &frees, perm.as_ref(), &exclude, &dirty, What::Aggs(aggs.to_vec())) else {
            return Ok(None);
        };
        let (n, flagged) = ans.count.unwrap_or((0, 0));
        // rows the interpreter must decide (row rules SQL could not apply among them): it
        // aggregates them itself
        if (perm.is_some() && !ans.perm_sql) || flagged > 0 || !ans.residual.is_empty() || ans.aggs.len() != aggs.len() {
            return Ok(None);
        }
        Ok(Some((n, ans.aggs)))
    }

    /// Groups in SQL (a page of them, with their number): None when a row needs the
    /// interpreter, or what SQL read changed (then the caller groups the rows itself).
    #[allow(clippy::too_many_arguments)]
    pub fn sql_groups(&mut self, spec: &Spec, ctx: &mut Ctx, keys: &[GroupKey], aggs: &[Agg], order: GroupOrder, desc: bool, offset: usize, limit: usize, within: Vec<Option<Vec<Option<String>>>>) -> R<Option<Groups>> {
        if !self.sql_possible(spec, ctx) {
            return Ok(None);
        }
        let perm = self.read_perm(spec);
        let frees = self.frees_of(spec, aggs, perm.as_ref(), ctx);
        let (exclude, dirty) = self.dirty_split(spec.entity);
        if !exclude.is_empty() {
            return Ok(None);
        }
        let what = What::Groups { keys: keys.to_vec(), aggs: aggs.to_vec(), order, desc, offset, limit, within };
        let Some(ans) = self.ask(spec, &frees, perm.as_ref(), &exclude, &dirty, what) else {
            return Ok(None);
        };
        let Some((total, flagged)) = ans.groups_total else {
            return Ok(None);
        };
        if (perm.is_some() && !ans.perm_sql) || flagged > 0 || !ans.residual.is_empty() {
            return Ok(None);
        }
        Ok(Some(Groups { groups: ans.groups, total }))
    }

    // ---- the language: `Entity.where(…)` chains -----------------------------------------

    /// `Entity.where(λ).where(μ)…`: the entity and the lambdas (parameter, body).
    pub(super) fn table_chain<'e>(&mut self, e: &'e Expr, ctx: &mut Ctx) -> Option<(String, Vec<(String, &'e Expr)>)> {
        match &e.kind {
            ExprKind::Name(n) => {
                // as `name` resolves it: locals, `me`, the record's members come first
                if ctx.local(n).is_some() || n == "me" || n == "current" {
                    return None;
                }
                if let (Some(V::Ref { .. } | V::Rec { .. }), Some(ent)) = (&ctx.this, &ctx.entity) {
                    if self.field(ent, n).is_some() || crate::query::IMPLICIT.contains(&n.as_str()) {
                        return None;
                    }
                }
                let en = self.m.entity(n)?;
                if en.kind != EntityKind::Entity || en.is_ledger || en.temporary {
                    return None;
                }
                Some((n.clone(), Vec::new()))
            }
            ExprKind::Call { callee, args } => {
                let ExprKind::Field { base, name, optional: false } = &callee.kind else {
                    return None;
                };
                if name != "where" {
                    return None;
                }
                let lambda = single_lambda(args)?;
                let (t, mut l) = self.table_chain(base, ctx)?;
                l.push(lambda);
                Some((t, l))
            }
            _ => None,
        }
    }

    /// `Entity.where(…).count()` and the other methods over a whole entity, in SQL when the
    /// source can: None to evaluate them the interpreter's way.
    pub(super) fn query_call(&mut self, base: &Expr, name: &str, args: &[Arg], ctx: &mut Ctx) -> R<Option<V>> {
        if !self.store.lazy() || ctx.as_of.is_some() {
            return Ok(None);
        }
        // `….group_by(…)….map(…)`: one GROUP BY
        if let Some(v) = self.group_call(base, name, args, ctx)? {
            return Ok(Some(v));
        }
        let viewing = self.viewing();
        // `….first(n)` / `.last(n)`, after a `sort_by(k)` or in id order: one page of SQL
        if let ("first" | "last", [Arg { name: None, spread: false, value }]) = (name, args) {
            if !matches!(value.kind, ExprKind::Lambda { .. }) {
                if let Some(v) = self.query_take(base, name == "last", value, ctx)? {
                    return Ok(Some(v));
                }
            }
        }
        // `….sort_by(k).first()` / `.last()`
        if matches!(name, "first" | "last") && args.is_empty() {
            if let ExprKind::Call { callee, args: sargs } = &base.kind {
                if let ExprKind::Field { base: inner, name: sname, optional: false } = &callee.kind {
                    if sname == "sort_by" {
                        let Some((p, key)) = single_lambda(sargs) else {
                            return Ok(None);
                        };
                        let Some((ent, lambdas)) = self.table_chain(inner, ctx) else {
                            return Ok(None);
                        };
                        let binders: Vec<Binder> = lambdas.iter().map(|(p, _)| Binder::Param(p.clone())).collect();
                        let kb = Binder::Param(p);
                        if viewing {
                            for (p, e) in lambdas.iter().chain(std::iter::once(&(p_of(&kb), key))) {
                                self.viewer_lambda(&ent, p, e)?;
                            }
                        }
                        let mut spec = Spec::new(&ent);
                        spec.read_check = viewing;
                        spec.conds = lambdas.iter().zip(binders.iter()).map(|((_, e), b)| Cond { binder: b, expr: e, negate: false }).collect();
                        spec.order = vec![Key::Lang { binder: &kb, expr: key, desc: name == "last" }];
                        spec.id_desc = name == "last";
                        return Ok(self.sql_first(&spec, ctx)?.map(|id| id.map(|id| V::Ref { ent: Rc::from(ent.as_str()), id }).unwrap_or(V::None)));
                    }
                }
            }
        }
        let Some((ent, mut lambdas)) = self.table_chain(base, ctx) else {
            return Ok(None);
        };
        let own = match args {
            [] => None,
            _ => match single_lambda(args) {
                Some(l) => Some(l),
                None => return Ok(None),
            },
        };
        let terminal_cond = matches!(name, "where" | "count" | "any" | "none" | "first" | "last" | "find" | "all");
        if let (true, Some(l)) = (terminal_cond, &own) {
            lambdas.push(l.clone());
        }
        if viewing {
            for (p, e) in lambdas.iter().chain(own.iter()) {
                self.viewer_lambda(&ent, p, e)?;
            }
        }
        let binders: Vec<Binder> = lambdas.iter().map(|(p, _)| Binder::Param(p.clone())).collect();
        let mut spec = Spec::new(&ent);
        spec.read_check = viewing;
        spec.conds = lambdas.iter().zip(binders.iter()).map(|((_, e), b)| Cond { binder: b, expr: e, negate: false }).collect();
        let refv = |id: Id| V::Ref { ent: Rc::from(ent.as_str()), id };
        let fast: Option<V> = match (name, own.is_some()) {
            ("where", true) | ("all", false) => match self.sql_ids(&spec, ctx)? {
                Some(ids) => {
                    self.store.prefetch_as(&ent, &ids);
                    Some(list(ids.into_iter().map(refv).collect()))
                }
                None => None,
            },
            ("count", _) => self.sql_count(&spec, ctx, None)?.map(|(n, _)| V::Int(n as i64)),
            ("any", _) | ("none", _) => self.sql_first(&spec, ctx)?.map(|f| V::Bool(f.is_some() == (name == "any"))),
            ("all", true) => {
                // no row where the condition is not true
                if let Some(last) = spec.conds.last_mut() {
                    last.negate = true;
                }
                self.sql_first(&spec, ctx)?.map(|f| V::Bool(f.is_none()))
            }
            ("first" | "find", _) => self.sql_first(&spec, ctx)?.map(|f| f.map(refv).unwrap_or(V::None)),
            ("last", _) => {
                spec.id_desc = true;
                self.sql_first(&spec, ctx)?.map(|f| f.map(refv).unwrap_or(V::None))
            }
            ("last_by" | "min" | "max", true) => {
                let (p, key) = own.clone().unwrap();
                let kb = Binder::Param(p);
                let desc = name != "min";
                let mut ks = spec.clone();
                ks.order = vec![Key::Lang { binder: &kb, expr: key, desc }];
                ks.id_desc = desc;
                match self.sql_first(&ks, ctx)? {
                    None => None,
                    Some(None) => Some(V::None),
                    Some(Some(id)) if name == "last_by" => Some(refv(id)),
                    Some(Some(id)) => {
                        let c = Cond { binder: &kb, expr: key, negate: false };
                        Some(self.value_of(&c, &refv(id), &ent, ctx)?)
                    }
                }
            }
            ("sum", true) => {
                let (p, val) = own.clone().unwrap();
                let vb = Binder::Param(p);
                let agg = Agg { of: crate::query::AggOf::Expr { binder: &vb, expr: val }, func: AggFn::Sum };
                match self.sql_aggs(&spec, ctx, &[agg])? {
                    Some((_, outs)) => lang_sum(&outs[0]).map(V::normalized),
                    None => None,
                }
            }
            _ => None,
        };
        if fast.is_some() {
            return Ok(fast);
        }
        // the method runs on the rows SQL filtered (the `where`s before it)
        let chain = lambdas.len() - usize::from(terminal_cond && own.is_some());
        // a method over every record: the interpreter's way, except for a user's expression,
        // which goes through the rows the user may read (SQL finds them)
        if chain == 0 && !viewing {
            return Ok(None);
        }
        let mut fspec = Spec::new(&ent);
        fspec.read_check = viewing;
        fspec.conds = spec.conds[..chain].iter().map(|c| Cond { negate: false, ..*c }).collect();
        let Some(ids) = self.sql_ids(&fspec, ctx)? else {
            return Ok(None);
        };
        // `first(n)` / `last(n)` of the rows: no row is read
        let counted = matches!(args, [Arg { name: None, value, .. }] if !matches!(value.kind, ExprKind::Lambda { .. }));
        if viewing && !(counted && matches!(name, "first" | "last")) && ids.len() > super::viewer::ENGINE_ROWS {
            return fail(format!("{ent}: {} rows to read one by one for `{name}` (not answered by the database); at most {}: filter with .where(…) first", ids.len(), super::viewer::ENGINE_ROWS));
        }
        self.store.prefetch_as(&ent, &ids);
        let items: Vec<V> = ids.into_iter().map(refv).collect();
        if name == "all" && args.is_empty() {
            return Ok(Some(list(items)));
        }
        Ok(Some(self.coll_method(items, false, name, args, ctx)?))
    }

    /// `Entity.where(…)[.sort_by(k)].first(n)` (or `.last(n)`): the first (last) n rows of the
    /// list the interpreter would sort, as one page of SQL in that order (ties by id). None
    /// when SQL cannot, or the working set wrote rows of the entity.
    fn query_take(&mut self, base: &Expr, last: bool, n: &Expr, ctx: &mut Ctx) -> R<Option<V>> {
        let (chain_expr, key) = match &base.kind {
            ExprKind::Call { callee, args } => match (&callee.kind, single_lambda(args)) {
                (ExprKind::Field { base: inner, name, optional: false }, Some(k)) if name == "sort_by" => (inner.as_ref(), Some(k)),
                _ => (base, None),
            },
            _ => (base, None),
        };
        let Some((ent, lambdas)) = self.table_chain(chain_expr, ctx) else { return Ok(None) };
        if !self.dirty_split(&ent).0.is_empty() {
            return Ok(None);
        }
        let count = match self.eval(n, ctx)? {
            V::Int(k) if k >= 0 => k as usize,
            other => return fail(format!("{}(n) takes a whole number ≥ 0, not {other}", if last { "last" } else { "first" })),
        };
        let viewing = self.viewing();
        if viewing {
            for (p, e) in &lambdas {
                self.viewer_lambda(&ent, p, e)?;
            }
            if let Some((p, e)) = &key {
                self.viewer_lambda(&ent, p, e)?;
            }
        }
        let binders: Vec<Binder> = lambdas.iter().map(|(p, _)| Binder::Param(p.clone())).collect();
        let kb = key.as_ref().map(|(p, _)| Binder::Param(p.clone()));
        let mut spec = Spec::new(&ent);
        spec.read_check = viewing;
        spec.conds = lambdas.iter().zip(binders.iter()).map(|((_, e), b)| Cond { binder: b, expr: e, negate: false }).collect();
        if let (Some((_, k)), Some(b)) = (&key, &kb) {
            spec.order = vec![Key::Lang { binder: b, expr: k, desc: last }];
        }
        spec.id_desc = last;
        let Some(page) = self.sql_page(&spec, ctx, None, 0, count.max(1), usize::MAX)? else { return Ok(None) };
        let mut ids = page.ids;
        ids.truncate(count);
        if last {
            ids.reverse();
        }
        self.store.prefetch_as(&ent, &ids);
        Ok(Some(list(ids.into_iter().map(|id| V::Ref { ent: Rc::from(ent.as_str()), id }).collect())))
    }

    /// `for x in Entity.where(…)`: the rows page by page, as the list would give them (the
    /// list is what matches when the loop starts: what the loop writes does not change it).
    /// None when it must be the list.
    pub(super) fn query_loop(&mut self, iter: &Expr, names: &[String], body: &[Stmt], ctx: &mut Ctx) -> R<Option<()>> {
        if !self.store.lazy() || ctx.as_of.is_some() || names.len() != 1 {
            return Ok(None);
        }
        let (ent, lambdas) = match &iter.kind {
            ExprKind::Call { callee, args } if args.is_empty() => match &callee.kind {
                ExprKind::Field { base, name, optional: false } if name == "all" => match self.table_chain(base, ctx) {
                    Some((e, l)) if l.is_empty() => (e, l),
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            },
            _ => match self.table_chain(iter, ctx) {
                Some((e, l)) if !l.is_empty() => (e, l),
                _ => return Ok(None),
            },
        };
        let binders: Vec<Binder> = lambdas.iter().map(|(p, _)| Binder::Param(p.clone())).collect();
        let mut spec = Spec::new(&ent);
        spec.conds = lambdas.iter().zip(binders.iter()).map(|((_, e), b)| Cond { binder: b, expr: e, negate: false }).collect();
        if !self.sql_possible(&spec, ctx) {
            return Ok(None);
        }
        // what matches when the loop starts: the working set's own records now, and SQL's rows
        // that the interpreter has nothing to decide on (committed data does not change while
        // the loop runs); otherwise the list
        let frees = self.frees_of(&spec, &[], None, ctx);
        let (exclude, dirty) = self.dirty_split(&ent);
        let Some(first) = self.ask(&spec, &frees, None, &exclude, &dirty, What::Rows { after: None, limit: 1000, only_checked: false }) else {
            return Ok(None);
        };
        if !first.residual.is_empty() || first.guarded {
            return Ok(None);
        }
        let mine = self.overlay(&spec, ctx, &exclude)?;
        let mut mine = mine.into_iter().peekable();
        let mut ans = first;
        loop {
            let n = ans.rows.len();
            let mut ids: Vec<Id> = ans.rows.iter().map(|r| r.id).collect();
            let last = ids.last().copied();
            // the working set's records that come before the end of this page
            while let Some(&m) = mine.peek() {
                if n == 1000 && last.map_or(false, |l| m > l) {
                    break;
                }
                ids.push(m);
                mine.next();
            }
            ids.sort();
            // the records this page brings in (not those the working set already had)
            let loaded_here: Vec<Id> = ids.iter().copied().filter(|id| !self.store.records.contains_key(id)).collect();
            self.store.prefetch_as(&ent, &ids);
            for id in ids {
                ctx.scopes.push(Default::default());
                ctx.bind(&names[0], V::Ref { ent: Rc::from(ent.as_str()), id });
                let r = self.stmts(body, ctx);
                ctx.scopes.pop();
                r?;
            }
            // memory bounded: the page's records nobody wrote are read again when needed
            self.store.release(&loaded_here);
            if n < 1000 {
                return Ok(Some(()));
            }
            let after = After { keys: Vec::new(), id: last.unwrap() };
            let Some(next) = self.ask(&spec, &frees, None, &exclude, &dirty, What::Rows { after: Some(after), limit: 1000, only_checked: false }) else {
                // the data changed under the loop: the transaction cannot rely on what it read
                return Err(Flow::Conflict(vec![Conflict { tx: 0, at: String::new(), user: None, cause: "a concurrent commit".into(), what: vec![format!("{ent} rows read by a loop")] }]));
            };
            if !next.residual.is_empty() || next.guarded {
                return fail("internal: a loop's query changed while it ran");
            }
            ans = next;
        }
    }
}

/// The single literal lambda of one parameter among `args`: (parameter, body).
fn single_lambda(args: &[Arg]) -> Option<(String, &Expr)> {
    match args {
        [Arg { name: None, spread: false, value }] => match &value.kind {
            ExprKind::Lambda { params, body } if params.len() == 1 => Some((params[0].clone(), body.as_ref())),
            _ => None,
        },
        _ => None,
    }
}

fn p_of(b: &Binder) -> String {
    match b {
        Binder::Param(p) => p.clone(),
        Binder::This => "self".into(),
    }
}

pub(super) fn lang_sum_pub(a: &AggOut) -> Option<V> {
    lang_sum(a)
}

/// The value of the language's `sum` from SQL's (as the interpreter adds: from `0`, one
/// currency or unit).
fn lang_sum(a: &AggOut) -> Option<V> {
    if a.rows == 0 {
        return Some(V::Int(0));
    }
    let d = |s: &Option<String>| s.as_deref().and_then(|x| Decimal::from_str(x).ok());
    match a.kind {
        "int" => Some(V::Int(a.value.as_deref()?.parse().ok()?)),
        "num" => Some(V::Dec(d(&a.value)?)),
        "money" if a.units == 1 => Some(V::Money { amt: d(&a.value)?, cur: Rc::from(a.unit.as_deref()?) }),
        "qty" if a.units == 1 => Some(V::Qty { v: d(&a.value)?, unit: Rc::from(a.unit.as_deref()?) }),
        _ => None,
    }
}
