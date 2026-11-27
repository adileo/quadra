//! Project loading, modules, imports, `extend` and name resolution (phase 2).
//!
//! A project is a directory with a `quadra.toml`. Every `.qd` file below it is a module
//! named after its path: `core/money.qd` → `core.money`. Files under `tests/` and
//! `migrations/` are *scripts*: they see every declaration of the project without imports.

pub mod ide;
pub mod manifest;
mod resolve;

use quadra_syntax::{Parse, TextRange};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use resolve::{
    Analysis, Decl, DeclId, DeclKind, Member, MemberKind, Reference, Target,
    BUILTIN_TYPES as BUILTIN_TYPE_NAMES,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Module,
    Script,
    /// The standard library, embedded in the compiler and visible everywhere.
    Std,
    /// An interface file (`interfaces/**/*.qdi`: apps and saved views). It is not Quadra code:
    /// its text is checked by `quadra_qdi::check` (its `parse` is that of an empty file).
    Interface,
}

/// The standard library sources, bundled into the binary.
pub const STD_SOURCES: &[(&str, &str)] = &[
    ("money", include_str!("../../../std/money.qd")),
    ("uom", include_str!("../../../std/uom.qd")),
    ("org", include_str!("../../../std/org.qd")),
    ("time", include_str!("../../../std/time.qd")),
];

/// Pseudo-directory of the standard library files.
pub fn std_dir() -> PathBuf {
    PathBuf::from("<std>")
}

pub struct SourceFile {
    pub id: FileId,
    pub path: PathBuf,
    pub module: String,
    pub text: String,
    pub parse: Parse,
    pub kind: FileKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    /// how the code meets the database (`quadra_store::hints`): the editor shows it discreetly
    Hint,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub file: FileId,
    pub range: TextRange,
    pub severity: Severity,
    pub message: String,
}

pub struct Project {
    pub root: PathBuf,
    /// the edition of the language the project is written in (`edition` in quadra.toml)
    pub edition: String,
    /// the project's time zone (`timezone` in quadra.toml, an IANA name; default `UTC`): where
    /// `today()`, `.date`, `.hour` and a date-time made from a date and a time are read
    pub timezone: String,
    /// the variables and connections of quadra.toml (`manifest`)
    pub manifest: manifest::Manifest,
    files: Vec<SourceFile>,
}

/// The editions of the language this compiler reads. A project names one in quadra.toml
/// (`[project] edition = "2026"`); without it, the project is in the default one.
pub const EDITIONS: &[&str] = &["2026"];
pub const DEFAULT_EDITION: &str = "2026";

/// The edition a project declares in its quadra.toml (the default one when it declares none
/// or has no quadra.toml), or why it cannot be read.
pub fn read_edition(root: &Path) -> Result<String, String> {
    let Ok(text) = std::fs::read_to_string(root.join("quadra.toml")) else {
        return Ok(DEFAULT_EDITION.to_string());
    };
    let mut section = String::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            section = line
                .trim_matches(|c| c == '[' || c == ']')
                .trim()
                .to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if section == "project" && key.trim() == "edition" {
            let edition = value.trim().trim_matches('"').to_string();
            if !EDITIONS.contains(&edition.as_str()) {
                return Err(format!("quadra.toml: edition `{edition}` is not known to this version of Quadra (known: {})", EDITIONS.join(", ")));
            }
            return Ok(edition);
        }
    }
    Ok(DEFAULT_EDITION.to_string())
}

/// A key of the `[project]` section of quadra.toml, if set.
fn project_setting(root: &Path, want: &str) -> Option<String> {
    let text = std::fs::read_to_string(root.join("quadra.toml")).ok()?;
    let mut section = String::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            section = line
                .trim_matches(|c| c == '[' || c == ']')
                .trim()
                .to_string();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if section == "project" && key.trim() == want {
                return Some(value.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

/// Whether a name is a time zone of the IANA database (`Europe/Rome`, `UTC`).
pub fn is_timezone(name: &str) -> bool {
    name.parse::<chrono_tz::Tz>().is_ok()
}

/// The time zone a project declares in its quadra.toml (UTC when it declares none), or why it
/// cannot be used.
pub fn read_timezone(root: &Path) -> Result<String, String> {
    match project_setting(root, "timezone") {
        None => Ok("UTC".into()),
        Some(tz) if is_timezone(&tz) => Ok(tz),
        Some(tz) => Err(format!("quadra.toml: timezone `{tz}` is not a time zone (an IANA name such as `Europe/Rome`, `America/New_York` or `UTC`)")),
    }
}

const SCRIPT_DIRS: &[&str] = &["tests", "migrations"];

/// The directory of a project's interface files (`.qdi`).
pub const INTERFACES_DIR: &str = "interfaces";

impl Project {
    pub fn load(root: &Path) -> std::io::Result<Project> {
        Project::load_with_overlays(root, &HashMap::new())
    }

    /// Load from disk, replacing file contents with `overlays` (unsaved editor buffers).
    pub fn load_with_overlays(
        root: &Path,
        overlays: &HashMap<PathBuf, String>,
    ) -> std::io::Result<Project> {
        read_edition(root).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        read_timezone(root).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        manifest::read(root)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut paths = Vec::new();
        collect_qd(root, &mut paths)?;
        let interfaces = root.join(INTERFACES_DIR);
        if interfaces.is_dir() {
            collect_qdi(&interfaces, &mut paths)?;
        }
        for p in overlays.keys() {
            let code = p.extension().map_or(false, |e| e == "qd");
            let interface =
                p.extension().map_or(false, |e| e == "qdi") && p.starts_with(&interfaces);
            if p.starts_with(root) && (code || interface) && !paths.contains(p) {
                paths.push(p.clone());
            }
        }
        paths.sort();
        let mut sources = Vec::new();
        for path in paths {
            let text = match overlays.get(&path) {
                Some(t) => t.clone(),
                None => std::fs::read_to_string(&path)?,
            };
            sources.push((path, text));
        }
        Ok(Project::from_sources(root, sources))
    }

    pub fn from_sources(root: &Path, sources: Vec<(PathBuf, String)>) -> Project {
        let mut files: Vec<SourceFile> = STD_SOURCES
            .iter()
            .map(|(name, text)| SourceFile {
                id: FileId(0),
                path: std_dir().join(format!("{name}.qd")),
                module: format!("std.{name}"),
                text: text.to_string(),
                parse: quadra_syntax::parse(text),
                kind: FileKind::Std,
            })
            .collect();
        for (path, text) in sources {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            if path.extension().map_or(false, |e| e == "qdi") {
                let module = format!(
                    "qdi:{}",
                    rel.with_extension("").to_string_lossy().replace('\\', "/")
                );
                let parse = quadra_syntax::parse("");
                files.push(SourceFile {
                    id: FileId(0),
                    path,
                    module,
                    text,
                    parse,
                    kind: FileKind::Interface,
                });
                continue;
            }
            let module = module_name(&rel);
            let kind = match rel.components().next().and_then(|c| c.as_os_str().to_str()) {
                Some(first) if SCRIPT_DIRS.contains(&first) && rel.components().count() > 1 => {
                    FileKind::Script
                }
                _ => FileKind::Module,
            };
            let parse = quadra_syntax::parse(&text);
            files.push(SourceFile {
                id: FileId(0),
                path,
                module,
                text,
                parse,
                kind,
            });
        }
        for (i, f) in files.iter_mut().enumerate() {
            f.id = FileId(i as u32);
        }
        // read again here (it was checked by `load`): projects built from sources keep it too
        let edition = read_edition(root).unwrap_or_else(|_| DEFAULT_EDITION.to_string());
        let timezone = read_timezone(root).unwrap_or_else(|_| "UTC".to_string());
        let manifest = manifest::read(root).unwrap_or_default();
        Project {
            root: root.to_path_buf(),
            edition,
            timezone,
            manifest,
            files,
        }
    }

    /// The code written by the user: `.qd` modules and scripts (not the standard library, not
    /// interface files).
    pub fn user_files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files
            .iter()
            .filter(|f| matches!(f.kind, FileKind::Module | FileKind::Script))
    }

    /// The interface files (`interfaces/**/*.qdi`).
    pub fn interface_files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter().filter(|f| f.kind == FileKind::Interface)
    }

    /// The same project with more script files (generated code, checked with the project).
    pub fn with_scripts(&self, scripts: Vec<(PathBuf, String)>) -> Project {
        let mut sources: Vec<(PathBuf, String)> = self
            .files
            .iter()
            .filter(|f| f.kind != FileKind::Std)
            .map(|f| (f.path.clone(), f.text.clone()))
            .collect();
        sources.extend(scripts);
        Project::from_sources(&self.root, sources)
    }

    pub fn files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter()
    }

    pub fn file(&self, id: FileId) -> &SourceFile {
        &self.files[id.0 as usize]
    }

    pub fn file_by_path(&self, path: &Path) -> Option<&SourceFile> {
        self.files.iter().find(|f| f.path == path)
    }

    pub fn analyze(&self) -> Analysis {
        resolve::analyze(self)
    }
}

/// `core/money.qd` → `core.money`, `tests/a.test.qd` → `tests.a.test`.
pub fn module_name(rel: &Path) -> String {
    let s = rel.with_extension("");
    s.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(".")
}

fn collect_qd(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            // a nested project is not part of this one
            if path.join("quadra.toml").exists() {
                continue;
            }
            collect_qd(&path, out)?;
        } else if path.extension().map_or(false, |e| e == "qd") {
            out.push(path);
        }
    }
    Ok(())
}

fn collect_qdi(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_qdi(&path, out)?;
        } else if path.extension().map_or(false, |e| e == "qdi") {
            out.push(path);
        }
    }
    Ok(())
}

/// Nearest directory containing `quadra.toml`, starting from `path` (file or directory).
pub fn find_project_root(path: &Path) -> Option<PathBuf> {
    let path = std::fs::canonicalize(path).ok()?;
    let start = if path.is_file() {
        path.parent()?.to_path_buf()
    } else {
        path
    };
    start
        .ancestors()
        .find(|d| d.join("quadra.toml").is_file())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod edition_tests {
    use super::*;

    #[test]
    fn the_edition_comes_from_quadra_toml() {
        let dir = std::env::temp_dir().join(format!("quadra-edition-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("quadra.toml"), "[project]\nname = \"x\"\n").unwrap();
        assert_eq!(read_edition(&dir).unwrap(), DEFAULT_EDITION);
        assert_eq!(Project::load(&dir).unwrap().edition, DEFAULT_EDITION);
        std::fs::write(
            dir.join("quadra.toml"),
            "[project]\nname = \"x\"\nedition = \"2026\"  # the first\n",
        )
        .unwrap();
        assert_eq!(read_edition(&dir).unwrap(), "2026");
        // an edition under another section is not the project's
        std::fs::write(
            dir.join("quadra.toml"),
            "[project]\nname = \"x\"\n\n[database]\nedition = \"1999\"\n",
        )
        .unwrap();
        assert_eq!(read_edition(&dir).unwrap(), DEFAULT_EDITION);
        std::fs::write(
            dir.join("quadra.toml"),
            "[project]\nname = \"x\"\nedition = \"2031\"\n",
        )
        .unwrap();
        let e = Project::load(&dir).err().unwrap().to_string();
        assert!(e.contains("edition `2031` is not known"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
