//! Retention at run time: `erase rec, "why"` empties the `personal` fields of a record and
//! its lines; a field whose `expire` came is emptied; a record whose entity's `expire` came
//! disappears, with its lines. Each takes the past values out of the history too: the store
//! notes the erasure (`Store::erasures`) and the commit removes the values from the database,
//! keeping their digests.
//!
//! `retain` holds the other way: a record cannot be erased before its retention ends, nor while
//! a record kept by its own retention refers to it.

use super::{fail, ty_names, ty_names_inner, Interp, R};
use crate::store::Erasure;
use crate::value::{Id, V};
use chrono::NaiveDateTime;
use quadra_ir::*;
use std::rc::Rc;

/// What has come due at a moment: a field to empty, or a whole record to take away.
#[derive(Clone, Debug, PartialEq)]
pub enum Due {
    Field {
        ent: String,
        id: Id,
        field: String,
        term: String,
    },
    Record {
        ent: String,
        id: Id,
        term: String,
    },
}

/// The key of an `expire` term, as the database keeps the terms in force: `Candidate: 6 months
/// after last_contact`, `Candidate.notes: 30 days after created_at`.
pub fn term_key(ent: &str, field: Option<&str>, t: &Term) -> String {
    match field {
        Some(f) => format!("{ent}.{f}: {}", t.text()),
        None => format!("{ent}: {}", t.text()),
    }
}

/// Every `expire` term of a model, by key.
pub fn expire_terms(m: &Model) -> Vec<String> {
    let mut out = Vec::new();
    for e in &m.entities {
        if let Some(t) = &e.retention.expire {
            out.push(term_key(&e.name, None, t));
        }
        for f in &e.fields {
            if let Some(t) = &f.retention.expire {
                out.push(term_key(&e.name, Some(&f.name), t));
            }
        }
    }
    out
}

impl<'m> Interp<'m> {
    /// When a term ends for a record: the value of its field plus the time. None when the
    /// field has no value (the time is not counted).
    pub fn term_end(&mut self, rec: &V, t: &Term) -> R<Option<NaiveDateTime>> {
        let v = self.get(rec, &t.after, None, None)?;
        let from = match v {
            V::Date(d) => d.and_hms_opt(0, 0, 0).unwrap(),
            V::DateTime(dt) => dt,
            _ => return Ok(None),
        };
        Ok(t.after(from))
    }

    /// The records that refer to `id` (of entity `ent`) through a stored field of `of`: the
    /// field holds it, or a list or set of it does.
    fn referrers(&mut self, of: &Entity, field: &Field, id: Id, ent: &str) -> Vec<Id> {
        let target = V::Ref {
            ent: Rc::from(ent),
            id,
        };
        let single = matches!(field.ty, Some(Ty::Path { .. }) | Some(Ty::Optional(_)));
        if single {
            if let Some(ids) = self.store.find(&of.name, &field.name, &target) {
                return ids;
            }
        }
        let mut out = Vec::new();
        for r in self.ids(&of.name, None) {
            let v = self.store.read(r, &field.name, None).unwrap_or(V::None);
            if crate::store::ref_ids(&v).contains(&id) {
                out.push(r);
            }
        }
        out
    }

    /// The fields of the model that refer to entity `ent` (or a trait it implements): (the
    /// entity they are in, the field).
    fn references_to(&self, ent: &str) -> Vec<(&'m Entity, &'m Field)> {
        let m = self.m;
        let names: Vec<String> = std::iter::once(ent.to_string())
            .chain(m.entity(ent).map(|e| e.traits.clone()).unwrap_or_default())
            .collect();
        let mut out = Vec::new();
        for e in &m.entities {
            for f in &e.fields {
                if f.formula.is_some() {
                    continue;
                }
                if ty_names_inner(f.ty.as_ref()).is_some_and(|n| names.contains(&n)) {
                    out.push((e, f));
                }
            }
        }
        out
    }

    /// The end of the latest retention that keeps `id`: its own entity's `retain`, or that of a
    /// record referring to it. None when nothing keeps it now.
    fn kept_until(&mut self, ent: &str, id: Id) -> R<Option<(NaiveDateTime, String)>> {
        let rec = V::Ref {
            ent: Rc::from(ent),
            id,
        };
        let mut latest: Option<(NaiveDateTime, String)> = None;
        let now = self.clock;
        let mut keep = |at: NaiveDateTime, why: String| {
            if at > now && latest.as_ref().map_or(true, |(l, _)| at > *l) {
                latest = Some((at, why));
            }
        };
        if let Some(t) = self.m.entity(ent).and_then(|e| e.retention.retain.clone()) {
            if let Some(at) = self.term_end(&rec, &t)? {
                keep(at, format!("it is kept {}", t.text()));
            }
        }
        for (of, field) in self.references_to(ent) {
            let Some(t) = of.retention.retain.clone() else {
                continue;
            };
            for r in self.referrers(of, field, id, ent) {
                let rv = V::Ref {
                    ent: Rc::from(of.name.as_str()),
                    id: r,
                };
                if let Some(at) = self.term_end(&rv, &t)? {
                    keep(
                        at,
                        format!("{}#{r} refers to it and is kept {}", of.name, t.text()),
                    );
                }
            }
        }
        Ok(latest)
    }

    /// The records that refer to a record, and through which field: (entity, id, field). Its
    /// lines are among them (their `parent` field).
    pub fn referring(&mut self, ent: &str, id: Id) -> Vec<(String, Id, String)> {
        let mut out = Vec::new();
        for (of, field) in self.references_to(ent) {
            for r in self.referrers(of, field, id, ent) {
                out.push((of.name.clone(), r, field.name.clone()));
            }
        }
        out
    }

    /// The lines of a record (records whose `parent` field holds it), and theirs.
    pub fn lines_of(&mut self, ent: &str, id: Id) -> Vec<(String, Id)> {
        let m = self.m;
        let mut out = Vec::new();
        for e in &m.entities {
            for f in e.fields.iter().filter(|f| f.parent) {
                if ty_names(f.ty.as_ref()).as_deref() != Some(ent) {
                    continue;
                }
                for l in self.referrers(e, f, id, ent) {
                    out.push((e.name.clone(), l));
                    out.extend(self.lines_of(&e.name, l));
                }
            }
        }
        out
    }

    /// Empties fields of a record: each written `none` (the formulas reading it follow), and
    /// its past values noted for the commit to take out of the history.
    fn empty_fields(&mut self, ent: &str, id: Id, fields: &[String], reason: &str) -> R<()> {
        if fields.is_empty() {
            return Ok(());
        }
        let e = self.entity(ent)?;
        for name in fields {
            let Some(f) = self.m.find_field(e, name) else {
                continue;
            };
            let empty = match &f.ty {
                Some(Ty::List(_)) => V::List(Rc::new(Vec::new())),
                Some(Ty::Set(_)) => V::Set(Rc::new(Vec::new())),
                _ => V::None,
            };
            let had = self.store.read(id, name, None).unwrap_or(V::None);
            if had != empty {
                self.write_field(ent, id, f, empty, None)?;
            }
        }
        // the values before this transaction disappear from the record's history in memory
        // too: read as of any time, an erased value is none
        let tx = self.store.tx;
        if let Some(r) = self.store.get_mut(id) {
            for (t, k, v) in r.history.iter_mut() {
                if *t < tx && fields.contains(k) {
                    *v = V::None;
                }
            }
        }
        self.store.erasures.push(Erasure {
            id,
            ent: Rc::from(ent),
            fields: fields.to_vec(),
            whole: false,
            reason: reason.to_string(),
            by: self.user,
            before: None,
        });
        Ok(())
    }

    /// `erase rec, "why"`: its `personal` fields and its lines' emptied. Refused while a
    /// retention keeps it.
    pub fn erase(&mut self, rec: &V, reason: &str) -> R<Vec<String>> {
        let V::Ref { id, .. } = rec else {
            return fail(format!("`erase` erases a record, not {rec}"));
        };
        let id = *id;
        let ent = self.ent_of(id).to_string();
        self.check_permission("erase", &ent, None, Some(rec))?;
        if reason.trim().is_empty() {
            return fail("`erase` needs its reason: it is kept in the history");
        }
        if let Some((at, why)) = self.kept_until(&ent, id)? {
            return fail(format!(
                "{ent}#{id} cannot be erased before {}: {why}",
                at.format("%Y-%m-%d")
            ));
        }
        let mut erased = Vec::new();
        let mut targets = vec![(ent.clone(), id)];
        targets.extend(self.lines_of(&ent, id));
        for (te, tid) in targets {
            let e = self.entity(&te)?;
            let now = self.clock;
            let trec = V::Ref {
                ent: Rc::from(te.as_str()),
                id: tid,
            };
            let mut fields = Vec::new();
            for f in e
                .fields
                .iter()
                .filter(|f| f.personal && f.formula.is_none())
            {
                // a field's own retention keeps it until then
                if let Some(t) = &f.retention.retain {
                    if self.term_end(&trec, t)?.is_some_and(|at| at > now) {
                        continue;
                    }
                }
                fields.push(f.name.clone());
            }
            if !fields.is_empty() {
                erased.extend(fields.iter().map(|f| format!("{te}.{f}")));
                self.empty_fields(&te, tid, &fields, reason)?;
            }
        }
        Ok(erased)
    }

    /// What has come due now: fields whose `expire` ended and that still have a value, records
    /// whose entity's `expire` ended (lines go with their document, not on their own).
    pub fn due(&mut self) -> R<Vec<Due>> {
        let m = self.m;
        let now = self.clock;
        let mut out = Vec::new();
        for e in &m.entities {
            if e.kind == EntityKind::Trait {
                continue;
            }
            let fields: Vec<&Field> = e
                .fields
                .iter()
                .filter(|f| f.retention.expire.is_some() && f.formula.is_none())
                .collect();
            // only the terms in force (`retention_armed`): a new one waits for the deploy that
            // says how many records it erases at once
            let armed = |key: String| {
                self.retention_armed
                    .as_ref()
                    .map_or(true, |a| a.contains(&key))
            };
            let whole = e
                .retention
                .expire
                .as_ref()
                .filter(|t| armed(term_key(&e.name, None, t)));
            let fields: Vec<&Field> = fields
                .into_iter()
                .filter(|f| {
                    armed(term_key(
                        &e.name,
                        Some(&f.name),
                        f.retention.expire.as_ref().unwrap(),
                    ))
                })
                .collect();
            if whole.is_none() && fields.is_empty() {
                continue;
            }
            for id in self.ids(&e.name, None) {
                if *self.ent_of(id) != *e.name {
                    continue;
                }
                let rec = V::Ref {
                    ent: Rc::from(e.name.as_str()),
                    id,
                };
                if let Some(t) = whole {
                    if self.term_end(&rec, t)?.is_some_and(|at| at <= now) {
                        out.push(Due::Record {
                            ent: e.name.clone(),
                            id,
                            term: t.text(),
                        });
                        continue;
                    }
                }
                for f in &fields {
                    let t = f.retention.expire.as_ref().unwrap();
                    let v = self.store.read(id, &f.name, None).unwrap_or(V::None);
                    let empty = match &v {
                        V::None => true,
                        V::List(l) | V::Set(l) => l.is_empty(),
                        _ => false,
                    };
                    if !empty && self.term_end(&rec, t)?.is_some_and(|at| at <= now) {
                        out.push(Due::Field {
                            ent: e.name.clone(),
                            id,
                            field: f.name.clone(),
                            term: t.text(),
                        });
                    }
                }
            }
        }
        Ok(out)
    }

    /// Erases what came due, each record in a transaction of its own, as the system, with the
    /// cause `expire: Candidate#12 (6 months after last_contact)`. A record still kept (by a
    /// `retain`, or a required reference of a record that does not expire yet) waits. Returns
    /// what was erased.
    pub fn expire_due(&mut self) -> R<Vec<Due>> {
        let due = self.due()?;
        let mut done = Vec::new();
        for d in due {
            let (ent, id, term) = match &d {
                Due::Field { ent, id, term, .. } | Due::Record { ent, id, term } => {
                    (ent.clone(), *id, term.clone())
                }
            };
            let cause = format!("expire: {ent}#{id} ({term})");
            self.system += 1;
            let r = self.with_cause(cause.clone(), |i| {
                i.transaction(|i| match &d {
                    Due::Field { field, .. } => {
                        i.empty_fields(&ent, id, std::slice::from_ref(field), &cause)?;
                        Ok(true)
                    }
                    Due::Record { .. } => i.remove_record(&ent, id, &cause),
                })
            });
            self.system -= 1;
            if r? {
                done.push(d);
            }
        }
        Ok(done)
    }

    /// `history 1 year`: for each entity that has it, the values replaced before then leave
    /// the history (their digests kept), in a transaction of its own. Returns how many
    /// entities.
    pub fn compact_history(&mut self) -> R<usize> {
        let m = self.m;
        let mut n = 0;
        for e in &m.entities {
            let Some(t) = &e.history else { continue };
            let Some(before) = t.before(self.clock) else {
                continue;
            };
            let reason = format!(
                "history {} {}: the values replaced before {}",
                t.n,
                t.unit,
                before.format("%Y-%m-%d %H:%M")
            );
            let ent = e.name.clone();
            self.system += 1;
            let r = self.with_cause(format!("history: {ent}"), |i| {
                i.transaction(|i| {
                    i.store.erasures.push(Erasure {
                        id: 0,
                        ent: Rc::from(ent.as_str()),
                        fields: Vec::new(),
                        whole: false,
                        reason: reason.clone(),
                        by: None,
                        before: Some(before),
                    });
                    Ok(())
                })
            });
            self.system -= 1;
            r?;
            n += 1;
        }
        Ok(n)
    }

    /// A record whose entity's `expire` ended taken away, with its lines; optional references
    /// to it become none. False (nothing done) while it is kept: a `retain`, or a required
    /// reference from a record that stays.
    fn remove_record(&mut self, ent: &str, id: Id, cause: &str) -> R<bool> {
        if self.kept_until(ent, id)?.is_some() {
            return Ok(false);
        }
        for (of, field) in self.references_to(ent) {
            if field.parent {
                continue;
            }
            let required = !matches!(
                field.ty,
                Some(Ty::Optional(_)) | Some(Ty::List(_)) | Some(Ty::Set(_))
            );
            for r in self.referrers(of, field, id, ent) {
                if required {
                    // its record goes first (it expires too: the checker says so)
                    return Ok(false);
                }
                let v = self.store.read(r, &field.name, None).unwrap_or(V::None);
                let left = match &v {
                    V::List(l) => V::List(Rc::new(
                        l.iter()
                            .filter(|x| x.as_ref_id() != Some(id))
                            .cloned()
                            .collect(),
                    )),
                    V::Set(l) => V::Set(Rc::new(
                        l.iter()
                            .filter(|x| x.as_ref_id() != Some(id))
                            .cloned()
                            .collect(),
                    )),
                    _ => V::None,
                };
                let oe = self.ent_of(r).to_string();
                self.write_field(&oe, r, field, left, None)?;
            }
        }
        let mut gone = self.lines_of(ent, id);
        gone.push((ent.to_string(), id));
        for (ge, gid) in gone {
            let e = self.entity(&ge)?;
            let stored: Vec<String> = e
                .fields
                .iter()
                .filter(|f| f.formula.is_none())
                .map(|f| f.name.clone())
                .collect();
            // each value written none first: the indexes of the references forget it
            for f in &stored {
                if self.store.read(gid, f, None).is_some_and(|v| !v.is_none()) {
                    self.store.write(gid, f, V::None);
                }
            }
            self.store.remove_record(gid);
            self.store.erasures.push(Erasure {
                id: gid,
                ent: Rc::from(ge.as_str()),
                fields: stored,
                whole: true,
                reason: cause.to_string(),
                by: None,
                before: None,
            });
        }
        Ok(true)
    }
}
