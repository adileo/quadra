//! `quadra apps pull|push|new`: apps and saved views as interface files of the project
//! (`interfaces/**/*.qdi`, the interface files reference (quadra.run/reference/interfaces)).
//!
//! - `pull [<name|id>|--all] [--to interfaces/]`: apps and views made in the client become
//!   files (an app in the directories of its folders, the views of an entity together in
//!   `views/<entity>.qdi`); the data copies are archived (deleted, their history kept): the
//!   files' apps take their place when the server starts. Shares with users and teams are
//!   data: they are not written (a warning says which).
//! - `push`: the project's interface files installed now, as `quadra serve` and `quadra dev`
//!   do when they start (refused when they do not check).
//! - `new "Name" [--entity E]`: a starting file to change.

use quadra_store::conn::Conn;
use quadra_store::views::{self as store, ViewChange, ViewRow};
use serde_json::{json, Value as J};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const FLAGS: &[&str] = &["--url", "--to", "--entity"];

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn positionals(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if FLAGS.contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            out.push(a.clone());
        }
    }
    out
}

/// A file name: lower-case words joined by `-`.
pub fn file_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "app".into()
    } else {
        out
    }
}

/// A path not taken: `x.qdi`, else `x-2.qdi`, `x-3.qdi`…
fn free(dir: &Path, base: &str, taken: &[PathBuf]) -> PathBuf {
    let mut n = 1;
    loop {
        let name = if n == 1 { format!("{base}.qdi") } else { format!("{base}-{n}.qdi") };
        let p = dir.join(name);
        if !p.exists() && !taken.contains(&p) {
            return p;
        }
        n += 1;
    }
}

fn definition(v: &ViewRow, shares: &[store::Share], warnings: &mut Vec<String>) -> J {
    let mut sh = Vec::new();
    for s in shares {
        match s.target.split_once(':') {
            Some(("role", r)) => sh.push(json!({ "type": "role", "name": r, "edit": s.edit })),
            None if s.target == "everyone" => sh.push(json!({ "type": "everyone", "edit": s.edit })),
            _ => warnings.push(format!("`{}`: its share with {} is not written in the file (share it again in the app)", v.name, s.target)),
        }
    }
    json!({ "kind": v.kind, "entity": v.entity, "type": v.vtype, "name": v.name, "description": v.description, "config": v.config, "shares": sh })
}

pub fn apps(args: &[String]) -> ExitCode {
    let Some(cmd) = args.first().map(String::as_str) else {
        eprintln!("usage: quadra apps pull [<name|id>|--all] [--to interfaces/] [path] [--url URL] | push [path] [--url URL] | new \"Name\" [--entity E] [--to interfaces/] [path]");
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    let r = match cmd {
        "pull" => pull(rest),
        "push" => push(rest),
        "new" => new(rest),
        other => Err(format!("unknown apps command `{other}` (pull, push, new)")),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The project's root, from the positional argument (`.` when none).
fn root_of(path: Option<&String>) -> Result<PathBuf, String> {
    let p = PathBuf::from(path.map(String::as_str).unwrap_or("."));
    quadra_sema::find_project_root(&p).ok_or_else(|| format!("no quadra.toml found in {} or its parents", p.display()))
}

fn pull(args: &[String]) -> Result<(), String> {
    let all = args.iter().any(|a| a == "--all");
    let pos = positionals(args);
    // `pull <name> [path]` or `pull --all [path]`
    let (which, path) = if all { (None, pos.first()) } else { (pos.first().cloned(), pos.get(1)) };
    if which.is_none() && !all {
        return Err("which app or view? `quadra apps pull <name|id>` (or `--all`)".into());
    }
    let root = root_of(path)?;
    let url = super::database_url(&root, args);
    let to = root.join(flag(args, "--to").unwrap_or_else(|| quadra_sema::INTERFACES_DIR.to_string()));
    let mut conn = Conn::open(&url)?;
    let rows = store::views(&mut conn, None, None)?;
    let folders: std::collections::HashMap<u64, ViewRow> = rows.iter().filter(|v| v.kind == "folder").map(|v| (v.id, v.clone())).collect();
    let chosen: Vec<ViewRow> = rows
        .iter()
        .filter(|v| v.origin.as_deref() != Some("file") && (v.kind == "app" || v.kind == "view"))
        .filter(|v| all || which.as_deref().is_some_and(|w| w == v.name || w.parse::<u64>().ok() == Some(v.id)))
        .cloned()
        .collect();
    if chosen.is_empty() {
        return Err(match which {
            Some(w) => format!("no app or view `{w}` made in the client"),
            None => "no app or view made in the client".into(),
        });
    }
    let ids: Vec<u64> = chosen.iter().map(|v| v.id).collect();
    let shares = store::shares(&mut conn, Some(&ids))?;
    let mut warnings = Vec::new();
    let mut taken: Vec<PathBuf> = Vec::new();
    let mut written: Vec<(PathBuf, Vec<u64>)> = Vec::new();
    // apps: one file each, in the directories of their folders
    for v in chosen.iter().filter(|v| v.kind == "app") {
        let mut dirs = Vec::new();
        let mut cur = v.parent;
        while let Some(p) = cur.and_then(|p| folders.get(&p)) {
            dirs.push(p.name.clone());
            cur = p.parent;
        }
        dirs.reverse();
        let dir = dirs.iter().fold(to.clone(), |d, x| d.join(x));
        let path = free(&dir, &file_name(&v.name), &taken);
        let def = definition(v, shares.get(&v.id).map(Vec::as_slice).unwrap_or(&[]), &mut warnings);
        let (text, w) = quadra_qdi::from_json(&[def])?;
        warnings.extend(w.into_iter().filter(|w| !w.contains("owner")).map(|w| format!("{}: {w}", v.name)));
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        taken.push(path.clone());
        written.push((path, vec![v.id]));
    }
    // views: those of an entity together, after the ones already in the file
    let mut by_entity: std::collections::BTreeMap<String, Vec<&ViewRow>> = Default::default();
    for v in chosen.iter().filter(|v| v.kind == "view") {
        by_entity.entry(v.entity.clone().unwrap_or_default()).or_default().push(v);
    }
    for (entity, views) in by_entity {
        let dir = to.join("views");
        let path = dir.join(format!("{}.qdi", file_name(&entity)));
        let mut defs: Vec<J> = match std::fs::read_to_string(&path) {
            Ok(t) => quadra_qdi::to_json(&t).map_err(|e| format!("{}: {}", path.display(), e.first().map(|d| d.message.clone()).unwrap_or_default()))?,
            Err(_) => Vec::new(),
        };
        for v in &views {
            let mut def = definition(v, shares.get(&v.id).map(Vec::as_slice).unwrap_or(&[]), &mut warnings);
            let mut name = v.name.clone();
            let mut n = 2;
            while defs.iter().any(|d| d["name"] == json!(name)) {
                name = format!("{} ({n})", v.name);
                n += 1;
            }
            def["name"] = json!(name);
            defs.push(def);
        }
        let (text, _) = quadra_qdi::from_json(&defs)?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        written.push((path, views.iter().map(|v| v.id).collect()));
    }
    // the data copies are archived: the files' take their place
    let at = chrono::Local::now().naive_local().format("%Y-%m-%dT%H:%M:%S").to_string();
    for (path, ids) in &written {
        let rel = path.strip_prefix(&root).unwrap_or(path).display().to_string();
        for id in ids {
            let del = ViewChange { delete: true, ..Default::default() };
            store::update_view(&mut conn, *id, None, &del, &at, None, &format!("pulled into {rel}"))?.ok();
        }
        println!("✓ {rel}");
    }
    for w in warnings {
        eprintln!("warning: {w}");
    }
    println!("{} file(s) written: they are installed when the server starts (or `quadra apps push`)", written.len());
    Ok(())
}

fn push(args: &[String]) -> Result<(), String> {
    let pos = positionals(args);
    let root = root_of(pos.iter().find(|p| !p.ends_with(".qdi")))?;
    let project = quadra_sema::Project::load(&root).map_err(|e| e.to_string())?;
    let checked = quadra_analysis::check(&project);
    let errors: Vec<String> = checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == quadra_sema::Severity::Error)
        .map(|d| {
            let f = project.file(d.file);
            let line = f.text[..usize::from(d.range.start()).min(f.text.len())].matches('\n').count() + 1;
            format!("{}:{line}: {}", f.path.strip_prefix(&root).unwrap_or(&f.path).display(), d.message)
        })
        .collect();
    if !errors.is_empty() {
        return Err(format!("the project does not check (`quadra check`):\n  {}", errors.join("\n  ")));
    }
    let base = root.join(quadra_sema::INTERFACES_DIR);
    let mut items = Vec::new();
    for f in project.interface_files() {
        let rel = f.path.strip_prefix(&base).unwrap_or(&f.path).with_extension("").to_string_lossy().replace('\\', "/");
        let dirs: Vec<String> = rel.split('/').map(str::to_string).collect::<Vec<_>>().split_last().map(|(_, d)| d.to_vec()).unwrap_or_default();
        for d in quadra_qdi::to_json(&f.text).map_err(|e| format!("{rel}.qdi: {}", e.first().map(|d| d.message.clone()).unwrap_or_default()))? {
            let app = d["kind"] == json!("app");
            let path = if app { rel.clone() } else { format!("{rel}#{}", d["name"].as_str().unwrap_or("")) };
            items.push(store::FileItem { path, dirs: if app { dirs.clone() } else { Vec::new() }, def: d });
        }
    }
    let url = super::database_url(&root, args);
    let mut conn = Conn::open(&url)?;
    let at = chrono::Local::now().naive_local().format("%Y-%m-%dT%H:%M:%S").to_string();
    let done = store::install_files(&mut conn, &items, &at, None)?;
    println!("✓ {} interface file(s): {} created, {} changed, {} removed", project.interface_files().count(), done.created, done.changed, done.removed);
    Ok(())
}

fn new(args: &[String]) -> Result<(), String> {
    let pos = positionals(args);
    let name = pos.first().ok_or("the app's name: `quadra apps new \"Warehouse board\"`")?.clone();
    let root = root_of(pos.get(1))?;
    let to = root.join(flag(args, "--to").unwrap_or_else(|| quadra_sema::INTERFACES_DIR.to_string()));
    let path = to.join(format!("{}.qdi", file_name(&name)));
    if path.exists() {
        return Err(format!("{} exists already", path.display()));
    }
    let mut text = format!("app {}:\n    description: \"\"\n    share: everyone use\n\n    page main \"Main\":\n        row height 2:\n            markdown title width 24:\n                {}\n", quadra_qdi::text::quote(&name), quadra_qdi::text::quote(&format!("## {name}")));
    if let Some(entity) = flag(args, "--entity") {
        let project = quadra_sema::Project::load(&root).map_err(|e| e.to_string())?;
        let checked = quadra_analysis::check(&project);
        let model = checked.model.as_ref().ok_or("the project has errors: run `quadra check`")?;
        let e = model.entity(&entity).ok_or(format!("no entity `{entity}`"))?;
        let fields: Vec<&str> = e.fields.iter().filter(|f| f.formula.is_none() && f.ty.as_ref().is_some_and(|t| !matches!(t, quadra_ir::Ty::List(_) | quadra_ir::Ty::Set(_)))).map(|f| f.name.as_str()).take(6).collect();
        let var = file_name(&entity).replace('-', "_");
        text.push_str(&format!(
            "\n        row height 3:\n            kpi count {} width 6:\n                source: {entity}.count()\n\n        row height 10:\n            view {var}s {} width 24:\n                of: {entity}\n                type: grid\n                select: {var}\n                fields: {}\n",
            quadra_qdi::text::quote(&format!("{entity} records")),
            quadra_qdi::text::quote(&entity),
            fields.join(", ")
        ));
    }
    std::fs::create_dir_all(&to).map_err(|e| format!("{}: {e}", to.display()))?;
    // the canonical form, as the client writes it back
    let text = quadra_qdi::format(&text).map_err(|e| format!("{}", e.first().map(|d| d.message.clone()).unwrap_or_default()))?;
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("✓ {} (check it: `quadra check`)", path.strip_prefix(&root).unwrap_or(&path).display());
    Ok(())
}
