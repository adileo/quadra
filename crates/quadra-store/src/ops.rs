//! Operations on a database: init, verify, migrate, changes. Used by the CLI and the tests.

use crate::db::{materialized_formulas, Database, Sink};
use crate::schema;
use chrono::NaiveDateTime;
use quadra_ir::{Block, Model};
use quadra_runtime::{ChangeDiff, Ctx, Flow, Interp, ScriptMeta, Store, V};
use quadra_types::Report;
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

fn flow(prefix: &str, e: Flow) -> String {
    match e {
        Flow::Fail(m) if prefix.is_empty() => m,
        Flow::Fail(m) => format!("{prefix}: {m}"),
        Flow::Return(_) => format!("{prefix}: unexpected return"),
        Flow::Conflict(c) => format!(
            "{prefix}: changed by others in the meantime: {}",
            c.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ),
    }
}

/// An interpreter over the committed state of the database, with units and the clock set.
pub fn loaded<'m>(
    db: &mut Database,
    model: &'m Model,
    time: NaiveDateTime,
) -> Result<Interp<'m>, String> {
    let mut interp = Interp::new(model);
    interp.set_clock(time);
    interp.store = db.load()?;
    interp.load_units();
    Ok(interp)
}

/// Create the tables and commit the seeds as the first transaction. Every migration of the
/// project counts as applied: the schema is born migrated. On failure the database is left
/// empty. Returns the number of tables.
pub fn init(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
) -> Result<usize, String> {
    let mut db = Database::open(url, model, report)?;
    db.init()?;
    crate::deploy::Catalog::of(model, &db.schema, None).write(&mut db)?;
    let tables = db.schema.tables.len();
    let mut interp = Interp::new(model);
    interp.set_clock(time);
    interp.materialize = materialized_formulas(&db.schema);
    interp.sink = Some(Box::new(Sink { db, model }));
    interp.system += 1;
    let seeded = interp.transaction(|i| {
        for s in &model.seeds {
            i.stmts(&s.body, &mut Ctx::default())?;
        }
        let tx = i.store.tx;
        for m in &model.migrations {
            i.store.applied.insert(format!("migration:{}", m.name), tx);
        }
        Ok(())
    });
    if let Err(e) = seeded {
        drop(interp);
        Database::open(url, model, report)?.uninit()?;
        return Err(flow("seed failed", e));
    }
    Ok(tables)
}

/// The events of the outbox, oldest first; with `pending`, only those not delivered yet.
pub fn outbox(
    url: &str,
    model: &Model,
    report: &Report,
    pending: bool,
) -> Result<Vec<crate::OutboxEvent>, String> {
    let mut db = Database::open(url, model, report)?;
    if !db.status()?.initialized {
        return Err("the database is not initialized (run `quadra db init`)".into());
    }
    db.outbox(pending)
}

/// Mark outbox events delivered; returns how many were pending.
pub fn outbox_delivered(
    url: &str,
    model: &Model,
    report: &Report,
    ids: &[u64],
    time: NaiveDateTime,
) -> Result<usize, String> {
    let mut db = Database::open(url, model, report)?;
    if !db.status()?.initialized {
        return Err("the database is not initialized (run `quadra db init`)".into());
    }
    db.mark_delivered(ids, time)
}

fn outbox_db(url: &str, model: &Model, report: &Report) -> Result<Database, String> {
    let mut db = Database::open(url, model, report)?;
    if !db.status()?.initialized {
        return Err("the database is not initialized (run `quadra db init`)".into());
    }
    Ok(db)
}

/// Events that failed for good or were skipped made pending again (`quadra outbox retry`);
/// returns how many.
pub fn outbox_retry(
    url: &str,
    model: &Model,
    report: &Report,
    ids: &[u64],
    time: NaiveDateTime,
    by: &str,
) -> Result<usize, String> {
    let mut db = outbox_db(url, model, report)?;
    let at = time.format("%Y-%m-%dT%H:%M:%S").to_string();
    crate::deliveries::retry(&mut db.conn, ids, &at, by)
}

/// Events that will not be sent, with a reason (`quadra outbox skip`); returns how many.
pub fn outbox_skip(
    url: &str,
    model: &Model,
    report: &Report,
    ids: &[u64],
    time: NaiveDateTime,
    by: &str,
    reason: &str,
) -> Result<usize, String> {
    let mut db = outbox_db(url, model, report)?;
    let at = time.format("%Y-%m-%dT%H:%M:%S").to_string();
    crate::deliveries::skip(&mut db.conn, ids, &at, by, reason)
}

/// The messages of the inbox (`quadra inbox list`).
pub fn inbox(
    url: &str,
    model: &Model,
    report: &Report,
    state: Option<&str>,
) -> Result<Vec<crate::deliveries::Message>, String> {
    let mut db = outbox_db(url, model, report)?;
    crate::deliveries::inbox(&mut db.conn, state)
}

/// What happened to a message, step by step (`quadra inbox log`).
pub fn inbox_log(
    url: &str,
    model: &Model,
    report: &Report,
    id: u64,
) -> Result<Vec<crate::deliveries::Step>, String> {
    let mut db = outbox_db(url, model, report)?;
    crate::deliveries::inbox_log(&mut db.conn, id)
}

/// Messages rejected or skipped, handled again by the code deployed now (`quadra inbox retry`).
pub fn inbox_retry(
    url: &str,
    model: &Model,
    report: &Report,
    ids: &[u64],
    time: NaiveDateTime,
    by: &str,
) -> Result<usize, String> {
    let mut db = outbox_db(url, model, report)?;
    let at = time.format("%Y-%m-%dT%H:%M:%S").to_string();
    crate::deliveries::inbox_retry(&mut db.conn, ids, &at, by)
}

/// What the code of now would do with messages already received (`quadra inbox replay
/// --dry-run`): for each, its handling run on the data as it is, in memory, and then dropped;
/// the lines say what it would create, change and emit, or why it would be rejected. Nothing
/// is written.
pub fn inbox_dry_run(
    url: &str,
    model: &Model,
    report: &Report,
    ids: &[u64],
    time: NaiveDateTime,
) -> Result<Vec<String>, String> {
    let mut db = outbox_db(url, model, report)?;
    let all = crate::deliveries::inbox(&mut db.conn, None)?;
    let mut out = Vec::new();
    for id in ids {
        let Some(m) = all.iter().find(|m| m.id == *id) else {
            out.push(format!("#{id}: no such message"));
            continue;
        };
        let Some(i) = model.inbounds.iter().find(|i| i.name == m.inbound) else {
            out.push(format!(
                "#{id}: no inbound `{}` in the code of now",
                m.inbound
            ));
            continue;
        };
        let doc = match quadra_runtime::formats::parse(model, &i.ty, &m.body) {
            Ok(d) => d,
            Err(e) => {
                out.push(format!(
                    "#{id} {}: would be rejected: not a {}: {e}",
                    i.name, i.ty
                ));
                continue;
            }
        };
        let db = Database::open(url, model, report)?;
        let src = crate::source::DbSource::open(url, db.schema.clone(), None)?;
        let mut interp = Interp::new(model);
        interp.set_clock(time);
        interp.store = Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
        interp.load_units();
        interp.system += 1;
        let r = interp.with_cause(format!("inbound:{}#{id} replayed", i.name), |it| {
            it.transaction(|it| {
                let mut ctx = Ctx::default();
                ctx.bind(&i.binding, doc.clone());
                match it.stmts(&i.body, &mut ctx) {
                    Err(Flow::Return(_)) => Ok(()),
                    other => other,
                }
            })
        });
        if let Err(e) = r {
            out.push(format!(
                "#{id} {}: would be rejected: {}",
                i.name,
                flow(&format!("inbound {}", i.name), e)
            ));
            continue;
        }
        let tx = interp.store.tx;
        let mut lines = Vec::new();
        let mut written: Vec<u64> = interp.store.dirty.iter().copied().collect();
        written.sort();
        for rid in written {
            let Some(rec) = interp.store.records.get(&rid) else {
                continue;
            };
            let (ent, created) = (rec.ent.clone(), rec.created_tx == tx);
            // each field written, with its last value
            let mut fields: Vec<(String, V)> = Vec::new();
            for (t, f, v) in &rec.history {
                if *t != tx {
                    continue;
                }
                match fields.iter_mut().find(|(n, _)| n == f) {
                    Some(x) => x.1 = v.clone(),
                    None => fields.push((f.clone(), v.clone())),
                }
            }
            let shown: Vec<String> = fields
                .iter()
                .map(|(f, v)| format!("{f}={}", interp.display(v)))
                .collect();
            let what = if created { "create" } else { "change" };
            lines.push(format!("  would {what} {ent}#{rid} ({})", shown.join(", ")));
        }
        for v in interp.store.outbox.clone() {
            if let V::Event { name, fields } = &v {
                let shown: Vec<String> = fields
                    .iter()
                    .map(|(k, x)| format!("{k}={}", interp.display(x)))
                    .collect();
                lines.push(format!("  would emit {name}({})", shown.join(", ")));
            }
        }
        if lines.is_empty() {
            lines.push("  would change nothing".into());
        }
        out.push(format!(
            "#{id} {} ({}, {}):",
            i.name, m.state, m.received_at
        ));
        out.extend(lines);
    }
    Ok(out)
}

/// Messages not to be handled, with a reason (`quadra inbox skip`).
pub fn inbox_skip(
    url: &str,
    model: &Model,
    report: &Report,
    ids: &[u64],
    time: NaiveDateTime,
    by: &str,
    reason: &str,
) -> Result<usize, String> {
    let mut db = outbox_db(url, model, report)?;
    let at = time.format("%Y-%m-%dT%H:%M:%S").to_string();
    crate::deliveries::inbox_skip(&mut db.conn, ids, &at, by, reason)
}

/// What happened to an event, step by step (`quadra outbox log`).
pub fn outbox_log(
    url: &str,
    model: &Model,
    report: &Report,
    id: u64,
) -> Result<Vec<crate::deliveries::Step>, String> {
    let mut db = outbox_db(url, model, report)?;
    crate::deliveries::log(&mut db.conn, id)
}

/// Hash chain and tables against the history, a page at a time (`crate::verify`). Returns
/// the problems and the record count.
pub fn verify(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
) -> Result<(Vec<String>, usize), String> {
    crate::verify::verify(url, model, report, time)
}

/// `verify` with everything loaded in memory and every formula computed from the history
/// (the reference the tests compare the streamed verify with).
pub fn verify_in_memory(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
) -> Result<(Vec<String>, usize), String> {
    let mut db = Database::open(url, model, report)?;
    let mut interp = loaded(&mut db, model, time)?;
    interp.materialize = materialized_formulas(&db.schema);
    // the data does not change while it is verified: a formula read again (a rollup of
    // rollups) is not computed again
    interp.formula_memo = Some(std::collections::HashMap::new());
    interp.materialize_formulas();
    let problems = db.verify(model, &interp.store)?;
    Ok((problems, interp.store.records.len()))
}

#[derive(Debug, Default)]
pub struct MigrateReport {
    pub schema_changed: bool,
    pub fingerprint: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// migration name → its report lines
    pub scripts: Vec<(String, Vec<String>)>,
    pub new_defaults: usize,
    /// deployed incrementally (false: every table rebuilt, see `deploy_full`)
    pub incremental: bool,
    /// why the tables were rebuilt instead
    pub rebuilt_because: Option<String>,
    /// the changes of the tables, in order (`add column order.channel`, …)
    pub steps: Vec<String>,
    pub indexes_created: Vec<String>,
    pub indexes_dropped: Vec<String>,
    /// indexes that could not be built after the commit (PostgreSQL: retried by the next deploy)
    pub index_errors: Vec<String>,
}

/// Run the pending migrations and validate the data against the new model, in one
/// transaction; unless `dry`, rebuild the tables and commit. `None`: nothing to do.
pub fn migrate(
    url: &str,
    model: &Model,
    report: &Report,
    root: &Path,
    time: NaiveDateTime,
    dry: bool,
) -> Result<Option<MigrateReport>, String> {
    migrate_with(url, model, report, root, time, dry, false, None)
}

/// A new version of the code: like `migrate`, but always validates every record against the
/// new model (new checks included) and, unless `dry`, rebuilds the tables (formulas may have
/// changed even when the schema did not). `label` becomes the cause of the migration commit.
pub fn deploy(
    url: &str,
    model: &Model,
    report: &Report,
    root: &Path,
    time: NaiveDateTime,
    dry: bool,
    label: &str,
) -> Result<MigrateReport, String> {
    Ok(migrate_with(url, model, report, root, time, dry, true, Some(label))?.unwrap_or_default())
}

#[allow(clippy::too_many_arguments)]
fn migrate_with(
    url: &str,
    model: &Model,
    report: &Report,
    root: &Path,
    time: NaiveDateTime,
    dry: bool,
    always: bool,
    label: Option<&str>,
) -> Result<Option<MigrateReport>, String> {
    match crate::deploy::deploy(url, model, report, root, time, dry, always, label)? {
        Ok(crate::deploy::Done::Nothing) => Ok(None),
        Ok(crate::deploy::Done::Report(r)) => Ok(Some(r)),
        Err(crate::deploy::Fallback(why)) => {
            let r = migrate_full(url, model, report, root, time, dry, always, label)?;
            Ok(r.map(|mut r| {
                r.rebuilt_because = Some(why);
                r
            }))
        }
    }
}

/// `migrate`/`deploy` by rebuilding every table: the reference behaviour, used when the
/// database has no catalog of what is deployed (created by an older version) or the change
/// is one the incremental deploy does not handle. Loads the whole database in memory.
pub fn deploy_full(
    url: &str,
    model: &Model,
    report: &Report,
    root: &Path,
    time: NaiveDateTime,
    dry: bool,
    label: &str,
) -> Result<MigrateReport, String> {
    Ok(migrate_full(url, model, report, root, time, dry, true, Some(label))?.unwrap_or_default())
}

#[allow(clippy::too_many_arguments)]
fn migrate_full(
    url: &str,
    model: &Model,
    report: &Report,
    root: &Path,
    time: NaiveDateTime,
    dry: bool,
    always: bool,
    label: Option<&str>,
) -> Result<Option<MigrateReport>, String> {
    let mut db = Database::open(url, model, report)?;
    let status = db.status()?;
    if !status.initialized {
        return Err("the database is not initialized (run `quadra db init`)".into());
    }
    let mut interp = loaded(&mut db, model, time)?;
    let pending: Vec<&Block> = model
        .migrations
        .iter()
        .filter(|m| {
            !interp
                .store
                .applied
                .contains_key(&format!("migration:{}", m.name))
        })
        .collect();
    if status.schema_current && pending.is_empty() && !always {
        return Ok(None);
    }
    let (added, removed) = db.plan()?;
    let mut out = MigrateReport {
        schema_changed: !status.schema_current,
        fingerprint: status.fingerprint,
        added,
        removed,
        ..Default::default()
    };
    let before = interp.store.clone();
    let names: Vec<String> = pending.iter().map(|m| m.name.clone()).collect();
    let mut scripts = Vec::new();
    let mut new_defaults = 0;
    interp.system += 1;
    let cause = match (label, names.is_empty()) {
        (Some(l), true) => l.to_string(),
        (Some(l), false) => format!("{l} (migrations: {})", names.join(", ")),
        (None, true) => "schema migration".to_string(),
        (None, false) => format!("schema migration: {}", names.join(", ")),
    };
    interp.whole_data = true;
    interp.violations.clear();
    let r = interp.with_cause(cause.clone(), |i| {
        i.transaction(|i| {
            for m in &pending {
                let lines = i
                    .run_migration(m, Some(root))
                    .map_err(|e| Flow::Fail(flow(&format!("migration {}", m.name), e)))?;
                scripts.push((m.name.clone(), lines));
            }
            // new fields with a default get it on every existing record
            let missing: Vec<(u64, String)> = i
                .store
                .records
                .values()
                .flat_map(|r| {
                    model
                        .entity(&r.ent)
                        .map(|e| {
                            model
                                .all_fields(e)
                                .into_iter()
                                .filter(|f| {
                                    f.default.is_some()
                                        && f.formula.is_none()
                                        && !f.per_company
                                        && !r.fields.contains_key(&f.name)
                                })
                                .map(|f| (r.id, f.name.clone()))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .collect();
            new_defaults = missing.len();
            for (id, f) in missing {
                if let Some(r) = i.store.records.get_mut(&id) {
                    r.pending_defaults.push(f);
                }
            }
            Ok(())
        })
    });
    interp.whole_data = false;
    if !interp.violations.is_empty() {
        // the report says it all, without the generic prefix
        return Err(interp.violations_report());
    }
    r.map_err(|e| flow("the existing data does not satisfy the new model", e))?;
    out.scripts = scripts;
    out.new_defaults = new_defaults;
    interp.materialize = materialized_formulas(&db.schema);
    interp.materialize_formulas();
    if !dry {
        let old = crate::deploy::Catalog::read(&mut db)?;
        db.apply_migration(model, &before, &interp.store, &cause, interp.clock)?;
        crate::deploy::Catalog::of(model, &db.schema, old.as_ref()).write(&mut db)?;
    }
    Ok(Some(out))
}

// ---- changes ------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ChangeStatus {
    pub name: String,
    pub meta: ScriptMeta,
    pub applied_tx: Option<u64>,
    pub reverted_tx: Option<u64>,
}

pub fn list_changes(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
) -> Result<Vec<ChangeStatus>, String> {
    let mut db = Database::open(url, model, report)?;
    let mut interp = loaded(&mut db, model, time)?;
    Ok(model
        .changes
        .iter()
        .map(|c| ChangeStatus {
            name: c.name.clone(),
            meta: interp.script_meta(c),
            applied_tx: interp
                .store
                .applied
                .get(&format!("change:{}", c.name))
                .copied(),
            reverted_tx: interp
                .store
                .applied
                .get(&format!("revert:{}", c.name))
                .copied(),
        })
        .collect())
}

#[derive(Debug)]
pub struct ChangeOutcome {
    pub meta: ScriptMeta,
    pub diff: ChangeDiff,
    /// certified metrics that read a changed field (directly or through formulas)
    pub metrics: Vec<String>,
    /// the transaction written (None for a preview)
    pub tx: Option<u64>,
}

fn find_change<'m>(model: &'m Model, name: &str) -> Result<&'m Block, String> {
    model
        .changes
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| {
            let names: Vec<&str> = model.changes.iter().map(|c| c.name.as_str()).collect();
            format!(
                "unknown change `{name}` (changes: {})",
                if names.is_empty() {
                    "none".into()
                } else {
                    names.join(", ")
                }
            )
        })
}

fn user_by_login(interp: &mut Interp, login: &str) -> Option<u64> {
    let ids = interp.ids("User", None);
    ids.into_iter().find(|id| {
        interp
            .store
            .read(*id, "login", None)
            .map_or(false, |v| v.to_string() == login)
    })
}

/// The user holds `role`, directly or through a role that inherits it (`role Director is
/// Manager`).
fn has_role(interp: &mut Interp, user: u64, role: &str) -> bool {
    interp.roles_of(user).iter().any(|r| r == role)
}

/// "a Manager", "an Accountant".
fn article(word: &str) -> String {
    let an = word
        .chars()
        .next()
        .map_or(false, |c| "AEIOUaeiou".contains(c));
    format!("{} {word}", if an { "an" } else { "a" })
}

/// Requester and approver: the requester runs the change; the approver must hold the role
/// named by `approved_by` and must not be the requester.
fn authorize(
    interp: &mut Interp,
    meta: &ScriptMeta,
    approver: Option<&str>,
) -> Result<(Option<u64>, String), String> {
    let requester = match &meta.requested_by {
        Some(login) => Some(
            user_by_login(interp, login)
                .ok_or_else(|| format!("the requester `{login}` is not a user"))?,
        ),
        None => None,
    };
    let mut signed = String::new();
    if let Some(role) = &meta.approved_by_role {
        let Some(login) = approver else {
            return Err(format!(
                "this change needs the approval of {}: pass --approved-by <login>",
                article(role)
            ));
        };
        let id = user_by_login(interp, login)
            .ok_or_else(|| format!("the approver `{login}` is not a user"))?;
        if !has_role(interp, id, role) {
            return Err(format!(
                "`{login}` is not {}: the change needs the approval of {}",
                article(role),
                article(role)
            ));
        }
        if Some(id) == requester {
            return Err(format!(
                "`{login}` requested the change and cannot approve it too"
            ));
        }
        signed = format!(", approved by {login}");
    }
    let user = requester.or_else(|| approver.and_then(|a| user_by_login(interp, a)));
    Ok((user, signed))
}

/// Metrics that read one of `fields`, following formulas.
fn metrics_touched(
    model: &Model,
    report: &Report,
    fields: &HashSet<(String, String)>,
) -> Vec<String> {
    let mut out = Vec::new();
    for m in &model.metrics {
        let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
        let mut stack = vec![("metric".to_string(), m.name.clone())];
        let mut hit = false;
        while let Some(n) = stack.pop() {
            if !seen.insert(n.clone()) {
                continue;
            }
            if fields.contains(&n) {
                hit = true;
                break;
            }
            for (r, _) in report.reads.get(&n).map(|v| v.as_slice()).unwrap_or(&[]) {
                stack.push(r.clone());
            }
        }
        if hit {
            out.push(m.name.clone());
        }
    }
    out
}

/// Run a change: as a preview (nothing written) or for real (`apply`).
pub fn run_change(
    url: &str,
    model: &Model,
    report: &Report,
    name: &str,
    approver: Option<&str>,
    time: NaiveDateTime,
    apply: bool,
) -> Result<ChangeOutcome, String> {
    let change = find_change(model, name)?;
    let mut db = Database::open(url, model, report)?;
    let mut interp = loaded(&mut db, model, time)?;
    let meta = interp.script_meta(change);
    if let Some(tx) = interp.store.applied.get(&format!("change:{name}")) {
        return Err(format!(
            "change `{name}` was already applied (transaction {tx})"
        ));
    }
    if apply {
        if let Some(d) = meta.effective {
            if time.date() < d {
                return Err(format!("change `{name}` is effective from {d}: it can be applied on or after that date"));
            }
        }
    }
    let (user, signed) = if apply {
        authorize(&mut interp, &meta, approver)?
    } else {
        (
            meta.requested_by
                .as_deref()
                .and_then(|l| user_by_login(&mut interp, l)),
            String::new(),
        )
    };
    interp.user = user;
    let before: Store = interp.store.clone();
    if apply {
        interp.materialize = materialized_formulas(&db.schema);
        interp.sink = Some(Box::new(Sink { db, model }));
    }
    let requested = meta
        .requested_by
        .as_deref()
        .map(|r| format!(", requested by {r}"))
        .unwrap_or_default();
    let cause = format!(
        "change {name}: {}{requested}{signed}",
        meta.reason.clone().unwrap_or_default()
    );
    let mut diff = ChangeDiff::default();
    interp
        .with_cause(cause, |i| {
            i.transaction(|i| {
                i.run_change(change)?;
                Ok(())
            })?;
            diff = i.diff_since(&before);
            Ok(())
        })
        .map_err(|e| flow(&format!("change {name}"), e))?;
    let fields: HashSet<(String, String)> = diff
        .changed
        .iter()
        .map(|(e, _, f, _, _)| (e.clone(), f.split('@').next().unwrap_or(f).to_string()))
        .collect();
    let metrics = metrics_touched(model, report, &fields);
    Ok(ChangeOutcome {
        meta,
        diff,
        metrics,
        tx: if apply { Some(interp.store.tx) } else { None },
    })
}

/// Undo an applied change with a new transaction (same approval rules). Returns the
/// number of values restored.
pub fn revert_change(
    url: &str,
    model: &Model,
    report: &Report,
    name: &str,
    approver: Option<&str>,
    time: NaiveDateTime,
) -> Result<usize, String> {
    let change = find_change(model, name)?;
    let mut db = Database::open(url, model, report)?;
    let mut interp = loaded(&mut db, model, time)?;
    let meta = interp.script_meta(change);
    let (user, signed) = authorize(&mut interp, &meta, approver)?;
    interp.user = user;
    interp.materialize = materialized_formulas(&db.schema);
    interp.sink = Some(Box::new(Sink { db, model }));
    interp.system += 1;
    let mut n = 0;
    interp
        .with_cause(format!("revert of change {name}{signed}"), |i| {
            i.transaction(|i| {
                n = i.revert_change(name)?;
                Ok(())
            })
        })
        .map_err(|e| flow("", e))?;
    Ok(n)
}

/// Provisional values: (entity, id, field, reason, until).
pub fn provisional(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
) -> Result<Vec<(String, u64, String, String, Option<chrono::NaiveDate>)>, String> {
    let mut db = Database::open(url, model, report)?;
    let interp = loaded(&mut db, model, time)?;
    Ok(interp.provisional_values())
}

/// An engine over the database at `url`, committing to it directly (no sequencer: run with the
/// project stopped, as `quadra change apply` is), its clock at `time`.
pub fn engine<'m>(
    url: &str,
    model: &'m Model,
    report: &Report,
    time: NaiveDateTime,
) -> Result<Interp<'m>, String> {
    let db = Database::open(url, model, report)?;
    let src = crate::source::DbSource::open(url, db.schema.clone(), None)?;
    let mut interp = Interp::new(model);
    interp.set_clock(time);
    interp.store = Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    interp.load_units();
    interp.materialize = materialized_formulas(&db.schema);
    interp.sink = Some(Box::new(Sink { db, model }));
    Ok(interp)
}

/// `quadra erase Customer 12 --reason …`: the record's `personal` fields and its lines'
/// emptied, their past values out of the history, as the operator `who`. Returns the fields
/// erased.
#[allow(clippy::too_many_arguments)]
pub fn erase(
    url: &str,
    model: &Model,
    report: &Report,
    entity: &str,
    id: u64,
    reason: &str,
    time: NaiveDateTime,
    who: &str,
) -> Result<Vec<String>, String> {
    let mut i = engine(url, model, report, time)?;
    if !i.store.exists(id, None) || i.store.get(id).is_some_and(|r| &*r.ent != entity) {
        return Err(format!("{entity}#{id} does not exist"));
    }
    let rec = quadra_runtime::V::Ref {
        ent: std::rc::Rc::from(entity),
        id,
    };
    i.system += 1;
    i.with_cause(format!("quadra erase by {who}"), |i| {
        i.transaction(|i| i.erase(&rec, reason))
    })
    .map_err(|e| flow("erase", e))
}

/// Re-evaluate the rules that read the clock (`today()`, `now()`) on every record: automatic
/// transitions, `becomes` reactions, postings. Loading on demand evaluates rules only where
/// data changed, so time-driven ones need this periodic pass. Returns the transaction
/// written, if anything changed.
pub fn sweep(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
) -> Result<Option<u64>, String> {
    sweep_with(url, model, report, time, None)
}

/// `sweep`, committed through the project's sequencer when it has one running.
pub fn sweep_with(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
    sequencer: Option<&crate::Sequencer>,
) -> Result<Option<u64>, String> {
    let mut db = Database::open(url, model, report)?;
    // when the previous sweep ran: a `becomes` condition that reads the clock is compared with
    // what it was then
    let last = db
        .meta("last_sweep")?
        .and_then(|t| NaiveDateTime::parse_from_str(&t, "%Y-%m-%dT%H:%M:%S%.f").ok());
    let schema = db.schema.clone();
    let src = crate::source::DbSource::open(url, schema, None)?;
    let mut interp = Interp::new(model);
    interp.set_clock(time);
    interp.store = Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    interp.load_units();
    interp.materialize = materialized_formulas(&db.schema);
    interp.sink = Some(match sequencer {
        Some(s) => Box::new(crate::SequencedSink {
            db,
            model,
            sequencer: s.clone(),
            tickets: None,
        }),
        None => Box::new(Sink { db, model }),
    });
    interp.sweep = true;
    interp.last_sweep = last;
    interp.system += 1;
    let before = interp.store.tx;
    interp
        .with_cause("periodic sweep of time-driven rules".into(), |i| {
            i.transaction(|_| Ok(()))
        })
        .map_err(|e| flow("sweep", e))?;
    let mut written = if interp.store.tx > before && !interp.store.dirty.is_empty() {
        Some(interp.store.tx)
    } else {
        None
    };
    // the retentions that ended: values emptied, records taken away, a transaction each
    let expired = interp.expire_due().map_err(|e| flow("expire", e))?;
    if !expired.is_empty() {
        written = Some(interp.store.tx);
    }
    drop(interp);
    // through the sequencer when there is one: a database rebuilt from the log knows the last
    // sweep too (else a `becomes` already fired could fire again)
    let at = time.format("%Y-%m-%dT%H:%M:%S%.f").to_string();
    match sequencer {
        Some(s) => s.side(move |c| {
            c.exec("INSERT INTO \"_quadra_meta\" (\"key\", \"value\") VALUES ('last_sweep', ?text) ON CONFLICT (\"key\") DO UPDATE SET \"value\" = excluded.\"value\"", &[crate::conn::Sql::Text(at)]).map(|_| ())
        })?,
        None => Database::open(url, model, report)?.set_meta("last_sweep", &at)?,
    }
    Ok(written)
}

/// The reactions to the outcome of an event's delivery (`on OrderShipped delivered as d:`),
/// run in a transaction of their own as the system, with `cause` (`outbound:OrderShipped#41`).
/// Returns how many reactions ran (none: nothing written).
#[allow(clippy::too_many_arguments)]
pub fn outcome_reactions(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
    sequencer: Option<&crate::Sequencer>,
    event: &str,
    want: quadra_ir::Trigger,
    outcome: quadra_runtime::V,
    cause: String,
) -> Result<usize, String> {
    if !model
        .reactions
        .iter()
        .any(|r| r.entity == event && r.trigger == want)
    {
        return Ok(0);
    }
    let db = Database::open(url, model, report)?;
    let schema = db.schema.clone();
    let src = crate::source::DbSource::open(url, schema, None)?;
    let mut interp = Interp::new(model);
    interp.set_clock(time);
    interp.store = Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    interp.load_units();
    interp.materialize = materialized_formulas(&db.schema);
    interp.sink = Some(match sequencer {
        Some(s) => Box::new(crate::SequencedSink {
            db,
            model,
            sequencer: s.clone(),
            tickets: None,
        }),
        None => Box::new(Sink { db, model }),
    });
    interp
        .with_cause(cause, |i| {
            i.transaction(|i| i.run_outcome_of(event, &want, &outcome))
        })
        .map_err(|e| flow("the reaction", e))
}

/// An expression of an `outbound … as e:` for one event: `e` its fields (`fields`, as the
/// outbox keeps them), the code run as the system, reading the data as it was at transaction
/// `tx` (the one that emitted it: every attempt reads the same) or, with `as_of` false, as it
/// is now (`unless`).
#[allow(clippy::too_many_arguments)]
pub fn eval_for_event(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
    outbound: &quadra_ir::Outbound,
    expr: &quadra_ir::Expr,
    fields: &serde_json::Value,
    tx: u64,
    as_of: bool,
) -> Result<V, String> {
    let Some(binding) = &outbound.binding else {
        return Err("this outbound has no `as e:`".into());
    };
    let db = Database::open(url, model, report)?;
    let src = crate::source::DbSource::open(url, db.schema.clone(), None)?;
    let mut interp = Interp::new(model);
    interp.set_clock(time);
    interp.store = Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    interp.load_units();
    interp.system += 1;
    interp.building_request = true;
    let event: Vec<(Option<String>, V)> = fields
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| (Some(k.clone()), quadra_runtime::codec::from_json(v)))
                .collect()
        })
        .unwrap_or_default();
    let mut ctx = Ctx {
        as_of: as_of.then_some(tx),
        ..Default::default()
    };
    // a `publish`: the record by its own name too
    if let Some(p) = &outbound.publishes {
        if let Some((_, r)) = event.iter().find(|(k, _)| k.as_deref() == Some("record")) {
            ctx.bind(p, r.clone());
        }
    }
    ctx.bind(binding, V::Tuple(std::rc::Rc::new(event)));
    interp
        .eval(expr, &mut ctx)
        .map_err(|e| flow(&format!("outbound {}", outbound.event), e))
}

/// The request the block of a batched `outbound` builds for its events (`events`: their fields
/// and the transactions that emitted them): the binding is the list of them, the data read as
/// it was at the last of those transactions.
pub fn build_batch_request(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
    outbound: &quadra_ir::Outbound,
    events: &[(serde_json::Value, u64)],
) -> Result<V, String> {
    let (Some(binding), Some(body)) = (&outbound.binding, &outbound.body) else {
        return Err("this outbound has no block".into());
    };
    let db = Database::open(url, model, report)?;
    let src = crate::source::DbSource::open(url, db.schema.clone(), None)?;
    let mut interp = Interp::new(model);
    interp.set_clock(time);
    interp.store = Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    interp.load_units();
    interp.system += 1;
    interp.building_request = true;
    let list: Vec<V> = events
        .iter()
        .map(|(fields, _)| {
            let parts: Vec<(Option<String>, V)> = fields
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| (Some(k.clone()), quadra_runtime::codec::from_json(v)))
                        .collect()
                })
                .unwrap_or_default();
            V::Tuple(std::rc::Rc::new(parts))
        })
        .collect();
    let mut ctx = Ctx {
        as_of: events.iter().map(|(_, tx)| *tx).max(),
        ..Default::default()
    };
    ctx.bind(binding, V::List(std::rc::Rc::new(list)));
    interp
        .eval(body, &mut ctx)
        .map_err(|e| flow(&format!("the block of outbound {}", outbound.event), e))
}

/// The request the block of an `outbound … as e:` builds for one event, reading the data as
/// it was at the emit (`eval_for_event`). Every attempt builds the same request.
pub fn build_request(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
    outbound: &quadra_ir::Outbound,
    fields: &serde_json::Value,
    tx: u64,
) -> Result<V, String> {
    let Some(body) = &outbound.body else {
        return Err("this outbound has no block".into());
    };
    eval_for_event(url, model, report, time, outbound, body, fields, tx, true)
}

/// Handles a message of the inbox: the code of its `inbound`, with `binding` its record (`doc`),
/// in a transaction of its own as the system, with `cause` (`inbound:ShopOrder#12 handled`).
/// Returns the transaction it committed (none when it wrote nothing).
#[allow(clippy::too_many_arguments)]
pub fn handle_inbound(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
    sequencer: Option<&crate::Sequencer>,
    inbound: &quadra_ir::Inbound,
    doc: V,
    cause: String,
    schema: Option<std::rc::Rc<crate::schema::Schema>>,
    units: Option<(&quadra_runtime::Units, Option<u64>)>,
) -> Result<Option<u64>, String> {
    // the engine's schema when it gives it: not built again for every message
    let db = match schema {
        Some(s) => Database::open_with(url, s)?,
        None => Database::open(url, model, report)?,
    };
    let schema = db.schema.clone();
    let src = crate::source::DbSource::open(url, schema, None)?;
    let mut interp = Interp::new(model);
    interp.set_clock(time);
    interp.store = Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    // the engine's units and company when it gives them: not read again for every message
    match units {
        Some((u, company)) => {
            interp.set_units(u);
            interp.company = company;
        }
        None => interp.load_units(),
    }
    interp.materialize = materialized_formulas(&db.schema);
    interp.sink = Some(match sequencer {
        Some(s) => Box::new(crate::SequencedSink {
            db,
            model,
            sequencer: s.clone(),
            tickets: None,
        }),
        None => Box::new(Sink { db, model }),
    });
    interp.system += 1;
    let before = interp.store.tx;
    interp
        .with_cause(cause, |i| {
            i.transaction(|i| {
                // a file of rows (`from csv`, `from fixed`): the code once per row, in this one
                // transaction (all or nothing), a failure saying its row
                let rows = model
                    .entity(&inbound.ty)
                    .is_some_and(|e| matches!(e.format.as_deref(), Some("csv" | "fixed")));
                let list: Vec<V> = match (&doc, rows) {
                    (V::List(l), true) => l.iter().cloned().collect(),
                    _ => vec![doc.clone()],
                };
                for (n, row) in list.into_iter().enumerate() {
                    let mut ctx = Ctx::default();
                    ctx.bind(&inbound.binding, row);
                    match i.stmts(&inbound.body, &mut ctx) {
                        Err(Flow::Return(_)) | Ok(()) => {}
                        Err(Flow::Fail(e)) if rows => {
                            return Err(Flow::Fail(format!("row {}: {e}", n + 1)))
                        }
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            })
        })
        .map_err(|e| flow(&format!("inbound {}", inbound.name), e))?;
    Ok((interp.store.tx > before && !interp.store.dirty.is_empty()).then_some(interp.store.tx))
}

/// One transaction of rows: committed (its transaction, if it wrote), or the index of the
/// row that failed (none: at its commit) and why; the outer error is not a row's.
type RowsRun = Result<Result<Option<u64>, (Option<usize>, String)>, String>;

/// What came of the rows of a file handled by an inbound (`handle_inbound_rows`).
#[derive(Debug, Default)]
pub struct RowsOutcome {
    /// the last transaction committed
    pub tx: Option<u64>,
    /// the rows whose code failed: their line (record) and why
    pub refused: Vec<(usize, String)>,
    /// the rows committed now
    pub committed: usize,
}

/// The rows of a chunk committed in one transaction (`atomic per row`): large enough that a
/// file of a hundred thousand rows is two hundred commits, small enough that a row that fails
/// costs little to isolate.
pub const ROWS_PER_COMMIT: usize = 500;

/// The rows of a file (`(line, row)`) run through an inbound's code. `per_row` false (`atomic
/// per file`): all of them in one transaction with the cause `<cause> handled`, or none (the
/// error says the line). `per_row` (`atomic per row`): by chunks of [`ROWS_PER_COMMIT`], each
/// a transaction with the cause `<cause>/lines A-B handled`; a row that fails is refused and
/// the rows before it in its chunk committed without it; a chunk that fails as a whole (a
/// rule checked at its commit) is run again a row at a time (`<cause>/line N handled`). The
/// rows `done` committed before (a server stopped halfway) are not run again. `unit` names a
/// row in what is reported (`line`, `record`).
#[allow(clippy::too_many_arguments)]
pub fn handle_inbound_rows(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
    sequencer: Option<&crate::Sequencer>,
    inbound: &quadra_ir::Inbound,
    rows: &[(usize, V)],
    per_row: bool,
    cause: &str,
    done: impl Fn(usize) -> bool,
    unit: &str,
) -> Result<RowsOutcome, String> {
    let what = format!("inbound {}", inbound.name);
    let open = || -> Result<Interp<'_>, String> {
        let db = Database::open(url, model, report)?;
        let schema = db.schema.clone();
        let src = crate::source::DbSource::open(url, schema, None)?;
        let mut interp = Interp::new(model);
        interp.set_clock(time);
        interp.store = Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
        interp.load_units();
        interp.materialize = materialized_formulas(&db.schema);
        interp.sink = Some(match sequencer {
            Some(s) => Box::new(crate::SequencedSink {
                db,
                model,
                sequencer: s.clone(),
                tickets: None,
            }),
            None => Box::new(Sink { db, model }),
        });
        interp.system += 1;
        Ok(interp)
    };
    // one transaction of `rows`: the transaction, or the index of the row that failed (none:
    // it failed at its commit) and why
    let run = |interp: &mut Interp, rows: &[(usize, V)], cause: String| -> RowsRun {
        let before = interp.store.tx;
        let failed_at = std::cell::Cell::new(None);
        let r = interp.with_cause(cause, |i| {
            i.transaction(|i| {
                for (n, (_, row)) in rows.iter().enumerate() {
                    let mut ctx = Ctx::default();
                    ctx.bind(&inbound.binding, row.clone());
                    match i.stmts(&inbound.body, &mut ctx) {
                        Err(Flow::Return(_)) | Ok(()) => {}
                        Err(e) => {
                            failed_at.set(Some(n));
                            return Err(e);
                        }
                    }
                }
                Ok(())
            })
        });
        match r {
            Ok(()) => Ok(Ok((interp.store.tx > before
                && !interp.store.dirty.is_empty())
            .then_some(interp.store.tx))),
            Err(Flow::Fail(e)) => Ok(Err((failed_at.get(), e))),
            Err(e) => Err(flow(&what, e)),
        }
    };
    let mut out = RowsOutcome::default();
    if !per_row {
        let mut interp = open()?;
        return match run(&mut interp, rows, format!("{cause} handled"))? {
            Ok(tx) => {
                out.tx = tx;
                out.committed = rows.len();
                Ok(out)
            }
            Err((Some(n), e)) => Err(format!("{what}: {unit} {}: {e}", rows[n].0)),
            Err((None, e)) => Err(format!("{what}: {e}")),
        };
    }
    let span = |rows: &[(usize, V)]| match (rows.first(), rows.last()) {
        (Some(a), Some(b)) if a.0 != b.0 => format!("{unit}s {}-{}", a.0, b.0),
        (Some(a), _) => format!("{unit} {}", a.0),
        _ => String::new(),
    };
    let todo: Vec<(usize, V)> = rows.iter().filter(|(n, _)| !done(*n)).cloned().collect();
    let mut rest: &[(usize, V)] = &todo;
    while !rest.is_empty() {
        let chunk = &rest[..rest.len().min(ROWS_PER_COMMIT)];
        let mut interp = open()?;
        match run(
            &mut interp,
            chunk,
            format!("{cause}/{} handled", span(chunk)),
        )? {
            Ok(tx) => {
                out.tx = tx.or(out.tx);
                out.committed += chunk.len();
                rest = &rest[chunk.len()..];
            }
            // a row failed: the rows before it go without it, the next chunk starts after it
            Err((Some(n), e)) => {
                out.refused.push((chunk[n].0, e));
                if n > 0 {
                    let before = &chunk[..n];
                    match run(
                        &mut interp,
                        before,
                        format!("{cause}/{} handled", span(before)),
                    )? {
                        Ok(tx) => {
                            out.tx = tx.or(out.tx);
                            out.committed += before.len();
                        }
                        Err(_) => one_by_one(&mut interp, before, &mut out, &run, cause, unit)?,
                    }
                }
                rest = &rest[n + 1..];
            }
            // it failed at its commit (a rule over the rows): a row at a time
            Err((None, _)) => {
                one_by_one(&mut interp, chunk, &mut out, &run, cause, unit)?;
                rest = &rest[chunk.len()..];
            }
        }
    }
    out.refused.sort_by_key(|(n, _)| *n);
    Ok(out)
}

/// Each row in a transaction of its own: those that fail are refused.
#[allow(clippy::type_complexity)]
fn one_by_one<'m>(
    interp: &mut Interp<'m>,
    rows: &[(usize, V)],
    out: &mut RowsOutcome,
    run: &dyn Fn(&mut Interp<'m>, &[(usize, V)], String) -> RowsRun,
    cause: &str,
    unit: &str,
) -> Result<(), String> {
    for row in rows {
        match run(
            interp,
            std::slice::from_ref(row),
            format!("{cause}/{unit} {} handled", row.0),
        )? {
            Ok(tx) => {
                out.tx = tx.or(out.tx);
                out.committed += 1;
            }
            Err((_, e)) => out.refused.push((row.0, e)),
        }
    }
    Ok(())
}

/// The answer of an endpoint to a request (`q`, its record): the code of its block run as the
/// system; with `cause` (a `POST`) in a transaction of its own, else (a `GET`) reading only.
/// Returns the answer and the transaction it committed, if any.
#[allow(clippy::too_many_arguments)]
pub fn run_endpoint(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
    sequencer: Option<&crate::Sequencer>,
    endpoint: &quadra_ir::Endpoint,
    q: V,
    cause: Option<String>,
) -> Result<(V, Option<u64>), String> {
    let db = Database::open(url, model, report)?;
    let schema = db.schema.clone();
    let src = crate::source::DbSource::open(url, schema, None)?;
    let mut interp = Interp::new(model);
    interp.set_clock(time);
    interp.store = Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    interp.load_units();
    interp.system += 1;
    let what = format!("endpoint {} {}", endpoint.method, endpoint.path);
    let Some(cause) = cause else {
        let mut ctx = Ctx::default();
        ctx.bind(&endpoint.binding, q);
        let v = interp
            .eval(&endpoint.body, &mut ctx)
            .map_err(|e| flow(&what, e))?;
        return Ok((v, None));
    };
    interp.materialize = materialized_formulas(&db.schema);
    interp.sink = Some(match sequencer {
        Some(s) => Box::new(crate::SequencedSink {
            db,
            model,
            sequencer: s.clone(),
            tickets: None,
        }),
        None => Box::new(Sink { db, model }),
    });
    let before = interp.store.tx;
    let v = interp
        .with_cause(cause, |i| {
            i.transaction(|i| {
                let mut ctx = Ctx::default();
                ctx.bind(&endpoint.binding, q.clone());
                i.eval(&endpoint.body, &mut ctx)
            })
        })
        .map_err(|e| flow(&what, e))?;
    Ok((v, (interp.store.tx > before).then_some(interp.store.tx)))
}

/// A fixed value of a policy (`3 days`): evaluated without data.
pub fn eval_fixed(model: &Model, expr: &quadra_ir::Expr) -> Result<V, String> {
    let mut interp = Interp::new(model);
    interp
        .eval(expr, &mut Ctx::default())
        .map_err(|e| flow("the policy", e))
}

/// When a project next needs a sweep, from its database (`quadra_analysis::clock`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Due {
    /// nothing in it changes with time alone
    Never,
    /// it reads the clock in a way only a periodic sweep follows
    Periodic,
    /// at the nearest threshold not passed yet at the last sweep (None: none ahead): a sweep
    /// at that instant, or later, sees it passed
    At(Option<NaiveDateTime>),
}

/// The next sweep a project needs: the nearest instant after its last sweep (`last_sweep` in
/// the database's meta, or `since`) at which one of its thresholds passes for one of its
/// records. One column read per threshold, no interpreter. Rounded up to the second after.
pub fn next_due(
    url: &str,
    model: &Model,
    report: &Report,
    since: NaiveDateTime,
) -> Result<Due, String> {
    use quadra_analysis::clock::{schedule, Schedule};
    let thresholds = match schedule(model) {
        Schedule::Never => return Ok(Due::Never),
        Schedule::Periodic(_) => return Ok(Due::Periodic),
        Schedule::At(t) => t,
    };
    let mut db = Database::open(url, model, report)?;
    let after = db
        .meta("last_sweep")?
        .and_then(|t| NaiveDateTime::parse_from_str(&t, "%Y-%m-%dT%H:%M:%S%.f").ok())
        .unwrap_or(since);
    let tz = quadra_runtime::time::zone(&model.timezone)
        .or_else(|_| quadra_runtime::time::zone("UTC"))?;
    let schema = db.schema.clone();
    let mut best: Option<NaiveDateTime> = None;
    for t in &thresholds {
        let entities: Vec<String> = match model.entity(&t.entity) {
            Some(e) if e.kind == quadra_ir::EntityKind::Trait => model
                .implementors(&t.entity)
                .iter()
                .map(|e| e.name.clone())
                .collect(),
            _ => vec![t.entity.clone()],
        };
        for ent in entities {
            let column = schema
                .tables
                .iter()
                .find(|tb| matches!(&tb.source, crate::schema::Source::Entity(n) if *n == ent))
                .and_then(|tb| {
                    let f = tb
                        .fields
                        .iter()
                        .find(|f| f.field == t.field && !f.formula)?;
                    Some((tb.name.clone(), f.columns.first()?.name.clone()))
                });
            // a field with no column of its own: only a periodic sweep follows it
            let Some((table, col)) = column else {
                return Ok(Due::Periodic);
            };
            // the due instant grows with the column's value: the smallest values past a bound,
            // in order, give the nearest (days are calendar days of the project's zone: the
            // bound leaves two days of margin and the exact instant is computed for each)
            let margin = if t.days != 0 || t.date {
                chrono::Duration::days(t.days + 2)
            } else {
                chrono::Duration::zero()
            };
            // (a date: compared as its text, the day of the bound)
            let bound = (after - chrono::Duration::seconds(t.secs + 1) - margin)
                .format(if t.date {
                    "%Y-%m-%d"
                } else {
                    "%Y-%m-%d %H:%M:%S"
                })
                .to_string();
            let mut offset = 0;
            'scan: loop {
                let rows = db.conn.query(&format!("SELECT \"{col}\" FROM \"{table}\" WHERE \"{col}\" > ?text ORDER BY \"{col}\" LIMIT 64 OFFSET {offset}"), &[crate::conn::Sql::Text(bound.clone())])?;
                let n = rows.len();
                for r in rows {
                    let due = if t.date {
                        // the midnight starting the day, in the project's zone
                        let Some(day) = r[0].as_deref().and_then(|v| {
                            chrono::NaiveDate::parse_from_str(v.get(..10).unwrap_or(v), "%Y-%m-%d")
                                .ok()
                        }) else {
                            continue;
                        };
                        let midnight = (day + chrono::Duration::days(t.days))
                            .and_hms_opt(0, 0, 0)
                            .unwrap();
                        quadra_runtime::time::instant(midnight, tz) + chrono::Duration::seconds(1)
                    } else {
                        let Some(at) = r[0].as_deref().and_then(parse_instant) else {
                            continue;
                        };
                        let due = if t.days != 0 {
                            quadra_runtime::time::add_days(at, t.days, tz)
                        } else {
                            at
                        };
                        due + chrono::Duration::seconds(t.secs + 1)
                    };
                    if due > after {
                        if best.is_none_or(|b| due < b) {
                            best = Some(due);
                        }
                        break 'scan;
                    }
                }
                if n < 64 {
                    break;
                }
                offset += 64;
            }
        }
    }
    Ok(Due::At(best))
}

/// When the project's database was last swept, if ever.
pub fn last_sweep(
    url: &str,
    model: &Model,
    report: &Report,
) -> Result<Option<NaiveDateTime>, String> {
    let mut db = Database::open(url, model, report)?;
    Ok(db
        .meta("last_sweep")?
        .and_then(|t| NaiveDateTime::parse_from_str(&t, "%Y-%m-%dT%H:%M:%S%.f").ok()))
}

fn parse_instant(v: &str) -> Option<NaiveDateTime> {
    [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
    ]
    .iter()
    .find_map(|f| NaiveDateTime::parse_from_str(v, f).ok())
}

/// Load a fixture of the tests (with the fixtures it extends) into a database, as one
/// transaction: demo and development data.
pub fn fixture(
    url: &str,
    model: &Model,
    report: &Report,
    name: &str,
    time: NaiveDateTime,
) -> Result<u64, String> {
    let mut chain = Vec::new();
    let mut cur = Some(name.to_string());
    while let Some(n) = cur {
        let f = model
            .fixtures
            .iter()
            .find(|f| f.name == n)
            .ok_or_else(|| format!("unknown fixture `{n}`"))?;
        chain.push(f);
        cur = f.extends.clone();
    }
    chain.reverse();
    let mut db = Database::open(url, model, report)?;
    let mut interp = loaded(&mut db, model, time)?;
    interp.materialize = materialized_formulas(&db.schema);
    interp.sink = Some(Box::new(Sink { db, model }));
    interp.system += 1;
    let mut ctx = Ctx::default();
    ctx.scopes.push(Default::default());
    interp
        .with_cause(format!("fixture {name}"), |i| {
            i.transaction(|i| {
                for f in &chain {
                    for st in &f.body {
                        match &st.kind {
                            quadra_ir::StmtKind::Clock(_) => {}
                            _ => i.stmt(st, &mut ctx)?,
                        }
                    }
                }
                Ok(())
            })
        })
        .map_err(|e| flow(&format!("fixture {name}"), e))?;
    Ok(interp.store.tx)
}

/// `quadra dev`: a user to sign in with, when the database has none (`Admin` role if the
/// project defines one). Whether it was created.
pub fn ensure_user(
    url: &str,
    model: &Model,
    report: &Report,
    login: &str,
    time: NaiveDateTime,
) -> Result<bool, String> {
    let mut db = Database::open(url, model, report)?;
    let mut interp = loaded(&mut db, model, time)?;
    if !interp.ids("User", None).is_empty() {
        return Ok(false);
    }
    interp.materialize = materialized_formulas(&db.schema);
    interp.sink = Some(Box::new(Sink { db, model }));
    interp.system += 1;
    let roles: Vec<V> = model
        .roles
        .iter()
        .filter(|r| r.name == "Admin")
        .map(|r| V::Role(std::rc::Rc::from(r.name.as_str())))
        .collect();
    interp
        .with_cause(format!("quadra dev: user {login}"), |i| {
            i.transaction(|i| {
                i.create(
                    "User",
                    vec![
                        ("login".to_string(), V::Text(std::rc::Rc::from(login))),
                        ("roles".to_string(), V::Set(std::rc::Rc::new(roles))),
                    ],
                )?;
                Ok(())
            })
        })
        .map_err(|e| flow("the dev user", e))?;
    Ok(true)
}

/// Number of tables of the schema (for messages).
pub fn table_count(model: &Model, report: &Report) -> usize {
    schema::build(model, report).tables.len()
}

#[cfg(test)]
mod tests {
    #[test]
    fn article_before_a_role() {
        assert_eq!(super::article("Manager"), "a Manager");
        assert_eq!(super::article("Accountant"), "an Accountant");
        assert_eq!(super::article("Editor"), "an Editor");
    }
}

/// A document an `attach` rendered, to be kept and named by its record.
#[derive(Clone, Debug)]
pub struct Attached {
    pub document: String,
    pub entity: String,
    pub id: u64,
    pub field: String,
    pub file: crate::files::File,
    /// the document's bytes, for the store of files
    pub bytes: Vec<u8>,
}

/// The documents of the `attach`es due: each record whose condition holds and whose field is
/// still empty, rendered from the data as it is (at most `limit`), with the records whose
/// document could not be rendered and why. Nothing is written: `attach_write` names them.
pub fn attach_render(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
    limit: usize,
    skip: &HashSet<(String, u64)>,
) -> Result<(Vec<Attached>, Vec<(String, u64, String)>), String> {
    let mut done = Vec::new();
    let mut failed = Vec::new();
    if model.attaches.is_empty() {
        return Ok((done, failed));
    }
    let db = Database::open(url, model, report)?;
    let src = crate::source::DbSource::open(url, db.schema.clone(), None)?;
    let mut interp = Interp::new(model);
    interp.set_clock(time);
    interp.store = Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    interp.load_units();
    interp.system += 1;
    for a in &model.attaches {
        for id in interp.ids(&a.entity, None) {
            if done.len() + failed.len() >= limit {
                return Ok((done, failed));
            }
            if skip.contains(&(a.document.clone(), id)) {
                continue;
            }
            if !interp
                .store
                .read(id, &a.field, None)
                .unwrap_or(V::None)
                .is_none()
            {
                continue;
            }
            let rec = V::Ref {
                ent: std::rc::Rc::from(a.entity.as_str()),
                id,
            };
            if let Some(w) = &a.when {
                let mut ctx = Ctx {
                    this: Some(rec.clone()),
                    entity: Some(a.entity.clone()),
                    ..Ctx::default()
                };
                match interp.eval(w, &mut ctx) {
                    Ok(V::Bool(true)) => {}
                    Ok(_) => continue,
                    Err(e) => {
                        failed.push((a.document.clone(), id, flow("its `when`", e)));
                        continue;
                    }
                }
            }
            let doc = interp
                .render_document(&a.document, vec![rec], false)
                .and_then(|d| match &a.signer {
                    Some(by) => interp.sign_call("sign", vec![d, quadra_runtime::value::text(by)]),
                    None => Ok(d),
                });
            match doc {
                Ok(V::Tuple(t)) => {
                    let get = |k: &str| {
                        t.iter()
                            .find(|(n, _)| n.as_deref() == Some(k))
                            .map(|(_, v)| v.clone())
                            .unwrap_or(V::None)
                    };
                    let s = |v: V| match v {
                        V::Text(t) => t.to_string(),
                        other => other.to_string(),
                    };
                    let size = match get("size") {
                        V::Int(n) => n as u64,
                        _ => 0,
                    };
                    use base64::Engine as _;
                    let Ok(bytes) =
                        base64::engine::general_purpose::STANDARD.decode(s(get("content")))
                    else {
                        failed.push((a.document.clone(), id, "its bytes are not readable".into()));
                        continue;
                    };
                    done.push(Attached {
                        document: a.document.clone(),
                        entity: a.entity.clone(),
                        id,
                        field: a.field.clone(),
                        file: crate::files::File {
                            hash: s(get("hash")),
                            name: s(get("name")),
                            ctype: s(get("type")),
                            size,
                            created_at: time.format("%Y-%m-%dT%H:%M:%S").to_string(),
                        },
                        bytes,
                    });
                }
                Ok(_) => failed.push((a.document.clone(), id, "not a document".into())),
                Err(e) => failed.push((a.document.clone(), id, flow("", e))),
            }
        }
    }
    Ok((done, failed))
}

/// Name the documents rendered in their records, each in a transaction of its own as the
/// system (`attach:InvoicePdf Invoice#45`): the field gets `sha256:<hex>` unless something was
/// put there meanwhile. The files themselves are kept first (their bytes in the store of
/// files, what they are with `files::put`).
pub fn attach_write(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
    sequencer: Option<&crate::Sequencer>,
    items: &[Attached],
) -> Result<usize, String> {
    if items.is_empty() {
        return Ok(0);
    }
    let db = Database::open(url, model, report)?;
    let schema = db.schema.clone();
    let src = crate::source::DbSource::open(url, schema, None)?;
    let mut interp = Interp::new(model);
    interp.set_clock(time);
    interp.store = Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    interp.load_units();
    interp.materialize = materialized_formulas(&db.schema);
    interp.sink = Some(match sequencer {
        Some(s) => Box::new(crate::SequencedSink {
            db,
            model,
            sequencer: s.clone(),
            tickets: None,
        }),
        None => Box::new(Sink { db, model }),
    });
    interp.system += 1;
    let mut n = 0;
    for it in items {
        let rec = V::Ref {
            ent: std::rc::Rc::from(it.entity.as_str()),
            id: it.id,
        };
        let value = quadra_runtime::value::text(&crate::files::reference(&it.file.hash));
        let wrote = interp
            .with_cause(
                format!("attach:{} {}#{}", it.document, it.entity, it.id),
                |i| {
                    i.transaction(|i| {
                        if !i
                            .store
                            .read(it.id, &it.field, None)
                            .unwrap_or(V::None)
                            .is_none()
                        {
                            return Ok(false);
                        }
                        i.set(&rec, &it.field, value.clone(), None)?;
                        Ok(true)
                    })
                },
            )
            .map_err(|e| {
                flow(
                    &format!("attach {} {}#{}", it.document, it.entity, it.id),
                    e,
                )
            })?;
        if wrote {
            n += 1;
        }
    }
    Ok(n)
}
