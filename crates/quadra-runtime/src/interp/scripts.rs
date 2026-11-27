//! `migration` and `change` blocks.
//!
//! A migration runs inside the schema migration transaction: `rename` records a mapping
//! (the history is never rewritten: entries written before the rename are read under the new
//! name), `convert` maps every stored value with match arms, `backfill` runs code, `verify`
//! must hold. A change is a data script with a reason, a requester and an approval; it can be
//! previewed, applied once, and reverted by a new transaction when nothing changed since.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The `reason:`, `requested_by:`, `approved_by: role X`, `effective:` lines of a script.
#[derive(Debug, Default, Clone)]
pub struct ScriptMeta {
    pub reason: Option<String>,
    pub requested_by: Option<String>,
    pub approved_by_role: Option<String>,
    pub effective: Option<NaiveDate>,
}

/// What a transaction changed compared to the store it started from.
#[derive(Debug, Default, Clone)]
pub struct ChangeDiff {
    /// (entity, id, field, before, after) for records that existed before
    pub changed: Vec<(String, Id, String, V, V)>,
    /// new records per entity
    pub created: BTreeMap<String, usize>,
    pub ledger_rows: usize,
    /// records touched per entity (created or changed)
    pub affected: BTreeMap<String, usize>,
}

impl<'m> Interp<'m> {
    pub fn script_meta(&mut self, b: &Block) -> ScriptMeta {
        let mut meta = ScriptMeta::default();
        for s in &b.body {
            let StmtKind::Meta { key, value } = &s.kind else {
                continue;
            };
            match key.as_str() {
                "reason" | "requested_by" => {
                    let v = self
                        .eval(value, &mut Ctx::default())
                        .map(|v| v.to_string())
                        .unwrap_or_else(|_| print::expr(value));
                    if key == "reason" {
                        meta.reason = Some(v);
                    } else {
                        meta.requested_by = Some(v);
                    }
                }
                "approved_by" => {
                    let p = print::expr(value);
                    meta.approved_by_role = Some(p.trim_start_matches("role ").trim().to_string());
                }
                "effective" => {
                    if let Ok(V::Date(d)) = self.eval(value, &mut Ctx::default()) {
                        meta.effective = Some(d);
                    }
                }
                _ => {}
            }
        }
        meta
    }

    /// Run a migration inside the current transaction; returns a report, one line per step.
    pub fn run_migration(&mut self, b: &Block, root: Option<&Path>) -> R<Vec<String>> {
        self.system += 1;
        self.script += 1;
        let temps = self.load_temporaries(root);
        let mut report = Vec::new();
        let r = match temps {
            Ok(_) => {
                let mut ctx = Ctx::default();
                ctx.scopes.push(Default::default());
                self.migration_steps(&b.body, &mut ctx, &mut report)
            }
            Err(e) => Err(e),
        };
        // temporary records disappear before the commit
        let temp_ents: Vec<String> = self
            .m
            .entities
            .iter()
            .filter(|e| e.temporary)
            .map(|e| e.name.clone())
            .collect();
        for ent in temp_ents {
            for id in self.store.by_entity.remove(&ent).unwrap_or_default() {
                self.store.records.remove(&id);
            }
        }
        self.system -= 1;
        self.script -= 1;
        r?;
        self.store
            .applied
            .insert(format!("migration:{}", b.name), self.store.tx);
        Ok(report)
    }

    fn migration_steps(&mut self, body: &[Stmt], ctx: &mut Ctx, report: &mut Vec<String>) -> R<()> {
        for s in body {
            match &s.kind {
                StmtKind::Meta { .. } => {}
                StmtKind::Migration {
                    keyword,
                    paths,
                    body,
                    arms,
                } => match keyword.as_str() {
                    "rename" => {
                        let (Some(from), Some(to)) = (paths.first(), paths.get(1)) else {
                            return fail("rename needs `Entity.old -> Entity.new`");
                        };
                        let (ent, old, new) = (
                            from[0].clone(),
                            from[from.len() - 1].clone(),
                            to[to.len() - 1].clone(),
                        );
                        let n = if self.store.lazy() {
                            self.rename_field_lazy(&ent, &old, &new)
                        } else {
                            self.rename_field(&ent, &old, &new)
                        };
                        report.push(format!("rename {ent}.{old} → {ent}.{new}: {n} values (the history keeps the old name)"));
                    }
                    "convert" => {
                        let Some(p) = paths.first().filter(|p| p.len() == 2) else {
                            return fail("convert needs `Entity.field`");
                        };
                        let n = self.convert_field(&p[0], &p[1], arms)?;
                        report.push(format!("convert {}.{}: {n} values", p[0], p[1]));
                    }
                    "backfill" => {
                        let Some(p) = paths.first().filter(|p| p.len() == 2) else {
                            return fail("backfill needs `Entity.field`");
                        };
                        self.stmts(body, ctx)?;
                        let tx = self.store.tx;
                        let (mut n, mut prov) = (0, 0);
                        for id in self.ids(&p[0], None) {
                            let Some(r) = self.store.records.get(&id) else {
                                continue;
                            };
                            if r.history.iter().any(|(t, f, _)| *t == tx && f == &p[1]) {
                                n += 1;
                            }
                            if r.fields
                                .get(&format!("{}#provisional", p[1]))
                                .map_or(false, |v| !v.is_none())
                            {
                                prov += 1;
                            }
                        }
                        report.push(if prov > 0 {
                            format!("backfill {}.{}: {n} values, {prov} provisional", p[0], p[1])
                        } else {
                            format!("backfill {}.{}: {n} values", p[0], p[1])
                        });
                    }
                    other => return fail(format!("unknown migration step `{other}`")),
                },
                StmtKind::Block { keyword, body } => {
                    self.script_checks(keyword, body, ctx)?;
                    report.push(format!("{keyword}: ok"));
                }
                _ => self.stmt(s, ctx)?,
            }
        }
        Ok(())
    }

    /// `verify:` / `expect:` blocks: every expression must hold.
    fn script_checks(&mut self, keyword: &str, body: &[Stmt], ctx: &mut Ctx) -> R<()> {
        for s in body {
            match &s.kind {
                StmtKind::Expr(e) => {
                    if !self.eval(e, ctx)?.truthy() {
                        return fail(format!("{keyword} failed: {}", print::expr(e)));
                    }
                }
                _ => self.stmt(s, ctx)?,
            }
        }
        Ok(())
    }

    /// Move `old` to `new` in every record of `ent` (values, per-company values, markers).
    /// In memory the history is renamed too; in the database it stays as written and the
    /// rename is applied when loading.
    pub fn rename_field(&mut self, ent: &str, old: &str, new: &str) -> usize {
        let renamed = |k: &str| -> Option<String> {
            if k == old {
                Some(new.to_string())
            } else if k.starts_with(&format!("{old}@")) || k.starts_with(&format!("{old}#")) {
                Some(format!("{new}{}", &k[old.len()..]))
            } else {
                None
            }
        };
        let mut n = 0;
        let ids = self.ids(ent, None);
        for name in ids
            .iter()
            .filter_map(|id| self.store.records.get(id).map(|r| r.ent.to_string()))
            .collect::<BTreeSet<String>>()
        {
            self.store.forget_indexes(&name);
        }
        for id in ids {
            let Some(r) = self.store.records.get_mut(&id) else {
                continue;
            };
            let keys: Vec<String> = r
                .fields
                .keys()
                .filter(|k| renamed(k).is_some())
                .cloned()
                .collect();
            for k in keys {
                let v = r.fields.remove(&k).unwrap();
                r.fields.insert(renamed(&k).unwrap(), v);
                n += 1;
            }
            for h in r.history.iter_mut() {
                if let Some(nk) = renamed(&h.1) {
                    h.1 = nk;
                }
            }
        }
        self.store.renames.push((
            ent.to_string(),
            old.to_string(),
            new.to_string(),
            self.store.tx,
        ));
        n
    }

    /// `rename` with storage on demand: the records in the working set are renamed here, the
    /// source applies the rename to the records it loads from now on (the history keeps the
    /// old name) and counts the committed values it moves.
    fn rename_field_lazy(&mut self, ent: &str, old: &str, new: &str) -> usize {
        let renamed = |k: &str| -> Option<String> {
            if k == old {
                Some(new.to_string())
            } else if k.starts_with(&format!("{old}@")) || k.starts_with(&format!("{old}#")) {
                Some(format!("{new}{}", &k[old.len()..]))
            } else {
                None
            }
        };
        let names: Vec<String> = match self.m.entity(ent) {
            Some(e) if e.kind == EntityKind::Trait => self
                .m
                .implementors(ent)
                .iter()
                .map(|e| e.name.clone())
                .collect(),
            _ => vec![ent.to_string()],
        };
        let tx = self.store.tx;
        let base = self.store.base;
        let mut n = 0;
        for name in &names {
            if let Some(src) = self.store.src.clone() {
                n += src.borrow_mut().rename(name, old, new, tx);
            }
            for id in self.store.known_ids(name) {
                let Some(r) = self.store.records.get_mut(&id) else {
                    continue;
                };
                let keys: Vec<String> = r
                    .fields
                    .keys()
                    .filter(|k| renamed(k).is_some())
                    .cloned()
                    .collect();
                // the values of records created here are not in the database
                if r.created_tx > base {
                    n += keys.len();
                }
                for k in keys {
                    let v = r.fields.remove(&k).unwrap();
                    r.fields.insert(renamed(&k).unwrap(), v);
                }
                for h in r.history.iter_mut() {
                    if let Some(nk) = renamed(&h.1) {
                        h.1 = nk;
                    }
                }
            }
            self.store.renamed_field(name, old, new);
        }
        self.store
            .renames
            .push((ent.to_string(), old.to_string(), new.to_string(), tx));
        n
    }

    fn convert_field(&mut self, ent: &str, field: &str, arms: &[Arm]) -> R<usize> {
        let e = self.entity(ent)?;
        let Some(f) = self.m.find_field(e, field).cloned() else {
            return fail(format!("`{ent}` has no field `{field}`"));
        };
        let mut n = 0;
        let mut errors = Vec::new();
        for id in self.ids(ent, None) {
            let Some(v) = self.store.read(id, field, None).filter(|v| !v.is_none()) else {
                continue;
            };
            let mut ctx = Ctx::default();
            let mut out = None;
            'arms: for arm in arms {
                for p in &arm.patterns {
                    let mut binds = Vec::new();
                    if self.pattern(p, &v, &mut binds) {
                        ctx.scopes.push(binds.into_iter().collect());
                        out = Some(self.eval(&arm.body, &mut ctx));
                        ctx.scopes.pop();
                        break 'arms;
                    }
                }
            }
            match out {
                None => errors.push(format!("{ent}#{id}: no arm for {v}")),
                Some(Err(Flow::Fail(m))) => errors.push(format!("{ent}#{id}: {m}")),
                Some(Err(other)) => return Err(other),
                Some(Ok(nv)) => {
                    if nv != v {
                        self.write_field(ent, id, &f, nv, None)?;
                        n += 1;
                    }
                }
            }
        }
        if !errors.is_empty() {
            let more = if errors.len() > 20 {
                format!("\n  … and {} more", errors.len() - 20)
            } else {
                String::new()
            };
            return fail(format!(
                "convert {ent}.{field}: {} values cannot be converted:\n  {}{more}",
                errors.len(),
                errors.into_iter().take(20).collect::<Vec<_>>().join("\n  ")
            ));
        }
        Ok(n)
    }

    /// Records of `temporary` entities with `load from "file.csv"`, created for the migration.
    fn load_temporaries(&mut self, root: Option<&Path>) -> R<()> {
        let temps: Vec<(String, String)> = self
            .m
            .entities
            .iter()
            .filter(|e| e.temporary)
            .filter_map(|e| e.load_from.clone().map(|f| (e.name.clone(), f)))
            .collect();
        for (ent, file) in temps {
            // a file of the project only: a relative path inside its folder, not a link (a deploy
            // must not read the files of the machine it runs on)
            let rel = std::path::Path::new(&file);
            if rel.is_absolute()
                || rel.components().any(|c| {
                    !matches!(
                        c,
                        std::path::Component::Normal(_) | std::path::Component::CurDir
                    )
                })
            {
                return Err(Flow::Fail(format!("{ent}: `load from \"{file}\"`: a file of the project, by a path inside its folder")));
            }
            let path = root
                .map(|r| r.join(&file))
                .unwrap_or_else(|| file.clone().into());
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(Flow::Fail(format!(
                    "{ent}: `load from \"{file}\"` is a link: give the file itself"
                )));
            }
            if let (Some(r), Ok(real)) = (root, path.canonicalize()) {
                if r.canonicalize().is_ok_and(|r| !real.starts_with(r)) {
                    return Err(Flow::Fail(format!(
                        "{ent}: `load from \"{file}\"` is outside the project"
                    )));
                }
            }
            let text = std::fs::read_to_string(&path)
                .map_err(|e| Flow::Fail(format!("{ent}: cannot read {}: {e}", path.display())))?;
            let rows = parse_csv(&text);
            let Some((header, data)) = rows.split_first() else {
                continue;
            };
            for (i, row) in data.iter().enumerate() {
                let mut args = Vec::new();
                for (col, cell) in header.iter().zip(row.iter()) {
                    let v = self.csv_value(&ent, col, cell).map_err(|e| match e {
                        Flow::Fail(m) => {
                            Flow::Fail(format!("{} line {}: {m}", path.display(), i + 2))
                        }
                        other => other,
                    })?;
                    args.push((col.clone(), v));
                }
                self.create(&ent, args)?;
            }
        }
        Ok(())
    }

    fn csv_value(&mut self, ent: &str, col: &str, cell: &str) -> R<V> {
        if cell.is_empty() {
            return Ok(V::None);
        }
        let e = self.entity(ent)?;
        let Some(f) = self.m.find_field(e, col).cloned() else {
            return fail(format!("`{ent}` has no field `{col}`"));
        };
        let ty = match &f.ty {
            Some(Ty::Optional(t)) => Some((**t).clone()),
            t => t.clone(),
        };
        let name = ty_names(ty.as_ref()).unwrap_or_default();
        if let Some(target) = self
            .m
            .entity(&name)
            .filter(|t| t.kind != EntityKind::Record)
        {
            // a reference: the record whose unique (or any) text field equals the cell
            let target = target.name.clone();
            let te = self.entity(&target)?;
            let mut keys: Vec<String> = te
                .fields
                .iter()
                .filter(|f| f.unique && f.formula.is_none())
                .map(|f| f.name.clone())
                .collect();
            keys.extend(
                te.fields
                    .iter()
                    .filter(|f| f.formula.is_none() && !f.unique)
                    .map(|f| f.name.clone()),
            );
            for k in keys {
                for id in self.ids(&target, None) {
                    if self
                        .store
                        .read(id, &k, None)
                        .map_or(false, |v| v.to_string() == cell)
                    {
                        return Ok(V::Ref {
                            ent: Rc::from(target.as_str()),
                            id,
                        });
                    }
                }
            }
            return fail(format!("no {target} matches `{cell}`"));
        }
        Ok(match name.as_str() {
            "Int" => V::Int(
                cell.parse()
                    .map_err(|_| Flow::Fail(format!("`{cell}` is not an integer")))?,
            ),
            "Decimal" => V::Dec(
                Decimal::from_str(cell)
                    .map_err(|_| Flow::Fail(format!("`{cell}` is not a number")))?,
            ),
            "Bool" => V::Bool(matches!(cell, "true" | "yes" | "1")),
            "Date" => V::Date(
                NaiveDate::from_str(cell)
                    .map_err(|_| Flow::Fail(format!("`{cell}` is not a date (YYYY-MM-DD)")))?,
            ),
            _ => {
                if self.m.enum_def(&name).is_some() {
                    V::Variant {
                        name: Rc::from(cell),
                        fields: Rc::new(Vec::new()),
                    }
                } else {
                    text(cell)
                }
            }
        })
    }

    /// Run a change inside the current transaction (permissions of the current user apply).
    pub fn run_change(&mut self, b: &Block) -> R<()> {
        let mut ctx = Ctx::default();
        ctx.scopes.push(Default::default());
        // a change is a data fix: it may set the field of a flow (`o.state = Draft`)
        self.script += 1;
        let r = b.body.iter().try_for_each(|s| match &s.kind {
            StmtKind::Meta { .. } => Ok(()),
            StmtKind::Block { keyword, body } => self.script_checks(keyword, body, &mut ctx),
            _ => self.stmt(s, &mut ctx),
        });
        self.script -= 1;
        r?;
        self.store
            .applied
            .insert(format!("change:{}", b.name), self.store.tx);
        Ok(())
    }

    /// Undo the writes of an applied change with a new transaction. Refused when the change
    /// created records or posted to ledgers, or when a value changed again afterwards.
    pub fn revert_change(&mut self, name: &str) -> R<usize> {
        let Some(&tx) = self.store.applied.get(&format!("change:{name}")) else {
            return fail(format!("change `{name}` has not been applied"));
        };
        if self.store.applied.contains_key(&format!("revert:{name}")) {
            return fail(format!("change `{name}` has already been reverted"));
        }
        let mut writes = Vec::new();
        let mut conflicts = Vec::new();
        for r in self.store.sorted() {
            if r.created_tx == tx {
                return fail(format!("change `{name}` created records ({}#{}): it cannot be reverted automatically; write a change that undoes it", r.ent, r.id));
            }
            for (t, f, v) in &r.history {
                if *t != tx {
                    continue;
                }
                if r.fields.get(f) != Some(v) {
                    conflicts.push(format!(
                        "{}#{}.{f} changed again after the change",
                        r.ent, r.id
                    ));
                    continue;
                }
                let prev = r
                    .history
                    .iter()
                    .rev()
                    .find(|(t2, f2, _)| *t2 < tx && f2 == f)
                    .map(|(_, _, v)| v.clone())
                    .unwrap_or(V::None);
                writes.push((r.id, f.clone(), prev));
            }
        }
        if self.store.rows.iter().any(|r| r.tx == tx) {
            return fail(format!("change `{name}` posted to ledgers: it cannot be reverted automatically; write a change that undoes it"));
        }
        if !conflicts.is_empty() {
            return fail(format!(
                "change `{name}` cannot be reverted:\n  {}",
                conflicts.join("\n  ")
            ));
        }
        writes.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
        let n = writes.len();
        for (id, key, v) in writes {
            self.store.write(id, &key, v);
            if let Some(r) = self.store.records.get_mut(&id) {
                r.cause = self.cause.clone();
            }
        }
        self.store
            .applied
            .insert(format!("revert:{name}"), self.store.tx);
        Ok(n)
    }

    /// What the current transaction changed compared to `before`.
    pub fn diff_since(&self, before: &Store) -> ChangeDiff {
        let tx = self.store.tx;
        let mut d = ChangeDiff::default();
        for r in self.store.sorted() {
            let touched = r.created_tx == tx || r.history.iter().any(|(t, _, _)| *t == tx);
            if !touched {
                continue;
            }
            *d.affected.entry(r.ent.to_string()).or_default() += 1;
            match before.records.get(&r.id) {
                None => *d.created.entry(r.ent.to_string()).or_default() += 1,
                Some(b) => {
                    let mut keys: Vec<&String> = r
                        .history
                        .iter()
                        .filter(|(t, _, _)| *t == tx)
                        .map(|(_, f, _)| f)
                        .collect();
                    keys.sort();
                    keys.dedup();
                    for k in keys {
                        let old = b.fields.get(k).cloned().unwrap_or(V::None);
                        let new = r.fields.get(k).cloned().unwrap_or(V::None);
                        if old != new {
                            d.changed
                                .push((r.ent.to_string(), r.id, k.clone(), old, new));
                        }
                    }
                }
            }
        }
        d.ledger_rows = self.store.rows.iter().filter(|r| r.tx == tx).count();
        d
    }

    /// Records touched in the current transaction for `affected(Entity)`.
    pub(super) fn affected_count(&mut self, ent: &str) -> usize {
        let tx = self.store.tx;
        let ids = self.ids(ent, None);
        ids.into_iter()
            .filter(|id| {
                self.store.records.get(id).map_or(false, |r| {
                    r.created_tx == tx || r.history.iter().any(|(t, _, _)| *t == tx)
                })
            })
            .count()
    }

    /// Provisional values that expired, on records written in the current transaction.
    pub fn check_provisional(&self) -> R<()> {
        let tx = self.store.tx;
        let today = self.clock.date();
        for r in self.store.sorted() {
            if !r
                .history
                .iter()
                .any(|(t, f, _)| *t == tx && !f.ends_with("#provisional"))
            {
                continue;
            }
            for (k, v) in &r.fields {
                let Some(field) = k.strip_suffix("#provisional") else {
                    continue;
                };
                if let Some((reason, until)) = provisional_marker(v) {
                    if until.map_or(false, |u| u < today) {
                        return fail(format!("{}#{}.{field} is provisional ({reason}) and expired on {}: give it a real value", r.ent, r.id, until.unwrap()));
                    }
                }
            }
        }
        Ok(())
    }

    /// Every provisional value: (entity, id, field, reason, until).
    pub fn provisional_values(&self) -> Vec<(String, Id, String, String, Option<NaiveDate>)> {
        let mut out = Vec::new();
        for r in self.store.sorted() {
            for (k, v) in &r.fields {
                if let (Some(field), Some((reason, until))) =
                    (k.strip_suffix("#provisional"), provisional_marker(v))
                {
                    out.push((r.ent.to_string(), r.id, field.to_string(), reason, until));
                }
            }
        }
        out.sort_by(|a, b| {
            (a.0.as_str(), a.1, a.2.as_str()).cmp(&(b.0.as_str(), b.1, b.2.as_str()))
        });
        out
    }
}

/// `provisional(value, reason=…, until=…)` stores this marker next to the value.
pub(super) fn provisional_marker(v: &V) -> Option<(String, Option<NaiveDate>)> {
    let V::Tuple(items) = v else { return None };
    let get = |n: &str| {
        items
            .iter()
            .find(|(k, _)| k.as_deref() == Some(n))
            .map(|(_, v)| v.clone())
    };
    let reason = get("reason").map(|v| v.to_string()).unwrap_or_default();
    let until = match get("until") {
        Some(V::Date(d)) => Some(d),
        _ => None,
    };
    Some((reason, until))
}

/// Minimal CSV: comma separated, `"…"` quoting with `""` escapes, first line is the header.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let mut cells = Vec::new();
        let mut cur = String::new();
        let mut quoted = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match (c, quoted) {
                ('"', true) if chars.peek() == Some(&'"') => {
                    cur.push('"');
                    chars.next();
                }
                ('"', _) => quoted = !quoted,
                (',', false) => cells.push(std::mem::take(&mut cur).trim().to_string()),
                (c, _) => cur.push(c),
            }
        }
        cells.push(cur.trim().to_string());
        rows.push(cells);
    }
    rows
}
