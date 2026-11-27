//! Where the bytes of files are kept: an S3 compatible bucket (AWS, Hetzner, R2, MinIO) or a
//! folder of the machine (`.quadra/files` in the project by default). Never the database.
//!
//! A file is kept under `files/<sha256>`: the same bytes uploaded twice are kept once, and an
//! object never changes, so a cache may keep it for ever. An upload goes to `uploads/<id>`
//! first, straight from the browser with a signed URL; the server reads it, checks it and
//! moves it under its hash.
//!
//! Both kinds give signed URLs: the bucket its own (S3 presigned URLs), the folder URLs the
//! server answers itself (`/v1/blobs/<key>?expires=…&sig=…`, an HMAC of what they allow). With
//! a CDN in front of the bucket (`[files] cdn`), downloads go through it, with a token signed
//! with its secret.

pub mod detect;
mod local;
mod s3;

pub use local::{Grant, Local};

use hmac::{Hmac, Mac};
use quadra_sema::manifest::{Manifest, Setting};
use sha2::Sha256;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub type Result<T> = std::result::Result<T, String>;

/// How long an upload URL is valid.
pub const UPLOAD_FOR: Duration = Duration::from_secs(15 * 60);
/// How long a download URL is valid.
pub const DOWNLOAD_FOR: Duration = Duration::from_secs(5 * 60);

/// The key of a file kept: `files/<sha256>`.
pub fn file_key(hash: &str) -> String {
    format!("files/{hash}")
}

/// The key of an upload: `uploads/<id>`.
pub fn upload_key(id: &str) -> String {
    format!("uploads/{id}")
}

/// A request a client may make without credentials: its method, URL and the headers to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// A store of objects by key. The calls block: in an async task, call them through
/// `spawn_blocking`.
pub trait Blobs: Send + Sync {
    /// Keeps the bytes under `key`, with the type and disposition a download is given.
    fn put(&self, key: &str, bytes: &[u8], ctype: &str, disposition: &str) -> Result<()>;
    /// The bytes under `key`, if there are any.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    /// The size of the object under `key`, if there is one.
    fn head(&self, key: &str) -> Result<Option<u64>>;
    /// Removes the object (nothing if there is none).
    fn delete(&self, key: &str) -> Result<()>;
    /// Copies an object to another key.
    fn copy(&self, from: &str, to: &str) -> Result<()>;
    /// A URL to write the object, of exactly `size` bytes, for `valid` from now.
    fn put_url(&self, key: &str, size: u64, valid: Duration) -> Result<Signed>;
    /// A URL to read the object for `valid` from now, answered with this type and
    /// disposition.
    fn get_url(&self, key: &str, valid: Duration, ctype: &str, disposition: &str)
        -> Result<String>;
    /// The folder, when the objects are in one (its URLs are answered by the server).
    fn local(&self) -> Option<&Local> {
        None
    }
    /// The path of a key from the root of the bucket (with its prefix): what a CDN in front of
    /// it serves.
    fn path_of(&self, key: &str) -> String {
        key.to_string()
    }
}

/// A CDN in front of the bucket: downloads go to `<base>/<path>?expires=<unix>&token=<hex>`,
/// the token an HMAC-SHA256, with the secret, of `<path><expires>` (`/files/ab…1700000000`).
#[derive(Clone)]
pub struct Cdn {
    pub base: String,
    pub secret: Vec<u8>,
}

impl Cdn {
    pub fn url(&self, path: &str, expires: u64) -> String {
        let path = format!("/{}", path.trim_start_matches('/'));
        let token = hmac_hex(&self.secret, format!("{path}{expires}").as_bytes());
        format!(
            "{}{path}?expires={expires}&token={token}",
            self.base.trim_end_matches('/')
        )
    }
}

/// Where a project keeps its files: the store, and the CDN downloads go through.
pub struct Files {
    pub blobs: Box<dyn Blobs>,
    pub cdn: Option<Cdn>,
}

impl Files {
    /// The URL a download of a file kept is sent to: through the CDN when there is one (the
    /// object has its type and disposition already), else signed by the store with the
    /// disposition asked for.
    pub fn download_url(
        &self,
        hash: &str,
        ctype: &str,
        name: &str,
        download: bool,
    ) -> Result<String> {
        let key = file_key(hash);
        let inline = detect::inline(ctype) && !download;
        if let (Some(cdn), false) = (&self.cdn, download) {
            return Ok(cdn.url(
                &self.blobs.path_of(&key),
                unix_now() + DOWNLOAD_FOR.as_secs(),
            ));
        }
        self.blobs
            .get_url(&key, DOWNLOAD_FOR, ctype, &disposition(inline, name))
    }

    /// Keeps a file's bytes under its hash (once: the same bytes are already there).
    pub fn keep(&self, hash: &str, bytes: &[u8], ctype: &str, name: &str) -> Result<()> {
        let key = file_key(hash);
        if self.blobs.head(&key)?.is_some() {
            return Ok(());
        }
        self.blobs.put(
            &key,
            bytes,
            ctype,
            &disposition(detect::inline(ctype), name),
        )
    }
}

/// A `Content-Disposition`: `inline` or `attachment`, with the name (ASCII, and in UTF-8).
pub fn disposition(inline: bool, name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || " .-_()".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let utf8: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b".-_".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!(
        "{}; filename=\"{}\"; filename*=UTF-8''{utf8}",
        if inline { "inline" } else { "attachment" },
        if ascii.is_empty() { "file" } else { &ascii }
    )
}

/// The hex of the SHA-256 of bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(Sha256::digest(bytes))
}

pub(crate) fn hmac_hex(key: &[u8], msg: &[u8]) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    m.update(msg);
    hex::encode(m.finalize().into_bytes())
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Text as it goes in a query: letters, digits and `-._~` as they are, the rest `%XX`.
pub(crate) fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The stores opened, by what they were opened with: every engine of a project shares one.
fn opened() -> &'static Mutex<HashMap<String, Arc<Files>>> {
    static OPENED: OnceLock<Mutex<HashMap<String, Arc<Files>>>> = OnceLock::new();
    OPENED.get_or_init(Default::default)
}

/// The files of the project at `root`, as its `quadra.toml` says (`[files]`, the values of its
/// variables in `values`): its bucket, or `.quadra/files` in the project.
pub fn open(root: &Path, m: &Manifest, values: &BTreeMap<String, String>) -> Result<Arc<Files>> {
    let value = |s: &Setting| -> Result<String> {
        match s {
            Setting::Literal(t) => Ok(t.clone()),
            Setting::Var(v) => m
                .value(values, v)
                .map(str::to_string)
                .ok_or(format!("[files]: the variable {v} has no value here")),
        }
    };
    let Some(d) = &m.files else {
        return open_folder(&root.join(".quadra").join("files"), None);
    };
    let cdn = match (&d.cdn, &d.cdn_secret) {
        (Some(base), Some(secret)) => Some(Cdn {
            base: value(base)?,
            secret: value(&Setting::Var(secret.clone()))?.into_bytes(),
        }),
        _ => None,
    };
    let url = value(&d.url)?;
    if let Some(folder) = url.strip_prefix("file:") {
        let folder = Path::new(folder);
        return open_folder(&root.join(folder), cdn);
    }
    let Some(rest) = url.strip_prefix("s3://") else {
        return Err(format!(
            "[files]: url `{url}` is s3://bucket/prefix or file:folder"
        ));
    };
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    let spec = s3::Spec {
        bucket: bucket.to_string(),
        prefix: prefix.trim_matches('/').to_string(),
        endpoint: d.endpoint.as_ref().map(&value).transpose()?,
        region: d.region.as_ref().map(&value).transpose()?,
        key: value(&Setting::Var(d.key.clone().unwrap_or_default()))?,
        secret: value(&Setting::Var(d.secret.clone().unwrap_or_default()))?,
    };
    let id = format!(
        "s3 {} {} {:?} {:?} {} {} {:?}",
        spec.bucket,
        spec.prefix,
        spec.endpoint,
        spec.region,
        spec.key,
        sha256_hex(spec.secret.as_bytes()),
        cdn.as_ref()
            .map(|c| (c.base.clone(), sha256_hex(&c.secret)))
    );
    let mut all = opened().lock().map_err(|_| "the stores of files")?;
    if let Some(f) = all.get(&id) {
        return Ok(f.clone());
    }
    let f = Arc::new(Files {
        blobs: Box::new(s3::S3::new(spec)?),
        cdn,
    });
    all.insert(id, f.clone());
    Ok(f)
}

fn open_folder(dir: &Path, cdn: Option<Cdn>) -> Result<Arc<Files>> {
    let id = format!("folder {}", dir.display());
    let mut all = opened().lock().map_err(|_| "the stores of files")?;
    if let Some(f) = all.get(&id) {
        return Ok(f.clone());
    }
    let f = Arc::new(Files {
        blobs: Box::new(Local::open(dir)?),
        cdn,
    });
    all.insert(id, f.clone());
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispositions_and_cdn_tokens() {
        assert_eq!(
            disposition(true, "scan.pdf"),
            "inline; filename=\"scan.pdf\"; filename*=UTF-8''scan.pdf"
        );
        assert_eq!(
            disposition(false, "fattura è.pdf"),
            "attachment; filename=\"fattura _.pdf\"; filename*=UTF-8''fattura%20%C3%A8.pdf"
        );
        let cdn = Cdn {
            base: "https://cdn.example.com/".into(),
            secret: b"k".to_vec(),
        };
        let u = cdn.url("p/files/ab", 1700000000);
        assert_eq!(
            u,
            format!(
                "https://cdn.example.com/p/files/ab?expires=1700000000&token={}",
                hmac_hex(b"k", b"/p/files/ab1700000000")
            )
        );
    }

    #[test]
    fn a_project_without_files_settings_keeps_them_in_its_folder() {
        let root = std::env::temp_dir().join(format!("quadra-blobs-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let f = open(&root, &Manifest::default(), &BTreeMap::new()).unwrap();
        assert!(f.blobs.local().is_some());
        assert!(root.join(".quadra/files").is_dir());
        // the same project: the same store
        let g = open(&root, &Manifest::default(), &BTreeMap::new()).unwrap();
        assert!(Arc::ptr_eq(&f, &g));
        let _ = std::fs::remove_dir_all(&root);
    }
}
