//! `Entity.group_by(…)` in SQL. A chain like
//!
//! ```quadra
//! Order.where(o => o.state is Confirmed)
//!     .group_by(o => o.customer.region, o => o.date.month)
//!     .sort_by(g => -g.items.sum(o => o.total))
//!     .first(10)
//!     .map(g => (region: g.key, month: g.key2, n: g.items.count(), total: g.items.sum(o => o.total)))
//! ```
//!
//! is one `GROUP BY` of the database (`What::Groups`, the same query as the API's groups),
//! when the groups are only read through their keys and aggregates of their rows
//! (`g.items.count()`, `.sum(x => …)`, `.min`, `.max`, `.avg`): the stages after `group_by`
//! (`where`, `sort_by`, `first(n)`, `last(n)`, and the final `map` or `count`) then run on the
//! groups with those aggregates computed by the database. Otherwise (a key SQL does not group
//! by, `g.items` used for something else, a row SQL cannot decide) the chain is evaluated the
//! interpreter's way, over the rows. Either way the answer is the interpreter's: the groups in
//! the order their first row appears (by id), the same keys, the same sums.

use super::*;
use crate::query::{Agg, AggFn, AggOf, AggOut, Binder, Bucket, Cond, GroupKey, GroupOrder};

/// At most this many groups: beyond, the formula must filter or group more coarsely.
pub const MAX_GROUPS: usize = 10_000;

/// A stage after `group_by`, with its lambda (parameter, body).
enum Stage<'e> {
    Where(String, &'e Expr),
    Sort(String, &'e Expr),
    Take { first: bool, n: &'e Expr },
    Map(String, &'e Expr),
    Count,
}

/// How the value of a group's key is rebuilt from the database's text (`query::group_value`).
#[derive(Clone)]
enum KeyConv {
    Ref,
    Text,
    Variant,
    Int,
    Bool,
    Date,
    /// `date.month`
    Month,
    /// `date.year`
    Year,
    /// `date.start_of_month()`: the interpreter's method on the first day (a `DateTime` field
    /// gives a date-time)
    StartOfMonth {
        datetime: bool,
    },
}

/// An aggregate of a group's rows found in a stage: what the database computes, and where.
struct Found<'e> {
    call: &'e Expr,
    func: &'static str,
    lambda: Option<(String, &'e Expr)>,
}

impl<'m> Interp<'m> {
    /// `….group_by(…)…map(g => …)` / `.count()` over a whole entity, in SQL: None to evaluate
    /// it the interpreter's way.
    pub(super) fn group_call(
        &mut self,
        base: &Expr,
        name: &str,
        args: &[Arg],
        ctx: &mut Ctx,
    ) -> R<Option<V>> {
        let last = match (name, args) {
            ("map", _) => match single(args) {
                Some((p, b)) => Stage::Map(p, b),
                None => return Ok(None),
            },
            ("count", []) => Stage::Count,
            _ => return Ok(None),
        };
        // the stages, outermost first, down to `group_by`
        let mut stages = vec![last];
        let mut cur = base;
        let (keys_args, inner) = loop {
            let ExprKind::Call { callee, args } = &cur.kind else {
                return Ok(None);
            };
            let ExprKind::Field {
                base: inner,
                name,
                optional: false,
            } = &callee.kind
            else {
                return Ok(None);
            };
            match name.as_str() {
                "group_by" => break (args, inner.as_ref()),
                "where" | "sort_by" => {
                    let Some((p, b)) = single(args) else {
                        return Ok(None);
                    };
                    stages.push(if name == "where" {
                        Stage::Where(p, b)
                    } else {
                        Stage::Sort(p, b)
                    });
                }
                "first" | "last" => match args.as_slice() {
                    [Arg {
                        name: None,
                        spread: false,
                        value,
                    }] if !matches!(value.kind, ExprKind::Lambda { .. }) => {
                        stages.push(Stage::Take {
                            first: name == "first",
                            n: value,
                        })
                    }
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            }
            cur = inner;
        };
        stages.reverse();
        if keys_args.is_empty()
            || keys_args.len() > 2
            || keys_args.iter().any(|a| a.name.is_some() || a.spread)
        {
            return Ok(None);
        }
        let Some((ent, lambdas)) = self.table_chain(inner, ctx) else {
            return Ok(None);
        };
        // the keys
        let mut keys = Vec::new();
        let mut convs = Vec::new();
        for a in keys_args {
            let ExprKind::Lambda { params, body } = &a.value.kind else {
                return Ok(None);
            };
            let [p] = params.as_slice() else {
                return Ok(None);
            };
            let Some((k, c)) = self.sql_key(&ent, p, body)? else {
                return Ok(None);
            };
            keys.push(k);
            convs.push(c);
        }
        // the aggregates the stages read (every other use of the group: not in SQL)
        let mut found: Vec<Found> = Vec::new();
        for s in &stages {
            let (p, body) = match s {
                Stage::Where(p, b) | Stage::Sort(p, b) | Stage::Map(p, b) => (p, *b),
                // `first(n)`: `n` is evaluated as it is
                Stage::Take { .. } => continue,
                Stage::Count => continue,
            };
            if !collect_aggs(p, body, &mut found) {
                return Ok(None);
            }
        }
        // the database's aggregates: per found one, one (or, for `avg`, two) of `What::Groups`;
        // then the first row of each group (its order)
        let mut binders: Vec<Binder> = Vec::new();
        for f in &found {
            binders.push(Binder::Param(
                f.lambda
                    .as_ref()
                    .map(|l| l.0.clone())
                    .unwrap_or_else(|| "_".into()),
            ));
        }
        let mut aggs: Vec<Agg> = Vec::new();
        let mut slots: Vec<(usize, Option<usize>)> = Vec::new();
        for (k, f) in found.iter().enumerate() {
            let Some((_, expr)) = &f.lambda else {
                // `count()`: the group's rows
                slots.push((usize::MAX, None));
                continue;
            };
            let of = || AggOf::Expr {
                binder: &binders[k],
                expr,
            };
            match f.func {
                "sum" => {
                    slots.push((aggs.len(), None));
                    aggs.push(Agg {
                        of: of(),
                        func: AggFn::Sum,
                    });
                }
                "min" | "max" => {
                    slots.push((aggs.len(), None));
                    aggs.push(Agg {
                        of: of(),
                        func: if f.func == "min" {
                            AggFn::Min
                        } else {
                            AggFn::Max
                        },
                    });
                }
                // `avg`: the sum and the rows with a value (`AggOut::rows`)
                _ => {
                    slots.push((aggs.len(), Some(aggs.len())));
                    aggs.push(Agg {
                        of: of(),
                        func: AggFn::Sum,
                    });
                }
            }
        }
        let first_row = aggs.len();
        aggs.push(Agg {
            of: AggOf::Path(vec!["id".into()]),
            func: AggFn::Min,
        });
        let cbinders: Vec<Binder> = lambdas
            .iter()
            .map(|(p, _)| Binder::Param(p.clone()))
            .collect();
        let mut spec = Spec::new(&ent);
        spec.conds = lambdas
            .iter()
            .zip(cbinders.iter())
            .map(|((_, e), b)| Cond {
                binder: b,
                expr: e,
                negate: false,
            })
            .collect();
        spec.read_check = self.viewing();
        if self.viewing() {
            for (p, e) in &lambdas {
                self.viewer_lambda(&ent, p, e)?;
            }
            for f in &found {
                if let Some((p, e)) = &f.lambda {
                    self.viewer_lambda(&ent, p, e)?;
                }
            }
        }
        let within = vec![None; keys.len()];
        // `sort_by(g => ±<an aggregate>).first(n)` (the top customers): the database ranks the
        // groups and gives the first `n`, in the language's order (by the value, then by the
        // first row of each group: a stable sort of the groups in that order)
        let top = match stages.as_slice() {
            [Stage::Sort(_, body), Stage::Take { first: true, n }, rest @ ..]
                if rest.len() <= 1 =>
            {
                let (inner, neg) = match &body.kind {
                    ExprKind::Unary {
                        op: UnOp::Neg,
                        expr,
                    } => (expr.as_ref(), true),
                    _ => (*body, false),
                };
                let by = found
                    .iter()
                    .position(|f| std::ptr::eq(f.call, inner))
                    .and_then(|k| match slots[k] {
                        (usize::MAX, None) => Some(None),
                        (a, None)
                            if found[k].func == "sum"
                                || found[k].func == "min"
                                || found[k].func == "max" =>
                        {
                            Some(Some(a))
                        }
                        _ => None,
                    });
                match by {
                    Some(by) => match self.eval(n, ctx)? {
                        V::Int(n) if n >= 0 && (n as usize) <= MAX_GROUPS => {
                            Some((by, neg, n as usize))
                        }
                        _ => None,
                    },
                    None => None,
                }
            }
            _ => None,
        };
        let ranked = top.is_some();
        let gs = match top {
            Some((by, desc, n)) => {
                let Some(page) = self.sql_groups(
                    &spec,
                    ctx,
                    &keys,
                    &aggs,
                    GroupOrder::Ranked {
                        by,
                        then: first_row,
                    },
                    desc,
                    0,
                    n,
                    within,
                )?
                else {
                    return Ok(None);
                };
                page.groups
            }
            None => {
                let Some(groups) = self.sql_groups(
                    &spec,
                    ctx,
                    &keys,
                    &aggs,
                    GroupOrder::Key,
                    false,
                    0,
                    MAX_GROUPS + 1,
                    within,
                )?
                else {
                    return Ok(None);
                };
                if groups.total as usize > MAX_GROUPS {
                    return fail(format!("group_by: {} groups, at most {MAX_GROUPS}: filter the rows first or group by a coarser key", groups.total));
                }
                let mut gs = groups.groups;
                // the order of the interpreter: of the first row of each group
                let first_id = |g: &crate::query::GroupOut| {
                    g.aggs
                        .get(first_row)
                        .and_then(|a| a.value.as_deref())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(u64::MAX)
                };
                gs.sort_by_key(first_id);
                gs
            }
        };
        if matches!(stages.as_slice(), [Stage::Count]) {
            return Ok(Some(V::Int(gs.len() as i64)));
        }
        // the groups, each with its keys and the values of the aggregates found
        struct G {
            tuple: V,
            vals: Vec<V>,
        }
        let mut items: Vec<G> = Vec::new();
        for g in &gs {
            let mut kv = Vec::new();
            for (k, c) in convs.iter().enumerate() {
                kv.push(self.key_value(c, g.keys.get(k).cloned().flatten(), ctx)?);
            }
            let mut vals = Vec::new();
            for (k, f) in found.iter().enumerate() {
                let v = match slots[k] {
                    (usize::MAX, _) => Some(V::Int(g.count as i64)),
                    (a, None) => match f.func {
                        "sum" => sum_value(&g.aggs[a]),
                        _ => minmax_value(&g.aggs[a]),
                    },
                    (s, Some(c)) => match (sum_value(&g.aggs[s]), Some(g.aggs[c].rows as i64)) {
                        (_, Some(0)) => Some(V::None),
                        (Some(sum), Some(n)) => {
                            let sum = match sum {
                                V::Int(i) => V::Dec(Decimal::from(i)),
                                other => other,
                            };
                            Some(self.binop(BinOp::Div, sum, V::Int(n))?.normalized())
                        }
                        _ => None,
                    },
                };
                // a value SQL gives in a form the language would not (several currencies…):
                // the interpreter's way
                let Some(v) = v else { return Ok(None) };
                let v = self.zero_for(f.call.span, v, None, None);
                vals.push(v);
            }
            let two = kv.len() == 2;
            let k2 = if two { kv.pop() } else { None };
            let tuple =
                crate::interp::group_tuple(kv.pop().unwrap_or(V::None), k2, list(Vec::new()));
            items.push(G { tuple, vals });
        }
        // the stages, on the groups
        let calls: Vec<&Expr> = found.iter().map(|f| f.call).collect();
        let mut result: Option<V> = None;
        // ranked by the database: its sort and its `first(n)` are done
        let skip = if ranked { 2 } else { 0 };
        for s in stages.iter().skip(skip) {
            match s {
                Stage::Where(p, body) => {
                    let mut keep = Vec::new();
                    for g in items {
                        if self
                            .group_eval(p, body, &g.tuple, &calls, &g.vals, ctx)?
                            .truthy()
                        {
                            keep.push(g);
                        }
                    }
                    items = keep;
                }
                Stage::Sort(p, body) => {
                    let mut keyed = Vec::new();
                    for g in items {
                        keyed.push((self.group_eval(p, body, &g.tuple, &calls, &g.vals, ctx)?, g));
                    }
                    let mut err = None;
                    keyed.sort_by(|a, b| match self.compare(&a.0, &b.0) {
                        Ok(c) => c.cmp(&0),
                        Err(e) => {
                            err = Some(e);
                            std::cmp::Ordering::Equal
                        }
                    });
                    if let Some(e) = err {
                        return Err(e);
                    }
                    items = keyed.into_iter().map(|(_, g)| g).collect();
                }
                Stage::Take { first, n } => {
                    let n = match self.eval(n, ctx)? {
                        V::Int(n) if n >= 0 => (n as usize).min(items.len()),
                        other => {
                            return fail(format!(
                                "{}(n) takes a whole number ≥ 0, not {other}",
                                if *first { "first" } else { "last" }
                            ))
                        }
                    };
                    let skip = if *first { 0 } else { items.len() - n };
                    items = items.into_iter().skip(skip).take(n).collect();
                }
                Stage::Map(p, body) => {
                    let mut out = Vec::new();
                    for g in &items {
                        out.push(self.group_eval(p, body, &g.tuple, &calls, &g.vals, ctx)?);
                    }
                    result = Some(list(out));
                }
                Stage::Count => result = Some(V::Int(items.len() as i64)),
            }
        }
        Ok(result)
    }

    /// A stage's lambda evaluated on a group: its aggregates replaced by their values.
    fn group_eval(
        &mut self,
        p: &str,
        body: &Expr,
        group: &V,
        calls: &[&Expr],
        vals: &[V],
        ctx: &mut Ctx,
    ) -> R<V> {
        let body = replace_aggs(body, calls);
        ctx.scopes.push(Default::default());
        ctx.bind(p, group.clone());
        for (k, v) in vals.iter().enumerate() {
            ctx.bind(&format!("__group_agg_{k}"), v.clone());
        }
        self.viewer_in_query += 1;
        let r = self.eval(&body, ctx);
        self.viewer_in_query -= 1;
        ctx.scopes.pop();
        r
    }

    /// A key lambda (`o => o.customer.region`, `o => o.date.month`) as a key SQL groups by.
    fn sql_key(&mut self, ent: &str, p: &str, body: &Expr) -> R<Option<(GroupKey, KeyConv)>> {
        // `path.start_of_month()`
        let (path_expr, call) = match &body.kind {
            ExprKind::Call { callee, args } if args.is_empty() => match &callee.kind {
                ExprKind::Field {
                    base,
                    name,
                    optional: false,
                } if name == "start_of_month" => (base.as_ref(), true),
                _ => return Ok(None),
            },
            _ => (body, false),
        };
        let Some(mut segs) = param_path(path_expr, p) else {
            return Ok(None);
        };
        if segs.is_empty() || segs.len() > 6 {
            return Ok(None);
        }
        let m = self.m;
        let mut cur = ent.to_string();
        let mut hidden_path = false;
        let mut bucket = None;
        let mut conv = None;
        let n = segs.len();
        for k in 0..n {
            let seg = segs[k].clone();
            let Some(e) = m.entity(&cur) else {
                return Ok(None);
            };
            let Some(f) = m.find_field(e, &seg) else {
                return Ok(None);
            };
            // a value per company is not one column (a formula SQL cannot read: the source says so)
            if f.per_company {
                return Ok(None);
            }
            if self.viewing() && self.hidden(&cur, &seg) {
                return fail(format!("`{cur}.{seg}` is hidden from you"));
            }
            let base = match &f.ty {
                Some(Ty::Optional(t)) => Some(t.as_ref().clone()),
                t => t.clone(),
            };
            let named = |x: &str| matches!(&base, Some(Ty::Path { segments, args, .. }) if args.is_empty() && segments.len() == 1 && segments[0] == x);
            let target = base
                .as_ref()
                .and_then(|t| ty_names(Some(t)))
                .filter(|t| m.entity(t).is_some_and(|x| x.kind == EntityKind::Entity));
            let date = named("Date") || named("DateTime");
            let rest = n - k - 1;
            if rest == 0 {
                conv = Some(if call {
                    if !date {
                        return Ok(None);
                    }
                    bucket = Some(Bucket::Month);
                    KeyConv::StartOfMonth {
                        datetime: named("DateTime"),
                    }
                } else if target.is_some() {
                    KeyConv::Ref
                } else if named("Date") {
                    KeyConv::Date
                } else if named("Int") {
                    KeyConv::Int
                } else if named("Bool") {
                    KeyConv::Bool
                } else if matches!(base, Some(Ty::Enum(_)))
                    || base
                        .as_ref()
                        .and_then(|t| ty_names(Some(t)))
                        .is_some_and(|t| m.enum_def(&t).is_some())
                {
                    KeyConv::Variant
                } else if crate::query::text_type(base.as_ref()) {
                    KeyConv::Text
                } else {
                    return Ok(None);
                });
                break;
            }
            if date && rest == 1 && !call && matches!(segs[k + 1].as_str(), "month" | "year") {
                let year = segs[k + 1] == "year";
                bucket = Some(if year { Bucket::Year } else { Bucket::Month });
                conv = Some(if year { KeyConv::Year } else { KeyConv::Month });
                segs.truncate(k + 1);
                break;
            }
            let Some(next) = target else { return Ok(None) };
            if self.viewing() && !self.read_unconditional(&next) {
                hidden_path = true;
            }
            cur = next;
        }
        let Some(conv) = conv else { return Ok(None) };
        // a path through records the user may not all read: the interpreter groups them
        if hidden_path {
            return Ok(None);
        }
        Ok(Some((
            GroupKey {
                path: segs,
                bucket,
                hidden_path,
            },
            conv,
        )))
    }

    /// The value of a key from the database's text.
    fn key_value(&mut self, c: &KeyConv, s: Option<String>, ctx: &mut Ctx) -> R<V> {
        let Some(s) = s else { return Ok(V::None) };
        let bad = || fail::<V>(format!("internal: a group key `{s}`"));
        let date = |x: &str| NaiveDate::parse_from_str(x, "%Y-%m-%d").ok();
        Ok(match c {
            KeyConv::Ref => match s.parse::<Id>() {
                Ok(id) => V::Ref {
                    ent: self.ent_of(id),
                    id,
                },
                Err(_) => return bad(),
            },
            KeyConv::Text => V::Text(Rc::from(s.as_str())),
            KeyConv::Variant => V::Variant {
                name: Rc::from(s.as_str()),
                fields: Rc::new(Vec::new()),
            },
            KeyConv::Int | KeyConv::Year => match s.parse::<i64>() {
                Ok(i) => V::Int(i),
                Err(_) => return bad(),
            },
            KeyConv::Bool => V::Bool(s == "true"),
            KeyConv::Date => match date(&s) {
                Some(d) => V::Date(d),
                None => return bad(),
            },
            KeyConv::Month => match Bucket::Month.range(&s) {
                Some((d, _)) => V::YearMonth(d.year(), d.month()),
                None => return bad(),
            },
            KeyConv::StartOfMonth { datetime } => match Bucket::Month.range(&s) {
                Some((d, _)) => {
                    let v = if *datetime {
                        V::DateTime(d.and_hms_opt(0, 0, 0).unwrap())
                    } else {
                        V::Date(d)
                    };
                    self.method(v, "start_of_month", &[], ctx)?
                }
                None => return bad(),
            },
        })
    }
}

/// The single literal lambda of one parameter among `args`.
fn single(args: &[Arg]) -> Option<(String, &Expr)> {
    match args {
        [Arg {
            name: None,
            spread: false,
            value,
        }] => match &value.kind {
            ExprKind::Lambda { params, body } if params.len() == 1 => {
                Some((params[0].clone(), body.as_ref()))
            }
            _ => None,
        },
        _ => None,
    }
}

/// `p.a.b` → `[a, b]`.
fn param_path(e: &Expr, p: &str) -> Option<Vec<String>> {
    match &e.kind {
        ExprKind::Name(n) if n == p => Some(Vec::new()),
        // `x?.y`: none for a missing value, as the database groups it
        ExprKind::Field { base, name, .. } => {
            let mut v = param_path(base, p)?;
            v.push(name.clone());
            Some(v)
        }
        _ => None,
    }
}

/// The aggregates of `g.items` in a stage's lambda body, in order; false when the group is
/// used otherwise (its rows themselves, the group as a value).
fn collect_aggs<'e>(g: &str, e: &'e Expr, out: &mut Vec<Found<'e>>) -> bool {
    // `g.items.<agg>(…)`
    if let ExprKind::Call { callee, args } = &e.kind {
        if let ExprKind::Field {
            base,
            name,
            optional: false,
        } = &callee.kind
        {
            if let ExprKind::Field {
                base: gb,
                name: items,
                optional: false,
            } = &base.kind
            {
                if items == "items" && matches!(&gb.kind, ExprKind::Name(n) if n == g) {
                    let func: &'static str = match name.as_str() {
                        "count" => "count",
                        "sum" => "sum",
                        "min" => "min",
                        "max" => "max",
                        "avg" => "avg",
                        _ => return false,
                    };
                    let lambda = single(args);
                    if func == "count" && !args.is_empty() || func != "count" && lambda.is_none() {
                        return false;
                    }
                    // the row lambda must not read the group
                    if let Some((_, b)) = &lambda {
                        if reads_name(b, g) {
                            return false;
                        }
                    }
                    out.push(Found {
                        call: e,
                        func,
                        lambda,
                    });
                    return true;
                }
            }
        }
    }
    match &e.kind {
        ExprKind::Name(n) => n != g,
        ExprKind::Field { base, name, .. } => {
            if matches!(&base.kind, ExprKind::Name(n) if n == g) {
                return name == "key" || name == "key2";
            }
            collect_aggs(g, base, out)
        }
        // a lambda rebinding the name does not read the group
        ExprKind::Lambda { params, .. } if params.iter().any(|p| p == g) => true,
        // statements: not looked into
        ExprKind::Block(_) => false,
        _ => {
            let mut ok = true;
            e.for_each_child(&mut |c| {
                if ok && !collect_aggs(g, c, out) {
                    ok = false;
                }
            });
            ok
        }
    }
}

/// Whether `e` reads the name `n` (lambdas rebinding it aside).
fn reads_name(e: &Expr, n: &str) -> bool {
    match &e.kind {
        ExprKind::Name(x) => x == n,
        ExprKind::Lambda { params, body } => !params.iter().any(|p| p == n) && reads_name(body, n),
        ExprKind::Block(_) => true,
        _ => {
            let mut found = false;
            e.for_each_child(&mut |c| {
                if !found && reads_name(c, n) {
                    found = true;
                }
            });
            found
        }
    }
}

/// `e` with each of `calls` replaced by the local holding its value.
fn replace_aggs(e: &Expr, calls: &[&Expr]) -> Expr {
    if let Some(k) = calls.iter().position(|c| std::ptr::eq(*c, e)) {
        return Expr {
            kind: ExprKind::Name(format!("__group_agg_{k}")),
            span: e.span,
        };
    }
    e.map_children(&mut |c| replace_aggs(c, calls))
}

/// `sum` as the language adds (from 0, one currency or unit); None when SQL's answer is not
/// one the language gives.
fn sum_value(a: &AggOut) -> Option<V> {
    super::query::lang_sum_pub(a).map(V::normalized)
}

/// `min` / `max` of values (none skipped); None when the language would not compare them.
fn minmax_value(a: &AggOut) -> Option<V> {
    if a.rows == 0 {
        return Some(V::None);
    }
    let s = a.value.as_deref()?;
    let d = || Decimal::from_str(s).ok();
    Some(
        match a.kind {
            "int" => V::Int(s.parse().ok()?),
            "num" => V::Dec(d()?),
            "money" if a.units == 1 => V::Money {
                amt: d()?,
                cur: Rc::from(a.unit.as_deref()?),
            },
            "qty" if a.units == 1 => V::Qty {
                v: d()?,
                unit: Rc::from(a.unit.as_deref()?),
            },
            "date" => V::Date(NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()?),
            "datetime" => V::DateTime(
                [
                    "%Y-%m-%dT%H:%M:%S",
                    "%Y-%m-%d %H:%M:%S",
                    "%Y-%m-%dT%H:%M",
                    "%Y-%m-%d %H:%M",
                ]
                .iter()
                .find_map(|f| NaiveDateTime::parse_from_str(s.get(..19.min(s.len()))?, f).ok())?,
            ),
            "text" => V::Text(Rc::from(s)),
            _ => return None,
        }
        .normalized(),
    )
}
