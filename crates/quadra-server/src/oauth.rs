//! OAuth2 with a user's consent: the *authorization code* grant, with PKCE
//! (`oauth2_grant = "authorization_code"` on an HTTP connection or a mailbox).
//!
//! 1. An admin opens `GET /v1/connections/<name>/authorize`: the server answers with a redirect
//!    to the provider's `oauth2_authorize_url`, with a `state` and a PKCE challenge. The state
//!    is sealed with the server's key: it carries the connection, the PKCE verifier, the
//!    redirect url, who asked and until when it holds (ten minutes), so nothing is kept
//!    meanwhile and no one else can forge one.
//! 2. The provider sends the admin back to `GET /oauth/<name>/callback?code=…&state=…`; the
//!    server opens the state, exchanges the code (with the verifier) at `oauth2_token_url`, and
//!    keeps the access and refresh tokens in the tenant's database (`_quadra_oauth`), sealed.
//! 3. Requests use the access token; a minute before it expires, or when the other side
//!    answers 401, it is refreshed once with the refresh token. A refreshed token is kept in
//!    this process at once and written to the database by the engine right after the request
//!    ([`unsaved`]): the next request already uses it.
//!
//! **The key.** Tokens are sealed with AES-256-GCM under `QUADRA_TOKEN_KEY`, 32 random bytes
//! in base64 (`openssl rand -base64 32`) or hex, given to the server's process: in
//! self-hosting the environment of `quadra serve`, in Quadra Cloud the platform's. Without it a
//! connection with consent waits with an error that says so; nothing is ever written in the
//! clear. The connection's name is bound to its sealed tokens (they cannot be moved to another
//! connection), and a lost key means connecting again. Tokens never appear in the log, the API
//! or the studio: what is shown is whether it is connected, the scope, the expiry, who and when.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value as J;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::transport::Failed;

/// The variable of the server's environment with the key that seals the tokens.
pub const KEY_VAR: &str = "QUADRA_TOKEN_KEY";

/// How long a state handed to the provider holds.
const STATE_SECONDS: i64 = 600;

/// The key that seals the tokens, from the server's environment.
pub fn key() -> Result<[u8; 32], String> {
    let raw = std::env::var(KEY_VAR).map_err(|_| {
        format!("{KEY_VAR} is not set where the server runs: OAuth2 with a user's consent keeps its tokens encrypted with it (32 random bytes in base64: `openssl rand -base64 32`)")
    })?;
    parse_key(&raw)
}

/// A key written as 32 bytes in base64 or hex.
fn parse_key(raw: &str) -> Result<[u8; 32], String> {
    let raw = raw.trim();
    let bytes = if raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        hex::decode(raw).ok()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(raw)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(raw))
            .ok()
    };
    bytes
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .ok_or_else(|| {
            format!("{KEY_VAR} is not 32 bytes in base64 or hex (`openssl rand -base64 32`)")
        })
}

fn aead(key: &[u8; 32]) -> aws_lc_rs::aead::LessSafeKey {
    aws_lc_rs::aead::LessSafeKey::new(
        aws_lc_rs::aead::UnboundKey::new(&aws_lc_rs::aead::AES_256_GCM, key)
            .expect("a 32-byte key"),
    )
}

fn random<const N: usize>() -> [u8; N] {
    use aws_lc_rs::rand::SecureRandom;
    let mut b = [0u8; N];
    aws_lc_rs::rand::SystemRandom::new()
        .fill(&mut b)
        .expect("random bytes");
    b
}

/// `plain` encrypted and authenticated, bound to `context`: `v1.<base64 of nonce and text>`.
pub fn seal(key: &[u8; 32], context: &str, plain: &[u8]) -> String {
    let nonce = random::<12>();
    let mut buf = plain.to_vec();
    aead(key)
        .seal_in_place_append_tag(
            aws_lc_rs::aead::Nonce::assume_unique_for_key(nonce),
            aws_lc_rs::aead::Aad::from(context.as_bytes()),
            &mut buf,
        )
        .expect("sealed");
    let mut out = nonce.to_vec();
    out.extend(buf);
    format!(
        "v1.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(out)
    )
}

/// What [`seal`] sealed with this key and context, or why it cannot be read.
pub fn open(key: &[u8; 32], context: &str, sealed: &str) -> Result<Vec<u8>, String> {
    let bad = || "it was sealed with another key, or changed".to_string();
    let raw = sealed
        .strip_prefix("v1.")
        .and_then(|b| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(b)
                .ok()
        })
        .filter(|r| r.len() > 12 + 16)
        .ok_or_else(bad)?;
    let (nonce, rest) = raw.split_at(12);
    let mut buf = rest.to_vec();
    let plain = aead(key)
        .open_in_place(
            aws_lc_rs::aead::Nonce::try_assume_unique_for_key(nonce).map_err(|_| bad())?,
            aws_lc_rs::aead::Aad::from(context.as_bytes()),
            &mut buf,
        )
        .map_err(|_| bad())?;
    Ok(plain.to_vec())
}

/// The tokens a provider gave for a connection.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// unix seconds; none: the provider did not say (a token that does not expire)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl std::fmt::Debug for Tokens {
    /// Never the tokens themselves.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "…"))
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl Tokens {
    /// Still good for a minute (or half its life, for a short one).
    fn fresh(&self, now: i64) -> bool {
        self.expires_at.is_none_or(|e| now + 60 < e)
    }

    /// The context its sealed form is bound to.
    pub fn context(connection: &str) -> String {
        format!("quadra-oauth-tokens:{connection}")
    }

    pub fn seal(&self, key: &[u8; 32], connection: &str) -> String {
        seal(
            key,
            &Tokens::context(connection),
            &serde_json::to_vec(self).expect("tokens as JSON"),
        )
    }

    pub fn open(key: &[u8; 32], connection: &str, sealed: &str) -> Result<Tokens, String> {
        let plain = open(key, &Tokens::context(connection), sealed)?;
        serde_json::from_slice(&plain).map_err(|e| format!("the tokens kept: {e}"))
    }
}

/// How a request of a connection with consent gets its token: the provider's token url and
/// the client, the tokens as the database has them, and where this process keeps the latest.
#[derive(Clone, PartialEq, Eq)]
pub struct Use {
    /// the tenant and the connection: where this process keeps its latest tokens
    pub key: String,
    pub connection: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub in_body: bool,
    /// as the database has them
    pub stored: Option<Tokens>,
    /// why there are none to use (not connected, revoked, no key)
    pub problem: Option<String>,
}

impl std::fmt::Debug for Use {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Use")
            .field("connection", &self.connection)
            .field("token_url", &self.token_url)
            .finish_non_exhaustive()
    }
}

/// The key under which a tenant's connection keeps its tokens in this process.
pub fn cache_key(tenant: &str, connection: &str) -> String {
    format!("{tenant}\u{1f}{connection}")
}

/// The latest tokens of a connection in this process, and whether the database has them yet.
#[derive(Clone, Default)]
struct Entry {
    connection: String,
    tokens: Option<Tokens>,
    error: Option<String>,
    /// bumped at each change; `dirty`: not written to the database yet
    version: u64,
    dirty: bool,
}

type Entries = Mutex<HashMap<String, Entry>>;

fn entries() -> &'static Entries {
    static E: OnceLock<Entries> = OnceLock::new();
    E.get_or_init(Default::default)
}

/// One refresh at a time per connection: a provider that rotates refresh tokens refuses the
/// old one once the new one is given.
fn refresh_lock(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    static L: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    L.get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(key.to_string())
        .or_default()
        .clone()
}

/// A consent just given (or forgotten: `None`): what this process uses from now on, already in
/// the database.
pub fn replace(key: &str, connection: &str, tokens: Option<Tokens>) {
    let mut all = entries().lock().unwrap();
    let e = all.entry(key.to_string()).or_default();
    e.connection = connection.to_string();
    e.tokens = tokens;
    e.error = None;
    e.version += 1;
    e.dirty = false;
}

/// What changed in this process and is not in the database yet, for the tenant `tenant`:
/// (connection, tokens, error, version), to write and then mark with [`saved`].
pub fn unsaved(tenant: &str) -> Vec<(String, Option<Tokens>, Option<String>, u64)> {
    let prefix = format!("{tenant}\u{1f}");
    entries()
        .lock()
        .unwrap()
        .iter()
        .filter(|(k, e)| e.dirty && k.starts_with(&prefix))
        .map(|(_, e)| {
            (
                e.connection.clone(),
                e.tokens.clone(),
                e.error.clone(),
                e.version,
            )
        })
        .collect()
}

/// The tokens of `connection` written, as of `version`.
pub fn saved(tenant: &str, connection: &str, version: u64) {
    if let Some(e) = entries()
        .lock()
        .unwrap()
        .get_mut(&cache_key(tenant, connection))
    {
        if e.version == version {
            e.dirty = false;
        }
    }
}

/// The tokens to use now: this process's latest, else the database's.
fn current(u: &Use) -> Result<Tokens, Failed> {
    let all = entries().lock().unwrap();
    match all.get(&u.key) {
        Some(e) if e.version > 0 => match (&e.tokens, &e.error) {
            (_, Some(err)) => Err(Failed::Waiting(err.clone())),
            (Some(t), None) => Ok(t.clone()),
            (None, None) => Err(Failed::Waiting(not_connected(&u.connection))),
        },
        _ => match (&u.stored, &u.problem) {
            (Some(t), None) => Ok(t.clone()),
            (_, Some(p)) => Err(Failed::Waiting(p.clone())),
            (None, None) => Err(Failed::Waiting(not_connected(&u.connection))),
        },
    }
}

/// What a connection without consent says.
pub fn not_connected(connection: &str) -> String {
    format!("`{connection}` is not connected: an admin gives the consent at /v1/connections/{connection}/authorize")
}

fn set(u: &Use, tokens: Option<Tokens>, error: Option<String>) {
    let mut all = entries().lock().unwrap();
    let e = all.entry(u.key.clone()).or_default();
    e.connection = u.connection.clone();
    if tokens.is_some() {
        e.tokens = tokens;
    }
    e.error = error;
    e.version += 1;
    e.dirty = true;
}

/// The access token for a request: the one kept while it holds, else refreshed.
pub async fn bearer(
    u: &Use,
    client: &reqwest::Client,
    public_only: bool,
    timeout: Duration,
) -> Result<String, Failed> {
    let t = current(u)?;
    if t.fresh(chrono::Utc::now().timestamp()) {
        return Ok(t.access_token);
    }
    let lock = refresh_lock(&u.key);
    let _g = lock.lock().await;
    let t = current(u)?;
    if t.fresh(chrono::Utc::now().timestamp()) {
        return Ok(t.access_token);
    }
    refresh(u, &t, client, public_only, timeout).await
}

/// The access token `refused` was refused (401): a new one, unless another request already got
/// one meanwhile.
pub async fn refused(
    u: &Use,
    refused: &str,
    client: &reqwest::Client,
    public_only: bool,
    timeout: Duration,
) -> Result<String, Failed> {
    let lock = refresh_lock(&u.key);
    let _g = lock.lock().await;
    let t = current(u)?;
    if t.access_token != refused {
        return Ok(t.access_token);
    }
    refresh(u, &t, client, public_only, timeout).await
}

async fn refresh(
    u: &Use,
    t: &Tokens,
    client: &reqwest::Client,
    public_only: bool,
    timeout: Duration,
) -> Result<String, Failed> {
    let Some(rt) = &t.refresh_token else {
        let why = format!(
            "the access token of `{}` expired and the provider gave no refresh token: an admin connects it again at /v1/connections/{}/authorize",
            u.connection, u.connection
        );
        set(u, None, Some(why.clone()));
        return Err(Failed::Waiting(why));
    };
    let client_side = Client {
        token_url: u.token_url.clone(),
        client_id: u.client_id.clone(),
        client_secret: u.client_secret.clone(),
        in_body: u.in_body,
    };
    match token_request(
        &client_side,
        &[("grant_type", "refresh_token"), ("refresh_token", rt)],
        client,
        public_only,
        timeout,
    )
    .await
    {
        Ok(mut new) => {
            // a provider that does not rotate it keeps the one it gave
            if new.refresh_token.is_none() {
                new.refresh_token = t.refresh_token.clone();
            }
            if new.scope.is_none() {
                new.scope = t.scope.clone();
            }
            let access = new.access_token.clone();
            set(u, Some(new), None);
            Ok(access)
        }
        Err(TokenError::Refused(e)) => {
            let why = format!(
                "the provider refused to refresh the token of `{}` ({e}): the consent was revoked or expired; an admin connects it again at /v1/connections/{}/authorize",
                u.connection, u.connection
            );
            set(u, None, Some(why.clone()));
            Err(Failed::Waiting(why))
        }
        Err(TokenError::Again(e)) => Err(Failed::Again(e)),
    }
}

/// The client of a provider: where tokens are asked and with what.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub in_body: bool,
}

#[derive(Debug)]
pub enum TokenError {
    /// the grant refused (`invalid_grant`, a 400 or 401): asking again will not help
    Refused(String),
    Again(String),
}

impl TokenError {
    pub fn text(&self) -> &str {
        match self {
            TokenError::Refused(e) | TokenError::Again(e) => e,
        }
    }
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A request to the token url; the tokens it gave.
async fn token_request(
    c: &Client,
    params: &[(&str, &str)],
    client: &reqwest::Client,
    public_only: bool,
    timeout: Duration,
) -> Result<Tokens, TokenError> {
    let parsed = reqwest::Url::parse(&c.token_url)
        .map_err(|e| TokenError::Refused(format!("oauth2_token_url {}: {e}", c.token_url)))?;
    let host = parsed.host_str().unwrap_or("").to_string();
    if public_only
        && host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok()
    {
        crate::deliver::addresses(&host, 0, true)
            .await
            .map_err(TokenError::Refused)?;
    }
    let mut form: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", enc(v)))
        .collect();
    let mut req = client
        .post(parsed)
        .timeout(timeout)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .header("User-Agent", "Quadra");
    if c.in_body {
        form.push(format!("client_id={}", enc(&c.client_id)));
        form.push(format!("client_secret={}", enc(&c.client_secret)));
    } else {
        // RFC 6749 2.3.1: each form-encoded, then basic authentication; the id also in the
        // form, which some providers want (it does no harm to the others)
        let b = base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            enc(&c.client_id),
            enc(&c.client_secret)
        ));
        req = req.header("Authorization", format!("Basic {b}"));
        form.push(format!("client_id={}", enc(&c.client_id)));
    }
    let resp = req
        .body(form.join("&"))
        .send()
        .await
        .map_err(|e| TokenError::Again(format!("the token of {host}: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let j: J = serde_json::from_str(&text).unwrap_or(J::Null);
    if !(200..300).contains(&status) {
        // the provider's error code and description, never the body (it may echo a token)
        let code = j["error"].as_str().unwrap_or("").to_string();
        let what = j["error_description"]
            .as_str()
            .map(|d| d.chars().take(200).collect::<String>())
            .unwrap_or_default();
        let said = match (code.is_empty(), what.is_empty()) {
            (true, _) => format!("{host} answered {status}"),
            (false, true) => format!("{host} answered {status}: {code}"),
            (false, false) => format!("{host} answered {status}: {code}: {what}"),
        };
        return Err(
            if status == 400 || status == 401 || code == "invalid_grant" {
                TokenError::Refused(said)
            } else {
                TokenError::Again(said)
            },
        );
    }
    let access = j["access_token"]
        .as_str()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| TokenError::Again(format!("{host} answered with no access_token")))?;
    let lasts = j["expires_in"]
        .as_i64()
        .or_else(|| j["expires_in"].as_str().and_then(|s| s.parse().ok()));
    Ok(Tokens {
        access_token: access.to_string(),
        refresh_token: j["refresh_token"].as_str().map(str::to_string),
        expires_at: lasts.map(|s| chrono::Utc::now().timestamp() + s.max(0)),
        scope: j["scope"].as_str().map(str::to_string),
    })
}

/// What an authorization carries through the provider, sealed: the connection, the PKCE
/// verifier, the redirect url, who asked, until when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub connection: String,
    pub verifier: String,
    pub redirect_uri: String,
    pub by: String,
    pub until: i64,
}

const STATE_CONTEXT: &str = "quadra-oauth-state";

impl State {
    pub fn new(connection: &str, redirect_uri: &str, by: &str) -> State {
        State {
            connection: connection.to_string(),
            verifier: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random::<32>()),
            redirect_uri: redirect_uri.to_string(),
            by: by.to_string(),
            until: chrono::Utc::now().timestamp() + STATE_SECONDS,
        }
    }

    pub fn seal(&self, key: &[u8; 32]) -> String {
        seal(
            key,
            STATE_CONTEXT,
            &serde_json::to_vec(self).expect("state as JSON"),
        )
    }

    /// The state of the callback of `connection`, if it is one this server made and it holds.
    pub fn open(key: &[u8; 32], connection: &str, sealed: &str) -> Result<State, String> {
        let s: State = open(key, STATE_CONTEXT, sealed)
            .ok()
            .and_then(|p| serde_json::from_slice(&p).ok())
            .ok_or("the state is not one this server gave: start again from /v1/connections/<name>/authorize")?;
        if s.connection != connection {
            return Err("the state is for another connection".into());
        }
        if chrono::Utc::now().timestamp() > s.until {
            return Err(format!(
                "the authorization took more than ten minutes: start again from /v1/connections/{connection}/authorize"
            ));
        }
        Ok(s)
    }

    /// The PKCE challenge of its verifier (S256).
    pub fn challenge(&self) -> String {
        use sha2::Digest;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(self.verifier.as_bytes()))
    }
}

/// The provider's page that asks for the consent: `authorize_url` (with its own parameters, if
/// any) and the request's.
pub fn authorize_url(
    authorize_url: &str,
    client_id: &str,
    scope: Option<&str>,
    state: &State,
    sealed_state: &str,
) -> String {
    let mut params = vec![
        ("response_type", "code".to_string()),
        ("client_id", client_id.to_string()),
        ("redirect_uri", state.redirect_uri.clone()),
    ];
    if let Some(s) = scope {
        params.push(("scope", s.to_string()));
    }
    params.push(("state", sealed_state.to_string()));
    params.push(("code_challenge", state.challenge()));
    params.push(("code_challenge_method", "S256".to_string()));
    let q: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", enc(v)))
        .collect();
    let sep = if authorize_url.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{authorize_url}{sep}{}", q.join("&"))
}

/// The tokens for the code the provider sent back.
pub async fn exchange(
    c: &Client,
    code: &str,
    state: &State,
    client: &reqwest::Client,
    public_only: bool,
) -> Result<Tokens, String> {
    token_request(
        c,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &state.redirect_uri),
            ("code_verifier", &state.verifier),
        ],
        client,
        public_only,
        Duration::from_secs(20),
    )
    .await
    .map_err(|e| e.text().to_string())
}

/// A step of the callback, for the engine that holds the connection's settings and the database.
#[derive(Debug)]
pub enum Step {
    /// the state of the callback of `connection`: opened, and what the exchange needs
    Open { connection: String, state: String },
    /// the tokens got for `connection`: kept
    Keep {
        connection: String,
        tokens: Tokens,
        by: String,
    },
}

/// What the engine answers to a [`Step`].
#[derive(Debug)]
pub enum Next {
    Exchange { client: Client, state: State },
    Kept,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_tokens_open_only_with_their_key_and_connection() {
        let k = [7u8; 32];
        let t = Tokens {
            access_token: "at-1".into(),
            refresh_token: Some("rt-1".into()),
            expires_at: Some(100),
            scope: Some("orders".into()),
        };
        let s = t.seal(&k, "shop");
        assert!(s.starts_with("v1.") && !s.contains("at-1"));
        assert_eq!(Tokens::open(&k, "shop", &s).unwrap(), t);
        assert!(Tokens::open(&k, "erp", &s).is_err());
        assert!(Tokens::open(&[8u8; 32], "shop", &s).is_err());
        // never in a log
        let d = format!("{t:?}");
        assert!(!d.contains("at-1") && !d.contains("rt-1"), "{d}");
    }

    #[test]
    fn a_state_holds_ten_minutes_for_its_connection_and_carries_the_pkce_verifier() {
        let k = [1u8; 32];
        let s = State::new(
            "shop",
            "https://app.example.com/oauth/shop/callback",
            "user:1",
        );
        let sealed = s.seal(&k);
        assert_eq!(State::open(&k, "shop", &sealed).unwrap(), s);
        assert!(State::open(&k, "erp", &sealed).is_err());
        assert!(State::open(&k, "shop", "v1.AAAA").is_err());
        let mut old = s.clone();
        old.until -= 3600;
        assert!(State::open(&k, "shop", &old.seal(&k))
            .unwrap_err()
            .contains("ten minutes"));
        // RFC 7636: BASE64URL(SHA256(verifier))
        let mut v = s.clone();
        v.verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into();
        assert_eq!(v.challenge(), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let url = authorize_url(
            "https://accounts.google.com/o/oauth2/v2/auth?access_type=offline",
            "id 1",
            Some("https://mail.google.com/"),
            &v,
            "v1.x",
        );
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?access_type=offline&response_type=code&client_id=id%201&redirect_uri=https%3A%2F%2Fapp.example.com%2Foauth%2Fshop%2Fcallback&scope=https%3A%2F%2Fmail.google.com%2F&state=v1.x&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256"), "{url}");
    }

    #[test]
    fn the_key_is_32_bytes_in_base64_or_hex() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        assert_eq!(parse_key(&b64).unwrap(), [9u8; 32]);
        assert_eq!(parse_key(&"0a".repeat(32)).unwrap(), [10u8; 32]);
        assert!(parse_key("short").unwrap_err().contains("32 bytes"));
    }
}

/// The HTTP side: the redirect to the provider and its callback.
pub(crate) mod http {
    use super::{exchange, Next, Step};
    use crate::engine::{Job, Reply, Req};
    use crate::{ask_call, authenticate, fail, json_response, Shared, Ten};
    use axum::extract::{Path, Query, State};
    use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
    use axum::response::{IntoResponse, Response};
    use serde_json::Value as J;
    use std::collections::HashMap;

    /// Where the provider sends the admin back: this server's address as the request reached
    /// it (`X-Forwarded-Proto` behind a proxy that terminates TLS).
    fn redirect_uri(app: &crate::App, headers: &HeaderMap, connection: &str) -> String {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("localhost");
        let scheme = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .filter(|p| *p == "https" || *p == "http")
            .unwrap_or(if app.secure_cookies { "https" } else { "http" });
        format!("{scheme}://{host}/oauth/{connection}/callback")
    }

    /// `GET /v1/connections/{name}/authorize`: an admin is sent to the provider's page.
    pub(crate) async fn authorize(
        State(app): State<Shared>,
        Ten(t): Ten,
        method: Method,
        headers: HeaderMap,
        Path(name): Path<String>,
    ) -> Response {
        let who = match authenticate(&app, &t, &method, &headers).await {
            Ok(w) => w,
            Err(r) => return r,
        };
        let mut query = HashMap::new();
        query.insert(
            "redirect_uri".to_string(),
            redirect_uri(&app, &headers, &name),
        );
        let req = Req::Outbox {
            path: format!("/v1/connections/{name}/authorize"),
            query,
            body: J::Null,
        };
        match ask_call(&t, who, req).await {
            Ok(Some(Reply {
                status: 302, body, ..
            })) => {
                let to = body["location"].as_str().unwrap_or("").to_string();
                let mut r = json_response(200, serde_json::json!({ "authorize_url": to }));
                *r.status_mut() = StatusCode::FOUND;
                if let Ok(v) = HeaderValue::from_str(&to) {
                    r.headers_mut().insert(header::LOCATION, v);
                }
                r
            }
            Ok(Some(Reply { status, body, .. })) => json_response(status, body),
            Ok(None) => fail(503, "the engine is not available"),
            Err(r) => r,
        }
    }

    fn page(status: u16, title: &str, text: &str) -> Response {
        let esc = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        };
        let html = format!(
            "<!doctype html><meta charset=\"utf-8\"><title>{}</title><body style=\"font-family:system-ui,sans-serif;max-width:40em;margin:4em auto;padding:0 1em\"><h1>{}</h1><p>{}</p><p><a href=\"/\">Back to the app</a></p></body>",
            esc(title),
            esc(title),
            esc(text)
        );
        let mut r = (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            html,
        )
            .into_response();
        let h = r.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        r
    }

    /// `GET /oauth/{connection}/callback?code=…&state=…`: the provider sends the admin back;
    /// the code is exchanged and the tokens kept.
    pub(crate) async fn callback(
        State(app): State<Shared>,
        Ten(t): Ten,
        Path(connection): Path<String>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Response {
        if let Some(e) = q.get("error") {
            let what = q
                .get("error_description")
                .map(|d| format!(": {d}"))
                .unwrap_or_default();
            return page(
                400,
                "Not connected",
                &format!("The provider did not give the consent for `{connection}` ({e}{what})."),
            );
        }
        let (Some(code), Some(state)) = (q.get("code"), q.get("state")) else {
            return page(
                400,
                "Not connected",
                "The provider sent no code or no state back.",
            );
        };
        let opened = t
            .ask(|reply| Job::Consent {
                step: Step::Open {
                    connection: connection.clone(),
                    state: state.clone(),
                },
                reply,
            })
            .await;
        let (client, state) = match opened {
            Some(Ok(Next::Exchange { client, state })) => (client, state),
            Some(Err(e)) => return page(400, "Not connected", &e),
            _ => {
                return page(
                    503,
                    "Not connected",
                    "The engine is not available: try again.",
                )
            }
        };
        let tokens = match exchange(
            &client,
            code,
            &state,
            &app.egress.client,
            app.egress.public_only,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                return page(
                    502,
                    "Not connected",
                    &format!("The provider did not give the tokens for `{connection}`: {e}"),
                )
            }
        };
        let kept = t
            .ask(|reply| Job::Consent {
                step: Step::Keep {
                    connection: connection.clone(),
                    tokens,
                    by: state.by.clone(),
                },
                reply,
            })
            .await;
        match kept {
            Some(Ok(_)) => page(
                200,
                "Connected",
                &format!("`{connection}` is connected: the app uses it from now on. You can close this page."),
            ),
            Some(Err(e)) => page(500, "Not connected", &e),
            None => page(503, "Not connected", "The engine is not available: try again."),
        }
    }
}
