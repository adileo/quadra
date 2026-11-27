//! `quadra cloud …`: the client of Quadra Cloud's API (`api.quadra.run`).
//!
//! `login` is the OAuth device flow: it shows a code, the person approves it in the console
//! (from any machine: nothing secret is typed there), and the token it gets is kept in
//! `~/.config/quadra/cloud.json`, readable by the user only.
//!
//! Every request says the CLI's version (`x-quadra-version`). A version the cloud no longer
//! supports gets 426 `cli_too_old`: in a terminal the CLI offers to upgrade and runs the
//! command again with the new binary; elsewhere (CI) it stops with the command to run.

use crate::{http, upgrade};
use serde_json::{json, Value};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: quadra cloud <command>

  login [--no-browser]   sign in this machine (approve the code in the console)
  logout                 revoke this machine's token and forget it
  whoami                 the account signed in
  deploy [path] [-m msg] the project online (it is linked, and made, on the first deploy)
  link [path] [--name n] [--team slug]   this folder to a cloud project
  deploys [path]         the deploys of the project
  rollback [number] [path]   the deploy before (or #number) online again
  projects               the projects of your teams
  token create <name> | list | revoke <id> [path]   tokens for CI (QUADRA_TOKEN)

Environment: QUADRA_CLOUD_API (default https://api.quadra.run), QUADRA_TOKEN (CI)";

const DEFAULT_API: &str = "https://api.quadra.run";

pub fn main(args: &[String]) -> ExitCode {
    let r = match args.first().map(String::as_str) {
        Some("login") => login(args.iter().any(|a| a == "--no-browser")),
        Some("logout") => logout(),
        Some("whoami") => whoami(),
        Some("deploy") => crate::deploy::deploy(&args[1..]),
        Some("link") => crate::deploy::link(&args[1..]),
        Some("deploys") => crate::deploy::deploys(&args[1..]),
        Some("rollback") => crate::deploy::rollback(&args[1..]),
        Some("projects") => crate::deploy::projects(&args[1..]),
        Some("token") => crate::deploy::token_cmd(&args[1..]),
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let code = match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Message(e)) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
        Err(Failure::Exit(c)) => return c,
    };
    upgrade::notice();
    code
}

#[derive(Debug)]
pub enum Failure {
    Message(String),
    /// the command was run again by a newer binary: its exit code
    Exit(ExitCode),
}

impl From<String> for Failure {
    fn from(s: String) -> Failure {
        Failure::Message(s)
    }
}

impl From<&str> for Failure {
    fn from(s: &str) -> Failure {
        Failure::Message(s.into())
    }
}

type Result<T> = std::result::Result<T, Failure>;

pub fn config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("quadra"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".config").join("quadra"))
}

fn config_path() -> std::result::Result<PathBuf, String> {
    if let Ok(p) = std::env::var("QUADRA_CLOUD_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    config_dir()
        .map(|d| d.join("cloud.json"))
        .ok_or_else(|| "no home directory".into())
}

fn saved() -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(config_path().ok()?).ok()?).ok()
}

fn save(v: &Value) -> std::result::Result<PathBuf, String> {
    let p = config_path()?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = p.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(v).expect("JSON");
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(text.as_bytes())
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(p)
}

fn api_base() -> String {
    std::env::var("QUADRA_CLOUD_API")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| saved()?.get("api")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| DEFAULT_API.into())
        .trim_end_matches('/')
        .to_string()
}

/// A refusal of the API: its `code` and message.
pub struct Refused {
    pub code: String,
    pub message: String,
}

/// One call to the API. Ok(JSON) on success; Err(Ok(refusal)) for an answer of the API;
/// Err(Err(failure)) when it could not be reached, or after an upgrade.
pub fn call(
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Result<std::result::Result<Value, Refused>> {
    let (status, v) = http::json(method, &format!("{}{path}", api_base()), token, body)
        .map_err(Failure::Message)?;
    if (200..300).contains(&status) {
        return Ok(Ok(v));
    }
    let code = v["code"].as_str().unwrap_or("error").to_string();
    let message = v["error"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("HTTP {status}"));
    if status == 426 || code == "cli_too_old" {
        return Err(too_old(&message, v["min_version"].as_str()));
    }
    Ok(Err(Refused { code, message }))
}

/// The forced upgrade: in a terminal, offer it and run the same command with the new binary.
fn too_old(message: &str, min: Option<&str>) -> Failure {
    let need = min
        .map(|m| format!(" (version {m} or newer is needed)"))
        .unwrap_or_default();
    eprintln!("{message}{need}");
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    if !interactive {
        return Failure::Message(format!(
            "quadra {} is too old for Quadra Cloud: run `quadra upgrade`",
            upgrade::VERSION
        ));
    }
    eprint!("Upgrade quadra now? [Y/n] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no") {
        return Failure::Message("not upgraded: run `quadra upgrade` when you are ready".into());
    }
    if let Err(e) = upgrade::upgrade_to(None, true) {
        return Failure::Message(format!("the upgrade failed: {e}"));
    }
    // the same command, with the binary just installed
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return Failure::Message(e.to_string()),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    match std::process::Command::new(exe).args(args).status() {
        Ok(s) => Failure::Exit(ExitCode::from(s.code().unwrap_or(1).clamp(0, 255) as u8)),
        Err(e) => Failure::Message(format!(
            "upgraded, but the command could not run again: {e}"
        )),
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            let o = std::process::Command::new("hostname").output().ok()?;
            Some(String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| "this machine".into())
}

fn open_browser(url: &str) -> bool {
    // over SSH the browser is on another machine
    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        return false;
    }
    let r = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .status()
    } else {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return false;
        }
        std::process::Command::new("xdg-open").arg(url).status()
    };
    r.map(|s| s.success()).unwrap_or(false)
}

fn login(no_browser: bool) -> Result<()> {
    let api = api_base();
    let start = call(
        "POST",
        "/v1/device/code",
        None,
        Some(&json!({ "client_name": format!("quadra CLI on {}", hostname()) })),
    )?
    .map_err(|r| r.message)?;
    let device = start["device_code"].as_str().ok_or("no device code")?;
    let user = start["user_code"].as_str().ok_or("no user code")?;
    let uri = start["verification_uri"].as_str().unwrap_or("");
    let full = start["verification_uri_complete"].as_str().unwrap_or(uri);
    let mut interval = start["interval"].as_u64().unwrap_or(5);
    let until = Instant::now() + Duration::from_secs(start["expires_in"].as_u64().unwrap_or(600));

    eprintln!("To sign in, open {uri} in a browser and enter the code:\n\n    {user}\n");
    if !no_browser && open_browser(full) {
        eprintln!("(the browser was opened; the page must show the same code)");
    }
    eprintln!("Waiting for the approval… (Ctrl-C to stop)");
    loop {
        if Instant::now() >= until {
            return Err(
                "the code expired before it was approved: run `quadra cloud login` again".into(),
            );
        }
        std::thread::sleep(Duration::from_secs(interval));
        match call(
            "POST",
            "/v1/device/token",
            None,
            Some(&json!({ "device_code": device })),
        ) {
            Ok(Ok(v)) => {
                let token = v["access_token"].as_str().ok_or("no token in the answer")?;
                let email = v["email"].as_str().unwrap_or("");
                // the token this machine had is revoked: one per machine
                if let Some(old) = saved().and_then(|s| s["token"].as_str().map(str::to_owned)) {
                    let _ = call("DELETE", "/v1/me/token", Some(&old), None);
                }
                let p = save(&json!({ "api": api, "token": token, "email": email }))?;
                eprintln!("Signed in as {email} (token in {}).", p.display());
                return Ok(());
            }
            Ok(Err(r)) if r.code == "authorization_pending" => {}
            Ok(Err(r)) if r.code == "slow_down" => interval += 5,
            Ok(Err(r)) => return Err(r.message.into()),
            // the network went away for a moment: keep waiting
            Err(Failure::Message(_)) => {}
            Err(f) => return Err(f),
        }
    }
}

pub fn saved_token() -> Option<String> {
    saved().and_then(|s| s["token"].as_str().map(str::to_owned))
}

/// The sources of a deploy, uploaded: like [`call`], with a .tar.gz body.
pub fn upload(
    path: &str,
    token: &str,
    body: Vec<u8>,
) -> Result<std::result::Result<Value, Refused>> {
    let (status, v) =
        http::upload(&format!("{}{path}", api_base()), token, body).map_err(Failure::Message)?;
    if (200..300).contains(&status) {
        return Ok(Ok(v));
    }
    let code = v["code"].as_str().unwrap_or("error").to_string();
    let message = v["error"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("HTTP {status}"));
    if status == 426 || code == "cli_too_old" {
        return Err(too_old(&message, v["min_version"].as_str()));
    }
    Ok(Err(Refused { code, message }))
}

fn token() -> std::result::Result<String, String> {
    saved()
        .and_then(|s| s["token"].as_str().map(str::to_owned))
        .ok_or_else(|| "not signed in: run `quadra cloud login`".into())
}

fn whoami() -> Result<()> {
    let t = token()?;
    let v = call("GET", "/v1/me", Some(&t), None)?.map_err(|r| {
        if r.code == "bad_token" {
            "the token was revoked: run `quadra cloud login`".to_string()
        } else {
            r.message
        }
    })?;
    println!("{}", v["email"].as_str().unwrap_or("?"));
    for t in v["teams"].as_array().into_iter().flatten() {
        println!(
            "  {}  {} ({})",
            t["slug"].as_str().unwrap_or(""),
            t["name"].as_str().unwrap_or(""),
            t["role"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

fn logout() -> Result<()> {
    let t = token()?;
    if let Err(r) = call("DELETE", "/v1/me/token", Some(&t), None)? {
        if r.code != "bad_token" {
            return Err(format!("{} (the token is kept: try again)", r.message).into());
        }
    }
    let p = config_path()?;
    std::fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    eprintln!("Signed out: the token is revoked and removed.");
    Ok(())
}
