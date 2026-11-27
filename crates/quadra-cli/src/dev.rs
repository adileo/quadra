//! `quadra init`: a new project in a folder. `quadra dev`: the app and the code editor on
//! this machine, applying every saved change at once (hot reload, migrations included).

use crate::{check, database_url, flag, load, now, positional};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const STARTER_MAIN: &str = include_str!("../templates/starter/main.qd");
const STARTER_TEST: &str = include_str!("../templates/starter/tests/main.test.qd");

/// Ask on the terminal, with a default (`--yes` or no terminal: the default).
fn ask(interactive: bool, question: &str, default: &str) -> String {
    if !interactive {
        return default.to_string();
    }
    print!("  {question} \x1b[2m({default})\x1b[0m ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    let a = line.trim();
    if a.is_empty() {
        default.to_string()
    } else {
        a.to_string()
    }
}

/// A project name from a folder name: lowercase letters, digits, `-` and `_`.
fn project_name(raw: &str) -> String {
    let mut s: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "my-app".into()
    } else {
        s
    }
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_or(false, |o| o.status.success())
}

pub fn init(args: &[String]) -> ExitCode {
    let dir = positional(args, &["--name", "--db"]);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("error: cannot create {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    let dir = dir.canonicalize().unwrap_or(dir);
    if dir.join("quadra.toml").exists() {
        eprintln!("error: {} already has a quadra.toml", dir.display());
        return ExitCode::FAILURE;
    }
    if dir.join("main.qd").exists() {
        eprintln!(
            "error: {} already has a main.qd: use an empty folder",
            dir.display()
        );
        return ExitCode::FAILURE;
    }
    let interactive =
        std::io::stdin().is_terminal() && !args.iter().any(|a| a == "--yes" || a == "-y");
    let folder = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if interactive {
        println!(
            "\n  \x1b[1mQuadra\x1b[0m · new project in {}\n",
            dir.display()
        );
    }
    let name = project_name(
        &flag(args, "--name")
            .unwrap_or_else(|| ask(interactive, "Project name", &project_name(&folder))),
    );
    let db_choice = flag(args, "--db")
        .unwrap_or_else(|| ask(interactive, "Database: sqlite or postgres?", "sqlite"));
    let url = if db_choice.starts_with("postgres://") || db_choice.starts_with("postgresql://") {
        Some(db_choice.clone())
    } else if db_choice.to_lowercase().starts_with("p") {
        Some(ask(
            interactive,
            "PostgreSQL URL",
            &format!("postgres://localhost:5432/{}", name.replace('-', "_")),
        ))
    } else {
        None
    };
    let use_git = !args.iter().any(|a| a == "--no-git")
        && Command::new("git").arg("--version").output().is_ok()
        && !git(&dir, &["rev-parse", "--is-inside-work-tree"]);
    let use_git = use_git
        && ask(
            interactive,
            "Create a git repository (the browser code editor commits to it)? [Y/n]",
            "y",
        )
        .to_lowercase()
        .starts_with('y');

    let database = match &url {
        Some(u) => format!("[database]\nurl = \"{u}\"\n"),
        None => format!("[database]\n# SQLite, in .quadra/ (nothing to install). For PostgreSQL:\n# url = \"postgres://localhost:5432/{}\"\nurl = \"sqlite:.quadra/data.db\"\n", name.replace('-', "_")),
    };
    let files: Vec<(&str, String)> = vec![
        ("quadra.toml", format!("[project]\nname = \"{name}\"\n\n{database}")),
        ("main.qd", STARTER_MAIN.to_string()),
        ("tests/main.test.qd", STARTER_TEST.to_string()),
        (".gitignore", ".quadra/\n".to_string()),
        (
            "README.md",
            format!(
                "# {name}\n\nA [Quadra](https://quadra.dev) project.\n\n```bash\nquadra dev      # the app and the code editor on http://127.0.0.1:8080, applying every change you save\nquadra test     # the tests\nquadra check    # names, types, rules\n```\n"
            ),
        ),
    ];
    for (path, text) in &files {
        let p = dir.join(path);
        if p.exists() && *path != ".gitignore" {
            eprintln!("error: {} already exists", p.display());
            return ExitCode::FAILURE;
        }
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&p, text) {
            eprintln!("error: cannot write {}: {e}", p.display());
            return ExitCode::FAILURE;
        }
    }
    let mut committed = false;
    if use_git && git(&dir, &["init", "-q"]) {
        committed =
            git(&dir, &["add", "-A"]) && git(&dir, &["commit", "-q", "-m", "New Quadra project"]);
    }
    println!(
        "\n  \x1b[32m✓\x1b[0m {name}: quadra.toml, main.qd, tests/main.test.qd{}",
        if use_git {
            if committed {
                ", git repository"
            } else {
                ", git repository (nothing committed: set user.name/user.email)"
            }
        } else {
            ""
        }
    );
    println!(
        "  database: {}",
        url.as_deref().unwrap_or("SQLite in .quadra/data.db")
    );
    let here = std::env::current_dir()
        .ok()
        .and_then(|c| c.canonicalize().ok());
    let cd = if here.as_deref() == Some(dir.as_path()) {
        String::new()
    } else {
        format!("cd {} && ", dir.display())
    };
    println!("\n  Next: \x1b[1m{cd}quadra dev\x1b[0m\n");
    ExitCode::SUCCESS
}

/// The project's files that matter and their versions (to wait for a fix).
fn stamp(root: &Path) -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with('.') || n == "node_modules" || n == "target" {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else if matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("qd" | "csv" | "toml")
            ) {
                out.push((
                    p,
                    e.metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH),
                ));
            }
        }
    }
    out.sort();
    out
}

fn wait_for_change(root: &Path) {
    let before = stamp(root);
    eprintln!("  \x1b[2mwaiting for a change…\x1b[0m");
    while stamp(root) == before {
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
}

pub fn dev(args: &[String]) -> ExitCode {
    let path = positional(args, &["--url", "--port", "--fixture", "--web", "--user"]);
    let (root, _) = match load(&path) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let url = database_url(&root, args);
    // the first free port from 8080 (also free on IPv6: browsers try `localhost` there first)
    let free = |p: u16| {
        std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()
            && std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, p)),
                std::time::Duration::from_millis(150),
            )
            .is_err()
    };
    let port: u16 = match flag(args, "--port").and_then(|p| p.parse().ok()) {
        Some(p) => p,
        None => (8080..8180).find(|p| free(*p)).unwrap_or(8080),
    };
    let login = flag(args, "--user").unwrap_or_else(|| "dev".into());
    if args.iter().any(|a| a == "--reset") {
        match url.strip_prefix("sqlite:") {
            Some(file) if file != ":memory:" => {
                let _ = std::fs::remove_file(file);
                eprintln!("  database reset: {file}");
            }
            _ => {
                eprintln!(
                    "error: --reset only works with SQLite (drop the PostgreSQL database yourself)"
                );
                return ExitCode::FAILURE;
            }
        }
    }
    if let Some(file) = url.strip_prefix("sqlite:") {
        if let Some(dir) = Path::new(file).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
    }
    // until the code compiles and the data fits it
    loop {
        let project = match quadra_sema::Project::load(&root) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };
        let checked = quadra_analysis::check(&project);
        let Some(model) = checked.model.as_ref() else {
            let _ = check(&[root.display().to_string()]);
            wait_for_change(&root);
            continue;
        };
        if checked
            .diagnostics
            .iter()
            .any(|d| d.severity == quadra_sema::Severity::Error)
        {
            let _ = check(&[root.display().to_string()]);
            wait_for_change(&root);
            continue;
        }
        let report = &checked.report;
        use quadra_store::ops;
        let prepared = (|| -> Result<(), String> {
            let mut db = quadra_store::Database::open(&url, model, report)?;
            let status = db.status()?;
            drop(db);
            if !status.initialized {
                let tables = ops::init(&url, model, report, now())?;
                eprintln!("  \x1b[32m✓\x1b[0m new database {url}: {tables} tables");
                let fixture = flag(args, "--fixture").or_else(|| {
                    model
                        .fixtures
                        .iter()
                        .find(|f| f.name == "demo")
                        .map(|f| f.name.clone())
                });
                if let Some(f) = fixture {
                    ops::fixture(&url, model, report, &f, now())?;
                    eprintln!("  \x1b[32m✓\x1b[0m demo data: fixture `{f}`");
                }
            } else {
                // what changed while the server was off: migrations, new defaults, checks
                ops::deploy(
                    &url,
                    model,
                    report,
                    &root,
                    now(),
                    false,
                    "quadra dev: start",
                )?;
            }
            if ops::ensure_user(&url, model, report, &login, now())? {
                eprintln!("  \x1b[32m✓\x1b[0m user `{login}` to sign in with");
            }
            Ok(())
        })();
        match prepared {
            Ok(()) => break,
            Err(e) => {
                let e = e
                    .lines()
                    .map(|l| format!("  {l}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                eprintln!("\n  \x1b[31m✗\x1b[0m the data does not fit the code:\n{e}");
                if e.contains("Either:") {
                    eprintln!("  - or start from empty data with `quadra dev --reset`");
                } else {
                    eprintln!("\n  fix the code (a default, a migration) or start from empty data with `quadra dev --reset`");
                }
                wait_for_change(&root);
            }
        }
    }
    let name = std::fs::read_to_string(root.join("quadra.toml"))
        .ok()
        .and_then(|t| {
            t.lines().find_map(|l| {
                l.trim().strip_prefix("name").map(|v| {
                    v.trim_start_matches([' ', '='])
                        .trim()
                        .trim_matches('"')
                        .to_string()
                })
            })
        })
        .unwrap_or_else(|| "project".into());
    eprintln!();
    eprintln!("  \x1b[1mQuadra dev\x1b[0m · {name}");
    eprintln!("  ➜ App          \x1b[36mhttp://127.0.0.1:{port}\x1b[0m");
    eprintln!("  ➜ Code editor  \x1b[36mhttp://127.0.0.1:{port}/#/~code\x1b[0m");
    eprintln!(
        "  \x1b[2mwatching {}: save a file (here or in the browser) and the app updates\x1b[0m\n",
        root.display()
    );
    let cfg = quadra_server::Config {
        url,
        port,
        dev_login: true,
        secure_cookies: false,
        cache_mb: 64,
        prewarm_days: None,
        web: flag(args, "--web").map(PathBuf::from),
        sweep_every: 0,
        editors: vec!["*".into()],
        watch: true,
        limits: quadra_server::Limits::default(),
        engines: quadra_server::Engines::one(),
        root,
    };
    crate::finish(quadra_server::serve(cfg))
}
