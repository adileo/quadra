//! `quadra agent`: the on-premise agent, and its registration.
//!
//!     quadra agent run [--server URL --code CODE] [--config FILE] [--concurrency N]
//!     quadra agent add <name> [path] [--url URL]      a one-time code (self-hosting)
//!     quadra agent list [path] [--url URL]
//!     quadra agent revoke <name> [path] [--url URL]
//!     quadra agent upgrade
//!
//! The first `run` registers with the code (made in the studio, Integrations → Agents, or with
//! `quadra agent add`) and keeps the server and the agent's key in its config file
//! (`quadra-agent.toml` in the folder it runs in, readable by its user only); the next ones read
//! it. The values of the variables of the connections it runs come from its environment and the
//! `.env` next to the config: the server never has them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const FLAGS: &[&str] = &["--server", "--code", "--config", "--concurrency", "--url"];

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

pub fn main(args: &[String]) -> ExitCode {
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            eprintln!("usage: quadra agent run | add <name> | list | revoke <name> | upgrade");
            return ExitCode::from(2);
        }
    };
    match cmd {
        "run" => finish(run(rest)),
        "add" | "list" | "revoke" => admin(cmd, rest),
        "upgrade" => crate::upgrade::main(rest),
        _ => {
            eprintln!("unknown agent command `{cmd}` (run, add, list, revoke, upgrade)");
            ExitCode::from(2)
        }
    }
}

fn finish(r: Result<(), String>) -> ExitCode {
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// What the agent keeps between runs.
struct Saved {
    server: String,
    name: String,
    key: String,
}

fn read_config(p: &Path) -> Result<Saved, String> {
    let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let t: toml_table::Table = text.parse().map_err(|e| format!("{}: {e}", p.display()))?;
    let get = |k: &str| {
        t.get(k)
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or(format!("{}: `{k}` is missing", p.display()))
    };
    Ok(Saved {
        server: get("server")?,
        name: get("name")?,
        key: get("key")?,
    })
}

fn write_config(p: &Path, s: &Saved) -> Result<(), String> {
    let text = format!(
        "# quadra agent: the server and this agent's key. Keep it private: it acts as the agent.\nserver = {:?}\nname = {:?}\nkey = {:?}\n",
        s.server, s.name, s.key
    );
    std::fs::write(p, text).map_err(|e| format!("{}: {e}", p.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn run(args: &[String]) -> Result<(), String> {
    let config = flag(args, "--config")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("quadra-agent.toml"));
    let saved = match (flag(args, "--server"), flag(args, "--code")) {
        (Some(server), Some(code)) => {
            let (name, key) = quadra_server::agent::register(&server, &code)?;
            let s = Saved { server, name, key };
            write_config(&config, &s)?;
            println!(
                "✓ registered as `{}`: the key is in {} (keep it private)",
                s.name,
                config.display()
            );
            s
        }
        (None, None) => read_config(&config).map_err(|e| {
            format!("{e}\nthe first time: `quadra agent run --server https://<app> --code <the code made in the studio>`")
        })?,
        _ => return Err("--server and --code go together: the first time, to register".into()),
    };
    // the values of the variables: its environment, then the `.env` next to the config
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    let dotenv = config
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join(".env");
    if let Ok(t) = std::fs::read_to_string(&dotenv) {
        for (k, v) in quadra_sema::manifest::parse_dotenv(&t) {
            env.entry(k).or_insert(v);
        }
    }
    let concurrency = flag(args, "--concurrency")
        .and_then(|c| c.parse().ok())
        .unwrap_or(4);
    println!(
        "quadra agent {} `{}` for {}: waiting for jobs (Ctrl-C to stop)",
        quadra_server::agent::VERSION,
        saved.name,
        saved.server
    );
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    quadra_server::agent::run(
        quadra_server::agent::Options {
            server: saved.server,
            key: saved.key,
            env,
            concurrency,
        },
        |line| {
            println!(
                "{}  {line}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
            )
        },
        stop,
    )
}

/// `add`, `list`, `revoke`: on a project's database (self-hosting), as `quadra inbox` does. In
/// the cloud, the studio does it (Integrations → Agents).
fn admin(cmd: &str, rest: &[String]) -> ExitCode {
    let mut rest = rest.to_vec();
    let mut name = None;
    if cmd != "list" {
        let (n, others) = crate::take_positional(&rest, FLAGS);
        name = n;
        rest = others;
        if name.is_none() {
            eprintln!("usage: quadra agent {cmd} <name> [path] [--url URL]");
            return ExitCode::from(2);
        }
    }
    let (_, _checked, url) = match crate::open_project(&rest) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let now = chrono::Utc::now().naive_utc();
    let stamp = |t: chrono::NaiveDateTime| t.format("%Y-%m-%dT%H:%M:%S").to_string();
    finish((|| {
        let mut c = quadra_store::Conn::open(&url)?;
        match cmd {
            "add" => {
                let name = name.unwrap();
                let code = quadra_store::agents::new_code();
                quadra_store::agents::add(
                    &mut c,
                    &name,
                    &code,
                    &stamp(now + chrono::Duration::hours(1)),
                    &stamp(now),
                    &crate::who(),
                )?;
                println!("✓ agent `{name}`: its one-time code is {code} (valid an hour)");
                println!("  in the network it reaches: quadra agent run --server <the app's address> --code {code}");
                println!("  and on its connections: location = \"agent:{name}\"");
            }
            "revoke" => {
                let name = name.unwrap();
                if !quadra_store::agents::revoke(&mut c, &name, &stamp(now))? {
                    return Err(format!("no agent `{name}` to revoke"));
                }
                println!("✓ `{name}` revoked: it receives nothing more within 5 s");
            }
            _ => {
                let all = quadra_store::agents::list(&mut c)?;
                if all.is_empty() {
                    println!("no agents (`quadra agent add <name>`)");
                }
                for a in all {
                    let state = match (&a.revoked_at, &a.registered_at, &a.code_expires) {
                        (Some(r), _, _) => format!("revoked {r}"),
                        (None, Some(_), _) => format!(
                            "registered, {} seen {}",
                            a.version.as_deref().unwrap_or("?"),
                            a.seen_at.as_deref().unwrap_or("never")
                        ),
                        (None, None, Some(e)) => format!("waiting for its code until {e}"),
                        _ => "not registered".into(),
                    };
                    println!("{:<16} {state}", a.name);
                }
            }
        }
        Ok(())
    })())
}

mod toml_table {
    pub use toml::Table;
}
