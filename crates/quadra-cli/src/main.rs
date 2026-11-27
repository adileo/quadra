//! `quadra`: command-line entry point (`quadra` alone prints the usage of every command).
//!
//!     quadra init [path]               a new project (asks the name and the database)
//!     quadra dev [path]                the app and the code editor, applying every saved change
//!     quadra check [path] [--json] [--fix]   names, types, cycles, flows, certified metrics, interface files
//!     quadra apps pull|push|new …     apps and views as interface files (`interfaces/**/*.qdi`)
//!     quadra trace <Entity.field> [path]   what a value depends on and who writes it
//!     quadra certify [name…] [path] [--approved-by login]   record fingerprints of metrics in metrics.lock
//!     quadra fmt [--check] [path]      format sources (whitespace only)
//!     quadra test [path] [--filter text] [--db sqlite|postgres://…]   run the tests
//!     quadra db init|status|migrate [--dry-run]|verify|provisional|sweep|fixture <name>|import <dir> [path] [--url URL]
//!     quadra db views export <file> | import <file> [path] [--url URL]   saved views, apps, folders and teams as JSON
//!     quadra change list | preview <name> | apply <name> | revert <name> [--approved-by login]
//!     quadra serve [path] [--url URL] [--port 8080] [--dev-login] [--secure-cookies] …
//!     quadra token create <login> … | revoke <name>
//!     quadra outbox list [--pending] [--json] | delivered <id…> [path] [--url URL]
//!     quadra bench gen|run …           the stress benchmark of examples/stress
//!     quadra parse <file>              print the syntax tree of a file
//!     quadra lsp                       start the language server on stdin/stdout
//!     quadra cloud login|logout|whoami Quadra Cloud (cloud.rs)
//!     quadra upgrade [--version X.Y.Z] the latest official binary in place of this one

use codespan_reporting::diagnostic::{Diagnostic, Label};
use codespan_reporting::files::SimpleFiles;
use codespan_reporting::term::{self, termcolor};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod apps;
mod bench;
mod cloud;
mod deploy;
mod dev;
mod http;
mod saved;
mod stress;
mod upgrade;

fn main() -> ExitCode {
    if cfg!(windows) {
        upgrade::clean_old();
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("check") => check(&args[1..]),
        Some("apps") => apps::apps(&args[1..]),
        Some("parse") => match args.get(1) {
            Some(file) => parse(Path::new(file)),
            None => usage(),
        },
        Some("trace") => match args.get(1) {
            Some(target) => trace(
                target,
                args.get(2)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(".")),
            ),
            None => usage(),
        },
        Some("certify") => certify(&args[1..]),
        Some("fmt") => fmt(&args[1..]),
        Some("test") => test(&args[1..]),
        Some("db") => db(&args[1..]),
        Some("change") => change(&args[1..]),
        Some("serve") => serve(&args[1..]),
        Some("init") => dev::init(&args[1..]),
        Some("dev") => dev::dev(&args[1..]),
        Some("token") => token(&args[1..]),
        Some("outbox") => outbox(&args[1..]),
        Some("bench") => bench::bench(&args[1..]),
        Some("cloud") => cloud::main(&args[1..]),
        Some("upgrade") => upgrade::main(&args[1..]),
        Some("lsp") => {
            quadra_lsp::run_with(Some(quadra_store::hints::hints));
            ExitCode::SUCCESS
        }
        Some("--version") | Some("-V") => {
            println!("quadra {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(cmd) if !cmd.starts_with('-') => external(cmd, &args[1..]),
        _ => usage(),
    }
}

/// `quadra <name> …` for a command it does not know: `quadra-<name>` on the PATH, if there is
/// one, runs with the rest of the arguments (as git and cargo do with their subcommands).
fn external(cmd: &str, rest: &[String]) -> ExitCode {
    let program = format!("quadra-{cmd}");
    match std::process::Command::new(&program).args(rest).status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("unknown command `{cmd}` (and no `{program}` on the PATH)");
            usage()
        }
        Err(e) => {
            eprintln!("cannot run `{program}`: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  quadra init [path] [--name n] [--db sqlite|postgres://…] [--yes] [--no-git]   a new project\n  \
         quadra dev [path] [--port 8080] [--url URL] [--fixture name] [--reset] [--web dir] [--user login]   app + code editor, applying every saved change\n  \
         quadra check [path] [--json] [--fix] check a project and its interface files (--fix: apply the suggested names in .qdi files)\n  \
         quadra apps pull [<name|id>|--all] [--to interfaces/] [path] [--url URL]   apps and views made in the client as .qdi files\n  \
         quadra apps push <file.qdi> [path] [--url URL]   install interface files now (`quadra serve`/`dev` do at start)\n  \
         quadra apps new <\"Name\"> [--entity E] [--to interfaces/] [path]   a starting .qdi file of an app\n  \
         quadra trace <Entity.field> [path]   what a value depends on and who writes it\n  \
         quadra certify [name…] [path] [--approved-by login]   record metric fingerprints in metrics.lock\n  \
         quadra fmt [--check] [path]          format sources\n  \
         quadra test [path] [--filter text] [--db sqlite|postgres://…]   run the tests\n  \
         quadra db init|status|migrate [--dry-run]|verify|provisional|sweep|fixture <name> [path] [--url URL]\n  \
         quadra db import <dir> [path] [--url URL] [--commit-size 250000]   bulk load of <Entity>.csv files\n  \
         quadra db views export <file> | import <file> [path] [--url URL]   saved views, apps, folders and teams as JSON\n  \
         quadra change list | preview <name> | apply <name> [--approved-by login] | revert <name> [--approved-by login]   [path] [--url URL]\n  \
         quadra serve [path] [--url URL] [--port 8080] [--web dir] [--dev-login] [--secure-cookies] [--cache-mb 256] [--prewarm-days N] [--sweep-every secs] [--editors login,…] [--job-timeout 120] [--max-rows N] [--max-list N] [--max-queue 1000] [--readers 2] [--writers 1]\n  \
         quadra token create <login> [--name n] [--read-only] [--days 90] | revoke <name>   [path] [--url URL]\n  \
         quadra outbox list [--pending] [--json] | delivered <id…>   [path] [--url URL]   events emitted by `emit`\n  \
         quadra bench gen --scale tiny|small|medium|large --out DIR [--seed N]   stress data as CSV\n  \
         quadra bench run [path] --url URL --scale … [--json file] [--timeout 60] [--reps 5] [--only a,b] [--skip a,b]   the stress benchmark\n  \
         quadra parse <file>                  print the syntax tree of a file\n  \
         quadra lsp                           start the language server\n  \
         quadra cloud login|deploy|deploys|rollback|link|projects|token|whoami|logout   Quadra Cloud (quadra cloud help)\n  \
         quadra upgrade [--version X.Y.Z]     the latest official binary in place of this one\n  \
         quadra --version"
    );
    ExitCode::from(2)
}

fn parse(file: &Path) -> ExitCode {
    let src = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };
    let parse = quadra_syntax::parse(&src);
    print!("{}", parse.debug_tree());
    for e in &parse.errors {
        eprintln!("error at {:?}: {}", e.range, e.message);
    }
    if parse.errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn load(path: &Path) -> Result<(PathBuf, quadra_sema::Project), ExitCode> {
    let root = match quadra_sema::find_project_root(path) {
        Some(r) => r,
        None => {
            eprintln!(
                "error: no quadra.toml found in {} or its parents",
                path.display()
            );
            return Err(ExitCode::FAILURE);
        }
    };
    match quadra_sema::Project::load(&root) {
        Ok(p) => Ok((root, p)),
        Err(e) => {
            eprintln!("error: {e}");
            Err(ExitCode::FAILURE)
        }
    }
}

fn trace(target: &str, path: PathBuf) -> ExitCode {
    let (_, project) = match load(&path) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let checked = quadra_analysis::check(&project);
    let Some(model) = &checked.model else {
        eprintln!("error: the project has errors, run `quadra check`");
        return ExitCode::FAILURE;
    };
    match quadra_analysis::trace(&project, model, &checked.report, target) {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `quadra certify [name…] [path] [--approved-by login]`: the metric names, the project path
/// (the argument that is a directory) and the approver. A flag's value is never a name.
fn certify_args(args: &[String]) -> (PathBuf, Vec<String>, Option<String>) {
    let mut words = positionals(args, &["--approved-by"]);
    let path = match words.iter().rposition(|w| Path::new(w).is_dir()) {
        Some(i) => PathBuf::from(words.remove(i)),
        None => PathBuf::from("."),
    };
    (path, words, flag(args, "--approved-by"))
}

fn certify(args: &[String]) -> ExitCode {
    let (path, names, approver) = certify_args(args);
    let (root, project) = match load(&path) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let checked = quadra_analysis::check(&project);
    let Some(model) = &checked.model else {
        eprintln!("error: the project has errors, run `quadra check`");
        return ExitCode::FAILURE;
    };
    let lock_path = root.join("metrics.lock");
    let old = std::fs::read_to_string(&lock_path)
        .map(|t| quadra_analysis::read_lock(&t))
        .unwrap_or_default();
    let today = std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let all = quadra_analysis::fingerprints(model, &checked.report);
    let unknown: Vec<&String> = names
        .iter()
        .filter(|n| !all.iter().any(|(m, _, _)| m == *n))
        .collect();
    if !unknown.is_empty() {
        let known: Vec<&str> = all.iter().map(|(m, _, _)| m.as_str()).collect();
        let list = unknown
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "error: unknown metric {list} (metrics: {})",
            if known.is_empty() {
                "none".into()
            } else {
                known.join(", ")
            }
        );
        return ExitCode::FAILURE;
    }
    let mut entries = Vec::new();
    for (name, fp, deps) in all {
        let prev = old.iter().find(|e| e.name == name);
        let selected = names.is_empty() || names.iter().any(|n| **n == name);
        let entry = match (prev, selected) {
            (None, false) => continue,
            (Some(p), false) => p.clone(),
            (Some(p), true) if p.fingerprint == fp => p.clone(),
            _ => {
                let by = approver
                    .clone()
                    .or_else(|| prev.map(|p| p.approved_by.clone()))
                    .unwrap_or_else(|| "unapproved".into());
                println!("certified {name}: {fp} ({deps} dependencies)");
                quadra_analysis::LockEntry {
                    name,
                    fingerprint: fp,
                    dependencies: deps,
                    approved_by: by,
                    approved_on: today.clone(),
                }
            }
        };
        entries.push(entry);
    }
    if let Err(e) = std::fs::write(&lock_path, quadra_analysis::write_lock(&entries)) {
        eprintln!("error: cannot write {}: {e}", lock_path.display());
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn fmt(args: &[String]) -> ExitCode {
    let check_only = args.iter().any(|a| a == "--check");
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let (_, project) = match load(&path) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let mut changed = 0;
    for f in project.user_files() {
        let formatted = quadra_analysis::format_source(&f.text);
        if formatted != f.text {
            changed += 1;
            if check_only {
                println!("would reformat {}", f.path.display());
            } else if let Err(e) = std::fs::write(&f.path, formatted) {
                eprintln!("error: cannot write {}: {e}", f.path.display());
                return ExitCode::FAILURE;
            }
        }
    }
    if check_only && changed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn test(args: &[String]) -> ExitCode {
    let filter = flag(args, "--filter");
    let target = flag(args, "--db");
    let path = positional(args, &["--filter", "--db"]);
    let (_, project) = match load(&path) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let mut backend = target.as_deref().map(quadra_store::DbBackend::new);
    let backend_ref = backend
        .as_mut()
        .map(|b| b as &mut dyn quadra_runtime::TestBackend);
    if let Some(t) = &target {
        println!("database: {t} (a fresh database per test, reloaded and verified at the end)\n");
    }
    match quadra_runtime::run_tests_with(&project, filter.as_deref(), backend_ref) {
        Err(diags) => {
            for d in diags
                .iter()
                .filter(|d| d.severity == quadra_sema::Severity::Error)
            {
                eprintln!("error: {}", d.message);
            }
            eprintln!("✗ the project has errors: run `quadra check`");
            ExitCode::FAILURE
        }
        Ok(results) => {
            let failed = results.iter().filter(|r| !r.passed).count();
            for r in &results {
                if r.passed {
                    println!("✓ {}", r.name);
                } else {
                    println!(
                        "✗ {}  ({})\n    {}",
                        r.name,
                        r.location,
                        r.message.as_deref().unwrap_or("").replace('\n', "\n    ")
                    );
                }
            }
            println!("\n{} passed, {failed} failed", results.len() - failed);
            if failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// The arguments that are neither flags nor a flag's value (`with_value`: the flags that
/// take one).
fn positionals(args: &[String], with_value: &[&str]) -> Vec<String> {
    args.iter()
        .enumerate()
        .filter(|(i, a)| {
            !a.starts_with("--") && (*i == 0 || !with_value.contains(&args[i - 1].as_str()))
        })
        .map(|(_, a)| a.clone())
        .collect()
}

/// The flags of the `db`, `change`, `token` and `outbox` commands that take a value.
const VALUE_FLAGS: &[&str] = &[
    "--url",
    "--approved-by",
    "--name",
    "--days",
    "--commit-size",
];

/// The first positional argument (see `positionals`) and the other arguments.
fn take_positional(args: &[String], with_value: &[&str]) -> (Option<String>, Vec<String>) {
    let at = args.iter().enumerate().position(|(i, a)| {
        !a.starts_with("--") && (i == 0 || !with_value.contains(&args[i - 1].as_str()))
    });
    match at {
        Some(i) => (
            Some(args[i].clone()),
            args.iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, a)| a.clone())
                .collect(),
        ),
        None => (None, args.to_vec()),
    }
}

/// First argument that is neither a flag nor a flag's value.
fn positional(args: &[String], with_value: &[&str]) -> PathBuf {
    positionals(args, with_value)
        .into_iter()
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `--url`, else `[database] url` in quadra.toml, else `sqlite:<root>/.quadra/data.db`.
pub(crate) fn database_url(root: &Path, args: &[String]) -> String {
    if let Some(u) = flag(args, "--url") {
        return u;
    }
    if let Ok(toml) = std::fs::read_to_string(root.join("quadra.toml")) {
        let mut in_db = false;
        for line in toml.lines() {
            let l = line.trim();
            if l.starts_with('[') {
                in_db = l == "[database]";
            } else if in_db && l.starts_with("url") {
                if let Some((_, v)) = l.split_once('=') {
                    let v = v.trim().trim_matches('"');
                    return match v.strip_prefix("sqlite:") {
                        Some(p) if !p.starts_with('/') && p != ":memory:" => {
                            format!("sqlite:{}", root.join(p).display())
                        }
                        _ => v.to_string(),
                    };
                }
            }
        }
    }
    let dir = root.join(".quadra");
    let _ = std::fs::create_dir_all(&dir);
    format!("sqlite:{}", dir.join("data.db").display())
}

/// Project, checked model and database url for the `db` and `change` commands.
fn open_project(rest: &[String]) -> Result<(PathBuf, quadra_analysis::Checked, String), ExitCode> {
    let path = positional(rest, VALUE_FLAGS);
    let (root, project) = load(&path)?;
    let checked = quadra_analysis::check(&project);
    if checked.model.is_none() {
        eprintln!("error: the project has errors, run `quadra check`");
        return Err(ExitCode::FAILURE);
    }
    // the interface files are part of the version deployed
    let bad = checked
        .diagnostics
        .iter()
        .filter(|d| {
            d.severity == quadra_sema::Severity::Error
                && project.file(d.file).kind == quadra_sema::FileKind::Interface
        })
        .count();
    if bad > 0 && matches!(rest.first().map(String::as_str), Some("migrate" | "init")) {
        eprintln!("error: the interface files have {bad} error(s), run `quadra check`");
        return Err(ExitCode::FAILURE);
    }
    let url = database_url(&root, rest);
    Ok((root, checked, url))
}

fn finish(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn db(args: &[String]) -> ExitCode {
    let Some(cmd) = args.first().map(String::as_str) else {
        return usage();
    };
    // `db fixture <name> [path]`: the name comes before the path
    let views_cmd = if cmd == "views" {
        args.get(1).cloned()
    } else {
        None
    };
    let (fixture_name, rest) = match cmd {
        "fixture" | "import" => take_positional(&args[1..], VALUE_FLAGS),
        // `db views export|import <file> [path]`
        "views" => take_positional(args.get(2..).unwrap_or(&[]), VALUE_FLAGS),
        _ => (None, args[1..].to_vec()),
    };
    let rest = rest.as_slice();
    let (root, checked, url) = match open_project(rest) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let model = checked.model.as_ref().unwrap();
    let report = &checked.report;
    use quadra_store::ops;
    finish((|| {
        match cmd {
        "init" => {
            let _writer = quadra_store::writer::acquire(&url)?;
            let tables = ops::init(&url, model, report, now())?;
            println!("✓ initialized {url}: {tables} tables, seeds committed");
            Ok(())
        }
        "status" => {
            let mut database = quadra_store::Database::open(&url, model, report)?;
            let s = database.status()?;
            if !s.initialized {
                println!("{url}: not initialized (run `quadra db init`)");
                return Ok(());
            }
            let interp = ops::loaded(&mut database, model, now())?;
            let pending: Vec<&str> =
                model.migrations.iter().filter(|m| !interp.store.applied.contains_key(&format!("migration:{}", m.name))).map(|m| m.name.as_str()).collect();
            println!("{url}");
            println!("  schema      {} ({})", s.fingerprint, if s.schema_current { "up to date" } else { "CHANGED: run `quadra db migrate`" });
            if !pending.is_empty() {
                println!("  migrations  {} pending: {}", pending.len(), pending.join(", "));
            }
            println!("  commits     {}", s.commits);
            println!("  last        {}", s.last_hash.unwrap_or_default());
            Ok(())
        }
        "verify" => {
            let (problems, records) = ops::verify(&url, model, report, now())?;
            if problems.is_empty() {
                println!("✓ hash chain intact, tables match the history ({records} records)");
                Ok(())
            } else {
                for p in &problems {
                    println!("✗ {p}");
                }
                Err(format!("{} problem(s) found", problems.len()))
            }
        }
        "migrate" => {
            let dry = rest.iter().any(|a| a == "--dry-run");
            let _writer = if dry { None } else { Some(quadra_store::writer::acquire(&url)?) };
            let Some(r) = ops::migrate(&url, model, report, &root, now(), dry)? else {
                println!("schema up to date, no pending migrations: nothing to migrate");
                return Ok(());
            };
            if !r.incremental {
                for t in &r.added {
                    println!("  + table {t}");
                }
                for t in &r.removed {
                    println!("  - table {t}");
                }
            }
            for (name, lines) in &r.scripts {
                println!("  migration {name}");
                for l in lines {
                    println!("    {l}");
                }
            }
            if r.new_defaults > 0 {
                println!("  {} new values filled from defaults", r.new_defaults);
            }
            if r.incremental {
                for s in &r.steps {
                    println!("  {s}");
                }
                if r.steps.is_empty() {
                    println!("  no table changes");
                }
            } else {
                println!("  tables are rebuilt from the history in one transaction ({})", r.rebuilt_because.as_deref().unwrap_or("first deploy"));
            }
            for i in &r.indexes_created {
                println!("  + index {i}");
            }
            for i in &r.indexes_dropped {
                println!("  - index {i}");
            }
            for e in &r.index_errors {
                println!("  ! index not built yet (the next deploy builds it): {e}");
            }
            if dry {
                println!("✓ dry run: the data satisfies the new model; nothing written");
            } else {
                println!("✓ migrated to schema {}", r.fingerprint);
            }
            Ok(())
        }
        "provisional" => {
            let values = ops::provisional(&url, model, report, now())?;
            if values.is_empty() {
                println!("no provisional values");
            }
            let today = now().date();
            for (ent, id, field, reason, until) in values {
                let when = match until {
                    Some(u) if u < today => format!("EXPIRED on {u}"),
                    Some(u) => format!("until {u}"),
                    None => "no deadline".into(),
                };
                println!("  {ent}#{id}.{field}: {reason} ({when})");
            }
            Ok(())
        }
        "fixture" => {
            let Some(name) = &fixture_name else { return Err("usage: quadra db fixture <name> [path]".into()) };
            let _writer = quadra_store::writer::acquire(&url)?;
            let tx = ops::fixture(&url, model, report, name, now())?;
            println!("✓ fixture {name} loaded in transaction {tx}");
            Ok(())
        }
        "import" => {
            let Some(dir) = &fixture_name else { return Err("usage: quadra db import <dir> [path] [--url URL] [--commit-size N]".into()) };
            let per_commit = match flag(rest, "--commit-size") {
                Some(n) => Some(n.parse::<usize>().map_err(|_| format!("--commit-size: `{n}` is not a number"))?),
                None => None,
            };
            let _writer = quadra_store::writer::acquire(&url)?;
            println!("importing {dir} into {url}");
            let r = quadra_store::bulk::import_dir(&url, model, report, Path::new(dir), now(), per_commit, &mut |l| println!("{l}"))?;
            for (e, n) in &r.entities {
                println!("  + {n} {e}");
            }
            let commits = match (r.commits.first(), r.commits.last()) {
                (Some(a), Some(b)) if a != b => format!("commits {a}–{b}"),
                (Some(a), _) => format!("commit {a}"),
                _ => "no commit".into(),
            };
            if r.warnings > 0 {
                println!("  {} records have warnings", r.warnings);
            }
            println!("✓ {} records, {} history entries, {commits}", r.records, r.history);
            Ok(())
        }
        "views" => {
            let usage = "usage: quadra db views export <file.json> | import <file.json> [path] [--url URL]";
            let Some(file) = &fixture_name else { return Err(usage.into()) };
            let mut database = quadra_store::Database::open(&url, model, report)?;
            if !database.status()?.initialized {
                return Err("the database is not initialized (run `quadra db init`)".into());
            }
            let schema = database.schema.clone();
            drop(database);
            match views_cmd.as_deref() {
                Some("export") => {
                    let doc = saved::export(&url, &schema)?;
                    std::fs::write(file, serde_json::to_string_pretty(&doc).unwrap()).map_err(|e| format!("cannot write {file}: {e}"))?;
                    let items = doc["views"].as_array().cloned().unwrap_or_default();
                    let folders = items.iter().filter(|v| v["kind"] == serde_json::json!("folder")).count();
                    println!("✓ {} teams, {folders} folders, {} views and apps written to {file}", doc["teams"].as_array().map_or(0, |a| a.len()), items.len() - folders);
                    Ok(())
                }
                Some("import") => {
                    let text = std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
                    let doc: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{file}: {e}"))?;
                    let (teams, folders, views) = saved::import(&url, &schema, &doc, &now().format("%Y-%m-%dT%H:%M:%S").to_string())?;
                    println!("✓ {teams} teams, {folders} folders, {views} views and apps imported (checked against the model when the server starts)");
                    Ok(())
                }
                _ => Err(usage.into()),
            }
        }
        "sweep" => {
            let _writer = quadra_store::writer::acquire(&url)?;
            match ops::sweep(&url, model, report, now())? {
                Some(tx) => println!("✓ time-driven rules applied in transaction {tx}"),
                None => println!("✓ nothing changed"),
            }
            Ok(())
        }
        _ => Err(format!("unknown db command `{cmd}` (init, status, migrate, verify, provisional, sweep, fixture, import, views)")),
    }
    })())
}

/// `quadra outbox list [--pending] [--json]`: the events emitted by `emit`, written with
/// their commit; `quadra outbox delivered <id…>`: an integration has handed them on.
fn outbox(args: &[String]) -> ExitCode {
    let Some(cmd) = args.first().map(String::as_str) else {
        return usage();
    };
    let mut rest = args[1..].to_vec();
    let mut ids: Vec<u64> = Vec::new();
    if cmd == "delivered" {
        // the ids (`12` or `#12`) are the positional arguments before the path
        while let (Some(word), others) = take_positional(&rest, VALUE_FLAGS) {
            let Ok(id) = word.trim_start_matches('#').parse::<u64>() else {
                break;
            };
            ids.push(id);
            rest = others;
        }
        if ids.is_empty() {
            eprintln!("usage: quadra outbox delivered <id…> [path] [--url URL]");
            return ExitCode::from(2);
        }
    }
    let rest = rest.as_slice();
    let (_, checked, url) = match open_project(rest) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let model = checked.model.as_ref().unwrap();
    let report = &checked.report;
    use quadra_store::ops;
    finish((|| match cmd {
        "list" => {
            let pending = rest.iter().any(|a| a == "--pending");
            let events = ops::outbox(&url, model, report, pending)?;
            if rest.iter().any(|a| a == "--json") {
                for e in &events {
                    let line = serde_json::json!({
                        "id": e.id, "tx": e.tx, "event": e.event, "fields": e.fields,
                        "created_at": e.created_at, "delivered_at": e.delivered_at,
                    });
                    println!("{line}");
                }
                return Ok(());
            }
            if events.is_empty() {
                println!(
                    "{}",
                    if pending {
                        "no pending events"
                    } else {
                        "the outbox is empty"
                    }
                );
            }
            for e in &events {
                let state = match &e.delivered_at {
                    Some(at) => format!("delivered {at}"),
                    None => "pending".into(),
                };
                println!(
                    "#{:<5} tx {:<6} {}  {}  {}  ({state})",
                    e.id, e.tx, e.created_at, e.event, e.fields
                );
            }
            Ok(())
        }
        "delivered" => {
            let n = ops::outbox_delivered(&url, model, report, &ids, now())?;
            println!("✓ {n} event(s) marked delivered");
            Ok(())
        }
        _ => Err(format!("unknown outbox command `{cmd}` (list, delivered)")),
    })())
}

const SERVE_FLAGS: &[&str] = &[
    "--url",
    "--port",
    "--web",
    "--cache-mb",
    "--prewarm-days",
    "--sweep-every",
    "--editors",
    "--job-timeout",
    "--max-rows",
    "--max-list",
    "--max-queue",
    "--readers",
    "--writers",
    "--log-dir",
];

/// `--job-timeout <seconds>` (0: none), `--max-rows <n>`, `--max-list <n>`, `--max-queue <n>`
/// (0: none): the limits of the server's requests, on top of the defaults (two minutes, no
/// caps on rows and lists, 1000 requests waiting).
fn serve_limits(args: &[String]) -> quadra_server::Limits {
    let mut l = quadra_server::Limits::default();
    if let Some(s) = flag(args, "--job-timeout").and_then(|v| v.parse::<u64>().ok()) {
        l.job = (s > 0).then(|| std::time::Duration::from_secs(s));
    }
    l.max_rows = flag(args, "--max-rows")
        .and_then(|v| v.parse().ok())
        .or(l.max_rows);
    l.max_list = flag(args, "--max-list")
        .and_then(|v| v.parse().ok())
        .or(l.max_list);
    if let Some(n) = flag(args, "--max-queue").and_then(|v| v.parse::<usize>().ok()) {
        l.max_queue = (n > 0).then_some(n);
    }
    l
}

fn serve(args: &[String]) -> ExitCode {
    let path = positional(args, SERVE_FLAGS);
    let (root, _) = match load(&path) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let url = database_url(&root, args);
    let num = |f: &str, d: u64| flag(args, f).and_then(|v| v.parse().ok()).unwrap_or(d);
    let cfg = quadra_server::Config {
        url,
        port: num("--port", 8080) as u16,
        dev_login: args.iter().any(|a| a == "--dev-login"),
        secure_cookies: args.iter().any(|a| a == "--secure-cookies"),
        cache_mb: num("--cache-mb", 256) as usize,
        prewarm_days: flag(args, "--prewarm-days").and_then(|v| v.parse().ok()),
        web: flag(args, "--web").map(PathBuf::from),
        sweep_every: num("--sweep-every", 3600),
        editors: flag(args, "--editors")
            .map(|e| {
                e.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        watch: false,
        limits: serve_limits(args),
        engines: quadra_server::Engines {
            readers: num("--readers", 2) as usize,
            writers: num("--writers", 1).max(1) as usize,
        },
        root,
    };
    // every commit also kept as a file in this folder (another disk) before it is confirmed
    let log_dir = flag(args, "--log-dir").map(std::path::PathBuf::from);
    finish(quadra_server::serve_with(cfg, log_dir))
}

fn token(args: &[String]) -> ExitCode {
    let Some(cmd) = args.first().map(String::as_str) else {
        return usage();
    };
    let (Some(arg), rest) = take_positional(&args[1..], VALUE_FLAGS) else {
        return usage();
    };
    let rest = rest.as_slice();
    let (_, checked, url) = match open_project(rest) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let model = checked.model.as_ref().unwrap();
    finish((|| -> Result<(), String> {
        let mut conn = quadra_store::Conn::open(&url)?;
        match cmd {
            "create" => {
                let mut database = quadra_store::Database::open(&url, model, &checked.report)?;
                let mut interp = quadra_store::ops::loaded(&mut database, model, now())?;
                let users = interp.ids("User", None);
                let user = users
                    .into_iter()
                    .find(|id| {
                        interp
                            .store
                            .read(*id, "login", None)
                            .map_or(false, |v| v.to_string() == arg)
                    })
                    .ok_or_else(|| format!("no user `{arg}`"))?;
                let name = flag(rest, "--name")
                    .unwrap_or_else(|| format!("{arg}-{}", now().format("%Y%m%d%H%M%S")));
                let days: i64 = flag(rest, "--days")
                    .and_then(|d| d.parse().ok())
                    .unwrap_or(90);
                let read_only = rest.iter().any(|a| a == "--read-only");
                let (secret, _) = quadra_store::auth::create(
                    &mut conn,
                    "token",
                    user,
                    &name,
                    read_only,
                    now(),
                    Some(now() + chrono::Duration::days(days)),
                )?;
                println!(
                    "token `{name}` for {arg}{}, valid {days} days:",
                    if read_only { " (read-only)" } else { "" }
                );
                println!("{secret}");
                println!("it is shown only now: store it in a secret manager");
                Ok(())
            }
            "revoke" => {
                let n = quadra_store::auth::revoke(&mut conn, &arg)?;
                println!("{n} credential(s) revoked");
                Ok(())
            }
            _ => Err(format!("unknown token command `{cmd}` (create, revoke)")),
        }
    })())
}

fn change(args: &[String]) -> ExitCode {
    let Some(cmd) = args.first().map(String::as_str) else {
        return usage();
    };
    let needs_name = matches!(cmd, "preview" | "apply" | "revert");
    let (name, rest) = if needs_name {
        match take_positional(&args[1..], VALUE_FLAGS) {
            (Some(n), rest) => (n, rest),
            (None, _) => return usage(),
        }
    } else {
        (String::new(), args[1..].to_vec())
    };
    let rest = rest.as_slice();
    let (_, checked, url) = match open_project(rest) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let model = checked.model.as_ref().unwrap();
    let report = &checked.report;
    let approver = flag(rest, "--approved-by");
    use quadra_store::ops;
    finish((|| match cmd {
        "list" => {
            let changes = ops::list_changes(&url, model, report, now())?;
            if changes.is_empty() {
                println!("no changes in the project");
            }
            for c in changes {
                let status = match (c.applied_tx, c.reverted_tx) {
                    (Some(a), Some(r)) => format!("applied (tx {a}), reverted (tx {r})"),
                    (Some(a), None) => format!("applied (tx {a})"),
                    _ => "pending".into(),
                };
                println!("  {:<28} {status}", c.name);
                if let Some(r) = &c.meta.reason {
                    println!("  {:<28} {r}", "");
                }
            }
            Ok(())
        }
        "preview" | "apply" => {
            let apply = cmd == "apply";
            let _writer = if apply {
                Some(quadra_store::writer::acquire(&url)?)
            } else {
                None
            };
            let o = ops::run_change(
                &url,
                model,
                report,
                &name,
                approver.as_deref(),
                now(),
                apply,
            )?;
            if let Some(r) = &o.meta.reason {
                println!("{name}: {r}");
            }
            if let Some(role) = &o.meta.approved_by_role {
                println!(
                    "  approval: {}",
                    if role.starts_with(|c: char| "AEIOUaeiou".contains(c)) {
                        format!("an {role}")
                    } else {
                        format!("a {role}")
                    }
                );
            }
            if let Some(d) = o.meta.effective {
                println!("  effective from {d}");
            }
            for (ent, n) in &o.diff.created {
                println!("  + {n} new {ent}");
            }
            for (ent, id, field, old, new) in o.diff.changed.iter().take(50) {
                println!("  ~ {ent}#{id}.{field}: {old} → {new}");
            }
            if o.diff.changed.len() > 50 {
                println!("  … and {} more values", o.diff.changed.len() - 50);
            }
            if o.diff.ledger_rows > 0 {
                println!("  {} ledger rows", o.diff.ledger_rows);
            }
            if !o.metrics.is_empty() {
                println!("  certified metrics affected: {}", o.metrics.join(", "));
            }
            match o.tx {
                Some(tx) => println!("✓ applied in transaction {tx}"),
                None => println!("✓ preview: constraints and expectations hold; nothing written"),
            }
            Ok(())
        }
        "revert" => {
            let _writer = quadra_store::writer::acquire(&url)?;
            let n = ops::revert_change(&url, model, report, &name, approver.as_deref(), now())?;
            println!("✓ reverted {name}: {n} values restored in a new transaction");
            Ok(())
        }
        _ => Err(format!(
            "unknown change command `{cmd}` (list, preview, apply, revert)"
        )),
    })())
}

fn check(args: &[String]) -> ExitCode {
    let json = args.iter().any(|a| a == "--json");
    let fix = args.iter().any(|a| a == "--fix");
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let (root, mut project) = match load(&path) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let mut diagnostics = quadra_analysis::check(&project).diagnostics;
    if fix {
        let fixed = apply_fixes(&project, &diagnostics);
        if fixed > 0 {
            eprintln!("✓ {fixed} name(s) fixed in the interface files");
            project = match quadra_sema::Project::load(&root) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            diagnostics = quadra_analysis::check(&project).diagnostics;
        }
    }
    let (mut errors, mut warnings) = (0, 0);
    for d in &diagnostics {
        match d.severity {
            quadra_sema::Severity::Error => errors += 1,
            quadra_sema::Severity::Warning => warnings += 1,
            quadra_sema::Severity::Hint => {}
        }
    }
    let modules = project.files().count();
    if json {
        let list: Vec<serde_json::Value> = diagnostics
            .iter()
            .filter(|d| d.severity != quadra_sema::Severity::Hint)
            .map(|d| diagnostic_json(&root, &project, d))
            .collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "ok": errors == 0, "errors": errors, "warnings": warnings, "diagnostics": list })).unwrap());
        return if errors == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let mut files = SimpleFiles::new();
    let mut ids = std::collections::HashMap::new();
    for f in project.files() {
        let display = f
            .path
            .strip_prefix(&root)
            .unwrap_or(&f.path)
            .display()
            .to_string();
        ids.insert(f.id, files.add(display, f.text.clone()));
    }
    let writer = termcolor::StandardStream::stderr(termcolor::ColorChoice::Auto);
    let config = term::Config::default();
    for d in &diagnostics {
        let diag = match d.severity {
            quadra_sema::Severity::Error => Diagnostic::error(),
            quadra_sema::Severity::Warning => Diagnostic::warning(),
            quadra_sema::Severity::Hint => Diagnostic::note(),
        };
        let range = usize::from(d.range.start())..usize::from(d.range.end());
        let mut diag = diag
            .with_message(&d.message)
            .with_labels(vec![Label::primary(ids[&d.file], range)]);
        if let Some(f) = fix_of(&d.message) {
            diag = diag.with_notes(vec![format!(
                "fix: write `{f}` (`quadra check --fix` does it in .qdi files)"
            )]);
        }
        let _ = term::emit(&mut writer.lock(), &config, &files, &diag);
    }
    let interfaces = project.interface_files().count();
    let what = if interfaces > 0 {
        format!("{modules} modules ({interfaces} interface files)")
    } else {
        format!("{modules} modules")
    };
    if errors == 0 {
        eprintln!("✓ {what} checked, {warnings} warnings");
        ExitCode::SUCCESS
    } else {
        eprintln!("✗ {errors} errors, {warnings} warnings in {what}");
        ExitCode::FAILURE
    }
}

/// The name a message suggests (`did you mean `x`?`).
fn fix_of(message: &str) -> Option<String> {
    let rest = message.split("did you mean `").nth(1)?;
    rest.split('`')
        .next()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// A diagnostic as `quadra check --json` gives it: the file, 1-based lines and columns
/// (in characters), and the fix when there is one.
fn diagnostic_json(
    root: &Path,
    project: &quadra_sema::Project,
    d: &quadra_sema::Diagnostic,
) -> serde_json::Value {
    let f = project.file(d.file);
    let pos = |at: usize| {
        let at = at.min(f.text.len());
        let before = &f.text[..at];
        (
            before.matches('\n').count() + 1,
            before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1,
        )
    };
    let (start, end) = (usize::from(d.range.start()), usize::from(d.range.end()));
    let ((l1, c1), (l2, c2)) = (pos(start), pos(end));
    let path = if f.kind == quadra_sema::FileKind::Std {
        f.path.display().to_string()
    } else {
        f.path
            .strip_prefix(root)
            .unwrap_or(&f.path)
            .display()
            .to_string()
    };
    serde_json::json!({
        "path": path,
        "line": l1, "column": c1, "end_line": l2, "end_column": c2,
        "severity": match d.severity { quadra_sema::Severity::Error => "error", quadra_sema::Severity::Warning => "warning", quadra_sema::Severity::Hint => "hint" },
        "message": d.message,
        "fix": fix_of(&d.message).map(|w| serde_json::json!({ "replace": &f.text[start.min(f.text.len())..end.min(f.text.len())], "with": w })),
    })
}

/// Apply the suggested names of the diagnostics of interface files (a name renamed by a
/// migration, a close one): the number of edits.
fn apply_fixes(project: &quadra_sema::Project, diagnostics: &[quadra_sema::Diagnostic]) -> usize {
    let mut by_file: std::collections::HashMap<quadra_sema::FileId, Vec<(usize, usize, String)>> =
        Default::default();
    for d in diagnostics {
        let f = project.file(d.file);
        if f.kind != quadra_sema::FileKind::Interface {
            continue;
        }
        let Some(w) = fix_of(&d.message) else {
            continue;
        };
        let (a, b) = (usize::from(d.range.start()), usize::from(d.range.end()));
        let old = f.text.get(a..b).unwrap_or("");
        // only a name, replaced by a name
        if old.is_empty() || !old.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        by_file.entry(d.file).or_default().push((a, b, w));
    }
    let mut n = 0;
    for (file, mut edits) in by_file {
        let f = project.file(file);
        edits.sort();
        edits.dedup();
        let mut text = f.text.clone();
        for (a, b, w) in edits.iter().rev() {
            text.replace_range(*a..*b, w);
            n += 1;
        }
        if let Err(e) = std::fs::write(&f.path, text) {
            eprintln!("error: {}: {e}", f.path.display());
        }
    }
    n
}

/// The instant (UTC) of commits made by `quadra db` (tests use their own clock).
fn now() -> chrono::NaiveDateTime {
    // an instant, in UTC (docs: a DateTime is an instant; the project's zone reads it)
    let t = chrono::Utc::now().naive_utc();
    t - chrono::Duration::nanoseconds(t.and_utc().timestamp_subsec_nanos() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    /// 26. A flag's value is not a metric name, nor a change name, nor a path.
    #[test]
    fn flag_values_are_not_positional_arguments() {
        let dir = std::env::temp_dir();
        let d = dir.display().to_string();
        let (path, names, by) = certify_args(&strings(&["--approved-by", "alice"]));
        assert_eq!(
            (path, names, by.as_deref()),
            (PathBuf::from("."), vec![], Some("alice"))
        );
        let (path, names, by) = certify_args(&strings(&["margin", "--approved-by", "alice", &d]));
        assert_eq!(
            (path, names, by.as_deref()),
            (dir.clone(), strings(&["margin"]), Some("alice"))
        );
        let (path, names, _) =
            certify_args(&strings(&["--approved-by", "alice", "margin", "revenue"]));
        assert_eq!(
            (path, names),
            (PathBuf::from("."), strings(&["margin", "revenue"]))
        );
        let (path, names, _) = certify_args(&strings(&[&d, "margin", "--approved-by", "alice"]));
        assert_eq!((path, names), (dir.clone(), strings(&["margin"])));
        // change apply / db fixture / token: the name is the first positional argument
        let (name, rest) = take_positional(
            &strings(&["--approved-by", "alex", "prices", &d]),
            VALUE_FLAGS,
        );
        assert_eq!(name.as_deref(), Some("prices"));
        assert_eq!(positional(&rest, VALUE_FLAGS), dir);
        let (name, rest) =
            take_positional(&strings(&["--url", "sqlite:x.db", "demo"]), VALUE_FLAGS);
        assert_eq!(
            (name.as_deref(), rest),
            (Some("demo"), strings(&["--url", "sqlite:x.db"]))
        );
        assert_eq!(
            positional(&strings(&["--url", "sqlite:x.db"]), VALUE_FLAGS),
            PathBuf::from(".")
        );
        // db import: the directory first, then the project
        let (name, rest) = take_positional(
            &strings(&["--commit-size", "1000", "data", &d]),
            VALUE_FLAGS,
        );
        assert_eq!(name.as_deref(), Some("data"));
        assert_eq!(positional(&rest, VALUE_FLAGS), dir);
        // bench: the project is the argument that is not a flag's value
        assert_eq!(
            bench::path_arg(&strings(&[
                "--url",
                "sqlite:x.db",
                "--scale",
                "small",
                &d,
                "--json",
                "out.json"
            ])),
            dir
        );
        assert_eq!(
            bench::path_arg(&strings(&["--scale", "tiny", "--only", "grid,load"])),
            PathBuf::from(".")
        );
    }

    /// Every scenario of the benchmark is described once, the load first.
    #[test]
    fn bench_scenarios() {
        let names: Vec<&str> = bench::STEPS.iter().map(|s| s.0).collect();
        assert_eq!(names[0], "load");
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
        assert!(names.iter().filter(|n| n.starts_with("deploy_")).count() == 8);
    }
}
