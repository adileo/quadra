//! `quadra cloud link | deploy | deploys | rollback | projects | token`: a project on Quadra
//! Cloud.
//!
//! A project folder is linked to a cloud project in `.quadra/cloud.json` (its name only, no
//! secret). `deploy` packs the sources the way git sees them (`.gitignore` is honoured, even
//! outside a git repository; `.git/` and `.quadra/` never go), uploads them, and follows the
//! pipeline's log until the deploy is online or failed. In CI, `QUADRA_TOKEN` holds a project's
//! token (`quadra cloud token create`).

use crate::cloud::{call, Failure, Refused};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

type Result<T> = std::result::Result<T, Failure>;

/// The largest package the cloud takes.
const MAX_PACKAGE: usize = 50 << 20;

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

/// The positional argument: the project's folder (default: here).
fn folder(args: &[String], with_value: &[&str]) -> PathBuf {
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if with_value.contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with('-') {
            return PathBuf::from(a);
        }
    }
    PathBuf::from(".")
}

fn root(path: &Path) -> Result<PathBuf> {
    quadra_sema::find_project_root(path)
        .ok_or_else(|| Failure::Message(format!("no quadra.toml in {} or above it: run this in a Quadra project", path.display())))
}

/// `[project] name` of quadra.toml, as a valid project name.
fn local_name(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("quadra.toml")).ok()?;
    let mut section = "";
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            section = l.trim_matches(['[', ']']);
        } else if section == "project" {
            if let Some((k, v)) = l.split_once('=') {
                if k.trim() == "name" {
                    let n: String = v
                        .trim()
                        .trim_matches('"')
                        .to_lowercase()
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                        .collect();
                    return Some(n.trim_matches('-').to_string());
                }
            }
        }
    }
    None
}

fn link_file(root: &Path) -> PathBuf {
    root.join(".quadra").join("cloud.json")
}

fn linked(root: &Path) -> Option<String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(link_file(root)).ok()?).ok()?;
    v["project"].as_str().map(str::to_owned)
}

/// The token: `QUADRA_TOKEN` (CI), else the one `quadra cloud login` saved.
fn token() -> Result<String> {
    if let Ok(t) = std::env::var("QUADRA_TOKEN") {
        if !t.trim().is_empty() {
            return Ok(t.trim().to_string());
        }
    }
    crate::cloud::saved_token().ok_or_else(|| Failure::Message("not signed in: run `quadra cloud login` (in CI, set QUADRA_TOKEN to a project's token)".into()))
}

fn refusal(r: Refused) -> Failure {
    Failure::Message(match r.code.as_str() {
        "bad_token" => "the token was revoked: run `quadra cloud login`".into(),
        _ => r.message,
    })
}

/// `link [path] [--name n] [--team slug]`: the cloud project of this folder, made if it does
/// not exist yet.
pub fn link(args: &[String]) -> Result<()> {
    let root = root(&folder(args, &["--name", "--team"]))?;
    let t = token()?;
    let name = flag(args, "--name").or_else(|| local_name(&root)).ok_or("no project name: pass --name")?;
    let p = link_to(&root, &t, &name, flag(args, "--team"))?;
    eprintln!("Linked {} to {} ({}).", root.display(), name, p["url"].as_str().unwrap_or(""));
    Ok(())
}

fn link_to(root: &Path, token: &str, name: &str, team: Option<String>) -> Result<Value> {
    let p = match call("GET", &format!("/v1/projects/{name}"), Some(token), None)? {
        Ok(p) => p,
        Err(r) if r.code == "not_found" => {
            let mut body = json!({ "name": name });
            if let Some(t) = team {
                body["team"] = json!(t);
            }
            let p = call("POST", "/v1/projects", Some(token), Some(&body))?.map_err(refusal)?;
            eprintln!("Created the project {name} in the team {}.", p["team"]["name"].as_str().unwrap_or("?"));
            p
        }
        Err(r) => return Err(refusal(r)),
    };
    let file = link_file(root);
    std::fs::create_dir_all(file.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&file, serde_json::to_string_pretty(&json!({ "project": name })).unwrap() + "\n")
        .map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(p)
}

/// The files of a project as git sees them: `.gitignore` (and `.git/info/exclude`, the global
/// excludes) honoured, hidden files included, `.git/` and `.quadra/` never.
pub fn files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let walk = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .require_git(false)
        .parents(false)
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | ".quadra")))
        .build();
    for e in walk {
        let e = e.map_err(|e| e.to_string())?;
        if e.file_type().is_some_and(|t| t.is_file()) {
            out.push(e.path().strip_prefix(root).unwrap().to_path_buf());
        }
    }
    out.sort();
    Ok(out)
}

/// The project's files as a .tar.gz.
pub fn pack(root: &Path, files: &[PathBuf]) -> Result<Vec<u8>> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    tar.mode(tar::HeaderMode::Deterministic);
    for f in files {
        let name = f.to_string_lossy().replace('\\', "/");
        tar.append_path_with_name(root.join(f), name).map_err(|e| format!("{}: {e}", f.display()))?;
    }
    let gz = tar.into_inner().map_err(|e| e.to_string())?;
    Ok(gz.finish().map_err(|e| e.to_string())?)
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `deploy [path] [--prod] [-m message]`: the sources online (production: previews come with
/// the GitHub App).
pub fn deploy(args: &[String]) -> Result<()> {
    let root = root(&folder(args, &["-m", "--message"]))?;
    let t = token()?;
    let name = match linked(&root) {
        Some(n) => n,
        None => {
            let n = local_name(&root).ok_or("the project is not linked and quadra.toml has no name: run `quadra cloud link --name …`")?;
            link_to(&root, &t, &n, None)?;
            eprintln!("Linked to the project {n}.");
            n
        }
    };
    let list = files(&root)?;
    let package = pack(&root, &list)?;
    if package.len() > MAX_PACKAGE {
        return Err(format!("the sources are {} MB packed, more than the {} MB the cloud takes: check .gitignore", package.len() >> 20, MAX_PACKAGE >> 20).into());
    }
    eprintln!("Packed {} files ({} KB).", list.len(), package.len().div_ceil(1024));
    let mut query = Vec::new();
    if let Some(sha) = git(&root, &["rev-parse", "HEAD"]) {
        query.push(format!("git_sha={sha}"));
    }
    let message = flag(args, "-m").or_else(|| flag(args, "--message")).or_else(|| git(&root, &["log", "-1", "--format=%s"]));
    if let Some(m) = &message {
        query.push(format!("message={}", encode(m)));
    }
    let path = format!("/v1/projects/{name}/deploys{}{}", if query.is_empty() { "" } else { "?" }, query.join("&"));
    let d = crate::cloud::upload(&path, &t, package)?.map_err(refusal)?;
    let id = d["id"].as_str().ok_or("no deploy id in the answer")?.to_string();
    eprintln!("Deploy #{} of {}:", d["number"], name);
    follow(&t, &id, d["url"].as_str().unwrap_or(""))
}

/// Prints the pipeline's log as it comes, until the deploy is over.
fn follow(token: &str, id: &str, url: &str) -> Result<()> {
    let mut after = 0i64;
    let started = std::time::Instant::now();
    loop {
        let v = match call("GET", &format!("/v1/deploys/{id}/logs?after={after}"), Some(token), None)? {
            Ok(v) => v,
            Err(r) => return Err(refusal(r)),
        };
        for l in v["lines"].as_array().into_iter().flatten() {
            after = l["seq"].as_i64().unwrap_or(after);
            eprintln!("  {}", l["line"].as_str().unwrap_or(""));
        }
        let _ = std::io::stderr().flush();
        if v["done"].as_bool() == Some(true) {
            let status = v["status"].as_str().unwrap_or("");
            return if status == "active" {
                eprintln!("\n✓ Online in {:.0} s: {url}", started.elapsed().as_secs_f64());
                Ok(())
            } else {
                Err(format!("the deploy {status}: {}", v["error"].as_str().unwrap_or("see the log above")).into())
            };
        }
        if started.elapsed() > Duration::from_secs(30 * 60) {
            return Err("the deploy is taking more than 30 minutes: see the console".into());
        }
        std::thread::sleep(Duration::from_millis(700));
    }
}

fn project_arg(args: &[String], with_value: &[&str]) -> Result<String> {
    if let Some(n) = flag(args, "--project") {
        return Ok(n);
    }
    let root = root(&folder(args, with_value))?;
    linked(&root).or_else(|| local_name(&root)).ok_or_else(|| "not linked: run `quadra cloud link`".into())
}

/// `deploys [path]`: the deploys of the project, newest first.
pub fn deploys(args: &[String]) -> Result<()> {
    let t = token()?;
    let name = project_arg(args, &["--project"])?;
    let v = call("GET", &format!("/v1/projects/{name}/deploys"), Some(&t), None)?.map_err(refusal)?;
    for d in v["deploys"].as_array().into_iter().flatten().take(30) {
        println!(
            "#{:<4} {:<10} {}  {}{}",
            d["number"],
            d["status"].as_str().unwrap_or(""),
            d["created_at"].as_str().unwrap_or("").get(..16).unwrap_or("").replace('T', " "),
            d["message"].as_str().unwrap_or(""),
            if d["active"].as_bool() == Some(true) { "  ← online" } else { "" },
        );
    }
    Ok(())
}

/// `rollback [number] [path]`: the deploy before the current one (or `number`) online again.
pub fn rollback(args: &[String]) -> Result<()> {
    let t = token()?;
    let number = args.iter().find_map(|a| a.trim_start_matches('#').parse::<i64>().ok());
    let rest: Vec<String> = args.iter().filter(|a| a.trim_start_matches('#').parse::<i64>().is_err()).cloned().collect();
    let name = project_arg(&rest, &["--project"])?;
    let body = number.map(|n| json!({ "number": n })).unwrap_or(json!({}));
    let d = call("POST", &format!("/v1/projects/{name}/rollback"), Some(&t), Some(&body))?.map_err(refusal)?;
    eprintln!("Deploy #{} of {} ({}):", d["number"], name, "rollback");
    follow(&t, d["id"].as_str().unwrap_or(""), d["url"].as_str().unwrap_or(""))
}

/// `projects`: the projects of your teams.
pub fn projects(_: &[String]) -> Result<()> {
    let t = token()?;
    let v = call("GET", "/v1/projects", Some(&t), None)?.map_err(refusal)?;
    let list = v["projects"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        eprintln!("No projects yet: `quadra cloud deploy` in a project folder makes one.");
    }
    for p in list {
        println!(
            "{:<24} {:<16} {}{}",
            p["name"].as_str().unwrap_or(""),
            p["team"]["slug"].as_str().unwrap_or(""),
            p["url"].as_str().unwrap_or(""),
            p["active"]["number"].as_i64().map(|n| format!("  (deploy #{n})")).unwrap_or_default(),
        );
    }
    Ok(())
}

/// `token create <name> | list | revoke <id>`: tokens of the project for CI.
pub fn token_cmd(args: &[String]) -> Result<()> {
    let t = token()?;
    match args.first().map(String::as_str) {
        Some("create") => {
            let label = args.get(1).filter(|a| !a.starts_with('-')).ok_or("usage: quadra cloud token create <name> [path]")?;
            let name = project_arg(&args[2..], &["--project"])?;
            let v = call("POST", &format!("/v1/projects/{name}/tokens"), Some(&t), Some(&json!({ "name": label })))?.map_err(refusal)?;
            eprintln!("A token for CI that can deploy {name} only (shown once): set it as QUADRA_TOKEN.");
            println!("{}", v["token"].as_str().unwrap_or(""));
            Ok(())
        }
        Some("list") => {
            let name = project_arg(&args[1..], &["--project"])?;
            let v = call("GET", &format!("/v1/projects/{name}/tokens"), Some(&t), None)?.map_err(refusal)?;
            for k in v["tokens"].as_array().into_iter().flatten() {
                println!("{}  {}…  {}", k["id"].as_str().unwrap_or(""), k["prefix"].as_str().unwrap_or(""), k["name"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        Some("revoke") => {
            let id = args.get(1).ok_or("usage: quadra cloud token revoke <id> [path]")?;
            let name = project_arg(&args[2..], &["--project"])?;
            call("DELETE", &format!("/v1/projects/{name}/tokens/{id}"), Some(&t), None)?.map_err(refusal)?;
            eprintln!("Revoked.");
            Ok(())
        }
        _ => Err("usage: quadra cloud token create <name> | list | revoke <id> [path]".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_package_honours_gitignore_outside_git() {
        let dir = std::env::temp_dir().join(format!("quadra-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join(".quadra")).unwrap();
        std::fs::write(dir.join("quadra.toml"), "[project]\nname = \"My Shop\"\n").unwrap();
        std::fs::write(dir.join("main.qd"), "entity A:\n    n: Int\n").unwrap();
        std::fs::write(dir.join("sub/b.qd"), "").unwrap();
        std::fs::write(dir.join(".gitignore"), ".env\n*.log\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(dir.join("x.log"), "").unwrap();
        std::fs::write(dir.join(".quadra/data.db"), "").unwrap();
        let f: Vec<String> = files(&dir).unwrap().iter().map(|p| p.to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(f, [".gitignore", "main.qd", "quadra.toml", "sub/b.qd"]);
        let gz = pack(&dir, &files(&dir).unwrap()).unwrap();
        let mut names: Vec<String> = tar::Archive::new(flate2::read::GzDecoder::new(&gz[..]))
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, f);
        assert_eq!(local_name(&dir).as_deref(), Some("my-shop"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
