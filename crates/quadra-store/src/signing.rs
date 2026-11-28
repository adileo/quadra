//! Signed commits, optional and off by default.
//!
//! The hash chain shows that the past was not altered, but whoever controls the database could
//! compute it again. With a signing key, every commit also carries an Ed25519 signature of its
//! hash: without the key, a chain computed again has no valid signatures.
//!
//! The private key is never in the database. A process *loads* the keys it holds (`load`: from
//! `QUADRA_SIGNING_KEY`, a file, or the platform), and each database says which public key
//! signs its commits (`_quadra_meta` `signing_key`): a commit is signed with the loaded key of
//! that public key, and refused when the process does not hold it. One process can serve many
//! databases, each with its own key.
//!
//! The keys of a database are in `_quadra_keys` (transaction, public key), each written by a
//! commit of its own, `["k", public]` in its hash: the first is signed by the new key itself,
//! every later one by the key before it. So `quadra db verify` follows the keys along the chain,
//! and the commits before a rotation stay verifiable with the old key.

use crate::conn::{Conn, Sql};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// The prefix of a private key as Quadra writes it: `ed25519:` + 64 hex digits (its seed).
const PREFIX: &str = "ed25519:";

fn keys() -> &'static RwLock<HashMap<String, SigningKey>> {
    static KEYS: OnceLock<RwLock<HashMap<String, SigningKey>>> = OnceLock::new();
    KEYS.get_or_init(Default::default)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A new private key, as text (`ed25519:…`), and its public key (64 hex digits).
pub fn generate() -> (String, String) {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).expect("the system's random numbers");
    let k = SigningKey::from_bytes(&seed);
    (
        format!("{PREFIX}{}", hex(&seed)),
        hex(k.verifying_key().as_bytes()),
    )
}

fn parse(secret: &str) -> Result<SigningKey, String> {
    let s = secret.trim();
    let body = s.strip_prefix(PREFIX).unwrap_or(s);
    let seed: [u8; 32] = unhex(body)
        .and_then(|b| b.try_into().ok())
        .ok_or("not a signing key: expected `ed25519:` and 64 hex digits (`quadra db key init`)")?;
    Ok(SigningKey::from_bytes(&seed))
}

/// The public key of a private key.
pub fn public_of(secret: &str) -> Result<String, String> {
    Ok(hex(parse(secret)?.verifying_key().as_bytes()))
}

/// Holds a private key in this process: the databases whose commits it signs sign with it.
/// Returns its public key.
pub fn load(secret: &str) -> Result<String, String> {
    let k = parse(secret)?;
    let public = hex(k.verifying_key().as_bytes());
    keys().write().unwrap().insert(public.clone(), k);
    Ok(public)
}

/// The key of `QUADRA_SIGNING_KEY`, or of the file `QUADRA_SIGNING_KEY_FILE`, or of
/// `<root>/.quadra/signing.key`, loaded; its public key. None when there is none.
pub fn load_configured(root: Option<&std::path::Path>) -> Result<Option<String>, String> {
    if let Ok(k) = std::env::var("QUADRA_SIGNING_KEY") {
        if !k.trim().is_empty() {
            return load(&k)
                .map(Some)
                .map_err(|e| format!("QUADRA_SIGNING_KEY: {e}"));
        }
    }
    let file = std::env::var_os("QUADRA_SIGNING_KEY_FILE")
        .map(std::path::PathBuf::from)
        .or_else(|| root.map(|r| r.join(".quadra").join("signing.key")));
    match file {
        Some(f) if f.exists() => {
            let text = std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?;
            load(&text)
                .map(Some)
                .map_err(|e| format!("{}: {e}", f.display()))
        }
        _ => Ok(None),
    }
}

thread_local! {
    static EXPECTED: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The public key the chains verified from now on, on this thread, must end with (`quadra db
/// verify --key`, or the key loaded): a chain without it, or signed by another key, is a
/// problem. Per thread: a process that verifies the databases of several projects (a host's
/// tenants, the tests) does not expect one project's key of another.
pub fn expect(public: Option<String>) {
    EXPECTED.with(|e| *e.borrow_mut() = public);
}

pub(crate) fn expected() -> Option<String> {
    EXPECTED.with(|e| e.borrow().clone())
}

/// The public key that signs the commits of the database, if any.
pub fn active(conn: &mut Conn) -> Result<Option<String>, String> {
    let r = conn.query(
        "SELECT \"value\" FROM \"_quadra_meta\" WHERE \"key\" = 'signing_key'",
        &[],
    )?;
    Ok(r.into_iter().next().and_then(|r| r[0].clone()))
}

/// The signature of a commit's hash with the loaded key of `public` (128 hex digits).
pub fn sign(public: &str, hash: &str) -> Result<String, String> {
    let keys = keys().read().unwrap();
    let k = keys.get(public).ok_or_else(|| {
        format!(
            "this database signs its commits with the key {}…, which this process does not \
             hold: set QUADRA_SIGNING_KEY (or QUADRA_SIGNING_KEY_FILE) to its private key",
            &public[..12.min(public.len())]
        )
    })?;
    Ok(hex(&k.sign(hash.as_bytes()).to_bytes()))
}

/// The signature of a commit by the database's key, or none when it signs nothing.
pub(crate) fn sign_commit(public: &Option<String>, hash: &str) -> Result<Sql, String> {
    match public {
        Some(p) => sign(p, hash).map(Sql::Text),
        None => Ok(Sql::Null),
    }
}

/// Whether `signature` is the signature of `hash` by `public`.
pub fn valid(public: &str, hash: &str, signature: &str) -> bool {
    let Some(pk) = unhex(public).and_then(|b| <[u8; 32]>::try_from(b).ok()) else {
        return false;
    };
    let Some(sig) = unhex(signature).and_then(|b| <[u8; 64]>::try_from(b).ok()) else {
        return false;
    };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else {
        return false;
    };
    vk.verify(hash.as_bytes(), &Signature::from_bytes(&sig))
        .is_ok()
}

/// Follows the keys along the chain: fed each commit in order, says what is wrong with its
/// signature.
pub(crate) struct Keys {
    /// the keys of the database by the transaction that set them
    by_tx: HashMap<i64, String>,
    /// the key signing the commits from here on
    pub(crate) current: Option<String>,
    /// how many commits were signed
    pub(crate) signed: u64,
}

impl Keys {
    pub(crate) fn read(conn: &mut Conn) -> Result<Keys, String> {
        let mut by_tx = HashMap::new();
        for r in conn.query(
            "SELECT CAST(\"tx\" AS TEXT), \"public\" FROM \"_quadra_keys\"",
            &[],
        )? {
            let tx = r[0].as_deref().and_then(|t| t.parse().ok()).unwrap_or(0);
            by_tx.insert(tx, r[1].clone().unwrap_or_default());
        }
        Ok(Keys {
            by_tx,
            current: None,
            signed: 0,
        })
    }

    /// The `["k", public]` entry of commit `tx`'s hash, when it set a key.
    pub(crate) fn entry(&self, tx: i64) -> Option<String> {
        self.by_tx
            .get(&tx)
            .map(|k| serde_json::json!(["k", k]).to_string())
    }

    /// Commit `tx` with `hash` and `signature`: the problem with its signature, if any.
    pub(crate) fn check(&mut self, tx: i64, hash: &str, signature: Option<&str>) -> Option<String> {
        let new = self.by_tx.get(&tx).cloned();
        // a new key is signed by the one before it, the first by itself
        let by = self.current.clone().or_else(|| new.clone());
        let problem = match (&by, signature) {
            (None, None) => None,
            (None, Some(_)) => Some(format!(
                "commit {tx}: it has a signature, but no key signs the chain yet"
            )),
            (Some(_), None) => Some(format!("commit {tx}: not signed")),
            (Some(k), Some(s)) if valid(k, hash, s) => {
                self.signed += 1;
                None
            }
            (Some(_), Some(_)) => Some(format!("commit {tx}: signature does not match the key")),
        };
        if new.is_some() {
            self.current = new;
        }
        problem
    }

    /// At the end of the chain: the problem when it is not signed by `expected`.
    pub(crate) fn last(&self, expected: Option<&str>) -> Option<String> {
        let e = expected?;
        match &self.current {
            Some(c) if c == e => None,
            Some(c) => Some(format!(
                "the chain is signed by the key {}…, not by the expected key {}…",
                &c[..12],
                &e[..12.min(e.len())]
            )),
            None => Some(format!(
                "the chain is not signed: the expected key is {}…",
                &e[..12.min(e.len())]
            )),
        }
    }
}
