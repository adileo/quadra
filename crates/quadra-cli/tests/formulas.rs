//! Incremental formulas read back from their columns give the interpreter's answers
//! (the notes on scaling, phase 3).
//!
//! Long random sequences of commits on the stress `tiny` world (`quadra bench gen`) and on
//! `examples/extended` (its fixture): quantities, prices and discounts of lines changed, lines
//! and shipments created, customers moved between regions, products reassigned to categories,
//! categories moved in the tree, orders confirmed, shipped and cancelled (frozen), and commits
//! that fail and must leave nothing. After each commit every materialized column equals a
//! recomputation from the history (`db verify`), and the records and grids read with their
//! stored formulas and projection rows equal those read the reference way (records from their
//! history, every formula computed). Conflicts: a commit is refused when another changed a
//! value it relied on, never when the reference would not refuse it, and nothing is lost when
//! it is not refused. On SQLite and, when `QUADRA_TEST_PG` holds a url, on PostgreSQL.

use quadra_runtime::{Flow, Interp, Store, V};
use quadra_store::conn::{Conn, Pooled};
use quadra_store::schema::Schema;
use quadra_store::source::{head, DbSource};
use serde_json::{json, Value as J};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

fn clock() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 11, 20).unwrap().and_hms_opt(10, 0, 0).unwrap()
}

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples").join(name)
}

fn fresh(target: &str, name: &str) -> String {
    if target == "sqlite" {
        let p = std::env::temp_dir().join(format!("quadra-formulas-{}-{name}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", p.display()));
        }
        format!("sqlite:{}", p.display())
    } else {
        let schema = format!("qf_{}_{name}", std::process::id());
        Conn::open(target).unwrap().batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")).unwrap();
        format!("{target}#schema={schema}")
    }
}

fn cleanup(target: &str, url: &str) {
    if let Some(path) = url.strip_prefix("sqlite:") {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
    } else if let Some((_, schema)) = url.split_once("#schema=") {
        let _ = Conn::open(target).and_then(|mut c| c.batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")));
    }
}

/// A small deterministic generator (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn pick<T: Clone>(&mut self, v: &[T]) -> Option<T> {
        if v.is_empty() {
            None
        } else {
            Some(v[self.below(v.len())].clone())
        }
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

/// How a commit ended.
#[derive(Debug, PartialEq)]
enum Outcome {
    Done,
    Refused(String),
    Conflict,
}

impl World {
    fn new(root: &Path, url: String) -> World {
        let project = quadra_sema::Project::load(root).unwrap();
        let sources: Vec<(PathBuf, String)> = project.user_files().map(|f| (f.path.clone(), f.text.clone())).collect();
        let checked = quadra_analysis::check(&project);
        assert!(checked.model.is_some(), "{} does not check", root.display());
        let schema = Rc::new(quadra_store::schema::build(checked.model.as_ref().unwrap(), &checked.report));
        World { root: root.to_path_buf(), url, sources, checked, schema, overlays: RefCell::new(Default::default()) }
    }

    /// The model with formulas added to an entity (as the server checks a query's `where`).
    fn overlay(&self, entity: &str, items: &[(String, String)]) -> Rc<quadra_analysis::Checked> {
        let key = format!("{entity}|{items:?}");
        if let Some(c) = self.overlays.borrow().get(&key) {
            return c.clone();
        }
        let mut sources = self.sources.clone();
        if !items.is_empty() {
            sources.push((self.root.join("tests/__api__.qd"), quadra_api::overlay_source(entity, items)));
        }
        let project = quadra_sema::Project::from_sources(&self.root, sources);
        let c = quadra_analysis::check(&project);
        assert!(c.model.is_some(), "{entity}: {items:?} does not check");
        let c = Rc::new(c);
        self.overlays.borrow_mut().insert(key, c.clone());
        c
    }

    fn model(&self) -> &quadra_ir::Model {
        self.checked.model.as_ref().unwrap()
    }

    /// An interpreter over the database: `fast` reads records from their projection rows with
    /// their stored formulas and answers queries in SQL; otherwise the reference (records from
    /// the history, every formula computed, the interpreter scanning).
    fn interp(&self, fast: bool, user: Option<u64>, base: Option<u64>) -> Interp<'_> {
        self.interp_on(self.model(), fast, user, base)
    }

    fn interp_on<'m>(&self, model: &'m quadra_ir::Model, fast: bool, user: Option<u64>, base: Option<u64>) -> Interp<'m> {
        let mut conn = Conn::open(&self.url).unwrap();
        let base = base.unwrap_or_else(|| head(&mut conn).unwrap());
        let mut src = DbSource::at(Pooled::own(conn), base, self.schema.clone(), None).unwrap();
        src.sql = fast;
        src.rows = fast;
        let mut i = Interp::new(model);
        i.set_clock(clock());
        i.store = Store::over(Rc::new(RefCell::new(src)));
        i.load_units();
        i.company = i.ids("Company", None).first().copied();
        i.user = user;
        i
    }

    fn head(&self) -> u64 {
        head(&mut Conn::open(&self.url).unwrap()).unwrap()
    }

    fn user(&self, login: &str) -> u64 {
        let mut i = self.interp(false, None, None);
        let ids = i.ids("User", None);
        ids.into_iter().find(|id| i.store.read(*id, "login", None).map_or(false, |v| v.to_string() == login)).unwrap_or_else(|| panic!("no user {login}"))
    }

    fn ids(&self, sql: &str) -> Vec<u64> {
        Conn::open(&self.url).unwrap().query(sql, &[]).unwrap().into_iter().filter_map(|r| r[0].as_deref().and_then(|x| x.parse().ok())).collect()
    }

    /// API operations committed as the server commits them, on a working set read at `base`.
    fn commit_at(&self, fast: bool, user: u64, base: Option<u64>, ops: &J) -> Outcome {
        let model = self.model();
        let mut i = self.interp(fast, Some(user), base);
        let db = quadra_store::Database::with(Pooled::own(Conn::open(&self.url).unwrap()), self.schema.clone());
        i.materialize = quadra_store::materialized_formulas(&db.schema);
        i.sink = Some(Box::new(quadra_store::Sink { db, model }));
        let ops = ops.as_array().unwrap().clone();
        let r = i.with_cause("test".into(), |i| i.transaction(|i| quadra_api::apply_ops(i, model, &ops).map(|_| ())));
        match r {
            Ok(()) => Outcome::Done,
            Err(Flow::Conflict(_)) => Outcome::Conflict,
            Err(Flow::Fail(m)) => Outcome::Refused(m),
            Err(e) => Outcome::Refused(format!("{e:?}")),
        }
    }

    fn commit(&self, user: u64, ops: &J) -> Outcome {
        let before = self.head();
        let r = self.commit_at(true, user, None, ops);
        if r != Outcome::Done {
            assert_eq!(self.head(), before, "a refused commit wrote something: {ops}");
        }
        r
    }

    /// Every materialized column against a recomputation from the history (streamed, and
    /// in memory: the same findings).
    fn verify(&self) {
        let (problems, n) = quadra_store::ops::verify(&self.url, self.model(), &self.checked.report, clock()).unwrap();
        assert!(problems.is_empty(), "verify: {}", problems.into_iter().take(10).collect::<Vec<_>>().join("\n"));
        let (reference, m) = quadra_store::ops::verify_in_memory(&self.url, self.model(), &self.checked.report, clock()).unwrap();
        assert!(reference.is_empty() && n == m, "verify in memory: {reference:?}, {m} records against {n}");
    }

    /// The problems both ways, sorted (the in-memory verify lists them in no fixed order).
    fn verify_both(&self) -> (Vec<String>, Vec<String>) {
        let (mut a, _) = quadra_store::ops::verify(&self.url, self.model(), &self.checked.report, clock()).unwrap();
        let (mut b, _) = quadra_store::ops::verify_in_memory(&self.url, self.model(), &self.checked.report, clock()).unwrap();
        a.sort();
        b.sort();
        (a, b)
    }

    /// A record read with its stored formulas and from its projection row, and the reference way.
    fn same_record(&self, entity: &str, id: u64, user: u64) {
        let mut fast = self.interp(true, Some(user), None);
        let got = quadra_api::record_json(&mut fast, self.model(), entity, id);
        let mut slow = self.interp(false, Some(user), None);
        let want = quadra_api::record_json(&mut slow, self.model(), entity, id);
        let show = |r: &Result<J, (u16, String)>| match r {
            Ok(j) => j.clone(),
            Err((s, m)) => json!({ "status": s, "error": m }),
        };
        assert_eq!(show(&got), show(&want), "{entity}#{id}: read with the stored formulas and the reference way differ");
    }

    /// A query read with SQL and the stored formulas, and the reference way.
    fn same_query(&self, q: &J, user: u64) {
        let (entity, items, columns) = quadra_api::query_items(q).unwrap();
        let c = self.overlay(&entity, &items);
        let model = c.model.as_ref().unwrap();
        let mut fast = self.interp_on(model, true, Some(user), None);
        let got = quadra_api::query_rows(&mut fast, model, &entity, q, &items, &columns);
        let mut slow = self.interp_on(model, false, Some(user), None);
        let want = quadra_api::query_rows(&mut slow, model, &entity, q, &items, &columns);
        let strip = |r: &Result<J, (u16, String)>| match r {
            Ok(j) => {
                let mut j = j.clone();
                j.as_object_mut().unwrap().remove("next");
                j
            }
            Err((s, m)) => json!({ "status": s, "error": m }),
        };
        assert_eq!(strip(&got), strip(&want), "query {q}: with the stored formulas and the reference way differ");
    }
}

// ---- the stress world ------------------------------------------------------------------------

fn stress_world(target: &str, name: &str) -> World {
    let root = example("stress");
    let url = fresh(target, name);
    let dir = std::env::temp_dir().join(format!("quadra-formulas-{}-gen-{name}-{}", std::process::id(), if target == "sqlite" { "s" } else { "p" }));
    let _ = std::fs::remove_dir_all(&dir);
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_quadra")).args(["bench", "gen", "--scale", "tiny", "--out"]).arg(&dir).output().unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
    let w = World::new(&root, url.clone());
    quadra_store::ops::init(&url, w.model(), &w.checked.report, clock()).unwrap();
    quadra_store::bulk::import_dir(&url, w.model(), &w.checked.report, &dir, clock(), None, &mut |_| {}).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    World::new(&root, url)
}

/// Which formulas of the stress model are incremental.
fn stress_classification(w: &World) {
    let inc: Vec<String> = w
        .schema
        .tables
        .iter()
        .flat_map(|t| t.fields.iter().filter(|f| f.formula && f.incremental).map(move |f| format!("{}.{}", t.name, f.field)))
        .collect();
    for want in [
        "region.revenue",
        "region.open_total",
        "region.customer_count",
        "region.best_customer",
        "region.to_ship",
        "customer.revenue",
        "customer.open_total",
        "customer.avg_order",
        "customer.first_order",
        "order.total",
        "order.to_ship",
        "order_line.amount",
        "order_line.shipped",
        "order_line.to_ship",
        "product.units_sold",
        "category.units_sold",
        "category.product_count",
        "tag.customer_count",
        // by recursion over the tree
        "category.depth",
        "category.path",
    ] {
        assert!(inc.iter().any(|x| x == want), "{want} is not incremental: {inc:?}");
    }
}

/// The reads the stress world's formulas show: records of every entity, grids ordered,
/// filtered and aggregated by formulas.
fn stress_reads(w: &World, admin: u64, ids: &[(String, u64)]) {
    for (e, id) in ids {
        w.same_record(e, *id, admin);
    }
    for q in [
        json!({ "entity": "Region", "fields": ["code", "revenue", "open_total", "customer_count", "best_customer", "to_ship", "open_orders"], "limit": 100 }),
        json!({ "entity": "Customer", "fields": ["code", "revenue", "avg_order", "first_order", "last_order"], "order_by": [{ "field": "revenue", "desc": true }], "limit": 10 }),
        json!({ "entity": "Customer", "fields": ["code", "open_total"], "where": "open_total > 100.00 EUR", "order_by": [{ "field": "open_total" }], "aggregates": { "revenue": "sum", "open_total": "max" }, "limit": 50 }),
        json!({ "entity": "Order", "fields": ["number", "total", "to_ship", "line_count"], "where": "total > 500.00 EUR and state is Confirmed", "order_by": [{ "field": "total", "desc": true }], "limit": 30 }),
        json!({ "entity": "OrderLine", "fields": ["amount", "shipped", "to_ship", "last_shipped"], "order_by": [{ "field": "to_ship", "desc": true }], "limit": 40 }),
        json!({ "entity": "Category", "fields": ["code", "units_sold", "product_count", "depth", "path"], "order_by": [{ "field": "units_sold" }], "limit": 100 }),
        json!({ "entity": "Category", "fields": ["code", "path"], "where": "depth >= 2", "order_by": [{ "field": "path", "desc": true }], "limit": 100 }),
        json!({ "entity": "Product", "fields": ["sku", "units_sold", "line_count"], "where": "units_sold >= 10 pcs", "aggregates": { "units_sold": "sum" }, "limit": 20 }),
    ] {
        w.same_query(&q, admin);
    }
}

/// One random operation on the stress world (it may be refused by the rules).
fn stress_op(w: &World, rng: &mut Rng) -> (J, Vec<(String, u64)>) {
    let lines = w.ids("SELECT CAST(l.\"id\" AS TEXT) FROM \"order_line\" l JOIN \"order\" o ON o.\"id\" = l.\"order_id\" WHERE o.\"state\" IN ('Draft', 'Confirmed') ORDER BY l.\"id\"");
    let open = w.ids("SELECT CAST(\"id\" AS TEXT) FROM \"order\" WHERE \"state\" IN ('Draft', 'Confirmed') ORDER BY \"id\"");
    let confirmed = w.ids("SELECT CAST(l.\"id\" AS TEXT) FROM \"order_line\" l JOIN \"order\" o ON o.\"id\" = l.\"order_id\" WHERE o.\"state\" = 'Confirmed' ORDER BY l.\"id\"");
    let shipments = w.ids("SELECT CAST(s.\"id\" AS TEXT) FROM \"shipment\" s JOIN \"order_line\" l ON l.\"id\" = s.\"line_id\" JOIN \"order\" o ON o.\"id\" = l.\"order_id\" WHERE o.\"state\" = 'Confirmed' ORDER BY s.\"id\"");
    let customers = w.ids("SELECT CAST(\"id\" AS TEXT) FROM \"customer\" ORDER BY \"id\"");
    let regions = w.ids("SELECT CAST(\"id\" AS TEXT) FROM \"region\" ORDER BY \"id\"");
    let products = w.ids("SELECT CAST(\"id\" AS TEXT) FROM \"product\" ORDER BY \"id\"");
    let categories = w.ids("SELECT CAST(\"id\" AS TEXT) FROM \"category\" ORDER BY \"id\"");
    let qty = |rng: &mut Rng| format!("{} pcs", 1 + rng.below(25));
    let one = |rng: &mut Rng, v: &[u64]| rng.pick(v).unwrap_or(0);
    let k = rng.below(16);
    let (op, touched) = match k {
        0 | 1 => {
            let l = one(rng, &lines);
            (json!({ "op": "update", "entity": "OrderLine", "id": l, "values": { "qty": qty(rng) } }), vec![("OrderLine".to_string(), l)])
        }
        2 => {
            let l = one(rng, &lines);
            (json!({ "op": "update", "entity": "OrderLine", "id": l, "values": { "discount": format!("{}%", rng.below(40)), "price": format!("{}.{} EUR/pcs", 1 + rng.below(90), rng.below(100)) } }), vec![("OrderLine".to_string(), l)])
        }
        3 => {
            let o = one(rng, &open);
            let p = one(rng, &products);
            (json!({ "op": "create", "entity": "OrderLine", "values": { "order": o, "product": p, "qty": qty(rng) } }), vec![("Order".to_string(), o), ("Product".to_string(), p)])
        }
        4 | 5 => {
            let l = one(rng, &confirmed);
            (json!({ "op": "create", "entity": "Shipment", "values": { "line": l, "qty": format!("{} pcs", 1 + rng.below(3)), "date": "2026-12-01", "carrier": "Rail" } }), vec![("OrderLine".to_string(), l)])
        }
        6 => {
            let s = one(rng, &shipments);
            (json!({ "op": "update", "entity": "Shipment", "id": s, "values": { "qty": format!("{} pcs", 1 + rng.below(4)) } }), vec![("Shipment".to_string(), s)])
        }
        7 | 8 => {
            let c = one(rng, &customers);
            let to = one(rng, &regions);
            (json!({ "op": "update", "entity": "Customer", "id": c, "values": { "region": to } }), vec![("Customer".to_string(), c), ("Region".to_string(), to)])
        }
        9 => {
            let p = one(rng, &products);
            let c = one(rng, &categories);
            (json!({ "op": "update", "entity": "Product", "id": p, "values": { "category": c } }), vec![("Product".to_string(), p), ("Category".to_string(), c)])
        }
        10 => {
            let c = one(rng, &categories);
            let p = one(rng, &categories);
            (json!({ "op": "update", "entity": "Category", "id": c, "values": { "parent": p } }), vec![("Category".to_string(), c)])
        }
        11 => {
            let o = one(rng, &open);
            let action = ["confirm", "cancel", "ship"][rng.below(3)];
            (json!({ "op": "action", "entity": "Order", "id": o, "action": action }), vec![("Order".to_string(), o)])
        }
        12 => {
            let c = one(rng, &customers);
            (json!({ "op": "update", "entity": "Customer", "id": c, "values": { "active": rng.below(4) != 0, "credit_limit": format!("{}.00 EUR", 1000 + 1000 * rng.below(60)) } }), vec![("Customer".to_string(), c)])
        }
        13 => {
            let p = one(rng, &products);
            (json!({ "op": "update", "entity": "Product", "id": p, "values": { "active": rng.below(3) != 0 } }), vec![("Product".to_string(), p)])
        }
        14 => {
            // refused: a quantity must be positive
            let l = one(rng, &lines);
            (json!({ "op": "update", "entity": "OrderLine", "id": l, "values": { "qty": "0 pcs" } }), vec![("OrderLine".to_string(), l)])
        }
        _ => {
            // several writes in one commit: a line moved to another order, quantities changed
            let l = one(rng, &lines);
            let o = one(rng, &open);
            let l2 = one(rng, &lines);
            (
                json!([{ "op": "update", "entity": "OrderLine", "id": l, "values": { "order": o } }, { "op": "update", "entity": "OrderLine", "id": l2, "values": { "qty": qty(rng) } }]),
                vec![("OrderLine".to_string(), l), ("OrderLine".to_string(), l2), ("Order".to_string(), o)],
            )
        }
    };
    let ops = if op.is_array() { op } else { json!([op]) };
    (ops, touched)
}

/// The records whose formulas a write to `touched` may change: up the chain, and a sample.
fn neighbourhood(w: &World, touched: &[(String, u64)], rng: &mut Rng) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = touched.to_vec();
    let mut i = w.interp(false, None, None);
    let add = |out: &mut Vec<(String, u64)>, e: &str, v: Option<V>| {
        if let Some(id) = v.and_then(|v| v.as_ref_id()) {
            out.push((e.to_string(), id));
        }
    };
    for (e, id) in touched.to_vec() {
        match e.as_str() {
            "Shipment" => add(&mut out, "OrderLine", i.store.read(id, "line", None)),
            _ => {}
        }
    }
    for (e, id) in out.clone() {
        if e == "OrderLine" {
            add(&mut out, "Order", i.store.read(id, "order", None));
            add(&mut out, "Product", i.store.read(id, "product", None));
        }
    }
    for (e, id) in out.clone() {
        match e.as_str() {
            "Order" => add(&mut out, "Customer", i.store.read(id, "customer", None)),
            "Product" => add(&mut out, "Category", i.store.read(id, "category", None)),
            _ => {}
        }
    }
    for (e, id) in out.clone() {
        if e == "Customer" {
            add(&mut out, "Region", i.store.read(id, "region", None));
        }
    }
    for e in ["Region", "Customer", "Order", "OrderLine", "Category", "Product", "Tag"] {
        let ids = i.ids(e, None);
        for _ in 0..2 {
            if let Some(id) = rng.pick(&ids) {
                out.push((e.to_string(), id));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn stress_sequence(w: &World, steps: usize, seed: u64) {
    let admin = w.user("admin");
    let mut rng = Rng(seed);
    let (mut done, mut refused) = (0, 0);
    for step in 0..steps {
        let (ops, touched) = stress_op(w, &mut rng);
        let out = w.commit(admin, &ops);
        match &out {
            Outcome::Done => done += 1,
            Outcome::Refused(_) => refused += 1,
            Outcome::Conflict => panic!("a conflict without concurrent commits: {ops}"),
        }
        let near = neighbourhood(w, &touched, &mut rng);
        for (e, id) in &near {
            w.same_record(e, *id, admin);
        }
        if step % 10 == 9 || step + 1 == steps {
            w.verify();
            stress_reads(w, admin, &[]);
        }
    }
    let n = |c: &std::sync::atomic::AtomicU64| c.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "{done} commits done, {refused} refused; formulas: {} stored values read, {} by delta, {} computed",
        n(&quadra_runtime::FORMULAS_STORED),
        n(&quadra_runtime::FORMULAS_BY_DELTA),
        n(&quadra_runtime::FORMULAS_COMPUTED)
    );
    assert!(done > steps / 3 && refused > 0, "{done} commits done, {refused} refused: the sequence should do both");
}

/// Two transactions read at the same snapshot: the second is refused when it relied on a value
/// the first changed, as the reference refuses it; when it is not refused, nothing is lost.
fn stress_conflicts(w: &World) {
    let admin = w.user("admin");
    let lines_of = |order: u64| w.ids(&format!("SELECT CAST(\"id\" AS TEXT) FROM \"order_line\" WHERE \"order_id\" = {order} ORDER BY \"id\""));
    let order = w.ids("SELECT CAST(o.\"id\" AS TEXT) FROM \"order\" o WHERE o.\"state\" = 'Confirmed' AND (SELECT COUNT(*) FROM \"order_line\" l WHERE l.\"order_id\" = o.\"id\") >= 2 ORDER BY o.\"id\" LIMIT 1")[0];
    let ls = lines_of(order);
    let customer = w.ids(&format!("SELECT CAST(\"customer_id\" AS TEXT) FROM \"order\" WHERE \"id\" = {order}"))[0];
    let region = w.ids(&format!("SELECT CAST(\"region_id\" AS TEXT) FROM \"customer\" WHERE \"id\" = {customer}"))[0];
    let other_order = w.ids(&format!("SELECT CAST(o.\"id\" AS TEXT) FROM \"order\" o JOIN \"customer\" c ON c.\"id\" = o.\"customer_id\" WHERE o.\"state\" = 'Confirmed' AND c.\"region_id\" = {region} AND o.\"customer_id\" <> {customer} ORDER BY o.\"id\" LIMIT 1"))[0];
    let other_line = lines_of(other_order)[0];
    let far = w.ids(&format!("SELECT CAST(o.\"id\" AS TEXT) FROM \"order\" o JOIN \"customer\" c ON c.\"id\" = o.\"customer_id\" WHERE o.\"state\" = 'Draft' AND c.\"region_id\" <> {region} ORDER BY o.\"id\" LIMIT 1"))[0];
    let qty = |l: u64, q: &str| json!([{ "op": "update", "entity": "OrderLine", "id": l, "values": { "qty": q } }]);
    let cases: Vec<(&str, J, J, bool)> = vec![
        // the same order's total, the same customer's and region's rollups
        ("two lines of one order", qty(ls[0], "7 pcs"), qty(ls[1], "9 pcs"), true),
        // two customers of one region: the region's revenue
        ("two customers of one region", qty(ls[0], "8 pcs"), qty(other_line, "11 pcs"), true),
        // the same line twice
        ("one line twice", qty(ls[1], "3 pcs"), qty(ls[1], "4 pcs"), true),
        // a draft order of another region: no rollup in common but the category's
        ("another region", qty(ls[0], "6 pcs"), json!([{ "op": "update", "entity": "Order", "id": far, "values": { "priority": "High" } }]), false),
    ];
    for (what, first, second, must) in cases {
        let mut outcomes = Vec::new();
        for fast in [true, false] {
            let base = w.head();
            assert_eq!(w.commit_at(fast, admin, Some(base), &first), Outcome::Done, "{what}: the first commit");
            let out = w.commit_at(fast, admin, Some(base), &second);
            outcomes.push(out);
            w.verify();
        }
        let (fast, reference) = (&outcomes[0], &outcomes[1]);
        if must {
            assert_eq!(*fast, Outcome::Conflict, "{what}: the second commit must be refused");
        }
        assert!(*fast != Outcome::Conflict || *reference == Outcome::Conflict, "{what}: refused where the reference is not: {outcomes:?}");
    }
}

fn stress_on(target: &str, steps: usize) {
    let w = stress_world(target, "stress");
    stress_classification(&w);
    let admin = w.user("admin");
    let sample: Vec<(String, u64)> = ["Region", "Customer", "Order", "OrderLine", "Shipment", "Category", "Product", "Tag"]
        .iter()
        .flat_map(|e| w.ids(&format!("SELECT CAST(\"id\" AS TEXT) FROM \"{}\" ORDER BY \"id\" LIMIT 4", quadra_store::schema::snake(e))).into_iter().map(move |id| (e.to_string(), id)))
        .collect();
    stress_reads(&w, admin, &sample);
    // cancelled orders (frozen): their formulas as of then
    for id in w.ids("SELECT CAST(\"id\" AS TEXT) FROM \"order\" WHERE \"state\" = 'Cancelled' ORDER BY \"id\" LIMIT 5") {
        w.same_record("Order", id, admin);
    }
    stress_sequence(&w, steps, 0x9e3779b97f4a7c15);
    stress_conflicts(&w);
    w.verify();
    cleanup(target, &w.url);
}

fn steps() -> usize {
    std::env::var("QUADRA_FORMULA_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(60)
}

#[test]
fn stress_formulas_on_sqlite() {
    stress_on("sqlite", steps());
}

#[test]
fn stress_formulas_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped");
        return;
    };
    stress_on(&pg, steps());
}


// ---- a fuzzer driven by the model (any project) ------------------------------------------------

/// The records of the concrete entities of the model, by entity.
fn records_by_entity(i: &mut Interp) -> Vec<(String, Vec<u64>)> {
    let m = i.m;
    let mut out = Vec::new();
    for e in m.entities.iter().filter(|e| e.kind == quadra_ir::EntityKind::Entity && !e.is_ledger && !e.temporary) {
        let ids = i.ids(&e.name, None);
        if !ids.is_empty() {
            out.push((e.name.clone(), ids));
        }
    }
    out
}

fn ty_entity(t: Option<&quadra_ir::Ty>) -> Option<String> {
    match t? {
        quadra_ir::Ty::Path { segments, .. } if segments.len() == 1 => Some(segments[0].clone()),
        quadra_ir::Ty::Optional(i) => ty_entity(Some(i)),
        _ => None,
    }
}

/// A value near `v` for a field of that kind (None: not one the fuzzer changes).
fn nudge(rng: &mut Rng, v: &V, f: &quadra_ir::Field, all: &[(String, Vec<u64>)]) -> Option<V> {
    use rust_decimal::Decimal;
    let small = |rng: &mut Rng| Decimal::from(1 + rng.below(20) as i64);
    Some(match v {
        V::Int(n) => V::Int((n + rng.below(7) as i64 - 3).max(0)),
        V::Dec(_) => V::Dec(small(rng) / Decimal::from(4)),
        V::Pct(_) => V::Pct(Decimal::from(rng.below(30) as i64) / Decimal::from(100)),
        V::Money { cur, .. } => V::Money { amt: small(rng) * Decimal::from(5) + Decimal::new(rng.below(100) as i64, 2), cur: cur.clone() },
        V::Qty { unit, .. } => V::Qty { v: small(rng), unit: unit.clone() },
        V::Price { cur, per, .. } => V::Price { amt: small(rng) + Decimal::new(50, 2), cur: cur.clone(), per: per.clone() },
        V::Bool(b) => V::Bool(!b),
        V::Date(d) => V::Date(*d + chrono::Duration::days(rng.below(60) as i64 - 30)),
        V::Ref { .. } => {
            let target = ty_entity(f.ty.as_ref())?;
            let ids = &all.iter().find(|(e, _)| *e == target)?.1;
            V::Ref { ent: Rc::from(target.as_str()), id: rng.pick(ids)? }
        }
        _ => return None,
    })
}

/// One random change in a transaction: a field of a record nudged, a line copied, an action
/// without parameters run. Returns the records it touched.
fn fuzz_once(i: &mut Interp, rng: &mut Rng) -> Result<Vec<(String, u64)>, Flow> {
    let m = i.m;
    let all = records_by_entity(i);
    let (ent, ids) = rng.pick(&all).ok_or(Flow::Fail("no records".into()))?;
    let e = m.entity(&ent).unwrap();
    let id = rng.pick(&ids).unwrap();
    let rec = V::Ref { ent: Rc::from(ent.as_str()), id };
    let flow_fields: Vec<&str> = e.flows.iter().map(|f| f.field.as_str()).collect();
    let stored: Vec<&quadra_ir::Field> = m
        .all_fields(e)
        .into_iter()
        .filter(|f| f.formula.is_none() && !f.per_company && f.kind == quadra_ir::FieldKind::Stored && !flow_fields.contains(&f.name.as_str()) && !matches!(f.ty, Some(quadra_ir::Ty::List(_)) | Some(quadra_ir::Ty::Set(_))))
        .collect();
    match rng.below(10) {
        // run an action (or a transition) without parameters
        0 | 1 => {
            let mut names: Vec<(String, bool)> = e.actions.iter().filter(|a| a.params.is_empty()).map(|a| (a.name.clone(), true)).collect();
            for fl in &e.flows {
                for t in &fl.transitions {
                    if let Some(b) = &t.by {
                        if !names.iter().any(|(n, _)| n == b) {
                            names.push((b.clone(), false));
                        }
                    }
                }
            }
            let Some((name, is_action)) = rng.pick(&names) else { return Ok(vec![(ent, id)]) };
            let action = if is_action { e.actions.iter().find(|a| a.name == name) } else { None };
            i.run_action(&rec, &ent, &name, action, Vec::new(), Vec::new())?;
        }
        // a copy of a line of a document (an entity with a parent), unique fields aside
        2 | 3 if e.fields.iter().any(|f| f.parent) && !m.all_fields(e).iter().any(|f| f.unique) => {
            let mut args = Vec::new();
            for f in &stored {
                if let Some(v) = i.store.read(id, &f.name, None) {
                    if !v.is_none() {
                        args.push((f.name.clone(), v));
                    }
                }
            }
            let new = i.create(&ent, args)?;
            return Ok(vec![(ent, new.as_ref_id().unwrap_or(id)), (e.name.clone(), id)]);
        }
        // a field nudged
        _ => {
            let Some(f) = rng.pick(&stored) else { return Ok(vec![(ent, id)]) };
            if f.unique {
                return Ok(vec![(ent, id)]);
            }
            let cur = i.get(&rec, &f.name, None, None)?;
            if let Some(v) = nudge(rng, &cur, f, &all) {
                i.set(&rec, &f.name, v, None)?;
            }
        }
    }
    Ok(vec![(ent, id)])
}

/// Records related to `touched`: the records their references point to and their documents,
/// and a sample of every entity.
fn related(w: &World, touched: &[(String, u64)], rng: &mut Rng) -> Vec<(String, u64)> {
    let mut i = w.interp(false, None, None);
    let m = w.model();
    let mut out: Vec<(String, u64)> = Vec::new();
    let mut todo: Vec<(String, u64, usize)> = touched.iter().map(|(e, id)| (e.clone(), *id, 0)).collect();
    while let Some((e, id, depth)) = todo.pop() {
        if out.iter().any(|(x, y)| *x == e && *y == id) || !i.store.exists(id, None) {
            continue;
        }
        let actual = i.store.get(id).map(|r| r.ent.to_string()).unwrap_or(e);
        out.push((actual.clone(), id));
        if depth >= 4 {
            continue;
        }
        let Some(ent) = m.entity(&actual) else { continue };
        for f in m.all_fields(ent) {
            if f.formula.is_some() {
                continue;
            }
            if let (Some(target), Some(V::Ref { id: t, .. })) = (ty_entity(f.ty.as_ref()), i.store.read(id, &f.name, None)) {
                todo.push((target, t, depth + 1));
            }
        }
    }
    for (e, ids) in records_by_entity(&mut i) {
        if let Some(id) = rng.pick(&ids) {
            out.push((e, id));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn fuzz(w: &World, user: u64, steps: usize, seed: u64) {
    let mut rng = Rng(seed);
    let model = w.model();
    let (mut done, mut refused) = (0, 0);
    for step in 0..steps {
        let mut i = w.interp(true, Some(user), None);
        let db = quadra_store::Database::with(Pooled::own(Conn::open(&w.url).unwrap()), w.schema.clone());
        i.materialize = quadra_store::materialized_formulas(&db.schema);
        i.sink = Some(Box::new(quadra_store::Sink { db, model }));
        let before = w.head();
        let mut touched = Vec::new();
        let n = 1 + rng.below(3);
        let r = i.with_cause("fuzz".into(), |i| {
            i.transaction(|i| {
                for _ in 0..n {
                    touched.extend(fuzz_once(i, &mut rng)?);
                }
                Ok(())
            })
        });
        drop(i);
        match r {
            Ok(()) => done += 1,
            Err(Flow::Conflict(c)) => panic!("a conflict without concurrent commits: {c:?}"),
            Err(_) => {
                refused += 1;
                assert_eq!(w.head(), before, "a refused commit wrote something");
            }
        }
        for (e, id) in related(w, &touched, &mut rng) {
            w.same_record(&e, id, user);
        }
        if step % 10 == 9 || step + 1 == steps {
            w.verify();
        }
    }
    eprintln!("fuzz: {done} commits done, {refused} refused");
    assert!(done > 0, "no commit went through");
}

fn extended_on(target: &str, steps: usize) {
    let root = example("extended");
    let url = fresh(target, "extended");
    let w = World::new(&root, url.clone());
    quadra_store::ops::init(&url, w.model(), &w.checked.report, clock()).unwrap();
    quadra_store::ops::fixture(&url, w.model(), &w.checked.report, "bom_example", clock()).unwrap();
    let w = World::new(&root, url);
    let admin = w.user("admin");
    // every record, read both ways
    let mut i = w.interp(false, None, None);
    let all = records_by_entity(&mut i);
    drop(i);
    for (e, ids) in &all {
        for id in ids {
            w.same_record(e, *id, admin);
        }
    }
    fuzz(&w, admin, steps, 0x2545f4914f6cdd1d);
    w.verify();
    cleanup(target, &w.url);
}

#[test]
fn extended_formulas_on_sqlite() {
    extended_on("sqlite", steps());
}

#[test]
fn extended_formulas_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped");
        return;
    };
    extended_on(&pg, steps());
}

#[test]
fn stress_fuzz_on_sqlite() {
    let w = stress_world("sqlite", "fuzz");
    let admin = w.user("admin");
    fuzz(&w, admin, steps(), 0x51afd7ed558ccd);
    w.verify();
    cleanup("sqlite", &w.url);
}

/// `db verify` streamed (pages of 7 rows) finds what the in-memory verify finds, on a database
/// whose tables, formulas and history are altered one way after the other.
fn verify_altered(target: &str) {
    quadra_store::verify::PAGE.store(7, std::sync::atomic::Ordering::Relaxed);
    let w = stress_world(target, "verify");
    let (a, b) = w.verify_both();
    assert!(a.is_empty() && b.is_empty(), "{a:?} {b:?}");
    let one = |sql: &str| w.ids(sql)[0];
    let line = one("SELECT CAST(\"id\" AS TEXT) FROM \"order_line\" ORDER BY \"id\" LIMIT 1 OFFSET 20");
    let order = one(&format!("SELECT CAST(\"order_id\" AS TEXT) FROM \"order_line\" WHERE \"id\" = {line}"));
    let customer = one("SELECT CAST(\"id\" AS TEXT) FROM \"customer\" ORDER BY \"id\" LIMIT 1 OFFSET 3");
    let region = one("SELECT CAST(\"id\" AS TEXT) FROM \"region\" ORDER BY \"id\" LIMIT 1");
    let shipment = one("SELECT CAST(\"id\" AS TEXT) FROM \"shipment\" ORDER BY \"id\" DESC LIMIT 1");
    let category = one("SELECT CAST(\"id\" AS TEXT) FROM \"category\" WHERE \"parent_id\" IS NOT NULL ORDER BY \"id\" LIMIT 1");
    let alterations: Vec<(&str, String)> = vec![
        // a formula column of a line (its order's total reads it)
        ("a line's amount", format!("UPDATE \"order_line\" SET \"amount\" = 999 WHERE \"id\" = {line}")),
        // an order's total and its customer's revenue (one reads the other)
        ("rollups", format!("UPDATE \"order\" SET \"total\" = 1 WHERE \"id\" = {order}; UPDATE \"customer\" SET \"revenue\" = 2 WHERE \"id\" = {customer}; UPDATE \"region\" SET \"revenue\" = 3 WHERE \"id\" = {region}")),
        // a stored field
        ("a stored quantity", format!("UPDATE \"order_line\" SET \"qty\" = 77 WHERE \"id\" = {line}")),
        // a recursion over the tree
        ("a category's depth", format!("UPDATE \"category\" SET \"depth\" = 9 WHERE \"id\" = {category}")),
        // a row removed, a row the history does not have
        ("rows", format!("DELETE FROM \"shipment\" WHERE \"id\" = {shipment}; INSERT INTO \"tag\" (\"id\", \"_created_tx\", \"_created_at\", \"name\", \"color\") VALUES (987654, 1, '2026-01-01 00:00:00', 'ghost', 'Grey')")),
        // the history: a value changed, an entry without its commit
        ("history", format!("UPDATE \"_quadra_history\" SET \"value\" = '\"X\"' WHERE \"record_id\" = {customer} AND \"field\" = 'name'; INSERT INTO \"_quadra_history\" (\"seq\", \"tx\", \"record_id\", \"field\", \"value\") VALUES (99999999, 424242, {customer}, 'name', '\"Y\"')")),
    ];
    // one after the other: each check sees this alteration and those before it
    for (what, sql) in &alterations {
        let mut c = Conn::open(&w.url).unwrap();
        for stmt in sql.split("; ") {
            c.batch(stmt).unwrap();
        }
        drop(c);
        let (a, b) = w.verify_both();
        assert!(!a.is_empty(), "{what}: nothing found");
        assert_eq!(a, b, "{what}: the streamed verify and the in-memory one differ");
    }
    cleanup(target, &w.url);
}

#[test]
fn verify_streamed_on_sqlite() {
    verify_altered("sqlite");
}

#[test]
fn verify_streamed_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else { return };
    verify_altered(&pg);
}

/// The sum of a ledger's rows posted to a record (`Product.stock` of `examples/minimal`) is
/// kept by adding the rows each commit posts: products read with their stored stock and the
/// reference way agree after restocks (posted, changed: the rows taken back and posted again),
/// orders shipped (rows posted by a transition), a shipment refused by the ledger's check
/// (nothing written), and `db verify` agrees with the columns.
fn ledger_on(target: &str) {
    let root = example("minimal");
    let url = fresh(target, "ledger");
    let w = World::new(&root, url.clone());
    quadra_store::ops::init(&url, w.model(), &w.checked.report, clock()).unwrap();
    let w = World::new(&root, url);
    let inc: Vec<String> = w.schema.tables.iter().flat_map(|t| t.fields.iter().filter(|f| f.formula && f.incremental).map(move |f| format!("{}.{}", t.name, f.field))).collect();
    assert!(inc.iter().any(|x| x == "product.stock"), "Product.stock is not incremental: {inc:?}");
    let user = 0;
    let done = |ops: J| assert_eq!(w.commit(user, &ops), Outcome::Done, "{ops}");
    done(json!([
        { "op": "create", "entity": "Customer", "ref": "anna", "values": { "name": "Anna", "credit_limit": { "money": "100000.00", "cur": "EUR" } } },
        { "op": "create", "entity": "Product", "ref": "pen", "values": { "code": "PEN", "name": "Pen", "price": "1.50 EUR/pcs" } },
        { "op": "create", "entity": "Product", "ref": "ink", "values": { "code": "INK", "name": "Ink", "price": "4.00 EUR/pcs" } },
        { "op": "create", "entity": "Restock", "values": { "product": { "ref": "pen" }, "qty": "10 pcs" } },
    ]));
    let products = w.ids("SELECT CAST(\"id\" AS TEXT) FROM \"product\" ORDER BY \"id\"");
    let customer = w.ids("SELECT CAST(\"id\" AS TEXT) FROM \"customer\"")[0];
    let check = |what: &str| {
        for p in &products {
            w.same_record("Product", *p, user);
        }
        w.same_query(&json!({ "entity": "Product", "fields": ["code", "stock"], "order_by": [{ "field": "stock" }], "limit": 10 }), user);
        w.verify();
        eprintln!("ledger: {what} ok");
    };
    check("a restock");
    let (pen, ink) = (products[0], products[1]);
    done(json!([{ "op": "create", "entity": "Restock", "values": { "product": ink, "qty": "5 pcs" } }, { "op": "create", "entity": "Restock", "values": { "product": pen, "qty": "3 pcs" } }]));
    check("two restocks in one commit");
    // a restock changed: its rows taken back, posted again
    let r = w.ids("SELECT CAST(\"id\" AS TEXT) FROM \"restock\" ORDER BY \"id\"")[0];
    done(json!([{ "op": "update", "entity": "Restock", "id": r, "values": { "qty": "12 pcs" } }]));
    check("a restock changed");
    for (n, (p, q)) in [(pen, "4 pcs"), (ink, "2 pcs"), (pen, "6 pcs")].iter().enumerate() {
        done(json!([{ "op": "create", "entity": "Order", "ref": "o", "values": { "customer": customer, "date": "2026-11-01", "lines": [{ "product": p, "qty": q }] } }]));
        let o = *w.ids("SELECT CAST(\"id\" AS TEXT) FROM \"order\" ORDER BY \"id\" DESC LIMIT 1").first().unwrap();
        done(json!([{ "op": "action", "entity": "Order", "id": o, "action": "confirm" }]));
        done(json!([{ "op": "action", "entity": "Order", "id": o, "action": "ship" }]));
        check(&format!("order {n} shipped"));
    }
    // more than the stock: refused by the ledger's check, nothing written
    done(json!([{ "op": "create", "entity": "Order", "values": { "customer": customer, "date": "2026-11-02", "lines": [{ "product": ink, "qty": "50 pcs" }] } }]));
    let o = *w.ids("SELECT CAST(\"id\" AS TEXT) FROM \"order\" ORDER BY \"id\" DESC LIMIT 1").first().unwrap();
    done(json!([{ "op": "action", "entity": "Order", "id": o, "action": "confirm" }]));
    assert!(matches!(w.commit(user, &json!([{ "op": "action", "entity": "Order", "id": o, "action": "ship" }])), Outcome::Refused(m) if m.contains("Not enough stock")));
    check("a shipment refused");
    // the stock read in a transaction that posts (before and after its rows are posted, at
    // the checks): as the reference reads it
    let model = w.model();
    let mut seen: Vec<Vec<V>> = Vec::new();
    for fast in [true, false] {
        let mut i = w.interp(fast, Some(user), None);
        let mut vals: Vec<V> = Vec::new();
        let r: Result<(), Flow> = i.with_cause("test".into(), |i| {
            i.transaction(|i| {
                quadra_api::apply_ops(i, model, &[json!({ "op": "create", "entity": "Restock", "values": { "product": pen, "qty": "7 pcs" } })])?;
                vals.push(i.get(&V::Ref { ent: std::rc::Rc::from("Product"), id: pen }, "stock", None, None)?);
                i.settle()?;
                vals.push(i.get(&V::Ref { ent: std::rc::Rc::from("Product"), id: pen }, "stock", None, None)?);
                Err(Flow::Fail("rolled back".into()))
            })
        });
        assert!(matches!(r, Err(Flow::Fail(_))));
        seen.push(vals);
    }
    assert_eq!(seen[0], seen[1], "the stock in a transaction that posts, with the stored value and the reference way");
    cleanup(target, &w.url);
}

#[test]
fn ledger_formulas_on_sqlite() {
    ledger_on("sqlite");
}

#[test]
fn ledger_formulas_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else { return };
    ledger_on(&pg);
}
