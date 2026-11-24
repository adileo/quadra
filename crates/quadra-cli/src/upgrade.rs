//! `quadra upgrade`: the latest official binary in place of this one.
//!
//! The releases of github.com/adileo/quadra hold, for every target, `quadra-<target>.gz` (the
//! binary, compressed) and its `.sha256`, and `latest.json` says the latest version. The
//! download is checked against the SHA-256 before it replaces anything. On Unix the new file is
//! renamed over the old one (a running program keeps its file); on Windows the running file is
//! renamed aside first and removed at the next start.

use crate::http;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const TARGET: &str = env!("QUADRA_TARGET");

/// Where the releases are: `QUADRA_RELEASES_URL` replaces it (tests, mirrors).
pub fn releases() -> String {
    std::env::var("QUADRA_RELEASES_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://github.com/adileo/quadra/releases".into())
        .trim_end_matches('/')
        .to_string()
}

/// `major.minor.patch` as numbers; a suffix (`-rc.1`) is ignored.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let core = v.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0)))
}

pub fn older(a: &str, b: &str) -> bool {
    matches!((parse_version(a), parse_version(b)), (Some(x), Some(y)) if x < y)
}

pub fn latest() -> Result<String, String> {
    let url = format!("{}/latest/download/latest.json", releases());
    let (status, v) = http::get_json(&url)?;
    if status != 200 {
        return Err(format!("{url}: HTTP {status}"));
    }
    v["version"].as_str().map(str::to_owned).ok_or_else(|| format!("{url}: no version"))
}

fn asset(version: &str, name: &str) -> String {
    format!("{}/download/v{version}/{name}", releases())
}

fn install_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("where am I: {e}"))?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// Removes what a previous upgrade left on Windows (the old binary, in use back then).
pub fn clean_old() {
    if let Ok(exe) = install_path() {
        let _ = std::fs::remove_file(exe.with_extension("old.exe"));
    }
}

/// Downloads, checks and installs `version` (the latest if None). Returns the version now
/// installed.
pub fn upgrade_to(version: Option<&str>, force: bool) -> Result<String, String> {
    let want = match version {
        Some(v) => v.trim_start_matches('v').to_string(),
        None => latest()?,
    };
    if !force && want == VERSION {
        eprintln!("quadra {VERSION} is the latest version.");
        return Ok(want);
    }
    let exe = install_path()?;
    if exe.components().any(|c| c.as_os_str() == "Cellar") {
        return Err("this quadra was installed with Homebrew: run `brew upgrade quadra`".into());
    }
    let name = format!("quadra-{TARGET}.gz");
    eprintln!("Downloading quadra {want} ({TARGET})…");
    let gz = http::get_bytes(&asset(&want, &name))?;
    let sum = http::get_bytes(&asset(&want, &format!("{name}.sha256")))?;
    let want_sum = String::from_utf8_lossy(&sum)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let got: String = Sha256::digest(&gz).iter().map(|b| format!("{b:02x}")).collect();
    if want_sum.len() != 64 || got != want_sum {
        return Err(format!("the download does not match its SHA-256 ({got} instead of {want_sum}): nothing was changed"));
    }
    let mut bin = Vec::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut bin)
        .map_err(|e| format!("{name}: {e}"))?;
    replace(&exe, &bin)?;
    eprintln!("quadra {VERSION} → {want} ({}).", exe.display());
    Ok(want)
}

fn replace(exe: &Path, bin: &[u8]) -> Result<(), String> {
    let dir = exe.parent().ok_or("no folder")?;
    let tmp = dir.join(format!(".quadra-upgrade-{}", std::process::id()));
    let denied = |e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!("cannot write in {}: install again with the installer, or run with the rights of that folder", dir.display())
        } else {
            format!("{}: {e}", dir.display())
        }
    };
    std::fs::write(&tmp, bin).map_err(denied)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, exe).map_err(denied)?;
    }
    #[cfg(windows)]
    {
        let old = exe.with_extension("old.exe");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).map_err(denied)?;
        if let Err(e) = std::fs::rename(&tmp, exe) {
            let _ = std::fs::rename(&old, exe);
            return Err(denied(e));
        }
    }
    Ok(())
}

pub fn main(args: &[String]) -> std::process::ExitCode {
    let mut version = None;
    let mut force = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--version" => version = it.next().cloned(),
            "--force" => force = true,
            other => {
                eprintln!("quadra upgrade: unknown option {other}\nusage: quadra upgrade [--version X.Y.Z] [--force]");
                return std::process::ExitCode::from(2);
            }
        }
    }
    match upgrade_to(version.as_deref(), force) {
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// At most once a day, in a terminal: a line when a newer version is out.
pub fn notice() {
    use std::io::IsTerminal;
    if !std::io::stderr().is_terminal() || std::env::var_os("QUADRA_NO_UPDATE_CHECK").is_some() {
        return;
    }
    let Some(file) = crate::cloud::config_dir().map(|d| d.join("update-check.json")) else { return };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cached: serde_json::Value = std::fs::read_to_string(&file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let latest = if now.saturating_sub(cached["checked"].as_u64().unwrap_or(0)) < 86_400 {
        cached["latest"].as_str().map(str::to_owned)
    } else {
        let l = latest().ok();
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&file, serde_json::json!({ "checked": now, "latest": l }).to_string());
        l
    };
    if let Some(l) = latest.filter(|l| older(VERSION, l)) {
        eprintln!("\nquadra {l} is out (you have {VERSION}): quadra upgrade");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.5.0"), Some((0, 5, 0)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("1.2.3-rc.1"), Some((1, 2, 3)));
        assert!(older("0.5.0", "0.5.1") && older("0.9.9", "0.10.0") && !older("1.0.0", "1.0.0"));
        assert!(!older("x", "1.0.0"));
    }
}
