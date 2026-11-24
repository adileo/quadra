//! Row permissions in SQL give the interpreter's answers (the notes on scaling, phase 2).
//!
//! For every user of a world and every entity, the rows the API lists (every page through the
//! cursor), counts (`total`), aggregates and finds (`GET /v1/search`) with the row rules
//! translated to SQL are exactly the rows the interpreter's `can read` allows, one by one; and
//! the answers equal those of the interpreter alone (`DbSource::sql` off). Then again after
//! writes that change the paths the rules follow: a salesperson reassigned, a customer moved
//! to another region, a region's manager, logistics and team changed, a region locked and
//! unlocked, an order cancelled (frozen). On `examples/stress` (the `tiny` world of `quadra
//! bench gen`, with the `Controller` role and its `deny read`) and `examples/extended`, on
//! SQLite and, when `QUADRA_TEST_PG` holds a url, on PostgreSQL. The tests assert that SQL
//! applied the rules (not the interpreter row by row) wherever they translate.

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
        let p = std::env::temp_dir().join(format!("quadra-perm-{}-{name}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", p.display()));
        }
        format!("sqlite:{}", p.display())
    } else {
        let schema = format!("qp_{}_{name}", std::process::id());
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

struct World {
    url: String,
    checked: quadra_analysis::Checked,
    schema: Rc<Schema>,
}

impl World {
    fn new(checked: quadra_analysis::Checked, url: String) -> World {
        assert!(checked.model.is_some(), "the project does not check");
        let schema = match quadra_store::Database::open(&url, checked.model.as_ref().unwrap(), &checked.report) {
            Ok(db) => db.schema.clone(),
            Err(_) => Rc::new(quadra_store::schema::build(checked.model.as_ref().unwrap(), &checked.report)),
        };
        World { url, checked, schema }
    }

    fn model(&self) -> &quadra_ir::Model {
        self.checked.model.as_ref().unwrap()
    }

    fn interp(&self, sql: bool, user: Option<u64>) -> Interp<'_> {
        let mut conn = Conn::open(&self.url).unwrap();
        let base = head(&mut conn).unwrap();
        let mut src = DbSource::at(Pooled::own(conn), base, self.schema.clone(), None).unwrap();
        src.sql = sql;
        let mut i = Interp::new(self.model());
        i.set_clock(clock());
        i.store = Store::over(Rc::new(RefCell::new(src)));
        i.load_units();
        i.company = i.ids("Company", None).first().copied();
        i.user = user;
        i
    }

    /// Every user: (login, id).
    fn users(&self) -> Vec<(String, u64)> {
        let mut i = self.interp(false, None);
        let mut out: Vec<(String, u64)> = i.ids("User", None).into_iter().map(|id| (i.store.read(id, "login", None).map(|v| v.to_string()).unwrap_or_default(), id)).collect();
        out.sort();
        out
    }

    fn pick(&self, sql: &str) -> u64 {
        Conn::open(&self.url).unwrap().query(sql, &[]).unwrap().into_iter().next().and_then(|r| r.into_iter().next().flatten()).and_then(|v| v.parse().ok()).unwrap_or_else(|| panic!("no row: {sql}"))
    }

    /// A transaction as the system (seeds, migrations: nothing checked), committed.
    fn write(&self, f: &dyn Fn(&mut Interp) -> Result<(), Flow>) {
        let model = self.model();
        let mut db = quadra_store::Database::open(&self.url, model, &self.checked.report).unwrap();
        let mut i = quadra_store::ops::loaded(&mut db, model, clock()).unwrap();
        i.materialize = quadra_store::materialized_formulas(&db.schema);
        i.sink = Some(Box::new(quadra_store::Sink { db, model }));
        i.transaction(|i| f(i)).unwrap_or_else(|e| panic!("the writes failed: {e:?}"));
    }
}

/// The entities a query can list.
fn entities(m: &quadra_ir::Model) -> Vec<String> {
    m.entities.iter().filter(|e| e.kind == quadra_ir::EntityKind::Entity && !e.temporary && !e.is_ledger).map(|e| e.name.clone()).collect()
}

/// A stored scalar field of the entity to aggregate (count and max), if any.
fn agg_field(m: &quadra_ir::Model, ent: &str) -> Option<String> {
    let e = m.entity(ent)?;
    m.all_fields(e)
        .iter()
        .find(|f| f.formula.is_none() && !f.per_company && matches!(&f.ty, Some(quadra_ir::Ty::Path { segments, .. }) if matches!(segments.last().map(String::as_str), Some("Date" | "Int" | "Text" | "Code"))))
        .map(|f| f.name.clone())
}

/// Group queries of an entity (`group_by`): by each kind of key it has (a reference, a path of
/// two references, a choice or text, a date by month, a yes/no, an integer), two keys, a pivot,
/// ordered by count, with a count and a sum.
fn group_queries(m: &quadra_ir::Model, ent: &str) -> Vec<J> {
    use quadra_ir::Ty;
    let Some(e) = m.entity(ent) else { return Vec::new() };
    let fields = m.all_fields(e);
    let base = |f: &quadra_ir::Field| match &f.ty {
        Some(Ty::Optional(t)) => Some(t.as_ref().clone()),
        t => t.clone(),
    };
    let named = |f: &quadra_ir::Field, n: &[&str]| matches!(base(f), Some(Ty::Path { segments, .. }) if segments.last().is_some_and(|s| n.contains(&s.as_str())));
    let refs = |f: &quadra_ir::Field| match base(f) {
        Some(Ty::Path { segments, .. }) => m.entity(segments.last().unwrap()).filter(|x| x.kind == quadra_ir::EntityKind::Entity).map(|x| x.name.clone()),
        _ => None,
    };
    let stored: Vec<&quadra_ir::Field> = fields.iter().copied().filter(|f| f.formula.is_none() && !f.per_company && !f.name.starts_with("__")).collect();
    let mut keys: Vec<J> = Vec::new();
    if let Some(f) = stored.iter().find(|f| refs(f).is_some()) {
        keys.push(json!(f.name));
        let target = refs(f).unwrap();
        if let Some(g) = m.entity(&target).and_then(|t| m.all_fields(t).into_iter().find(|g| g.formula.is_none() && !g.per_company && refs(g).is_some())) {
            keys.push(json!(format!("{}.{}", f.name, g.name)));
        }
    }
    if let Some(f) = stored.iter().find(|f| matches!(base(f), Some(Ty::Enum(_))) || quadra_runtime::query::text_type(f.ty.as_ref())) {
        keys.push(json!(f.name));
    }
    if let Some(f) = stored.iter().find(|f| named(f, &["Date", "DateTime"])) {
        keys.push(json!({ "field": f.name, "bucket": "month" }));
        keys.push(json!({ "field": f.name, "bucket": "week" }));
    }
    for k in [&["Bool"][..], &["Int"][..]] {
        if let Some(f) = stored.iter().find(|f| named(f, k)) {
            keys.push(json!(f.name));
        }
    }
    let mut aggs = serde_json::Map::new();
    if let Some(f) = fields.iter().find(|f| !f.per_company && named(f, &["Money", "Qty", "Int", "Decimal"])) {
        aggs.insert(f.name.clone(), json!("sum"));
    }
    if let Some(f) = agg_field(m, ent) {
        aggs.insert(f, json!("count"));
    }
    let mut out: Vec<J> = keys.iter().map(|k| json!({ "entity": ent, "group_by": [k], "aggregates": aggs, "limit": 1000 })).collect();
    if keys.len() >= 2 {
        let two = json!([keys[0], keys[keys.len() - 1]]);
        out.push(json!({ "entity": ent, "group_by": two, "aggregates": aggs, "limit": 7, "offset": 2 }));
        out.push(json!({ "entity": ent, "group_by": two, "pivot": true, "aggregates": aggs, "limit": 1000 }));
        out.push(json!({ "entity": ent, "group_by": [keys[0]], "group_order": { "by": "count", "desc": true }, "limit": 1000 }));
        if let Some((path, f)) = aggs.iter().next().filter(|(_, f)| *f == "sum") {
            out.push(json!({ "entity": ent, "group_by": [keys[1]], "aggregates": { path.clone(): f }, "group_order": { "by": path, "desc": true }, "limit": 1000 }));
        }
    }
    out
}

/// Groups compared with SQL and without: everything but whether SQL computed them.
fn groups_comparable(r: &Result<J, (u16, String)>) -> J {
    let mut j = comparable(r);
    if let Some(o) = j.as_object_mut() {
        o.remove("sql");
    }
    j
}

/// For one user and entity: groups in SQL equal the interpreter's, their rows add up to the
/// readable rows, and a group's `filter` lists exactly its rows; the full-text search finds
/// readable rows only, the same with SQL and without, and only by fields the user may see.
fn check_groups_and_search(w: &World, login: &str, user: u64, ent: &str, readable: &[u64]) {
    let model = w.model();
    for q in group_queries(model, ent) {
        let mut si = w.interp(true, Some(user));
        let got = quadra_api::query_rows(&mut si, model, ent, &q, &[], &[]);
        let mut ri = w.interp(false, Some(user));
        let want = quadra_api::query_rows(&mut ri, model, ent, &q, &[], &[]);
        assert_eq!(groups_comparable(&got), groups_comparable(&want), "{login} {ent}: groups {q} with SQL and without differ");
        let Ok(g) = got else { continue };
        let groups = g["groups"].as_array().unwrap();
        if q["offset"].is_null() && q["group_by"].as_array().unwrap().len() == 1 {
            let n: u64 = groups.iter().map(|x| x["count"].as_u64().unwrap()).sum();
            assert_eq!(n, readable.len() as u64, "{login} {ent}: the groups of {q} hold {n} rows, {} readable", readable.len());
            assert_eq!(g["groups_total"].as_u64(), Some(groups.len() as u64));
        }
        // the rows of the first group with a value: exactly its count
        if let Some(first) = groups.iter().find(|x| x["keys"].as_array().unwrap().iter().all(|k| k["hidden"].is_null())) {
            let rq = json!({ "entity": ent, "fields": [], "group_filter": first["filter"], "limit": 1000 });
            let mut si = w.interp(true, Some(user));
            let rows = quadra_api::query_rows(&mut si, model, ent, &rq, &[], &[]).unwrap();
            let mut ri = w.interp(false, Some(user));
            let rows2 = quadra_api::query_rows(&mut ri, model, ent, &rq, &[], &[]).unwrap();
            assert_eq!(rows["rows"].as_array().unwrap().len() as u64, first["count"].as_u64().unwrap(), "{login} {ent}: the rows of a group of {q}");
            assert_eq!(comparable(&Ok(rows)), comparable(&Ok(rows2)), "{login} {ent}: the rows of a group with SQL and without");
        }
    }
    // the full-text search, by words of the rows the user reads
    let fields = quadra_runtime::query::searchable(model, ent);
    if fields.is_empty() {
        return;
    }
    let mut ri = w.interp(false, Some(user));
    let mut probes: Vec<String> = Vec::new();
    for id in readable.iter().take(5) {
        for f in &fields {
            if let Some(V::Text(t)) = ri.store.read(*id, f, None) {
                for wd in quadra_runtime::query::words(&t).into_iter().take(2) {
                    let p: String = wd.chars().take(3).collect();
                    if !probes.contains(&p) {
                        probes.push(p);
                    }
                }
            }
        }
    }
    let visible: Vec<String> = fields.iter().filter(|f| !ri.hidden(ent, f)).cloned().collect();
    for p in probes.iter().take(4) {
        let q = json!({ "entity": ent, "fields": [], "search": p, "limit": 1000 });
        let mut si = w.interp(true, Some(user));
        let got = quadra_api::query_rows(&mut si, model, ent, &q, &[], &[]);
        let mut ri2 = w.interp(false, Some(user));
        let want = quadra_api::query_rows(&mut ri2, model, ent, &q, &[], &[]);
        assert_eq!(comparable(&got), comparable(&want), "{login} {ent}: search {p:?} with SQL and without differ");
        let ids: Vec<u64> = got.unwrap()["rows"].as_array().unwrap().iter().map(|r| r["id"].as_u64().unwrap()).collect();
        // readable rows matching by the fields the user may see: exactly those
        let mut expect = Vec::new();
        for id in readable {
            let texts: Vec<String> = visible.iter().filter_map(|f| match ri.store.read(*id, f, None) {
                Some(V::Text(t)) => Some(t.to_string()),
                _ => None,
            }).collect();
            if quadra_runtime::query::text_matches(&[p.clone()], &texts) {
                expect.push(*id);
            }
        }
        assert_eq!(ids, expect, "{login} {ent}: search {p:?}");
    }
}

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

/// For one user and entity: the rows the interpreter's `can read` allows, compared with the
/// API's pages, total, aggregates and search, with SQL and without. Returns whether the row
/// rules were applied in SQL (true), by the interpreter row by row (false), or not needed.
fn check(w: &World, login: &str, user: u64, ent: &str, search: bool) -> Option<bool> {
    let model = w.model();
    // the reference: every record, one by one
    let mut ri = w.interp(false, Some(user));
    let mut readable: Vec<u64> = Vec::new();
    for id in ri.ids(ent, None) {
        let rec = V::Ref { ent: Rc::from(ent), id };
        if ri.can("read", ent, None, Some(&rec)) {
            readable.push(id);
        }
    }
    readable.sort();
    let (s0, e0) = quadra_store::sqlq::perms();
    // every page through the cursor, with SQL
    let mut q = json!({ "entity": ent, "fields": [], "limit": 97 });
    let mut paged: Vec<u64> = Vec::new();
    let mut total = None;
    for _ in 0..200 {
        let mut si = w.interp(true, Some(user));
        match quadra_api::query_rows(&mut si, model, ent, &q, &[], &[]) {
            Ok(r) => {
                if total.is_none() {
                    total = r["total"].as_u64();
                }
                paged.extend(r["rows"].as_array().unwrap().iter().map(|r| r["id"].as_u64().unwrap()));
                match r["next"].as_str() {
                    Some(c) => {
                        q["after"] = json!(c);
                        q["count"] = json!(false);
                    }
                    None => break,
                }
            }
            Err((403, _)) => {
                assert!(readable.is_empty(), "{login} may not query {ent} but reads {} of its rows", readable.len());
                return None;
            }
            Err(e) => panic!("{login} {ent}: {e:?}"),
        }
    }
    let mut sorted = paged.clone();
    sorted.sort();
    assert_eq!(sorted, readable, "{login}: the {ent} rows listed with SQL are not those the interpreter allows");
    assert_eq!(paged, readable, "{login}: the {ent} pages are not in id order");
    assert_eq!(total, Some(readable.len() as u64), "{login}: the total of {ent}");
    // beyond the cap the total is an estimate: of the rows the user may read, never of the
    // table (a user whose rows are the first ids of the table was told the whole table)
    quadra_api::set_count_cap(5);
    let mut si = w.interp(true, Some(user));
    let r = quadra_api::query_rows(&mut si, model, ent, &json!({ "entity": ent, "fields": [], "limit": 3 }), &[], &[]);
    quadra_api::set_count_cap(quadra_api::COUNT_CAP);
    let r = r.unwrap();
    let est = r["total"].as_u64().unwrap();
    if r["estimated"] == json!(true) {
        assert!(readable.len() > 5 && est > 5, "{login} {ent}: estimated {est} for {} rows", readable.len());
        if w.url.starts_with("sqlite:") {
            assert!(est <= readable.len() as u64, "{login} {ent}: an estimate of {est} rows, {} readable", readable.len());
        }
    } else {
        assert_eq!(est, readable.len() as u64, "{login} {ent}: the total with a cap of 5");
    }
    // one page with its total and aggregates, the same with SQL and without
    let mut q = json!({ "entity": ent, "limit": 25 });
    if let Some(f) = agg_field(model, ent) {
        q["aggregates"] = json!({ f: "count" });
    }
    let mut si = w.interp(true, Some(user));
    let got = quadra_api::query_rows(&mut si, model, ent, &q, &[], &[]);
    let mut ri = w.interp(false, Some(user));
    let want = quadra_api::query_rows(&mut ri, model, ent, &q, &[], &[]);
    assert_eq!(comparable(&got), comparable(&want), "{login} {ent}: a page with SQL and without differ");
    // the search (references, pickers): every page, with SQL and without
    for text in if search { vec!["", "a"] } else { vec![""] } {
        let mut pages: Vec<Vec<J>> = Vec::new();
        for sql in [true, false] {
            let mut i = w.interp(sql, Some(user));
            let mut after: Option<String> = None;
            let mut rows: Vec<J> = Vec::new();
            for _ in 0..100 {
                let r = quadra_api::search_rows(&mut i, model, ent, text, 50, after.as_deref()).unwrap();
                rows.extend(r["rows"].as_array().unwrap().iter().cloned());
                match r["next"].as_str() {
                    Some(c) => after = Some(c.to_string()),
                    None => break,
                }
            }
            pages.push(rows);
        }
        assert_eq!(pages[0], pages[1], "{login} {ent}: search {text:?} with SQL and without differ");
        if text.is_empty() {
            let mut ids: Vec<u64> = pages[0].iter().map(|r| r["id"].as_u64().unwrap()).collect();
            ids.sort();
            assert_eq!(ids, readable, "{login}: the {ent} rows found are not those the interpreter allows");
        }
    }
    let (s1, e1) = quadra_store::sqlq::perms();
    if search {
        check_groups_and_search(w, login, user, ent, &readable);
    }
    let (sql, engine) = (s1 > s0, e1 > e0);
    match (sql, engine) {
        (_, true) => Some(false),
        (true, false) => Some(true),
        (false, false) => None,
    }
}

/// Every user × every entity; returns the (login, entity) pairs whose rules the interpreter
/// applied row by row.
fn check_all(w: &World, search: bool) -> Vec<(String, String)> {
    let mut engine = Vec::new();
    let mut sql = 0;
    for (login, user) in w.users() {
        for ent in entities(w.model()) {
            match check(w, &login, user, &ent, search) {
                Some(true) => sql += 1,
                Some(false) => engine.push((login.clone(), ent.clone())),
                None => {}
            }
        }
    }
    assert!(sql > 0, "no row rule was applied in SQL");
    engine
}

// ---- the stress world ------------------------------------------------------------------------

fn stress_world(target: &str) -> World {
    let root = example("stress");
    let url = fresh(target, "stress");
    let dir = std::env::temp_dir().join(format!("quadra-perm-{}-gen-{}", std::process::id(), if target == "sqlite" { "s" } else { "p" }));
    let _ = std::fs::remove_dir_all(&dir);
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_quadra")).args(["bench", "gen", "--scale", "tiny", "--out"]).arg(&dir).output().unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
    let checked = quadra_analysis::check(&quadra_sema::Project::load(&root).unwrap());
    quadra_store::ops::init(&url, checked.model.as_ref().unwrap(), &checked.report, clock()).unwrap();
    quadra_store::bulk::import_dir(&url, checked.model.as_ref().unwrap(), &checked.report, &dir, clock(), None, &mut |_| {}).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    World::new(checked, url)
}

fn r(ent: &str, id: u64) -> V {
    V::Ref { ent: Rc::from(ent), id }
}

fn stress_on(target: &str) {
    let w = stress_world(target);
    // every rule of the stress model runs in the database
    {
        let mut i = w.interp(false, None);
        let d = Conn::open(&w.url).unwrap().dialect();
        let report = quadra_store::sqlq::rules_sql(&mut i, &w.schema, d);
        assert!(report.len() >= 7, "{report:?}");
        for x in &report {
            assert!(x.sql.is_ok(), "{} ({}): {:?}", x.rule, x.entity, x.sql);
        }
    }
    let engine = check_all(&w, true);
    assert!(engine.is_empty(), "rules applied by the interpreter row by row: {engine:?}");

    // writes that change the paths of the rules
    let reg = |code: &str| w.pick(&format!("SELECT CAST(\"id\" AS TEXT) FROM \"region\" WHERE \"code\" = '{code}'"));
    let user = |login: &str| w.pick(&format!("SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = '{login}'"));
    let (r1, r2, r3) = (reg("R01"), reg("R02"), reg("R03"));
    let c1 = w.pick(&format!("SELECT CAST(\"id\" AS TEXT) FROM \"customer\" WHERE \"region_id\" = {r1} AND \"active\" ORDER BY \"id\" LIMIT 1"));
    let c2 = w.pick(&format!("SELECT CAST(\"id\" AS TEXT) FROM \"customer\" WHERE \"region_id\" = {r1} AND \"active\" ORDER BY \"id\" LIMIT 1 OFFSET 1"));
    let open = w.pick(&format!("SELECT CAST(o.\"id\" AS TEXT) FROM \"order\" o JOIN \"customer\" c ON c.\"id\" = o.\"customer_id\" WHERE c.\"region_id\" = {r2} AND o.\"state\" IN ('Draft', 'Confirmed') ORDER BY o.\"id\" LIMIT 1"));
    let (s2, s3, m3) = (user("sales0002"), user("sales0003"), user("manager03"));
    w.write(&|i| {
        // a salesperson reassigned, a customer moved to another region
        i.set(&r("Customer", c1), "salesperson", r("User", s2), None)?;
        i.set(&r("Customer", c2), "region", r("Region", r2), None)?;
        // a region's manager, logistics and team
        i.set(&r("Region", r2), "manager", r("User", m3), None)?;
        i.set(&r("Region", r1), "logistics", V::None, None)?;
        let team = i.store.read(r1, "team", None).unwrap_or(V::Set(Rc::new(Vec::new())));
        let mut members: Vec<V> = match team {
            V::Set(x) | V::List(x) => x.iter().cloned().collect(),
            _ => Vec::new(),
        };
        members.push(r("User", s3));
        i.set(&r("Region", r1), "team", V::Set(Rc::new(members)), None)?;
        i.set(&r("Region", r2), "team", V::Set(Rc::new(Vec::new())), None)?;
        // books closed on the first region; an order cancelled (frozen)
        i.set(&r("Region", r1), "locked", V::Bool(true), None)?;
        i.run_action(&r("Order", open), "Order", "cancel", None, Vec::new(), Vec::new())?;
        Ok(())
    });
    let engine = check_all(&w, false);
    assert!(engine.is_empty(), "{engine:?}");
    w.write(&|i| {
        // unlocked again, the last region unlocked too, the customer back, a manager back
        i.set(&r("Region", r1), "locked", V::Bool(false), None)?;
        i.set(&r("Region", r3), "locked", V::Bool(false), None)?;
        i.set(&r("Customer", c2), "region", r("Region", r1), None)?;
        i.set(&r("Region", r1), "logistics", r("User", user("logistics01")), None)?;
        Ok(())
    });
    let engine = check_all(&w, false);
    assert!(engine.is_empty(), "{engine:?}");
    cleanup(target, &w.url);
}

#[test]
fn stress_permissions_on_sqlite() {
    stress_on("sqlite");
}

#[test]
fn stress_permissions_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped");
        return;
    };
    stress_on(&pg);
}

// ---- the extended example ------------------------------------------------------------------------

/// Documents of several users (the fixtures have none).
const DOCS: &str = r#"
from tests.fixtures import italy

fixture perm_docs extends italy:
    let giulia = User.find(u => u.login == "giulia") ?? fail "no giulia"
    let marco = User.find(u => u.login == "marco") ?? fail "no marco"
    SalesOrder(customer=rossi, salesperson=giulia, lines=[SalesLine(product=widget, qty=3 pcs)])
    SalesOrder(customer=rossi, salesperson=marco, lines=[SalesLine(product=widget, qty=2 pcs), SalesLine(product=widget, qty=1 pcs)])
    SalesOrder(customer=acme_inc, salesperson=giulia, lines=[SalesLine(product=widget, qty=1 pcs)])
    PurchaseOrder(vendor=bianchi, lines=[PurchaseLine(product=widget, qty=10 pcs)])
"#;

fn extended_on(target: &str) {
    let root = example("extended");
    let project = quadra_sema::Project::load(&root).unwrap();
    let mut sources: Vec<(PathBuf, String)> = project.user_files().map(|f| (f.path.clone(), f.text.clone())).collect();
    sources.push((root.join("tests/__perm__.qd"), DOCS.to_string()));
    let checked = quadra_analysis::check(&quadra_sema::Project::from_sources(&root, sources));
    let errors: Vec<String> = checked.diagnostics.iter().filter(|d| d.severity == quadra_sema::Severity::Error).map(|d| d.message.clone()).collect();
    assert!(errors.is_empty(), "{errors:?}");
    let url = fresh(target, "extended");
    quadra_store::ops::init(&url, checked.model.as_ref().unwrap(), &checked.report, clock()).unwrap();
    quadra_store::ops::fixture(&url, checked.model.as_ref().unwrap(), &checked.report, "perm_docs", clock()).unwrap();
    let w = World::new(checked, url);
    // the rules that read formulas stay the interpreter's, and say why
    let mut i = w.interp(false, None);
    let d = Conn::open(&w.url).unwrap().dialect();
    let report = quadra_store::sqlq::rules_sql(&mut i, &w.schema, d);
    let engine: Vec<String> = report.iter().filter(|x| x.sql.is_err()).map(|x| format!("{} ({})", x.rule, x.entity)).collect();
    let sql: Vec<String> = report.iter().filter(|x| x.sql.is_ok()).map(|x| format!("{} ({})", x.rule, x.entity)).collect();
    assert!(sql.iter().any(|x| x.contains("SalesOrder where o => o.salesperson == me")), "{sql:?}");
    assert!(engine.iter().any(|x| x.contains("Invoice where")), "{engine:?}");
    assert!(sql.iter().any(|x| x.contains("deny run PurchaseOrder.approve where o => o.created_by == me")), "{sql:?}");
    for x in report.iter().filter(|x| x.sql.is_err()) {
        assert!(x.sql.as_ref().unwrap_err().contains("is a formula"), "{}: {:?}", x.rule, x.sql);
    }
    drop(i);
    let engine = check_all(&w, true);
    // the interpreter decides only where a rule reads a formula (`Invoice.direction`)
    for (login, ent) in &engine {
        assert!(matches!(ent.as_str(), "Invoice" | "InvoiceLine"), "{login} {ent}: applied row by row");
    }
    cleanup(target, &w.url);
}

#[test]
fn extended_permissions_on_sqlite() {
    extended_on("sqlite");
}

#[test]
fn extended_permissions_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped");
        return;
    };
    extended_on(&pg);
}
