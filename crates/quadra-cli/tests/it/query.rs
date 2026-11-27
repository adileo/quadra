//! Queries in SQL give the interpreter's answers (the notes on scaling, phase 1b).
//!
//! The same database is read twice: through the SQL translation (`DbSource::sql`) and through
//! the interpreter alone (the reference). For a corpus of `POST /v1/query` requests (filters
//! with none, enums, sets, quantities, money, dates, deep paths, collections, `??`, `not`;
//! orders; every page through the cursor; aggregates; row permissions) and of expressions of
//! the language over whole entities (`Entity.where/count/any/all/first/last/sum/min/max`,
//! `sort_by(…).first()`, `last_by`), the rows, their order, the totals and the values must be
//! identical. Also inside a transaction that wrote records (read-your-writes) and as of an
//! earlier commit (`base`). On `examples/stress` (the `tiny` world of `quadra bench gen`) and
//! `examples/extended` (its fixture), on SQLite and, when `QUADRA_TEST_PG` holds a url, on
//! PostgreSQL.

use quadra_runtime::{Flow, Interp, Store, V};
use quadra_store::conn::{Conn, Pooled};
use quadra_store::schema::Schema;
use quadra_store::source::{head, DbSource};
use serde_json::{json, Value as J};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::Ordering;

fn clock() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 11, 20)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap()
}

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

/// A fresh database: a SQLite file or a PostgreSQL schema.
fn fresh(target: &str, name: &str) -> String {
    if target == "sqlite" {
        let p = std::env::temp_dir().join(format!("quadra-query-{}-{name}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", p.display()));
        }
        format!("sqlite:{}", p.display())
    } else {
        let schema = format!("qq_{}_{name}", std::process::id());
        Conn::open(target)
            .unwrap()
            .batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
            .unwrap();
        format!("{target}#schema={schema}")
    }
}

fn cleanup(target: &str, url: &str) {
    if let Some(path) = url.strip_prefix("sqlite:") {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
    } else if let Some((_, schema)) = url.split_once("#schema=") {
        let _ = Conn::open(target)
            .and_then(|mut c| c.batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")));
    }
}

struct World {
    root: PathBuf,
    url: String,
    sources: Vec<(PathBuf, String)>,
    checked: quadra_analysis::Checked,
    schema: Rc<Schema>,
    overlays: RefCell<std::collections::HashMap<String, Rc<quadra_analysis::Checked>>>,
}

impl World {
    fn new(root: PathBuf, url: String) -> World {
        let project = quadra_sema::Project::load(&root).unwrap();
        let sources: Vec<(PathBuf, String)> = project
            .user_files()
            .map(|f| (f.path.clone(), f.text.clone()))
            .collect();
        let checked = quadra_analysis::check(&project);
        assert!(checked.model.is_some(), "{} does not check", root.display());
        let db =
            quadra_store::Database::open(&url, checked.model.as_ref().unwrap(), &checked.report);
        let schema = match db {
            Ok(db) => db.schema.clone(),
            Err(_) => Rc::new(quadra_store::schema::build(
                checked.model.as_ref().unwrap(),
                &checked.report,
            )),
        };
        World {
            root,
            url,
            sources,
            checked,
            schema,
            overlays: RefCell::new(Default::default()),
        }
    }

    fn model(&self) -> &quadra_ir::Model {
        self.checked.model.as_ref().unwrap()
    }

    /// The model with formulas added to an entity (as the server checks a query's `where`).
    fn overlay(&self, entity: &str, items: &[(String, String)]) -> Rc<quadra_analysis::Checked> {
        let key = format!("{entity}|{items:?}");
        if let Some(c) = self.overlays.borrow().get(&key) {
            return c.clone();
        }
        let mut sources = self.sources.clone();
        if !items.is_empty() {
            sources.push((
                self.root.join("tests/__api__.qd"),
                quadra_api::overlay_source(entity, items),
            ));
        }
        let project = quadra_sema::Project::from_sources(&self.root, sources);
        let c = quadra_analysis::check(&project);
        let errors: Vec<String> = c
            .diagnostics
            .iter()
            .filter(|d| d.severity == quadra_sema::Severity::Error)
            .map(|d| d.message.clone())
            .collect();
        assert!(
            errors.is_empty() && c.model.is_some(),
            "{entity}: {items:?} does not check: {errors:?}"
        );
        let c = Rc::new(c);
        self.overlays.borrow_mut().insert(key, c.clone());
        c
    }

    /// An interpreter over the database as of `base`, answering queries in SQL or not.
    fn interp<'m>(
        &self,
        model: &'m quadra_ir::Model,
        sql: bool,
        user: Option<u64>,
        base: Option<u64>,
    ) -> Interp<'m> {
        let mut conn = Conn::open(&self.url).unwrap();
        let base = base.unwrap_or_else(|| head(&mut conn).unwrap());
        let mut src = DbSource::at(Pooled::own(conn), base, self.schema.clone(), None).unwrap();
        src.sql = sql;
        let mut i = Interp::new(model);
        i.set_clock(clock());
        i.store = Store::over(Rc::new(RefCell::new(src)));
        i.load_units();
        i.company = i.ids("Company", None).first().copied();
        i.user = user;
        i
    }

    fn user(&self, login: &str) -> u64 {
        let mut i = self.interp(self.model(), false, None, None);
        let ids = i.ids("User", None);
        ids.into_iter()
            .find(|id| {
                i.store
                    .read(*id, "login", None)
                    .map_or(false, |v| v.to_string() == login)
            })
            .unwrap_or_else(|| panic!("no user {login}"))
    }

    fn head(&self) -> u64 {
        head(&mut Conn::open(&self.url).unwrap()).unwrap()
    }
}

/// What must be equal in two answers of `POST /v1/query` (the cursor's form differs: SQL keys
/// or an offset).
fn comparable(r: &Result<J, (u16, String)>) -> J {
    match r {
        Ok(j) => {
            let mut j = j.clone();
            j.as_object_mut().unwrap().remove("next");
            j
        }
        Err((s, m)) => json!({ "status": s, "error": m }),
    }
}

/// One query, with SQL and without: the same page, total, aggregates; then every page through
/// the cursor gives the reference's whole list in order. Returns whether SQL answered.
fn same_query(w: &World, q: &J, user: Option<u64>, base: Option<u64>) -> bool {
    same_query_paged(w, q, user, base, true)
}

fn same_query_paged(w: &World, q: &J, user: Option<u64>, base: Option<u64>, pages: bool) -> bool {
    let (entity, items, columns) = quadra_api::query_items(q).unwrap();
    let c = w.overlay(&entity, &items);
    let model = c.model.as_ref().unwrap();
    let before = quadra_store::sqlq::SELECTS.load(Ordering::Relaxed);
    let mut si = w.interp(model, true, user, base);
    let got = quadra_api::query_rows(&mut si, model, &entity, q, &items, &columns);
    let used_sql = quadra_store::sqlq::SELECTS.load(Ordering::Relaxed) > before;
    let mut ri = w.interp(model, false, user, base);
    let want = quadra_api::query_rows(&mut ri, model, &entity, q, &items, &columns);
    assert_eq!(
        comparable(&got),
        comparable(&want),
        "query {q} as {user:?} at {base:?}: SQL and the interpreter differ"
    );
    if want.is_err() || !pages {
        return used_sql;
    }
    // every page, 23 rows at a time, through the cursor
    let mut all = q.clone();
    all["limit"] = json!(1000);
    all["offset"] = json!(0);
    all.as_object_mut().unwrap().remove("aggregates");
    let mut ri = w.interp(model, false, user, base);
    let reference: Vec<J> = quadra_api::query_rows(&mut ri, model, &entity, &all, &items, &columns)
        .unwrap()["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    let mut paged: Vec<J> = Vec::new();
    let mut page = q.clone();
    page["limit"] = json!(23);
    page["count"] = json!(false);
    page.as_object_mut().unwrap().remove("offset");
    page.as_object_mut().unwrap().remove("aggregates");
    for _ in 0..400 {
        let mut si = w.interp(model, true, user, base);
        let r = quadra_api::query_rows(&mut si, model, &entity, &page, &items, &columns).unwrap();
        paged.extend(
            r["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].clone()),
        );
        match r["next"].as_str() {
            Some(c) => page["after"] = json!(c),
            None => break,
        }
    }
    assert_eq!(
        paged, reference,
        "query {q} as {user:?}: the pages through the cursor are not the whole list"
    );
    used_sql
}

/// Expressions of the language evaluated on one record (as formulas of an overlay), with SQL
/// and without, in a transaction that first runs `prepare` (and is rolled back).
fn same_values(
    w: &World,
    entity: &str,
    rec: u64,
    exprs: &[&str],
    prepare: &dyn Fn(&mut Interp) -> Result<(), Flow>,
) -> usize {
    let items: Vec<(String, String)> = exprs
        .iter()
        .enumerate()
        .map(|(n, e)| (format!("__q{n}"), e.to_string()))
        .collect();
    let c = w.overlay(entity, &items);
    let model = c.model.as_ref().unwrap();
    let mut answers: Vec<Vec<String>> = Vec::new();
    let mut sql_used = 0;
    for sql in [true, false] {
        let mut i = w.interp(model, sql, None, None);
        let before = quadra_store::sqlq::SELECTS.load(Ordering::Relaxed);
        let mut out: Vec<String> = Vec::new();
        let r: Result<(), Flow> = i.transaction(|i| {
            prepare(i)?;
            let rec = V::Ref {
                ent: Rc::from(entity),
                id: rec,
            };
            for (n, _) in exprs.iter().enumerate() {
                let v = i.get(&rec, &format!("__q{n}"), None, None);
                out.push(match v {
                    // records by what they show: the records a transaction creates get ids of
                    // its own block
                    Ok(v) => show(i, &v),
                    Err(Flow::Fail(m)) => format!("error: {m}"),
                    Err(e) => format!("{e:?}"),
                });
            }
            Err(Flow::Fail("roll back".into()))
        });
        assert!(
            matches!(r, Err(Flow::Fail(ref m)) if m == "roll back"),
            "{r:?}"
        );
        if sql {
            sql_used = (quadra_store::sqlq::SELECTS.load(Ordering::Relaxed) - before) as usize;
        }
        answers.push(out);
    }
    for (n, e) in exprs.iter().enumerate() {
        assert_eq!(
            answers[0][n], answers[1][n],
            "{entity}#{rec}: `{e}`: SQL and the interpreter differ"
        );
    }
    sql_used
}

fn show(i: &mut Interp, v: &V) -> String {
    match v {
        V::Ref { ent, .. } => format!("{ent} {}", i.display(v)),
        V::List(items) | V::Set(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|x| show(i, x))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => format!("{other:?}"),
    }
}

// ---- the stress world ------------------------------------------------------------------------

fn stress_world(target: &str) -> World {
    let root = example("stress");
    let url = fresh(target, "stress");
    let dir = std::env::temp_dir().join(format!(
        "quadra-query-{}-gen-{}",
        std::process::id(),
        if target == "sqlite" { "s" } else { "p" }
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_quadra"))
        .args(["bench", "gen", "--scale", "tiny", "--out"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "{}",
        String::from_utf8_lossy(&st.stderr)
    );
    let w = World::new(root, url.clone());
    quadra_store::ops::init(&url, w.model(), &w.checked.report, clock()).unwrap();
    quadra_store::bulk::import_dir(
        &url,
        w.model(),
        &w.checked.report,
        &dir,
        clock(),
        None,
        &mut |_| {},
    )
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    World::new(example("stress"), url)
}

/// A value of the data, picked by SQL (as the benchmark does).
fn pick(w: &World, sql: &str) -> String {
    Conn::open(&w.url)
        .unwrap()
        .query(sql, &[])
        .unwrap()
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next().flatten())
        .unwrap_or_default()
}

fn stress_queries(w: &World) {
    let code = pick(
        w,
        "SELECT \"code\" FROM \"customer\" ORDER BY \"id\" LIMIT 1 OFFSET 3",
    );
    let admin = Some(w.user("admin"));
    let sales = Some(w.user("sales0001"));
    let shipment_filters = [
        "line.qty >= 15 pcs",
        &format!("line.order.customer.code == \"{code}\""),
        "carrier is Air",
        "carrier in {Road, Rail} and qty > 2 pcs",
        "not (carrier in {Road, Rail})",
        "tracking == none",
        "tracking != none",
        "tracking?.starts_with(\"T\") ?? false",
        "date >= 2026-06-01 or qty < 2 pcs",
        "line.order.state is Cancelled or line.order.priority is High",
        "date <= (line.order.customer.region.last_delivery ?? date)",
        "line.product.category.active and line.order.customer.active",
        "line.order.customer.tags.any(t => t.color is Red)",
        "line.order.customer.tags.count() >= 2",
        "line.order.lines.count() > 3",
        "line.shipments.count(s => s.carrier is Air) >= 1",
        "line.order.customer.orders.any(o => o.priority is High and o.state is not Draft)",
        "line.order.lines.all(l => l.discount == 0%)",
        "line.order.lines.none(l => l.qty > 20 pcs)",
        "line.order.customer.region.team.contains(line.order.customer.salesperson)",
        "line.order.customer.region.manager == none",
        "line.order.customer.region.manager != none and line.order.customer.region.manager == line.order.customer.salesperson",
        "qty > 1 pcs implies carrier is Road",
        "(qty > 2 pcs if carrier is Air else qty > 4 pcs)",
        "line.amount > 100.00 EUR",
        "line.order.number.starts_with(\"O\") and line.order.number.contains(\"1\")",
        "line.order.date < date",
    ];
    let orders: Vec<J> = vec![
        json!([]),
        json!([{ "field": "date", "desc": true }]),
        json!([{ "field": "qty" }]),
        json!([{ "field": "carrier" }, { "field": "date", "desc": true }]),
        json!([{ "field": "tracking", "desc": true }]),
        json!([{ "field": "line.order.customer.name" }]),
        json!([{ "field": "line" }]),
    ];
    let mut sql = 0;
    let mut n = 0;
    for (k, f) in shipment_filters.iter().enumerate() {
        // every filter with two orders, each order with several filters; every page of some
        for o in [&orders[0], &orders[1 + k % (orders.len() - 1)]] {
            n += 1;
            let q = json!({ "entity": "Shipment", "where": f, "order_by": o, "limit": 50, "aggregates": { "qty": "sum", "date": "max", "tracking": "count" } });
            if same_query_paged(w, &q, admin, None, k % 4 == 0) {
                sql += 1;
            }
        }
    }
    for o in &orders {
        n += 1;
        let q = json!({ "entity": "Shipment", "order_by": o, "limit": 50 });
        if same_query(w, &q, admin, None) {
            sql += 1;
        }
    }
    assert!(
        sql * 10 >= n * 9,
        "only {sql} of {n} shipment queries went to SQL"
    );
    // without a filter, other entities, other orders, as other users
    let more = [
        (json!({ "entity": "Shipment", "limit": 30 }), sales),
        (
            json!({ "entity": "Shipment", "where": "carrier is Air", "order_by": [{ "field": "date" }], "limit": 30 }),
            sales,
        ),
        (
            json!({ "entity": "Customer", "where": "name.starts_with(\"R\")", "order_by": [{ "field": "name" }] }),
            admin,
        ),
        (
            json!({ "entity": "Customer", "where": "credit_limit > 40000.00 EUR and active", "order_by": [{ "field": "credit_limit", "desc": true }], "aggregates": { "credit_limit": "avg" } }),
            admin,
        ),
        (
            json!({ "entity": "Customer", "order_by": [{ "field": "region" }, { "field": "code", "desc": true }] }),
            sales,
        ),
        (
            json!({ "entity": "Customer", "where": "tags.any(t => t.name.ends_with(\"A\")) or region.active == false" }),
            admin,
        ),
        (
            json!({ "entity": "Order", "where": "state in {Confirmed, Shipped}", "order_by": [{ "field": "number" }], "aggregates": { "date": "min" } }),
            admin,
        ),
        (
            json!({ "entity": "Order", "where": "lines.any(l => l.qty > 10 pcs) and date < 2026-09-01", "order_by": [{ "field": "customer.region.name" }, { "field": "date" }] }),
            sales,
        ),
        (
            json!({ "entity": "Order", "where": "customer.salesperson == me" }),
            sales,
        ),
        (
            json!({ "entity": "OrderLine", "where": "discount > 0% and order.state is not Cancelled", "order_by": [{ "field": "discount", "desc": true }], "aggregates": { "discount": "avg", "qty": "sum" } }),
            admin,
        ),
        (
            json!({ "entity": "OrderLine", "where": "price > 10.00 EUR/pcs", "order_by": [{ "field": "price" }] }),
            admin,
        ),
        (
            json!({ "entity": "OrderLine", "where": "shipments.count() == 0" }),
            admin,
        ),
        (
            json!({ "entity": "Tag", "where": "customers.count() > 2" }),
            admin,
        ),
        (
            json!({ "entity": "Region", "where": "team.contains(manager)" }),
            admin,
        ),
        (
            json!({ "entity": "Region", "order_by": [{ "field": "last_delivery" }] }),
            admin,
        ),
        (
            json!({ "entity": "Category", "where": "parent?.active ?? true", "order_by": [{ "field": "parent" }] }),
            admin,
        ),
        (
            json!({ "entity": "Shipment", "where": "line.order.customer.region.code == \"R01\"", "refs": { "line": 0 } }),
            admin,
        ),
        (
            json!({ "entity": "Shipment", "offset": 13, "limit": 9, "order_by": [{ "field": "date" }] }),
            admin,
        ),
        (
            json!({ "entity": "Shipment", "where": "qty > none" }),
            admin,
        ),
    ];
    for (q, u) in more.iter() {
        same_query(w, q, *u, None);
    }
    // the lines of a record
    let order: u64 = pick(
        w,
        "SELECT CAST(\"id\" AS TEXT) FROM \"order\" ORDER BY \"id\" LIMIT 1 OFFSET 5",
    )
    .parse()
    .unwrap();
    assert!(same_query(
        w,
        &json!({ "entity": "OrderLine", "refs": { "order": order }, "order_by": [{ "field": "qty", "desc": true }] }),
        admin,
        None
    ));
    let tag: u64 = pick(
        w,
        "SELECT CAST(\"id\" AS TEXT) FROM \"tag\" ORDER BY \"id\" LIMIT 1",
    )
    .parse()
    .unwrap();
    assert!(same_query(
        w,
        &json!({ "entity": "Customer", "refs": { "tags": tag } }),
        admin,
        None
    ));
}

fn stress_language(w: &World) {
    let region: u64 = pick(
        w,
        "SELECT CAST(\"id\" AS TEXT) FROM \"region\" ORDER BY \"id\" LIMIT 1",
    )
    .parse()
    .unwrap();
    let exprs = [
        "Customer.where(c => c.region == self and c.active).count()",
        "Customer.count(c => c.region == self)",
        "Order.where(o => o.customer.region == self).count(o => o.state is Confirmed)",
        "Order.any(o => o.customer.region == self and o.priority is High)",
        "Order.none(o => o.customer.region == self and o.date > 2027-01-01)",
        "Order.where(o => o.customer.region == self).all(o => o.lines.count() > 0)",
        "Customer.where(c => c.region == self).first()",
        "Customer.where(c => c.region == self).last()",
        "Customer.find(c => c.region == self and c.name.contains(\"a\"))",
        "Shipment.where(s => s.line.order.customer.region == self).sum(s => s.qty)",
        "Customer.where(c => c.region == self).sum(c => c.credit_limit)",
        "OrderLine.where(l => l.order.customer.region == self).sum(l => l.qty)",
        "Customer.where(c => c.region == self).sort_by(c => c.code).first()",
        "Customer.where(c => c.region == self).sort_by(c => c.name).last()",
        "Order.where(o => o.customer.region == self).last_by(o => o.date)",
        "Order.where(o => o.customer.region == self).min(o => o.date)",
        "Order.where(o => o.customer.region == self).max(o => o.date)",
        "Customer.where(c => c.region == self and c.tags.count() > 1).map(c => c.code)",
        "Customer.where(c => c.region == self).sum(c => c.order_count)",
        "Customer.where(c => c.region == self and c.revenue > 0.00 EUR).count()",
        "Order.where(o => o.customer.region == self and o.state in {Draft, Cancelled}).sort_by(o => o.number).first()",
        "Shipment.where(s => s.line.order.customer.region == self and s.tracking != none).count()",
        "Customer.where(c => c.region == self)",
        "Tag.where(t => t.customers.any(c => c.region == self)).count()",
        "Customer.where(c => c.region.team.contains(c.salesperson)).count()",
        "Order.count(o => o.customer.code == \"nobody\")",
        "Order.where(o => o.customer.region == self).sort_by(o => o.date).first()",
        "Order.where(o => o.customer.region == self).sort_by(o => o.date).last()",
    ];
    let used = same_values(w, "Region", region, &exprs, &|_| Ok(()));
    assert!(
        used >= exprs.len(),
        "only {used} SQL queries for {} expressions",
        exprs.len()
    );
    // inside a transaction that wrote some of the records the queries read
    let customer: u64 = pick(w, &format!("SELECT CAST(\"id\" AS TEXT) FROM \"customer\" WHERE \"region_id\" = {region} ORDER BY \"id\" LIMIT 1")).parse().unwrap();
    let other: u64 = pick(w, &format!("SELECT CAST(\"id\" AS TEXT) FROM \"region\" WHERE \"id\" <> {region} ORDER BY \"id\" LIMIT 1")).parse().unwrap();
    let order: u64 = pick(w, &format!("SELECT CAST(o.\"id\" AS TEXT) FROM \"order\" o JOIN \"customer\" c ON c.\"id\" = o.\"customer_id\" WHERE c.\"region_id\" = {region} AND o.\"state\" = 'Draft' ORDER BY o.\"id\" LIMIT 1")).parse().unwrap();
    let writes = |i: &mut Interp| -> Result<(), Flow> {
        let c = V::Ref {
            ent: Rc::from("Customer"),
            id: customer,
        };
        i.set(&c, "name", V::Text(Rc::from("Aaaa first")), None)?;
        i.set(&c, "active", V::Bool(false), None)?;
        let o = V::Ref {
            ent: Rc::from("Order"),
            id: order,
        };
        i.set(
            &o,
            "priority",
            V::Variant {
                name: Rc::from("High"),
                fields: Rc::new(Vec::new()),
            },
            None,
        )?;
        // a customer moved to another region, and a new one
        let moved: u64 = i
            .ids("Customer", None)
            .into_iter()
            .find(|id| {
                *id != customer
                    && i.store
                        .read(*id, "region", None)
                        .and_then(|v| v.as_ref_id())
                        == Some(region)
            })
            .unwrap();
        i.set(
            &V::Ref {
                ent: Rc::from("Customer"),
                id: moved,
            },
            "region",
            V::Ref {
                ent: Rc::from("Region"),
                id: other,
            },
            None,
        )?;
        let sp = i.store.read(customer, "salesperson", None).unwrap();
        i.create(
            "Customer",
            vec![
                ("code".into(), V::Text(Rc::from("ZZ-NEW"))),
                ("name".into(), V::Text(Rc::from("Zeta new"))),
                (
                    "region".into(),
                    V::Ref {
                        ent: Rc::from("Region"),
                        id: region,
                    },
                ),
                ("salesperson".into(), sp),
            ],
        )?;
        Ok(())
    };
    same_values(w, "Region", region, &exprs, &writes);
}

/// As of an earlier commit: the interpreter answers (SQL reads the latest rows), the same as
/// without SQL at that commit.
fn stress_as_of(w: &World) {
    let before = w.head();
    {
        let model = w.model();
        let mut db = quadra_store::Database::open(&w.url, model, &w.checked.report).unwrap();
        let mut i = quadra_store::ops::loaded(&mut db, model, clock()).unwrap();
        i.materialize = quadra_store::materialized_formulas(&db.schema);
        i.sink = Some(Box::new(quadra_store::Sink { db, model }));
        let ship: u64 = pick(w, "SELECT CAST(\"id\" AS TEXT) FROM \"shipment\" WHERE \"carrier\" = 'Road' ORDER BY \"id\" LIMIT 1").parse().unwrap();
        i.transaction(|i| {
            i.set(
                &V::Ref {
                    ent: Rc::from("Shipment"),
                    id: ship,
                },
                "carrier",
                V::Variant {
                    name: Rc::from("Air"),
                    fields: Rc::new(Vec::new()),
                },
                None,
            )
        })
        .unwrap();
    }
    assert!(w.head() > before);
    let admin = Some(w.user("admin"));
    for q in [
        json!({ "entity": "Shipment", "where": "carrier is Air", "order_by": [{ "field": "date" }] }),
        json!({ "entity": "Shipment", "order_by": [{ "field": "carrier" }], "limit": 20 }),
    ] {
        same_query(w, &q, admin, Some(before));
        same_query(w, &q, admin, None);
    }
}

fn stress_on(target: &str) {
    let w = stress_world(target);
    stress_queries(&w);
    stress_language(&w);
    stress_as_of(&w);
    cleanup(target, &w.url);
}

#[test]
fn stress_queries_on_sqlite() {
    stress_on("sqlite");
}

#[test]
fn stress_queries_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped");
        return;
    };
    stress_on(&pg);
}

// ---- the extended example ------------------------------------------------------------------------

fn extended_on(target: &str) {
    let root = example("extended");
    let url = fresh(target, "extended");
    let w = World::new(root.clone(), url.clone());
    quadra_store::ops::init(&url, w.model(), &w.checked.report, clock()).unwrap();
    quadra_store::ops::fixture(&url, w.model(), &w.checked.report, "bom_example", clock()).unwrap();
    let w = World::new(root, url);
    let admin = Some(w.user("admin"));
    let queries = [
        json!({ "entity": "StockMove", "where": "qty > 1 pcs", "order_by": [{ "field": "qty", "desc": true }], "aggregates": { "qty": "sum" } }),
        json!({ "entity": "StockMove", "where": "qty >= 500 g", "aggregates": { "qty": "max" } }),
        json!({ "entity": "StockMove", "where": "date >= 2026-11-01T00:00", "order_by": [{ "field": "date" }] }),
        json!({ "entity": "StockMove", "order_by": [{ "field": "product.name" }, { "field": "unit_cost" }] }),
        json!({ "entity": "Product", "where": "list_price > 5.00 EUR/pcs or weight > 0.2 kg", "order_by": [{ "field": "weight" }] }),
        json!({ "entity": "Product", "where": "code.starts_with(\"W\")", "order_by": [{ "field": "category" }] }),
        json!({ "entity": "Partner", "where": "as_customer != none", "order_by": [{ "field": "name" }] }),
        json!({ "entity": "Partner", "where": "currency == USD", "aggregates": { "lead_time": "max" } }),
        json!({ "entity": "Tax", "where": "rate > 10%", "order_by": [{ "field": "rate", "desc": true }], "aggregates": { "rate": "max" } }),
        json!({ "entity": "PaymentTermLine", "where": "days >= 30 days", "aggregates": { "percent": "sum", "days": "max" } }),
        json!({ "entity": "BomLine", "where": "scrap > 0% and qty > 0.1 kg", "order_by": [{ "field": "qty" }] }),
        json!({ "entity": "Operation", "where": "run > 5 min", "aggregates": { "run": "sum", "setup": "avg" } }),
        json!({ "entity": "Account", "where": "type in {Bank, Receivable} or currency == USD", "order_by": [{ "field": "code", "desc": true }] }),
        json!({ "entity": "ExchangeRate", "where": "rate > 0.93", "order_by": [{ "field": "date", "desc": true }], "aggregates": { "rate": "avg" } }),
        json!({ "entity": "WorkCenter", "where": "efficiency >= 100%", "order_by": [{ "field": "cost_per_hour" }] }),
    ];
    for q in &queries {
        same_query(&w, q, admin, None);
    }
    let product: u64 = pick(
        &w,
        "SELECT CAST(\"id\" AS TEXT) FROM \"product\" ORDER BY \"id\" LIMIT 1",
    )
    .parse()
    .unwrap();
    let exprs = [
        "StockMove.where(m => m.product == self).sum(m => m.qty.to(pcs))",
        "StockMove.where(m => m.product == self and m.qty > 0 pcs).count()",
        "ExchangeRate.where(r => r.base.code == \"USD\" and r.date <= 2026-11-10).last_by(r => r.date)",
        "ExchangeRate.where(r => r.rate > 0.5).max(r => r.date)",
        "BomLine.where(l => l.product == self or l.bom.product == self).count()",
        "Product.where(p => p.category == self.category).sort_by(p => p.code).first()",
        "Operation.any(o => o.run > 5 min)",
        "Operation.where(o => o.setup >= 0 min).sum(o => o.run)",
        "WorkCenter.where(c => c.efficiency > 50%).count()",
        "Tax.where(t => t.rate >= 22%).count()",
    ];
    same_values(&w, "Product", product, &exprs, &|_| Ok(()));
    cleanup(target, &w.url);
}

#[test]
fn extended_queries_on_sqlite() {
    extended_on("sqlite");
}

#[test]
fn extended_queries_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped");
        return;
    };
    extended_on(&pg);
}
