//! The code of the project, edited from the browser.
//!
//! The project is a git repository. The editor works on the server's working copy: files are
//! saved there, checked with the unsaved buffers on top (the same messages as the editor's
//! language server), tested. A commit is a release: check, tests, a dry run on the real data
//! (pending migrations, new defaults, every constraint of the new model on every record),
//! `git commit` with the user as author, the database brought to the new model, the model
//! reloaded. If applying fails after the commit, the commit is reverted.

use super::*;
use std::path::{Component, Path};
use std::process::Command;

const EDITABLE: &[&str] = &["qd", "csv", "md"];
const READABLE: &[&str] = &["qd", "csv", "md", "toml", "lock", "expected"];
const MAX_FILE: usize = 1 << 20;

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// A path inside the project: relative, no `..`, no hidden folders, a known kind of file.
fn safe_path(root: &Path, rel: &str, kinds: &[&str]) -> Result<PathBuf, (u16, String)> {
    let p = Path::new(rel);
    let bad = rel.is_empty()
        || rel.len() > 300
        || p.is_absolute()
        || p.components().any(|c| !matches!(c, Component::Normal(_)))
        || p.components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        || !p
            .extension()
            .and_then(|e| e.to_str())
            .map_or(false, |e| kinds.contains(&e));
    if bad {
        return Err((
            400,
            format!("`{rel}`: not a project file that can be edited here"),
        ));
    }
    Ok(root.join(p))
}

/// The edit that imports `name` from `module` into a module's text: extends an existing
/// `from module import …` line, else adds a line after the other imports. UTF-16 offsets.
pub(super) fn import_edit(text: &str, module: &str, name: &str) -> Option<(usize, usize, String)> {
    let prefix = format!("from {module} import ");
    let mut offset = 0;
    let mut after_imports: Option<usize> = None;
    let mut first_code: Option<usize> = None;
    for line in text.split_inclusive('\n') {
        let l = line.trim_end_matches(['\n', '\r']);
        if let Some(names) = l.strip_prefix(&prefix) {
            let list = names.split('#').next().unwrap_or("");
            if list.split(',').any(|n| n.trim() == name) {
                return None;
            }
            let end = offset + list.trim_end().len() + prefix.len();
            return Some((utf16(text, end), utf16(text, end), format!(", {name}")));
        }
        if l.starts_with("from ") || l.starts_with("import ") {
            after_imports = Some(offset + line.len());
        } else if first_code.is_none() && !l.trim().is_empty() && !l.trim_start().starts_with('#') {
            first_code = Some(offset);
        }
        offset += line.len();
    }
    let line = format!("from {module} import {name}\n");
    match (after_imports, first_code) {
        (Some(at), _) => Some((utf16(text, at), utf16(text, at), line)),
        (None, Some(at)) => Some((utf16(text, at), utf16(text, at), format!("{line}\n"))),
        (None, None) => Some((utf16(text, text.len()), utf16(text, text.len()), line)),
    }
}

/// A folder inside the project: relative, no `..`, no hidden folders.
fn safe_dir(root: &Path, rel: &str) -> Result<PathBuf, (u16, String)> {
    let p = Path::new(rel.trim_end_matches('/'));
    let bad = rel.is_empty()
        || rel.len() > 300
        || p.is_absolute()
        || p.components().any(|c| !matches!(c, Component::Normal(_)))
        || p.components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'));
    if bad {
        return Err((400, format!("`{rel}`: not a folder of the project")));
    }
    Ok(root.join(p))
}

fn collect_dirs(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" || name == "target" || !e.path().is_dir()
        {
            continue;
        }
        if let Ok(rel) = e.path().strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
        collect_dirs(root, &e.path(), out);
    }
}

pub(super) fn safe_path_pub(root: &Path, rel: &str) -> Result<PathBuf, (u16, String)> {
    safe_path(root, rel, EDITABLE)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        let path = e.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if path
            .extension()
            .and_then(|x| x.to_str())
            .map_or(false, |x| READABLE.contains(&x))
        {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

fn utf16(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].encode_utf16().count()
}

impl Engine {
    pub(super) fn editor_login(&mut self, who: &Principal) -> Result<String, Fail> {
        let checked = self.checked.clone();
        let login = self.login_of(checked.model.as_ref().unwrap(), who.user_id);
        if !self.editors.iter().any(|e| e == &login || e == "*") {
            return Err((
                403,
                "editing the code is not allowed for this user (see `quadra serve --editors`)"
                    .into(),
            ));
        }
        Ok(login)
    }

    fn is_repo(&self) -> bool {
        git(&self.root, &["rev-parse", "--is-inside-work-tree"])
            .map_or(false, |o| o.trim() == "true")
    }

    /// `path → status` of changed files (git porcelain codes: M, A, D, ??).
    fn changes(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let Ok(prefix) = git(&self.root, &["rev-parse", "--show-prefix"]) else {
            return out;
        };
        let prefix = prefix.trim().to_string();
        if let Ok(s) = git(
            &self.root,
            &["status", "--porcelain", "--untracked-files=all", "--", "."],
        ) {
            for line in s.lines() {
                if line.len() < 4 {
                    continue;
                }
                let code = line[..2].trim().to_string();
                let path = line[3..].trim().trim_matches('"');
                let path = path.rsplit(" -> ").next().unwrap_or(path);
                out.insert(path.strip_prefix(&prefix).unwrap_or(path).to_string(), code);
            }
        }
        out
    }

    pub(super) fn dev_files(&mut self, who: &Principal) -> Result<Reply, Fail> {
        self.editor_login(who)?;
        let mut files = Vec::new();
        collect(&self.root, &self.root, &mut files);
        let changes = self.changes();
        let list: Vec<J> = files
            .iter()
            .map(|f| {
                let editable = Path::new(f)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map_or(false, |e| EDITABLE.contains(&e));
                json!({ "path": f, "status": changes.get(f), "editable": editable })
            })
            .collect();
        let deleted: Vec<&String> = changes
            .iter()
            .filter(|(_, c)| c.contains('D'))
            .map(|(p, _)| p)
            .collect();
        let mut dirs = Vec::new();
        collect_dirs(&self.root, &self.root, &mut dirs);
        dirs.sort();
        Ok(reply(
            200,
            json!({ "files": list, "dirs": dirs, "deleted": deleted, "repository": self.is_repo() }),
        ))
    }

    pub(super) fn dev_read(&mut self, who: &Principal, path: &str) -> Result<Reply, Fail> {
        self.editor_login(who)?;
        let p = safe_path(&self.root, path, READABLE)?;
        let text = std::fs::read_to_string(&p).map_err(|e| (404, format!("{path}: {e}")))?;
        let editable = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map_or(false, |e| EDITABLE.contains(&e));
        Ok(reply(
            200,
            json!({ "path": path, "text": text, "editable": editable }),
        ))
    }

    pub(super) fn dev_save(
        &mut self,
        who: &Principal,
        path: &str,
        text: &str,
    ) -> Result<Reply, Fail> {
        self.editor_login(who)?;
        let p = safe_path(&self.root, path, EDITABLE)?;
        if text.len() > MAX_FILE {
            return Err((413, "file too large (1 MB at most)".into()));
        }
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| (500, e.to_string()))?;
        }
        std::fs::write(&p, text).map_err(|e| (500, e.to_string()))?;
        self.dev_generation += 1;
        Ok(reply(
            200,
            json!({ "path": path, "status": self.changes().get(path) }),
        ))
    }

    pub(super) fn dev_delete(&mut self, who: &Principal, path: &str) -> Result<Reply, Fail> {
        self.editor_login(who)?;
        let p = safe_path(&self.root, path, EDITABLE)?;
        std::fs::remove_file(&p).map_err(|e| (404, format!("{path}: {e}")))?;
        self.dev_generation += 1;
        Ok(reply(200, json!({ "path": path })))
    }

    pub(super) fn dev_rename(
        &mut self,
        who: &Principal,
        from: &str,
        to: &str,
    ) -> Result<Reply, Fail> {
        self.editor_login(who)?;
        let src = self.root.join(from.trim_end_matches('/'));
        let dst = if src.is_dir() {
            safe_dir(&self.root, from)?;
            safe_dir(&self.root, to)?
        } else {
            safe_path(&self.root, from, EDITABLE)?;
            safe_path(&self.root, to, EDITABLE)?
        };
        if dst.exists() {
            return Err((409, format!("`{to}` already exists")));
        }
        if let Some(dir) = dst.parent() {
            std::fs::create_dir_all(dir).map_err(|e| (500, e.to_string()))?;
        }
        std::fs::rename(&src, &dst).map_err(|e| (404, format!("{from}: {e}")))?;
        self.dev_generation += 1;
        Ok(reply(200, json!({ "from": from, "to": to })))
    }

    pub(super) fn dev_mkdir(&mut self, who: &Principal, path: &str) -> Result<Reply, Fail> {
        self.editor_login(who)?;
        let p = safe_dir(&self.root, path)?;
        std::fs::create_dir_all(&p).map_err(|e| (500, e.to_string()))?;
        Ok(reply(200, json!({ "path": path })))
    }

    /// A folder and what it contains, if it contains only files that can be edited here.
    pub(super) fn dev_rmdir(&mut self, who: &Principal, path: &str) -> Result<Reply, Fail> {
        self.editor_login(who)?;
        let p = safe_dir(&self.root, path)?;
        if !std::fs::symlink_metadata(&p).map_or(false, |m| m.is_dir()) {
            return Err((404, format!("`{path}`: no such folder")));
        }
        // everything inside, hidden files included: only what could be deleted one by one
        let mut files = Vec::new();
        let mut todo = vec![p.clone()];
        while let Some(dir) = todo.pop() {
            for e in std::fs::read_dir(&dir)
                .map_err(|e| (500, e.to_string()))?
                .flatten()
            {
                let kind = e.file_type().map_err(|e| (500, e.to_string()))?;
                let rel = e
                    .path()
                    .strip_prefix(&self.root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                let name = e.file_name().to_string_lossy().to_string();
                let editable = Path::new(&name)
                    .extension()
                    .and_then(|x| x.to_str())
                    .map_or(false, |x| EDITABLE.contains(&x));
                if kind.is_dir() && !name.starts_with('.') {
                    todo.push(e.path());
                } else if kind.is_file()
                    && (editable && !name.starts_with('.') || name == ".gitkeep")
                {
                    files.push(rel);
                } else {
                    return Err((
                        409,
                        format!("`{path}` contains `{rel}`, which cannot be deleted from here"),
                    ));
                }
            }
        }
        files.sort();
        std::fs::remove_dir_all(&p).map_err(|e| (404, format!("{path}: {e}")))?;
        self.dev_generation += 1;
        Ok(reply(200, json!({ "path": path, "deleted": files })))
    }

    /// The working copy with unsaved buffers on top.
    pub(super) fn working_project(&self, buffers: &J) -> Result<Project, Fail> {
        let mut overlays = HashMap::new();
        if let Some(o) = buffers.as_object() {
            for (rel, text) in o {
                let p = safe_path(&self.root, rel, EDITABLE)?;
                overlays.insert(p, text.as_str().unwrap_or("").to_string());
            }
        }
        Project::load_with_overlays(&self.root, &overlays).map_err(|e| (500, e.to_string()))
    }

    fn diagnostics(&self, project: &Project, checked: &Checked) -> Vec<J> {
        // with the hints of how the code meets the database (no database read)
        let hints = quadra_store::hints::hints(checked);
        checked
            .diagnostics
            .iter()
            .chain(hints.iter())
            .filter_map(|d| {
                let f = project.file(d.file);
                if f.kind == quadra_sema::FileKind::Std {
                    return None;
                }
                let rel = f.path.strip_prefix(&self.root).unwrap_or(&f.path).to_string_lossy().replace('\\', "/");
                let (start, end) = (usize::from(d.range.start()), usize::from(d.range.end()));
                let line = f.text[..start.min(f.text.len())].matches('\n').count() + 1;
                let fix = d
                    .message
                    .split("add `from ")
                    .nth(1)
                    .and_then(|r| r.split('`').next())
                    .and_then(|r| r.split_once(" import "))
                    .and_then(|(module, name)| import_edit(&f.text, module, name).map(|(from, to, insert)| json!({ "label": format!("Import {name} from {module}"), "from": from, "to": to, "insert": insert })));
                Some(json!({
                    "fix": fix,
                    "path": rel,
                    "from": utf16(&f.text, start),
                    "to": utf16(&f.text, end),
                    "line": line,
                    "severity": match d.severity {
                        quadra_sema::Severity::Error => "error",
                        quadra_sema::Severity::Warning => "warning",
                        quadra_sema::Severity::Hint => "hint",
                    },
                    "message": d.message,
                }))
            })
            .collect()
    }

    pub(super) fn dev_check(&mut self, who: &Principal, buffers: &J) -> Result<Reply, Fail> {
        self.editor_login(who)?;
        let project = self.working_project(buffers)?;
        let checked = quadra_analysis::check(&project);
        let diags = self.diagnostics(&project, &checked);
        Ok(reply(200, json!({ "diagnostics": diags })))
    }

    pub(super) fn dev_test(&mut self, who: &Principal, buffers: &J) -> Result<Reply, Fail> {
        self.editor_login(who)?;
        let project = self.working_project(buffers)?;
        match quadra_runtime::run_tests(&project, None) {
            Ok(results) => {
                let list: Vec<J> = results.iter().map(|r| json!({ "name": r.name, "location": r.location, "passed": r.passed, "message": r.message })).collect();
                Ok(reply(200, json!({ "results": list })))
            }
            Err(diags) => Ok(reply(
                200,
                json!({ "error": format!("{} error(s): fix them first", diags.iter().filter(|d| d.severity == quadra_sema::Severity::Error).count()) }),
            )),
        }
    }

    pub(super) fn dev_status(&mut self, who: &Principal) -> Result<Reply, Fail> {
        self.editor_login(who)?;
        let repo = self.is_repo();
        let branch = git(&self.root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .map(|s| s.trim().to_string())
            .ok();
        let log = git(
            &self.root,
            &[
                "log",
                "-n",
                "15",
                "--format=%H%x1f%an%x1f%aI%x1f%s",
                "--",
                ".",
            ],
        )
        .unwrap_or_default();
        let commits: Vec<J> = log
            .lines()
            .filter_map(|l| {
                let p: Vec<&str> = l.split('\u{1f}').collect();
                (p.len() == 4)
                    .then(|| json!({ "sha": p[0], "author": p[1], "at": p[2], "message": p[3] }))
            })
            .collect();
        let checked = self.checked.clone();
        let mut db = Database::open(&self.url, checked.model.as_ref().unwrap(), &checked.report)
            .map_err(|e| (500, e))?;
        let deployed = db.meta("code_commit").map_err(|e| (500, e))?;
        let changes = self.changes();
        Ok(reply(
            200,
            json!({ "repository": repo, "branch": branch, "commits": commits, "deployed": deployed, "changes": changes }),
        ))
    }

    pub(super) fn dev_diff(&mut self, who: &Principal) -> Result<Reply, Fail> {
        self.editor_login(who)?;
        // new files take part in the diff without being staged
        let _ = git(&self.root, &["add", "--intent-to-add", "--", "."]);
        let base = if git(&self.root, &["rev-parse", "--verify", "HEAD"]).is_ok() {
            "HEAD"
        } else {
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        };
        let diff =
            git(&self.root, &["diff", "--no-color", base, "--", "."]).map_err(|e| (500, e))?;
        Ok(reply(200, json!({ "diff": diff })))
    }

    /// Commit the working copy and apply it. Every step is reported.
    pub(super) fn dev_commit(&mut self, who: &Principal, message: &str) -> Result<Reply, Fail> {
        let login = self.editor_login(who)?;
        if !self.is_repo() {
            return Err((
                409,
                "the project is not a git repository: run `git init` in it".into(),
            ));
        }
        let message = message.trim();
        if message.is_empty() || message.len() > 2000 {
            return Err((400, "a commit message is required".into()));
        }
        if self.changes().is_empty() {
            return Err((409, "nothing to commit".into()));
        }
        let mut steps: Vec<J> = Vec::new();
        let fail = |steps: Vec<J>, step: &str, detail: String, extra: J| -> Result<Reply, Fail> {
            let mut steps = steps;
            steps.push(json!({ "step": step, "ok": false, "detail": detail }));
            Ok(reply(
                422,
                json!({ "error": format!("{step}: {detail}"), "steps": steps, "diagnostics": extra }),
            ))
        };
        // 1. the new model
        let project = Project::load(&self.root).map_err(|e| (500, e.to_string()))?;
        let checked = quadra_analysis::check(&project);
        let errors = checked
            .diagnostics
            .iter()
            .filter(|d| d.severity == quadra_sema::Severity::Error)
            .count();
        if errors > 0 || checked.model.is_none() {
            let diags = self.diagnostics(&project, &checked);
            return fail(
                steps,
                "check",
                format!("{errors} error(s)"),
                J::Array(diags),
            );
        }
        steps.push(json!({ "step": "check", "ok": true, "detail": "no errors" }));
        // 2. tests
        let results = quadra_runtime::run_tests(&project, None).unwrap_or_default();
        let failed: Vec<String> = results
            .iter()
            .filter(|r| !r.passed)
            .map(|r| format!("{} ({})", r.name, r.message.clone().unwrap_or_default()))
            .collect();
        if !failed.is_empty() {
            return fail(steps, "tests", failed.join("; "), J::Null);
        }
        steps.push(
            json!({ "step": "tests", "ok": true, "detail": format!("{} passed", results.len()) }),
        );
        // 3. dry run on the real data
        let model = checked.model.as_ref().unwrap();
        let label = format!("deploy: {message} ({login})");
        match quadra_store::ops::deploy(
            &self.url,
            model,
            &checked.report,
            &self.root,
            now(),
            true,
            &label,
        ) {
            Ok(r) => {
                let mut d = Vec::new();
                if r.schema_changed {
                    d.push("schema changes".to_string());
                }
                for (name, _) in &r.scripts {
                    d.push(format!("migration {name}"));
                }
                if r.new_defaults > 0 {
                    d.push(format!("{} new values from defaults", r.new_defaults));
                }
                if d.is_empty() {
                    d.push("the data satisfies the new model".into());
                }
                steps.push(json!({ "step": "dry run", "ok": true, "detail": d.join(", ") }));
            }
            Err(e) => return fail(steps, "dry run", e, J::Null),
        }
        // 4. git
        if let Err(e) = git(&self.root, &["add", "-A", "--", "."]) {
            return fail(steps, "commit", e, J::Null);
        }
        let author = format!("{login} <{login}@quadra>");
        let name = format!("user.name={login}");
        let email = format!("user.email={login}@quadra");
        if let Err(e) = git(
            &self.root,
            &[
                "-c", &name, "-c", &email, "commit", "-q", "--author", &author, "-m", message,
            ],
        ) {
            return fail(steps, "commit", e, J::Null);
        }
        let sha = git(&self.root, &["rev-parse", "HEAD"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        steps.push(json!({ "step": "commit", "ok": true, "detail": sha }));
        // 5. apply
        let label = format!("deploy {}: {message} ({login})", &sha[..sha.len().min(10)]);
        if let Err(e) = quadra_store::ops::deploy(
            &self.url,
            model,
            &checked.report,
            &self.root,
            now(),
            false,
            &label,
        )
        .inspect(|_| self.sequencer_reload())
        {
            let _ = git(
                &self.root,
                &["-c", &name, "-c", &email, "revert", "--no-edit", "HEAD"],
            );
            return fail(
                steps,
                "apply",
                format!("{e} — the commit was reverted"),
                J::Null,
            );
        }
        if let Ok(mut db) = Database::open(&self.url, model, &checked.report) {
            let _ = db.set_meta("code_commit", &sha);
        }
        steps.push(json!({ "step": "apply", "ok": true, "detail": "database and server on the new version" }));
        // 6. the server serves the new model
        self.reload(project, checked).map_err(|e| (500, e))?;
        Ok(reply(
            200,
            json!({ "sha": sha, "steps": steps, "deployed": true }),
        ))
    }

    /// Serve a new version of the model.
    /// `quadra dev`: the files on disk changed (an editor, the browser IDE). Checked, applied to
    /// the database (migrations, new defaults, every constraint, in one transaction) and
    /// served at once. Code that does not compile, or that the data does not fit, is reported
    /// and the last version that worked keeps running. The event to broadcast, if any.
    pub(super) fn dev_reload(&mut self) -> Option<J> {
        let project = match Project::load(&self.root) {
            Ok(p) => p,
            Err(e) => {
                return Some(self.set_problems(json!({ "kind": "io", "message": e.to_string() })))
            }
        };
        let files: Vec<(PathBuf, String)> = project
            .user_files()
            .map(|f| (f.path.clone(), f.text.clone()))
            .collect();
        let interfaces = super::interface_texts(&project);
        let interfaces_changed = self.seen_interfaces.as_ref() != Some(&interfaces);
        if files == self.seen && !interfaces_changed {
            return None;
        }
        self.seen = files.clone();
        self.seen_interfaces = Some(interfaces);
        self.dev_generation += 1;
        if files == self.files && self.problems.is_none() && !interfaces_changed {
            return None;
        }
        let checked = quadra_analysis::check(&project);
        let errors: Vec<J> = checked
            .diagnostics
            .iter()
            .filter(|d| d.severity == quadra_sema::Severity::Error)
            .take(20)
            .map(|d| {
                let f = project.file(d.file);
                let at = usize::from(d.range.start()).min(f.text.len());
                let line = f.text[..at].matches('\n').count() + 1;
                let rel = f
                    .path
                    .strip_prefix(&self.root)
                    .unwrap_or(&f.path)
                    .to_string_lossy()
                    .replace('\\', "/");
                json!({ "path": rel, "line": line, "message": d.message })
            })
            .collect();
        if !errors.is_empty() || checked.model.is_none() {
            return Some(self.set_problems(json!({ "kind": "code", "errors": errors })));
        }
        let model = checked.model.as_ref().unwrap();
        if let Err(e) = quadra_store::ops::deploy(
            &self.url,
            model,
            &checked.report,
            &self.root,
            now(),
            false,
            "dev: the files changed",
        )
        .inspect(|_| self.sequencer_reload())
        {
            return Some(self.set_problems(json!({ "kind": "data", "message": e })));
        }
        if let Err(e) = self.reload(project, checked) {
            return Some(self.set_problems(json!({ "kind": "data", "message": e })));
        }
        self.problems = None;
        eprintln!("quadra dev: applied ✓");
        Some(json!({ "type": "deploy", "sha": "dev" }))
    }

    fn set_problems(&mut self, p: J) -> J {
        let indent = |s: &str| {
            s.lines()
                .map(|l| format!("  {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        match p["kind"].as_str() {
            Some("code") => {
                let errors = p["errors"].as_array().cloned().unwrap_or_default();
                eprintln!("quadra dev: the code has {} error(s), the last version that compiled keeps running:", errors.len());
                for e in errors.iter().take(10) {
                    eprintln!(
                        "  {}:{}  {}",
                        e["path"].as_str().unwrap_or(""),
                        e["line"],
                        e["message"].as_str().unwrap_or("")
                    );
                }
            }
            _ => eprintln!(
                "quadra dev: not applied, the last version keeps running:\n{}\n",
                indent(p["message"].as_str().unwrap_or(""))
            ),
        }
        self.problems = Some(p.clone());
        json!({ "type": "problems", "problems": p })
    }

    fn reload(&mut self, project: Project, checked: Checked) -> Result<(), String> {
        let model = checked.model.as_ref().ok_or("no model")?;
        let db = Database::open(&self.url, model, &checked.report)?;
        self.schema = db.schema.clone();
        self.renames = None;
        self.units = None;
        self.pool.borrow_mut().clear();
        self.files = project
            .user_files()
            .map(|f| (f.path.clone(), f.text.clone()))
            .collect();
        self.modules = project
            .files()
            .map(|f| {
                (
                    f.id,
                    if f.kind == quadra_sema::FileKind::Std {
                        "std".to_string()
                    } else {
                        f.module.clone()
                    },
                )
            })
            .collect();
        self.checked = Rc::new(super::with_env(checked, &self.env));
        self.overlays.clear();
        self.overlay_order.clear();
        self.dev_generation += 1;
        self.ide_cache = None;
        let max = self.cache.borrow().max_bytes;
        self.cache = Rc::new(RefCell::new(Cache::new(
            max,
            std::time::Duration::from_secs(600),
        )));
        // saved views follow the new model: renames applied, what no longer fits marked
        self.revalidate_views();
        self.install_interfaces(&project)?;
        Ok(())
    }
}
