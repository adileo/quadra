//! Retention: `retain` and `expire` on fields and entities, `personal` fields, `history`, and
//! `erase`. What is checked is what keeps an erasure from changing a number or a document that
//! must stay: a value that disappears is optional, a record kept for years does not show live
//! a value that disappears before it, and a ledger never reads one.

use super::Checker;
use crate::ty::T;
use quadra_ir::*;

/// The implicit fields a time can be counted from.
const IMPLICIT_TIMES: &[&str] = &["created_at", "updated_at"];

fn is_optional(t: &Ty) -> bool {
    matches!(t, Ty::Optional(_) | Ty::List(_) | Ty::Set(_))
}

fn is_time(t: &Ty) -> bool {
    match t {
        Ty::Optional(i) => is_time(i),
        Ty::Path { segments, .. } => {
            matches!(
                segments.last().map(String::as_str),
                Some("Date" | "DateTime")
            )
        }
        _ => false,
    }
}

/// Days a term lasts at least, to compare two terms counted from the same field.
fn days(t: &Term) -> i64 {
    t.n * match t.unit() {
        Some("hours") => return t.n / 24,
        Some("days") => 1,
        Some("weeks") => 7,
        Some("months") => 28,
        _ => 365,
    }
}

impl<'m> Checker<'m> {
    /// The terms, the fields they count from, and what an erasure would break.
    pub(super) fn check_retention(&mut self) {
        let m = self.m;
        for e in &m.entities {
            self.context = format!("entity {}", e.name);
            for t in [&e.retention.retain, &e.retention.expire, &e.history]
                .into_iter()
                .flatten()
            {
                self.check_term(e, t);
            }
            self.check_pair(&e.retention, &e.name);
            if let Some(h) = &e.history {
                if m.read_as_of().contains(&e.name) {
                    self.error(
                        h.span,
                        format!(
                            "{} is read by frozen records, which read it as it was when they froze: \
                             its history is kept whole (`history` drops old values)",
                            e.name
                        ),
                    );
                }
            }
            if let Some(x) = &e.retention.expire {
                if let Some(p) = e.posts.first() {
                    self.error(
                        x.span,
                        format!(
                            "{} posts to {}: its ledger rows are kept for ever, so its records \
                             cannot expire (`retain` keeps them at least as long as the law \
                             asks)",
                            e.name, p.ledger
                        ),
                    );
                }
            }
            for f in &e.fields {
                for t in [&f.retention.retain, &f.retention.expire]
                    .into_iter()
                    .flatten()
                {
                    self.check_term(e, t);
                }
                self.check_pair(&f.retention, &format!("{}.{}", e.name, f.name));
                let stored = f.formula.is_none();
                if !f.retention.is_empty() && !stored {
                    self.error(
                        f.span,
                        format!(
                            "`{}.{}` is a formula: it is computed, not kept; `retain` and \
                             `expire` go on stored fields",
                            e.name, f.name
                        ),
                    );
                    continue;
                }
                let erasable = f.personal || f.retention.expire.is_some();
                if erasable && stored && f.ty.as_ref().is_some_and(|t| !is_optional(t)) {
                    let why = if f.personal {
                        "is personal: `erase` empties it"
                    } else {
                        "expires: it is emptied then"
                    };
                    self.error(
                        f.span,
                        format!(
                            "`{}.{}` {why}, so it must be optional (`{}: {}?`)",
                            e.name,
                            f.name,
                            f.name,
                            f.ty.as_ref().map(ty_text).unwrap_or_default()
                        ),
                    );
                }
                // a required reference to records that expire: they would leave it empty
                if f.parent || f.formula.is_some() {
                    continue;
                }
                let Some(Ty::Path { segments, .. }) = &f.ty else {
                    continue;
                };
                let Some(target) = segments.last().and_then(|n| m.entity(n)) else {
                    continue;
                };
                if let Some(x) = &target.retention.expire {
                    if e.retention.expire.is_none() {
                        self.error(
                            f.span,
                            format!(
                                "`{}.{}` is required, and {} records expire ({}): {} must expire \
                                 too, or the reference be optional (`{}: {}?`)",
                                e.name,
                                f.name,
                                target.name,
                                x.text(),
                                e.name,
                                f.name,
                                target.name
                            ),
                        );
                    }
                }
            }
        }
        self.context = String::new();
    }

    fn check_term(&mut self, e: &Entity, t: &Term) {
        if t.unit().is_none() {
            self.error(
                t.span,
                format!(
                    "`{}` is not a unit of time here: hours, days, weeks, months or years",
                    t.unit
                ),
            );
        }
        if t.n <= 0 {
            self.error(t.span, "the time must be a positive number");
        }
        // `history 1 year` counts from when a value was replaced, not from a field
        if t.after.is_empty() {
            return;
        }
        if IMPLICIT_TIMES.contains(&t.after.as_str()) {
            return;
        }
        match self.m.find_field(e, &t.after) {
            Some(f) if f.formula.is_none() && f.ty.as_ref().is_some_and(is_time) => {}
            Some(_) => self.error(
                t.span,
                format!(
                    "`{}.{}` is not a stored `Date` or `DateTime`: the time is counted from one \
                     (or from `created_at`, `updated_at`)",
                    e.name, t.after
                ),
            ),
            None => self.error(
                t.span,
                format!(
                    "{} has no field `{}` to count the time from (`created_at`, `updated_at`, or \
                     a `Date` field)",
                    e.name, t.after
                ),
            ),
        }
    }

    /// `expire` shorter than `retain`, counted from the same field: the law's minimum would
    /// never be kept.
    fn check_pair(&mut self, r: &Retention, what: &str) {
        let (Some(keep), Some(x)) = (&r.retain, &r.expire) else {
            return;
        };
        if keep.after == x.after && days(x) < days(keep) {
            self.error(
                x.span,
                format!(
                    "{what} expires ({}) before the end of its retention ({}): it must be kept \
                     at least as long",
                    x.text(),
                    keep.text()
                ),
            );
        }
    }

    /// A field read in a formula of a record kept for years (`retain`), or in a ledger's
    /// posting: refused when it can disappear before.
    pub(super) fn retention_access(&mut self, base: &T, name: &str, span: quadra_ir::Span) {
        let T::Ent(en) = base else { return };
        let Some(target) = self.m.entity(en) else {
            return;
        };
        let Some(f) = self.m.find_field(target, name) else {
            return;
        };
        let erasable = if f.personal {
            Some("is personal: `erase` empties it".to_string())
        } else {
            f.retention
                .expire
                .as_ref()
                .map(|x| format!("expires ({})", x.text()))
        };
        let Some(why) = erasable else { return };
        if let Some(ledger) = self.in_post.clone() {
            self.error(
                span,
                format!(
                    "`{}.{name}` {why}: a ledger keeps its rows for ever, so {ledger}'s rows \
                     cannot read it",
                    target.name
                ),
            );
            return;
        }
        if !self.in_formula {
            return;
        }
        let Some((owner, field)) = self.current.clone() else {
            return;
        };
        let Some(oe) = self.m.entity(&owner) else {
            return;
        };
        let Some(keep) = oe.retention.retain.as_ref() else {
            return;
        };
        self.error(
            span,
            format!(
                "`{owner}.{field}` is kept {}, but `{}.{name}` {why}: the formula would change \
                 when it does. Keep a copy with a `default` (taken when the record is created)",
                keep.text(),
                target.name
            ),
        );
    }

    /// `erase rec, "why"`: a record of an entity with personal fields (its own or its lines').
    pub(super) fn check_erase(&mut self, target: &Expr, reason: &Option<Expr>, span: Span) {
        let t = self.infer(target);
        if let Some(r) = reason {
            let rt = self.infer(r);
            if !matches!(rt.unopt(), T::Text(_) | T::Unknown) {
                self.error(r.span, "the reason of an `erase` is a text");
            }
        } else {
            self.error(
                span,
                "`erase` needs its reason: `erase c, \"the customer asked\"` (it is kept in the \
                 history)",
            );
        }
        let en = match t.unopt() {
            T::Ent(en) => en.clone(),
            T::Unknown => return,
            other => {
                self.error(
                    target.span,
                    format!("`erase` erases a record, not `{other}`"),
                );
                return;
            }
        };
        let Some(e) = self.m.entity(&en) else { return };
        let lines = self.m.entities.iter().filter(|l| {
            l.fields.iter().any(|f| {
                f.parent
                    && matches!(&f.ty, Some(Ty::Path { segments, .. }) if segments.last() == Some(&en))
            })
        });
        let personal = e.fields.iter().any(|f| f.personal)
            || lines
                .into_iter()
                .any(|l| l.fields.iter().any(|f| f.personal));
        if !personal {
            self.error(
                target.span,
                format!(
                    "{en} has no personal fields: `erase` empties the fields marked `personal` \
                     (`name: Text? personal`)"
                ),
            );
        }
    }
}

fn ty_text(t: &Ty) -> String {
    match t {
        Ty::Path { segments, args, .. } => {
            let mut s = segments.join(".");
            if !args.is_empty() {
                s.push('<');
                s.push_str(&args.iter().map(ty_text).collect::<Vec<_>>().join(", "));
                s.push('>');
            }
            s
        }
        Ty::Optional(i) => format!("{}?", ty_text(i)),
        Ty::List(i) => format!("[{}]", ty_text(i)),
        Ty::Set(i) => format!("{{{}}}", ty_text(i)),
        _ => "…".into(),
    }
}
