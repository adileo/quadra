//! Grouping, pivots and the full-text search (the notes on scaling, phase 4).
//!
//! A small world with references two relations deep, dates over two years, amounts in two
//! currencies, a text field a role hides and a row rule: for each user, every group query
//! (each kind of key, date buckets, two keys, pivots, orders by count and by an aggregate,
//! pages) gives the same answer computed by the database and by the interpreter alone
//! (`DbSource::sql` off), the database does compute it where the query translates, a group's
//! `group_filter` lists exactly its rows, and the search finds only rows the user may read, by
//! fields the user may see. On SQLite and, when `QUADRA_TEST_PG` holds a url, on PostgreSQL.

use quadra_runtime::{Interp, Store};
use quadra_store::conn::{Conn, Pooled};
use quadra_store::source::{head, DbSource};
use serde_json::{json, Value as J};
use std::cell::RefCell;

use std::rc::Rc;

fn clock() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 11, 20)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap()
}

const MODEL: &str = r#"
entity Region:
    code: Code unique
    name: Text

entity Customer:
    code: Code unique
    name: Text
    notes: Text?
    region: Region
    referrer: Customer?
    owner: User?
    invoices: [Invoice]

entity Invoice:
    number: Code unique
    customer: Customer
    date: Date
    at: DateTime?
    currency: Currency
    amount: Money<currency>
    paid: Bool default false
    kind: enum(Goods, Services) default Goods
    lines: Int default 1

role Admin:
    all *

# `group_by` in the language (interp/grouping.rs): compared with SQL and without
fn lg_region():
    return Invoice.group_by(i => i.customer.region).map(g => (g.key, g.items.count(), g.items.sum(i => i.lines), g.items.max(i => i.date), g.items.min(i => i.number), g.items.avg(i => i.lines)))

fn lg_two():
    return Invoice.where(i => i.paid).group_by(i => i.kind, i => i.date.month).sort_by(g => (-g.items.count(), g.key2)).first(5).map(g => (g.key, g.key2, g.items.count()))

# ranked by the database: a stable sort of the groups (ties by their first row), then `first(n)`
fn lg_top():
    return Invoice.group_by(i => i.customer).sort_by(g => -g.items.sum(i => i.lines)).first(4).map(g => (g.key, g.items.sum(i => i.lines), g.items.count()))

fn lg_top_count():
    return Invoice.group_by(i => i.date.month).sort_by(g => g.items.count()).first(3).map(g => (g.key, g.items.count()))

fn lg_year():
    return Invoice.group_by(i => i.date.year).where(g => g.items.count() > 3).map(g => (g.key, g.items.count(), g.items.avg(i => i.lines)))

fn lg_month_start():
    return Invoice.group_by(i => i.date.start_of_month()).map(g => (g.key, g.items.count()))

fn lg_at_month():
    return Invoice.group_by(i => i.at?.month).map(g => (g.key, g.items.count(), g.items.min(i => i.at)))

fn lg_currency():
    return Invoice.group_by(i => i.currency).map(g => (g.key.code, g.items.sum(i => i.amount.raw), g.items.max(i => i.amount.raw)))

fn lg_mixed():
    return Invoice.where(i => i.currency.code == "EUR").group_by(i => i.customer.region).map(g => g.items.sum(i => i.amount.raw))

fn lg_text():
    return Invoice.group_by(i => i.customer.name).last(3).map(g => (g.key, g.items.count()))

fn lg_count():
    return Invoice.group_by(i => i.paid).count()

fn lg_hidden():
    return Invoice.group_by(i => i.customer.notes).map(g => (g.key, g.items.count()))

fn lg_referrer():
    return Invoice.group_by(i => i.customer.referrer).map(g => (g.key, g.items.count()))

fn lg_items():
    return Invoice.group_by(i => i.kind).map(g => (g.key, g.items.map(i => i.number).count()))

role Clerk:
    read Region, Invoice
    read Customer where c => c.owner == me
    hide Customer.notes
"#;

fn seed() -> String {
    let mut s = String::from("seed:\n");
    s.push_str("    let eur = Currency.find(c => c.code == \"EUR\") ?? Currency(code=\"EUR\", name=\"Euro\", symbol=\"€\")\n");
    s.push_str("    let usd = Currency.find(c => c.code == \"USD\") ?? Currency(code=\"USD\", name=\"Dollar\", symbol=\"$\")\n");
    s.push_str("    let admin = User(login=\"boss\", roles={Admin})\n");
    s.push_str("    let k1 = User(login=\"clerk1\", roles={Clerk})\n");
    s.push_str("    let k2 = User(login=\"clerk2\", roles={Clerk})\n");
    for r in 1..=3 {
        s.push_str(&format!(
            "    let r{r} = Region(code=\"R{r}\", name=\"{} region\")\n",
            ["North", "South", "east"][r - 1]
        ));
    }
    let names = [
        "Alpha Trading",
        "Beta Foods",
        "alpha Retail",
        "Gamma Tech",
        "Delta-Logistics",
        "Omega Spa",
        "Ålesund Fisk",
        "Zeta & Figli",
        "beta Group",
        "Kappa 42",
        "Lambda",
        "Sigma Trading",
    ];
    for (n, name) in names.iter().enumerate() {
        let c = n + 1;
        let owner = match c % 3 {
            0 => "k1",
            1 => "k2",
            _ => "none",
        };
        let notes = if c % 4 == 0 {
            "\"secretword inside\"".to_string()
        } else if c % 5 == 0 {
            "\"plain words\"".to_string()
        } else {
            "none".to_string()
        };
        let referrer = if c > 2 && c % 2 == 1 {
            format!("c{}", c - 2)
        } else {
            "none".to_string()
        };
        s.push_str(&format!("    let c{c} = Customer(code=\"C{c:02}\", name=\"{name}\", notes={notes}, region=r{}, referrer={referrer}, owner={owner})\n", c % 3 + 1));
    }
    for i in 1..=60u32 {
        let c = (i * 7) % 12 + 1;
        let month = (i % 12) + 1;
        let year = if i % 3 == 0 { 2025 } else { 2026 };
        let day = (i * 5) % 28 + 1;
        // the third region bills in two currencies
        let cur = if c % 3 + 1 == 3 && i % 2 == 0 {
            "usd"
        } else {
            "eur"
        };
        let at = if i % 4 == 0 {
            "none".to_string()
        } else {
            format!("{year}-{month:02}-{day:02} 1{}:30", i % 10)
        };
        s.push_str(&format!(
            "    Invoice(number=\"INV-{i:04}\", customer=c{c}, date={year}-{month:02}-{day:02}, at={at}, currency={cur}, amount={}.{:02} {}, paid={}, kind={}, lines={})\n",
            i * 13 % 500,
            i % 100,
            cur.to_uppercase(),
            i % 2 == 0,
            if i % 3 == 0 { "Services" } else { "Goods" },
            i % 4
        ));
    }
    s
}

struct World {
    url: String,
    checked: quadra_analysis::Checked,
    schema: Rc<quadra_store::schema::Schema>,
    dir: std::path::PathBuf,
}

impl World {
    fn new(target: &str) -> World {
        // each world its own files and schema: the tests run in parallel
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "quadra-groups-{}-{n}-{}",
            std::process::id(),
            if target == "sqlite" { "lite" } else { "pg" }
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("quadra.toml"), "[project]\nname = \"groups\"\n").unwrap();
        std::fs::write(dir.join("main.qd"), format!("{MODEL}\n{}", seed())).unwrap();
        let checked = quadra_analysis::check(&quadra_sema::Project::load(&dir).unwrap());
        let errors: Vec<String> = checked
            .diagnostics
            .iter()
            .filter(|d| d.severity == quadra_sema::Severity::Error)
            .map(|d| d.message.clone())
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
        let url = if target == "sqlite" {
            let p = dir.join("w.db");
            format!("sqlite:{}", p.display())
        } else {
            let schema = format!("qg_{}_{n}", std::process::id());
            Conn::open(target)
                .unwrap()
                .batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
                .unwrap();
            format!("{target}#schema={schema}")
        };
        quadra_store::ops::init(
            &url,
            checked.model.as_ref().unwrap(),
            &checked.report,
            clock(),
        )
        .unwrap();
        let schema =
            quadra_store::Database::open(&url, checked.model.as_ref().unwrap(), &checked.report)
                .unwrap()
                .schema
                .clone();
        World {
            url,
            checked,
            schema,
            dir,
        }
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
        i.user = user;
        i
    }

    fn user(&self, login: &str) -> u64 {
        let mut c = Conn::open(&self.url).unwrap();
        c.query(
            &format!("SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = '{login}'"),
            &[],
        )
        .unwrap()[0][0]
            .as_deref()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// A query with SQL and without: (with SQL, without).
    fn both(
        &self,
        user: u64,
        ent: &str,
        q: &J,
    ) -> (Result<J, (u16, String)>, Result<J, (u16, String)>) {
        let mut si = self.interp(true, Some(user));
        let a = quadra_api::query_rows(&mut si, self.model(), ent, q, &[], &[]);
        let mut ri = self.interp(false, Some(user));
        let b = quadra_api::query_rows(&mut ri, self.model(), ent, q, &[], &[]);
        (a, b)
    }

    fn cleanup(&self, target: &str) {
        if let Some((_, schema)) = self.url.split_once("#schema=") {
            let _ = Conn::open(target)
                .and_then(|mut c| c.batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")));
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn strip(r: &Result<J, (u16, String)>) -> J {
    match r {
        Ok(j) => {
            let mut j = j.clone();
            j.as_object_mut().unwrap().remove("sql");
            j
        }
        Err((s, m)) => json!({ "status": s, "error": m }),
    }
}

fn ids(r: &J) -> Vec<u64> {
    r["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_u64().unwrap())
        .collect()
}

fn run(target: &str) {
    let w = World::new(target);
    let (boss, k1) = (w.user("boss"), w.user("clerk1"));
    let keys: Vec<J> = vec![
        json!("customer"),
        json!("customer.region"),
        json!("customer.region.name"),
        json!("customer.referrer"),
        json!("customer.referrer.region"),
        json!("kind"),
        json!("paid"),
        json!("lines"),
        json!("number"),
        json!({ "field": "date" }),
        json!({ "field": "date", "bucket": "week" }),
        json!({ "field": "date", "bucket": "month" }),
        json!({ "field": "date", "bucket": "quarter" }),
        json!({ "field": "date", "bucket": "year" }),
        json!({ "field": "at", "bucket": "month" }),
        json!({ "field": "at", "bucket": "day" }),
    ];
    let aggs = json!({ "amount": "sum", "lines": "avg", "date": "max", "number": "count" });
    let mut queries: Vec<J> = Vec::new();
    for k in &keys {
        queries.push(json!({ "group_by": [k], "aggregates": aggs, "limit": 1000 }));
    }
    for (a, b) in [(1, 11), (5, 12), (0, 6), (2, 13), (3, 7)] {
        queries.push(json!({ "group_by": [keys[a], keys[b]], "aggregates": aggs, "limit": 1000 }));
        queries.push(json!({ "group_by": [keys[a], keys[b]], "aggregates": aggs, "pivot": true, "limit": 1000 }));
        queries.push(json!({ "group_by": [keys[a], keys[b]], "aggregates": aggs, "pivot": true, "limit": 2, "offset": 1 }));
    }
    queries.push(json!({ "group_by": ["customer"], "aggregates": aggs, "group_order": { "by": "count", "desc": true }, "limit": 1000 }));
    queries.push(json!({ "group_by": ["customer"], "aggregates": aggs, "group_order": { "by": "amount", "desc": false }, "limit": 1000 }));
    queries.push(json!({ "group_by": [{ "field": "date", "bucket": "month" }], "aggregates": aggs, "group_order": { "by": "date", "desc": true }, "limit": 1000 }));
    queries.push(json!({ "group_by": ["customer.region", "kind"], "aggregates": aggs, "group_order": { "by": "key", "desc": true }, "limit": 3, "offset": 2 }));
    queries.push(json!({ "group_by": ["kind"], "where": "paid", "search": "inv 00", "aggregates": aggs, "limit": 1000 }));
    let mut in_sql = 0;
    for (login, user) in [("boss", boss), ("clerk1", k1)] {
        let mut readable = w.interp(false, Some(user));
        let all: Vec<u64> = {
            let r = quadra_api::query_rows(
                &mut readable,
                w.model(),
                "Invoice",
                &json!({ "fields": [], "limit": 1000 }),
                &[],
                &[],
            )
            .unwrap();
            ids(&r)
        };
        for q in &queries {
            let (a, b) = w.both(user, "Invoice", q);
            assert_eq!(strip(&a), strip(&b), "{login}: {q} with SQL and without");
            let g = a.unwrap_or_else(|e| panic!("{login}: {q}: {e:?}"));
            if g["sql"] == json!(true) {
                in_sql += 1;
            }
            // through the customer (a row rule for a clerk): the interpreter's, which hides it
            if login == "boss" {
                assert_eq!(g["sql"], json!(true), "{login}: {q} not computed in SQL");
            }
            let groups = g["groups"].as_array().unwrap();
            if q["offset"].is_null() && q["where"].is_null() && q["pivot"].is_null() {
                let n: u64 = groups.iter().map(|x| x["count"].as_u64().unwrap()).sum();
                assert_eq!(
                    n,
                    all.len() as u64,
                    "{login}: {q}: the groups hold every row"
                );
            }
            // each group's (and each pivot cell's) rows, by its filter
            for x in groups.iter().take(4) {
                if x["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|k| k["hidden"] == json!(true))
                {
                    continue;
                }
                let mut rq = json!({ "fields": [], "group_filter": x["filter"], "limit": 1000 });
                for k in ["where", "search"] {
                    if !q[k].is_null() {
                        rq[k] = q[k].clone();
                    }
                }
                let (ra, rb) = w.both(user, "Invoice", &rq);
                let (ra, rb) = (ra.unwrap(), rb.unwrap());
                assert_eq!(ids(&ra), ids(&rb), "{login}: the rows of a group of {q}");
                assert_eq!(
                    ids(&ra).len() as u64,
                    x["count"].as_u64().unwrap(),
                    "{login}: the rows of group {} of {q}",
                    x["filter"]
                );
            }
        }
        // a group of several currencies: an error for its sum, not a wrong number
        let r = w.both(user, "Invoice", &json!({ "group_by": ["customer.region"], "aggregates": { "amount": "sum" }, "limit": 10 })).0.unwrap();
        let errs = r["groups"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| {
                x["aggregates"]["amount"]["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("several currencies"))
            })
            .count();
        assert_eq!(errs, 1, "{login}: {r}");
    }
    assert!(in_sql > 20, "only {in_sql} group queries computed in SQL");
    // refused: a hidden key, an amount, a bucket of something else, too many keys
    let bad = [
        (k1, json!({ "group_by": ["customer.notes"] }), 403),
        (boss, json!({ "group_by": ["amount"] }), 400),
        (
            boss,
            json!({ "group_by": [{ "field": "kind", "bucket": "month" }] }),
            400,
        ),
        (boss, json!({ "group_by": ["kind", "paid", "lines"] }), 400),
        (
            boss,
            json!({ "group_by": ["kind"], "aggregates": { "lines": "avg" }, "group_order": { "by": "lines" } }),
            400,
        ),
    ];
    for (u, q, status) in bad {
        let (a, b) = w.both(u, "Invoice", &q);
        assert_eq!(a.as_ref().err().map(|e| e.0), Some(status), "{q}: {a:?}");
        assert_eq!(strip(&a), strip(&b));
    }

    // ---- the full-text search ----
    let search = |user: u64, ent: &str, text: &str| -> (Vec<u64>, Vec<u64>) {
        let q = json!({ "fields": [], "search": text, "limit": 1000 });
        let (a, b) = w.both(user, ent, &q);
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(
            ids(&a),
            ids(&b),
            "search {text:?} in {ent} with SQL and without"
        );
        let mut si = w.interp(true, Some(user));
        let mut ri = w.interp(false, Some(user));
        let x = quadra_api::search_rows(&mut si, w.model(), ent, text, 200, None).unwrap();
        let y = quadra_api::search_rows(&mut ri, w.model(), ent, text, 200, None).unwrap();
        assert_eq!(
            x, y,
            "GET /v1/search {text:?} in {ent} with SQL and without"
        );
        let found: Vec<u64> = x["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_u64().unwrap())
            .collect();
        (ids(&a), found)
    };
    let readable = |user: u64| -> Vec<u64> {
        let mut i = w.interp(false, Some(user));
        ids(&quadra_api::query_rows(
            &mut i,
            w.model(),
            "Customer",
            &json!({ "fields": [], "limit": 1000 }),
            &[],
            &[],
        )
        .unwrap())
    };
    let (all, mine) = (readable(boss), readable(k1));
    // words, prefixes, case, accents kept, punctuation as a separator, several words
    let (a, _) = search(boss, "Customer", "alpha");
    assert_eq!(a.len(), 2);
    assert_eq!(search(boss, "Customer", "ALP TRAD").0.len(), 1);
    assert_eq!(search(boss, "Customer", "logistics delta").0.len(), 1);
    assert_eq!(search(boss, "Customer", "ålesund").0.len(), 1);
    assert_eq!(search(boss, "Customer", "alesund").0.len(), 0);
    assert_eq!(search(boss, "Customer", "42").0.len(), 1);
    assert_eq!(
        search(boss, "Customer", "pha").0.len(),
        0,
        "a word's middle is not a match"
    );
    // a hidden field is never matched: the boss finds the notes, a clerk does not
    let (secret, picked) = search(boss, "Customer", "secretw");
    assert_eq!(secret.len(), 3);
    assert!(picked.len() >= 3);
    assert!(
        secret.iter().any(|id| mine.contains(id)),
        "the clerk reads a customer with notes: the test needs one"
    );
    let (none, picked) = search(k1, "Customer", "secretw");
    assert!(
        none.is_empty() && picked.is_empty(),
        "a clerk found customers by a hidden field: {none:?} {picked:?}"
    );
    // only readable rows
    for text in ["a", "trading", "beta", "c0", "region"] {
        let (rows, picked) = search(k1, "Customer", text);
        assert!(
            rows.iter().chain(picked.iter()).all(|id| mine.contains(id)),
            "{text}: a clerk found customers it may not read"
        );
        let (rows, _) = search(boss, "Customer", text);
        assert!(rows.iter().all(|id| all.contains(id)));
    }
    assert_eq!(search(boss, "Invoice", "inv 0001").0.len(), 1);
    assert_eq!(search(boss, "Invoice", "inv-00").0.len(), 60);
    assert!(quadra_store::sqlq::search_index_problems(
        &mut Conn::open(&w.url).unwrap(),
        &w.schema,
        w.model()
    )
    .unwrap()
    .is_empty());
    w.cleanup(target);
}

/// `Entity.group_by(…)` of the language: the same answer from the database's GROUP BY and from
/// the interpreter alone, for the code (no user) and for a user's expression (`viewer`: their
/// rows, their hidden fields), and the database does answer it.
fn language(target: &str) {
    let w = World::new(target);
    let (boss, k1) = (w.user("boss"), w.user("clerk1"));
    let names = [
        "lg_region",
        "lg_two",
        "lg_top",
        "lg_top_count",
        "lg_year",
        "lg_month_start",
        "lg_at_month",
        "lg_currency",
        "lg_mixed",
        "lg_text",
        "lg_count",
        "lg_hidden",
        "lg_referrer",
        "lg_items",
    ];
    let run =
        |sql: bool, user: Option<u64>, viewer: bool, f: &str| -> (Result<String, String>, u64) {
            let mut i = w.interp(sql, user);
            i.viewer = viewer;
            let def = w.model().function(f).unwrap().clone();
            let Some(quadra_ir::StmtKind::Return(Some(e))) =
                def.body.as_ref().unwrap().last().map(|s| s.kind.clone())
            else {
                panic!("{f}")
            };
            let r = i.eval(&e, &mut quadra_runtime::Ctx::default());
            // the invoices the interpreter held (none when the database grouped them)
            let loaded = i
                .store
                .records
                .values()
                .filter(|r| &*r.ent == "Invoice")
                .count() as u64;
            (
                r.map(|v| format!("{v:?}"))
                    .map_err(|e| quadra_api::json::describe(e)),
                loaded,
            )
        };
    for (user, viewer) in [(None, false), (Some(boss), true), (Some(k1), true)] {
        for f in names {
            let (a, la) = run(true, user, viewer, f);
            let (b, lb) = run(false, user, viewer, f);
            assert!(
                lb > 0 || b.is_err(),
                "{f}: the interpreter alone reads the invoices"
            );
            assert_eq!(
                a, b,
                "{f} as {user:?} (viewer {viewer}): with SQL and without"
            );
            // answered by the database: the rows are not loaded
            if matches!(
                f,
                "lg_region"
                    | "lg_two"
                    | "lg_top"
                    | "lg_top_count"
                    | "lg_year"
                    | "lg_month_start"
                    | "lg_currency"
                    | "lg_count"
            ) && user != Some(k1)
            {
                assert_eq!(
                    la, 0,
                    "{f} as {user:?}: {la} invoices loaded, expected the database to group"
                );
            }
        }
    }
    // what a clerk may not see
    let (h, _) = run(true, Some(k1), true, "lg_hidden");
    assert!(h.as_ref().is_err_and(|e| e.contains("hidden")), "{h:?}");
    let (r, _) = run(true, Some(boss), true, "lg_region");
    assert!(r.is_ok(), "{r:?}");
    w.cleanup(target);
}

#[test]
fn language_group_by_on_sqlite() {
    language("sqlite");
}

#[test]
fn language_group_by_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped");
        return;
    };
    language(&pg);
}

#[test]
fn groups_and_search_on_sqlite() {
    run("sqlite");
}

#[test]
fn groups_and_search_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped");
        return;
    };
    run(&pg);
}
