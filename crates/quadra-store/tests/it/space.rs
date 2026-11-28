//! The space of what goes: an app with `history none` and `deletable` creates and deletes many
//! records, and its file goes back to about its first size, without their values; and what a
//! commit costs, in bytes and time, with the whole history and with none.
//! `QUADRA_SPACE_N` records (default 5,000; the criterion of phase 19d is 100,000).

use quadra_runtime::{Interp, V};
use quadra_sema::Project;
use quadra_store::{materialized_formulas, ops, Conn, Database, DbSource, Sink};
use std::time::Instant;

fn engine<'m>(url: &str, m: &'m quadra_ir::Model, r: &'m quadra_types::Report) -> Interp<'m> {
    let db = Database::open(url, m, r).unwrap();
    let src = DbSource::open(url, db.schema.clone(), None).unwrap();
    let mut i = Interp::new(m);
    i.store = quadra_runtime::Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    i.materialize = materialized_formulas(&db.schema);
    i.sink = Some(Box::new(Sink { db, model: m }));
    i.load_units();
    i
}

fn size(url: &str) -> u64 {
    let path = url.trim_start_matches("sqlite:");
    let mut c = Conn::open(url).unwrap();
    quadra_store::conn::reclaim(&mut c).unwrap();
    std::fs::metadata(path).unwrap().len()
}

fn setup(name: &str, keep: &str) -> (std::path::PathBuf, Project) {
    let dir = std::env::temp_dir().join(format!("quadra-space-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("p")).unwrap();
    std::fs::write(dir.join("p/quadra.toml"), "[project]\nname = \"s\"\n").unwrap();
    std::fs::write(
        dir.join("p/main.qd"),
        format!("entity Visit:\n    deletable\n    {keep}\n    token: Text\n    page: Text\n    n: Int\n"),
    )
    .unwrap();
    let p = Project::load(&dir.join("p")).unwrap();
    (dir, p)
}

fn n() -> usize {
    std::env::var("QUADRA_SPACE_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000)
}

#[test]
fn what_is_deleted_gives_its_space_back() {
    let n = n();
    let (dir, p) = setup("deleted", "history none");
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let url = format!("sqlite:{}", dir.join("data.db").display());
    ops::init(
        &url,
        m,
        r,
        chrono::NaiveDate::from_ymd_opt(2026, 3, 1)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap(),
    )
    .unwrap();
    let empty = size(&url);
    let mut i = engine(&url, m, r);
    for chunk in 0..n.div_ceil(1000) {
        i.transaction(|i| {
            for k in chunk * 1000..((chunk + 1) * 1000).min(n) {
                i.create(
                    "Visit",
                    vec![
                        (
                            "token".into(),
                            V::Text(format!("secret-token-{k:08}").into()),
                        ),
                        (
                            "page".into(),
                            V::Text(format!("/products/{k}/{}", "x".repeat(80)).into()),
                        ),
                        ("n".into(), V::Int(k as i64)),
                    ],
                )?;
            }
            Ok(())
        })
        .unwrap();
    }
    drop(i);
    let full = size(&url);
    let mut i = engine(&url, m, r);
    let ids = i.ids("Visit", None);
    assert_eq!(ids.len(), n);
    for chunk in ids.chunks(1000) {
        i.transaction(|i| {
            for id in chunk {
                i.delete(&V::Ref {
                    ent: "Visit".into(),
                    id: *id,
                })?;
            }
            Ok(())
        })
        .unwrap();
    }
    drop(i);
    let after = size(&url);
    eprintln!("{n} visits: empty {empty} bytes, full {full}, all deleted {after}");
    let bytes = std::fs::read(url.trim_start_matches("sqlite:")).unwrap();
    assert!(
        !bytes.windows(13).any(|w| w == b"secret-token-"),
        "the file still holds a token"
    );
    // what is left of each: its commit, a row of `_quadra_erased` and of `_quadra_erasures`
    assert!(
        after < empty + (full - empty) / 3,
        "{empty} → {full} → {after}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The bytes and the time of commits that change a value, with the whole history and with
/// none. Prints them; run with `--ignored --nocapture` (in release for real times).
#[test]
#[ignore]
fn history_full_and_none_compared() {
    let n = n();
    for keep in ["history full", "history none"] {
        let (dir, p) = setup(&keep.replace(' ', "-"), keep);
        let checked = quadra_analysis::check(&p);
        let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
        let url = format!("sqlite:{}", dir.join("data.db").display());
        ops::init(
            &url,
            m,
            r,
            chrono::NaiveDate::from_ymd_opt(2026, 3, 1)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
        )
        .unwrap();
        let mut i = engine(&url, m, r);
        i.transaction(|i| {
            for k in 0..100 {
                i.create(
                    "Visit",
                    vec![
                        ("token".into(), V::Text(format!("t{k}").into())),
                        ("page".into(), V::Text("/".into())),
                        ("n".into(), V::Int(0)),
                    ],
                )?;
            }
            Ok(())
        })
        .unwrap();
        let ids = i.ids("Visit", None);
        let start = size(&url);
        let t = Instant::now();
        for k in 0..n {
            let id = ids[k % ids.len()];
            i.transaction(|i| {
                i.set(
                    &V::Ref {
                        ent: "Visit".into(),
                        id,
                    },
                    "page",
                    V::Text(format!("/page/{k}/{}", "y".repeat(100)).into()),
                    None,
                )
            })
            .unwrap();
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        drop(i);
        let grown = size(&url) - start;
        eprintln!(
            "{keep}: {n} commits, {:.3} ms each, the file grew {} bytes ({} per commit)",
            ms,
            grown,
            grown / n as u64
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
