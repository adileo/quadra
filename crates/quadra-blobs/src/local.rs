//! A folder of the machine as a store: a key is a path under it. Its signed URLs are answered
//! by the server (`/v1/blobs/<key>`): what they allow (the method, the key, until when, the
//! size of an upload, the type and disposition of a download) is in the query, with an
//! HMAC-SHA256 of it under a key of the folder (`.key`, made the first time).

use crate::{encode, hmac_hex, unix_now, Blobs, Result, Signed};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Local {
    root: PathBuf,
    key: Vec<u8>,
}

/// What a signed URL of the folder allows, once checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub method: String,
    pub key: String,
    /// an upload: its size, exactly
    pub size: Option<u64>,
    /// a download: the type and disposition it is answered with
    pub ctype: Option<String>,
    pub disposition: Option<String>,
}

impl Local {
    /// The folder (made if it is not there), with its key.
    pub fn open(root: &Path) -> Result<Local> {
        std::fs::create_dir_all(root)
            .map_err(|e| format!("the folder of the files {}: {e}", root.display()))?;
        let at = root.join(".key");
        let key = match std::fs::read_to_string(&at) {
            Ok(k) if k.trim().len() >= 32 => k.trim().as_bytes().to_vec(),
            _ => {
                let mut b = [0u8; 32];
                getrandom::fill(&mut b).map_err(|e| e.to_string())?;
                let k = hex::encode(b);
                let tmp = root.join(format!(".key.{}", std::process::id()));
                write_private(&tmp, k.as_bytes())
                    .and_then(|_| std::fs::rename(&tmp, &at))
                    .map_err(|e| format!("the key of the files {}: {e}", at.display()))?;
                // another process may have made one at the same time: theirs or ours, the same
                // for both from now on
                std::fs::read_to_string(&at)
                    .map_err(|e| e.to_string())?
                    .trim()
                    .as_bytes()
                    .to_vec()
            }
        };
        Ok(Local {
            root: root.to_path_buf(),
            key,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path of a key: letters, digits, `-`, `_`, `.` in parts separated by `/`, never
    /// `..` nor a hidden file.
    fn path(&self, key: &str) -> Result<PathBuf> {
        let ok = !key.is_empty()
            && key.split('/').all(|p| {
                !p.is_empty()
                    && !p.starts_with('.')
                    && p.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            });
        if !ok {
            return Err(format!("`{key}` is not a key of a file"));
        }
        Ok(self.root.join(key))
    }

    fn signature(&self, method: &str, key: &str, expires: u64, rest: &[(&str, &str)]) -> String {
        let mut msg = format!("{method}\n{key}\n{expires}");
        for (k, v) in rest {
            msg.push_str(&format!("\n{k}={v}"));
        }
        hmac_hex(&self.key, msg.as_bytes())
    }

    fn url(&self, method: &str, key: &str, valid: Duration, rest: &[(&str, &str)]) -> String {
        let expires = unix_now() + valid.as_secs();
        let sig = self.signature(method, key, expires, rest);
        let mut url = format!("/v1/blobs/{key}?expires={expires}");
        for (k, v) in rest {
            url.push_str(&format!("&{k}={}", encode(v)));
        }
        url.push_str(&format!("&sig={sig}"));
        url
    }

    /// What a request to `/v1/blobs/<key>` with this query may do: refused if the signature
    /// does not match or it expired.
    pub fn check(&self, method: &str, key: &str, query: &HashMap<String, String>) -> Result<Grant> {
        self.path(key)?;
        let expires: u64 = query
            .get("expires")
            .and_then(|e| e.parse().ok())
            .ok_or("the link is not signed")?;
        let names: &[&str] = if method == "PUT" {
            &["size"]
        } else {
            &["type", "disposition"]
        };
        let rest: Vec<(&str, &str)> = names
            .iter()
            .filter_map(|n| query.get(*n).map(|v| (*n, v.as_str())))
            .collect();
        let expected = self.signature(method, key, expires, &rest);
        let given = query.get("sig").map(String::as_str).unwrap_or("");
        if !constant_eq(expected.as_bytes(), given.as_bytes()) {
            return Err("the signature of the link does not match".into());
        }
        if unix_now() > expires {
            return Err("the link has expired".into());
        }
        Ok(Grant {
            method: method.to_string(),
            key: key.to_string(),
            size: query.get("size").and_then(|s| s.parse().ok()),
            ctype: query.get("type").cloned(),
            disposition: query.get("disposition").cloned(),
        })
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |x, (p, q)| x | (p ^ q)) == 0
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

impl Blobs for Local {
    fn put(&self, key: &str, bytes: &[u8], _ctype: &str, _disposition: &str) -> Result<()> {
        let path = self.path(key)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        // whole or not at all: written beside it, then renamed
        let tmp = path.with_extension(format!("part{}", std::process::id()));
        std::fs::write(&tmp, bytes)
            .and_then(|_| std::fs::rename(&tmp, &path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("put {key}: {e}")
            })
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(key)?) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("get {key}: {e}")),
        }
    }

    fn head(&self, key: &str) -> Result<Option<u64>> {
        match std::fs::metadata(self.path(key)?) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("head {key}: {e}")),
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        match std::fs::remove_file(self.path(key)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("delete {key}: {e}")),
        }
    }

    fn copy(&self, from: &str, to: &str) -> Result<()> {
        let bytes = self.get(from)?.ok_or(format!("copy {from}: not found"))?;
        self.put(to, &bytes, "", "")
    }

    fn put_url(&self, key: &str, size: u64, valid: Duration) -> Result<Signed> {
        self.path(key)?;
        Ok(Signed {
            method: "PUT".into(),
            url: self.url("PUT", key, valid, &[("size", &size.to_string())]),
            headers: Vec::new(),
        })
    }

    fn get_url(
        &self,
        key: &str,
        valid: Duration,
        ctype: &str,
        disposition: &str,
    ) -> Result<String> {
        self.path(key)?;
        Ok(self.url(
            "GET",
            key,
            valid,
            &[("type", ctype), ("disposition", disposition)],
        ))
    }

    fn local(&self) -> Option<&Local> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(url: &str) -> HashMap<String, String> {
        let q = url.split_once('?').unwrap().1;
        q.split('&')
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap();
                (k.to_string(), decode(v))
            })
            .collect()
    }

    fn decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_folder_keeps_objects_and_signs_what_it_allows() {
        let dir = std::env::temp_dir().join(format!("quadra-blobs-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let l = Local::open(&dir).unwrap();
        l.put("uploads/abc", b"hello", "", "").unwrap();
        assert_eq!(
            l.get("uploads/abc").unwrap().as_deref(),
            Some(&b"hello"[..])
        );
        assert_eq!(l.head("uploads/abc").unwrap(), Some(5));
        l.copy("uploads/abc", "files/h1").unwrap();
        l.delete("uploads/abc").unwrap();
        l.delete("uploads/abc").unwrap();
        assert_eq!(l.get("uploads/abc").unwrap(), None);
        assert_eq!(l.head("files/h1").unwrap(), Some(5));
        for bad in ["../x", "files/../../etc", ".key", "files/.key", ""] {
            assert!(l.get(bad).is_err(), "{bad}");
        }
        // the same key after a restart: links made before still work
        let again = Local::open(&dir).unwrap();
        assert_eq!(again.key, l.key);

        let put = l.put_url("uploads/u1", 5, Duration::from_secs(60)).unwrap();
        let g = l.check("PUT", "uploads/u1", &query(&put.url)).unwrap();
        assert_eq!(g.size, Some(5));
        // another key, another method, another size: refused
        assert!(l.check("PUT", "uploads/u2", &query(&put.url)).is_err());
        assert!(l.check("GET", "uploads/u1", &query(&put.url)).is_err());
        let mut q = query(&put.url);
        q.insert("size".into(), "500".into());
        assert!(l.check("PUT", "uploads/u1", &q).is_err());

        let get = l
            .get_url(
                "files/h1",
                Duration::from_secs(60),
                "application/pdf",
                "inline; filename=\"a b.pdf\"",
            )
            .unwrap();
        let g = l.check("GET", "files/h1", &query(&get)).unwrap();
        assert_eq!(g.ctype.as_deref(), Some("application/pdf"));
        assert_eq!(
            g.disposition.as_deref(),
            Some("inline; filename=\"a b.pdf\"")
        );
        let mut q = query(&get);
        q.insert("disposition".into(), "attachment".into());
        assert!(l.check("GET", "files/h1", &q).is_err());
        // expired
        let old = l.url(
            "GET",
            "files/h1",
            Duration::ZERO,
            &[("type", "x"), ("disposition", "y")],
        );
        let mut q = query(&old);
        let e: u64 = q["expires"].parse().unwrap();
        q.insert("expires".into(), (e - 10).to_string());
        q.insert(
            "sig".into(),
            l.signature(
                "GET",
                "files/h1",
                e - 10,
                &[("type", "x"), ("disposition", "y")],
            ),
        );
        assert_eq!(
            l.check("GET", "files/h1", &q).unwrap_err(),
            "the link has expired"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
