//! Expressions written by a user and evaluated as that user (`Interp::viewer`): the formulas
//! of an app's cells. They see exactly what the grid would show the user:
//!
//! - a query over an entity (`Order.where(…).sum(…)`, `Customer.count()`) keeps the rows the
//!   user may read: in SQL with the row rules (as `POST /v1/query`), or in the interpreter by
//!   checking each row;
//! - reading a field hidden from the user (a `hide` rule), in the expression or in a query's
//!   lambdas, fails; so does reading a field of a record the user may not read, outside the
//!   lambdas: inside them (`o => o.customer.name`) references are read as the database's
//!   queries read them (the filters of `POST /v1/query`), the same with or without SQL — the
//!   rows of a query are those the user may read;
//! - the interpreter reads at most `ENGINE_ROWS` rows of an entity one by one (what SQL cannot
//!   answer): beyond, the formula fails with what to do, rather than taking minutes.
//!
//! The model's own code reached from the expression (a record's formula, a permission rule)
//! runs as it always does (`viewer_pause`): a formula the user may see shows its value. Code
//! running as the system (`system > 0`) is not checked either.

use super::*;

/// At most this many rows of an entity are read one by one by the interpreter for a user's
/// expression.
pub const ENGINE_ROWS: usize = 10_000;

impl<'m> Interp<'m> {
    /// Whether the expression being evaluated is the user's (checked as the user).
    pub fn viewing(&self) -> bool {
        self.viewer && self.viewer_pause == 0 && self.system == 0 && self.user.is_some()
    }

    /// Reading `name` of a record in a user's expression: the record must be readable by the
    /// user and the field not hidden from them.
    pub(super) fn viewer_read(&mut self, ent: &str, id: Id, name: &str) -> R<()> {
        let actual = self.ent_of(id);
        let ent = if actual.is_empty() { ent.to_string() } else { actual.to_string() };
        if self.m.entity(&ent).map_or(true, |e| e.kind != EntityKind::Entity) {
            return Ok(());
        }
        if self.field(&ent, name).is_some() && self.hidden(&ent, name) {
            return fail(format!("`{ent}.{name}` is hidden from you"));
        }
        // in a lambda: as the database reads through references
        if self.viewer_in_query > 0 {
            return Ok(());
        }
        let ok = match self.viewer_seen.get(&id) {
            Some(b) => *b,
            None => {
                let b = self.can("read", &ent, None, Some(&V::Ref { ent: Rc::from(ent.as_str()), id }));
                self.viewer_seen.insert(id, b);
                b
            }
        };
        if !ok {
            return fail(format!("you may not read {ent} #{id}"));
        }
        Ok(())
    }

    /// The rows of an entity the interpreter is about to go through for a user's expression:
    /// at most `ENGINE_ROWS`, and only those the user may read.
    pub(super) fn viewer_rows(&mut self, ent: &str, ids: Vec<Id>) -> R<Vec<Id>> {
        if ids.len() > ENGINE_ROWS {
            return fail(format!("{ent}: {} rows to read one by one (this part of the formula is not answered by the database); at most {ENGINE_ROWS}: filter with .where(…) first", ids.len()));
        }
        if self.read_unconditional(ent) {
            return Ok(ids);
        }
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let ok = match self.viewer_seen.get(&id) {
                Some(b) => *b,
                None => {
                    let e = self.ent_of(id);
                    let b = self.can("read", &e, None, Some(&V::Ref { ent: e.clone(), id }));
                    self.viewer_seen.insert(id, b);
                    b
                }
            };
            if ok {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// The fields a query's lambda reads (`o => o.customer.credit_limit > 0`), checked before
    /// the database answers it: one hidden from the user fails, as reading it would.
    pub(super) fn viewer_lambda(&mut self, ent: &str, param: &str, body: &Expr) -> R<()> {
        let mut scope: Vec<(String, String)> = vec![(param.to_string(), ent.to_string())];
        self.viewer_walk(body, &mut scope)
    }

    fn viewer_walk(&mut self, e: &Expr, scope: &mut Vec<(String, String)>) -> R<()> {
        match &e.kind {
            ExprKind::Field { base, name, .. } => {
                if let Some(owner) = self.viewer_entity(base, scope) {
                    if self.field(&owner, name).is_some() && self.hidden(&owner, name) {
                        return fail(format!("`{owner}.{name}` is hidden from you"));
                    }
                }
                self.viewer_walk(base, scope)
            }
            // `o.lines.sum(l => l.qty)`: the lambda's parameter is a row of the collection
            ExprKind::Call { callee, args } => {
                self.viewer_walk(callee, scope)?;
                let elem = match &callee.kind {
                    ExprKind::Field { base, .. } => self.viewer_collection(base, scope),
                    _ => None,
                };
                for a in args {
                    match (&a.value.kind, &elem) {
                        (ExprKind::Lambda { params, body }, Some(el)) if params.len() == 1 => {
                            scope.push((params[0].clone(), el.clone()));
                            let r = self.viewer_walk(body, scope);
                            scope.pop();
                            r?;
                        }
                        _ => self.viewer_walk(&a.value, scope)?,
                    }
                }
                Ok(())
            }
            _ => {
                let mut kids = Vec::new();
                e.for_each_child(&mut |c| kids.push(c.clone()));
                for k in &kids {
                    self.viewer_walk(k, scope)?;
                }
                Ok(())
            }
        }
    }

    /// The entity of the record an expression is, when it is a path from a lambda parameter.
    fn viewer_entity(&self, e: &Expr, scope: &[(String, String)]) -> Option<String> {
        match &e.kind {
            ExprKind::Name(n) => scope.iter().rev().find(|(p, _)| p == n).map(|(_, e)| e.clone()),
            ExprKind::Field { base, name, .. } => {
                let owner = self.viewer_entity(base, scope)?;
                let f = self.field(&owner, name)?;
                ty_names(f.ty.as_ref()).filter(|t| self.m.entity(t).is_some())
            }
            _ => None,
        }
    }

    /// The entity of the records of a collection field reached from a lambda parameter.
    fn viewer_collection(&self, e: &Expr, scope: &[(String, String)]) -> Option<String> {
        match &e.kind {
            ExprKind::Name(n) if self.m.entity(n).is_some() && !scope.iter().any(|(p, _)| p == n) => Some(n.clone()),
            ExprKind::Field { base, name, .. } => {
                let owner = self.viewer_entity(base, scope)?;
                let f = self.field(&owner, name)?;
                ty_names_inner(f.ty.as_ref()).filter(|t| self.m.entity(t).is_some())
            }
            _ => None,
        }
    }
}
