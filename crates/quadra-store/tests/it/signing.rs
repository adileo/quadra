//! Signed commits: whoever controls the database can compute the hash chain again after
//! altering a value, but not the signatures; and stripping them leaves a chain that does not
//! end with the key the verifier expects. A new key is signed by the old one, and the commits
//! before it stay verifiable.

use quadra_runtime::{Ctx, Interp, V};
use quadra_sema::Project;
use quadra_store::conn::Sql;
use quadra_store::{materialized_formulas, ops, signing, Conn, Database, DbSource, Sink};
use sha2::{Digest, Sha256};
use std::path::Path;

const SEED: &str = r#"
seed:
    Customer(name="Anna Bianchi")
"#;

fn project() -> Project {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/minimal");
    let main = std::fs::read_to_string(root.join("main.qd")).unwrap();
    Project::from_sources(&root, vec![(root.join("main.qd"), format!("{main}{SEED}"))])
}

fn time() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 12, 1)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap()
}

fn fresh(name: &str) -> String {
    let file =
        std::env::temp_dir().join(format!("quadra-signing-{}-{name}.db", std::process::id()));
    let _ = std::fs::remove_file(&file);
    format!("sqlite:{}", file.display())
}

/// A database with the seed, then a key, then a commit signed by it: (url, public, secret).
fn signed_db(p: &Project, name: &str) -> (String, String, String) {
    let checked = quadra_analysis::check(p);
    let model = checked.model.as_ref().unwrap();
    let url = fresh(name);
    let mut db = Database::open(&url, model, &checked.report).unwrap();
    db.init().unwrap();
    let mut i = Interp::new(model);
    i.materialize = materialized_formulas(&db.schema);
    i.sink = Some(Box::new(Sink { db, model }));
    i.system += 1;
    let seeds = model.seeds.clone();
    i.transaction(|i| {
        for s in &seeds {
            i.stmts(&s.body, &mut Ctx::default())?;
        }
        Ok(())
    })
    .unwrap();
    drop(i);
    let (secret, public) = signing::generate();
    let mut db = Database::open(&url, model, &checked.report).unwrap();
    db.set_signing_key(&secret, time()).unwrap();
    // a commit signed by it
    rename(p, &url, "Anna Bianchi");
    (url, public, secret)
}

/// An engine over the database, as the server has one.
fn engine<'m>(
    url: &str,
    model: &'m quadra_ir::Model,
    report: &'m quadra_types::Report,
) -> Interp<'m> {
    let db = Database::open(url, model, report).unwrap();
    let src = DbSource::open(url, db.schema.clone(), None).unwrap();
    let mut i = Interp::new(model);
    i.set_clock(time());
    i.store = quadra_runtime::Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    i.materialize = materialized_formulas(&db.schema);
    i.sink = Some(Box::new(Sink { db, model }));
    i.load_units();
    i
}

/// Renames the first customer, as the application would.
fn rename(p: &Project, url: &str, to: &str) {
    let checked = quadra_analysis::check(p);
    let model = checked.model.as_ref().unwrap();
    let mut i = engine(url, model, &checked.report);
    let c = V::Ref {
        ent: "Customer".into(),
        id: i.ids("Customer", None)[0],
    };
    i.transaction(|i| i.set(&c, "name", V::Text(to.into()), None))
        .unwrap();
}

fn verify(p: &Project, url: &str, expected: Option<&str>) -> Vec<String> {
    let checked = quadra_analysis::check(p);
    signing::expect(expected.map(str::to_string));
    ops::verify(
        url,
        checked.model.as_ref().unwrap(),
        &checked.report,
        time(),
    )
    .unwrap()
    .0
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// What an attacker with the database and Quadra's documentation does: every commit hashed
/// again from what the tables hold (`with_keys`: the keys' entries included), each commit
/// keeping the signature it had (they cannot make new ones).
fn rehash(url: &str, with_keys: bool) {
    let mut c = Conn::open(url).unwrap();
    // the example writes no ledger rows, migrations or renames: the history and the keys
    for t in ["_quadra_ledger_rows", "_quadra_applied", "_quadra_renames"] {
        let n = c
            .query(&format!("SELECT COUNT(*) FROM \"{t}\""), &[])
            .unwrap();
        assert_eq!(n[0][0].as_deref(), Some("0"), "{t}");
    }
    let hist = c
        .query("SELECT CAST(\"tx\" AS TEXT), CAST(\"record_id\" AS TEXT), \"field\", \"value\", \"salt\" FROM \"_quadra_history\" ORDER BY \"seq\"", &[])
        .unwrap();
    let keys = c
        .query(
            "SELECT CAST(\"tx\" AS TEXT), \"public\" FROM \"_quadra_keys\"",
            &[],
        )
        .unwrap();
    let commits = c
        .query("SELECT CAST(\"tx\" AS TEXT), \"at\", CAST(\"user_id\" AS TEXT), \"cause\" FROM \"_quadra_commits\" ORDER BY \"tx\"", &[])
        .unwrap();
    let mut prev = "0".repeat(64);
    for r in commits {
        let tx = r[0].clone().unwrap();
        let mut h = Sha256::new();
        h.update(prev.as_bytes());
        h.update(
            format!(
                "|{tx}|{}|{}|{}|",
                r[1].as_deref().unwrap(),
                r[2].as_deref().unwrap_or(""),
                r[3].as_deref().unwrap()
            )
            .as_bytes(),
        );
        for e in hist.iter().filter(|e| e[0] == r[0]) {
            let mut d = Sha256::new();
            d.update(e[4].as_deref().unwrap().as_bytes());
            d.update(b"|");
            d.update(e[3].as_deref().unwrap().as_bytes());
            let id: u64 = e[1].as_deref().unwrap().parse().unwrap();
            let entry = serde_json::json!(["h", id, e[2].as_deref().unwrap(), hex(&d.finalize())]);
            h.update(entry.to_string().as_bytes());
            h.update(b"\n");
        }
        if with_keys {
            for k in keys.iter().filter(|k| k[0] == r[0]) {
                h.update(
                    serde_json::json!(["k", k[1].as_deref().unwrap()])
                        .to_string()
                        .as_bytes(),
                );
                h.update(b"\n");
            }
        }
        let hash = hex(&h.finalize());
        c.exec(
            "UPDATE \"_quadra_commits\" SET \"prev_hash\" = ?text, \"hash\" = ?text WHERE \"tx\" = ?bigint",
            &[Sql::Text(prev.clone()), Sql::Text(hash.clone()), Sql::Int(tx.parse().unwrap())],
        )
        .unwrap();
        prev = hash;
    }
}

/// The value of the customer's name changed in the history and in the table, as someone with
/// the database would.
fn forge_name(url: &str, to: &str) {
    let mut c = Conn::open(url).unwrap();
    let v = serde_json::json!({ "t": to }).to_string();
    let n = c
        .query("SELECT \"value\" FROM \"_quadra_history\" WHERE \"field\" = 'name' AND \"value\" LIKE '%Anna Bianchi%' ORDER BY \"seq\" LIMIT 1", &[])
        .unwrap();
    let old = n[0][0].clone().unwrap();
    // the encoding of a text value, whatever it is: the old one with the text replaced
    let forged = old.replace("Anna Bianchi", to);
    assert_ne!(forged, old, "{old} vs {v}");
    c.exec(
        "UPDATE \"_quadra_history\" SET \"value\" = ?text WHERE \"value\" = ?text",
        &[Sql::Text(forged), Sql::Text(old)],
    )
    .unwrap();
    c.exec(
        "UPDATE \"customer\" SET \"name\" = ?text WHERE \"name\" = 'Anna Bianchi'",
        &[Sql::Text(to.into())],
    )
    .unwrap();
}

#[test]
fn a_chain_computed_again_without_the_key_is_found_out() {
    quadra_store::db::seed_all_salts();
    let p = project();
    let (url, public, _) = signed_db(&p, "forged");
    assert_eq!(verify(&p, &url, Some(&public)), Vec::<String>::new());
    // the attacker alters a value and computes the whole chain again: the hashes hold
    forge_name(&url, "Mallory");
    rehash(&url, true);
    let problems = verify(&p, &url, Some(&public));
    assert!(
        !problems
            .iter()
            .any(|p| p.contains("content does not match")),
        "the chain was computed again correctly: {problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.ends_with("signature does not match the key")),
        "{problems:?}"
    );
    let _ = std::fs::remove_file(url.trim_start_matches("sqlite:"));
}

#[test]
fn a_chain_stripped_of_its_signatures_does_not_end_with_the_key() {
    let p = project();
    let (url, public, _) = signed_db(&p, "stripped");
    forge_name(&url, "Mallory");
    let mut c = Conn::open(&url).unwrap();
    c.batch("UPDATE \"_quadra_commits\" SET \"signature\" = NULL")
        .unwrap();
    c.batch("DELETE FROM \"_quadra_keys\"").unwrap();
    c.batch("DELETE FROM \"_quadra_meta\" WHERE \"key\" = 'signing_key'")
        .unwrap();
    drop(c);
    rehash(&url, false);
    // without an expected key the chain looks fine: the verifier must know the key
    assert_eq!(verify(&p, &url, None), Vec::<String>::new());
    let problems = verify(&p, &url, Some(&public));
    assert_eq!(
        problems,
        vec![format!(
            "the chain is not signed: the expected key is {}…",
            &public[..12]
        )]
    );
    let _ = std::fs::remove_file(url.trim_start_matches("sqlite:"));
}

#[test]
fn a_new_key_is_signed_by_the_old_one() {
    let p = project();
    let (url, old, _) = signed_db(&p, "rotated");
    rename(&p, &url, "Anna Rossi");
    let checked = quadra_analysis::check(&p);
    let model = checked.model.as_ref().unwrap();
    let (secret, new) = signing::generate();
    let mut db = Database::open(&url, model, &checked.report).unwrap();
    db.set_signing_key(&secret, time()).unwrap();
    drop(db);
    rename(&p, &url, "Anna Verdi");
    assert_eq!(verify(&p, &url, Some(&new)), Vec::<String>::new());
    assert_eq!(
        verify(&p, &url, Some(&old)),
        vec![format!(
            "the chain is signed by the key {}…, not by the expected key {}…",
            &new[..12],
            &old[..12]
        )]
    );
    // the key commit signed by the new key instead of the old: refused
    let mut c = Conn::open(&url).unwrap();
    let k = c
        .query(
            "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_keys\"",
            &[],
        )
        .unwrap()[0][0]
        .clone()
        .unwrap();
    let h = c
        .query(
            "SELECT \"hash\" FROM \"_quadra_commits\" WHERE \"tx\" = ?bigint",
            &[Sql::Int(k.parse().unwrap())],
        )
        .unwrap()[0][0]
        .clone()
        .unwrap();
    let by_new = signing::sign(&new, &h).unwrap();
    c.exec(
        "UPDATE \"_quadra_commits\" SET \"signature\" = ?text WHERE \"tx\" = ?bigint",
        &[Sql::Text(by_new), Sql::Int(k.parse().unwrap())],
    )
    .unwrap();
    drop(c);
    assert_eq!(
        verify(&p, &url, Some(&new)),
        vec![format!("commit {k}: signature does not match the key")]
    );
    let _ = std::fs::remove_file(url.trim_start_matches("sqlite:"));
}

#[test]
fn a_database_whose_key_is_not_held_refuses_to_commit() {
    let p = project();
    let checked = quadra_analysis::check(&p);
    let model = checked.model.as_ref().unwrap();
    let (url, _, _) = signed_db(&p, "unheld");
    // another key is active, whose private key this process never had
    let mut c = Conn::open(&url).unwrap();
    c.exec(
        "UPDATE \"_quadra_meta\" SET \"value\" = ?text WHERE \"key\" = 'signing_key'",
        &[Sql::Text("ab".repeat(32))],
    )
    .unwrap();
    drop(c);
    let mut i = engine(&url, model, &checked.report);
    let c = V::Ref {
        ent: "Customer".into(),
        id: i.ids("Customer", None)[0],
    };
    let e = i
        .transaction(|i| i.set(&c, "name", V::Text("x".into()), None))
        .unwrap_err();
    assert!(
        format!("{e:?}").contains("which this process does not hold"),
        "{e:?}"
    );
    let _ = std::fs::remove_file(url.trim_start_matches("sqlite:"));
}
