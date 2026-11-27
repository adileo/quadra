//! The delivery of the events with an `outbound` (`outbound OrderShipped via shop`).
//!
//! The engine, which holds the model and the values of the variables, turns each due event of
//! the outbox into a [`Plan`]: where it goes and what is sent. The host runs the plan (HTTP or
//! SMTP, the only I/O), then the engine writes what came of it through the sequencer
//! (`quadra_store::deliveries`). Code never takes part: what an event carries was decided when
//! it was emitted, in a transaction like any other.
//!
//! - **HTTP**: `POST` of `{"id", "event", "tx", "created_at", "data"}` as JSON to the
//!   connection's url and the outbound's path, with `Quadra-Event`, `Quadra-Delivery` and
//!   `Idempotency-Key` headers, the connection's authentication and, with a `signing_secret`,
//!   `Quadra-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<body>">`. A 2xx is
//!   delivered; a 408, a 429 or a 5xx, a timeout or no connection is tried again; any other
//!   answer (a 4xx, a redirect: they are not followed) fails for good.
//! - **SMTP**: an email from the connection's `from` to the event's `to`, with its `subject`
//!   and `body` as plain text. A 4xx or no connection is tried again; a 5xx fails for good.
//!
//! An event is tried again after 30 s, then twice as long each time up to an hour, for a day;
//! then it fails for good, its error kept, and `quadra outbox retry` makes it pending again.
//! Delivery is at least once: a receiver sees the same `Idempotency-Key` again if a node stops
//! between sending and writing the outcome.
//!
//! With `public_only` (Quadra Cloud), connections only reach public addresses: a name that
//! resolves to a private, loopback, link-local or metadata address (`169.254.169.254`) is
//! refused, checked after the resolution, and the address checked is the one connected to.

use quadra_sema::manifest::{ConnectionKind, HttpAuth, Manifest, Setting, Signature};
use quadra_store::deliveries::{Attempt, Due, Outcome};
use serde_json::{json, Value as J};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// What to send for one event.
#[derive(Clone, Debug)]
pub struct Plan {
    pub id: u64,
    pub event: String,
    /// the connection it goes through
    pub connection: String,
    /// attempts made before this one
    pub attempts: u64,
    pub created_at: String,
    /// its fields, as the outbox keeps them (for the reactions to its outcome)
    pub fields: J,
    /// how long and how often it is tried (its outbound's `retry` and `backoff`)
    pub retry: Retry,
    /// what its connection's other side accepts (`rate`, `concurrency`, `pause_after`)
    pub limits: quadra_sema::manifest::Limits,
    /// a batch: the other events sent with this one (their ids, fields and attempts made),
    /// each given the same outcome
    pub batch: Vec<(u64, J, u64)>,
    pub send: Send,
}

/// How an event is tried again: for how long or how many times, and how long it waits
/// (doubled from `first_s` up to `max_s`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    pub for_s: u64,
    pub times: Option<u64>,
    pub first_s: u64,
    pub max_s: u64,
}

impl Default for Retry {
    /// For a day, waiting 30 s, doubled up to an hour.
    fn default() -> Self {
        Retry {
            for_s: 86_400,
            times: None,
            first_s: 30,
            max_s: 3600,
        }
    }
}

impl Plan {
    /// The idempotency key of its request: `key` of its outbound instead of the event's own.
    pub fn set_key(&mut self, key: &str) {
        if let Send::Http { headers, .. } = &mut self.send {
            for h in headers.iter_mut() {
                if h.0 == "Idempotency-Key" {
                    h.1 = key.to_string();
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum Send {
    Http {
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: String,
        /// what proves who sends it beyond its headers: a token, a certificate, a signature
        creds: Credentials,
        timeout: Duration,
    },
    Smtp {
        url: String,
        login: Option<(String, String)>,
        from: String,
        to: String,
        subject: String,
        body: String,
        /// an HTML version, sent with the text one (`email(…, html=…)`)
        html: Option<String>,
        /// `smtp://` without TLS (`insecure = true`: a local test server)
        plain: bool,
    },
    /// it cannot be sent as configured (a value missing where it runs): tried again later,
    /// once the configuration is fixed and deployed
    Broken(String),
    /// a file, a row: run by the server, or by the agent `agent` with its own values
    Job {
        job: crate::transport::Job,
        agent: Option<String>,
        /// the values here, for a job the server runs
        env: BTreeMap<String, String>,
    },
}

impl Send {
    /// The bytes it sends: its body (an email's, with its HTML).
    pub fn bytes(&self) -> u64 {
        match self {
            Send::Http { body, .. } => body.len() as u64,
            Send::Smtp { body, html, .. } => {
                (body.len() + html.as_ref().map_or(0, |h| h.len())) as u64
            }
            Send::Broken(_) => 0,
            Send::Job {
                job: crate::transport::Job::Put { put, .. },
                ..
            } => match put {
                crate::transport::Put::File { body, .. } => body.len() as u64,
                crate::transport::Put::Row { row, .. } => row.to_string().len() as u64,
                crate::transport::Put::Http { body, .. } => body.len() as u64,
            },
            Send::Job { .. } => 0,
        }
    }
}

/// A value of the outbox (the encoding of the history, which marks each kind of value) as plain
/// JSON for a receiver: decimals, percentages, dates and instants as text (exact, an instant
/// in UTC with a `Z`), an amount as `{"amount", "currency"}`, a quantity as
/// `{"quantity", "unit"}`, a record as `{"entity", "id"}`, lists and sets as arrays, an enum
/// value as its name.
pub fn plain(j: &J) -> J {
    let J::Object(o) = j else {
        return j.clone();
    };
    let s = |k: &str| o.get(k).cloned().unwrap_or(J::Null);
    let pairs = |k: &str| -> serde_json::Map<String, J> {
        o.get(k)
            .and_then(|f| f.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| Some((p.get(0)?.as_str()?.to_string(), plain(p.get(1)?))))
                    .collect()
            })
            .unwrap_or_default()
    };
    let has = |k: &str| o.contains_key(k);
    if o.len() == 1 {
        for k in ["d", "pct", "date", "ym", "role", "table"] {
            if has(k) {
                return s(k);
            }
        }
        if let Some(J::String(dt)) = o.get("dt") {
            return J::String(format!("{dt}Z"));
        }
        if has("dur") {
            return s("dur");
        }
        if let Some(J::Array(a)) = o.get("range") {
            return json!({ "from": a.first(), "to": a.get(1) });
        }
        for k in ["list", "set"] {
            if let Some(J::Array(a)) = o.get(k) {
                return J::Array(a.iter().map(plain).collect());
            }
        }
        if has("tuple") {
            return J::Object(pairs("tuple"));
        }
    }
    if has("money") && has("cur") {
        return json!({ "amount": s("money"), "currency": s("cur") });
    }
    if has("price") && has("cur") {
        return json!({ "amount": s("price"), "currency": s("cur"), "per": s("per") });
    }
    if has("qty") && has("unit") {
        return json!({ "quantity": s("qty"), "unit": s("unit") });
    }
    if has("ref") && has("ent") {
        return json!({ "entity": s("ent"), "id": s("ref") });
    }
    if has("rec") && has("fields") {
        return J::Object(pairs("fields"));
    }
    if has("event") && has("fields") {
        return J::Object(pairs("fields"));
    }
    if has("variant") {
        let fields = o
            .get("fields")
            .and_then(|f| f.as_array())
            .filter(|a| !a.is_empty());
        return match fields {
            None => s("variant"),
            Some(f) => {
                json!({ "variant": s("variant"), "fields": f.iter().map(plain).collect::<Vec<_>>() })
            }
        };
    }
    // a record's fields as the outbox keeps them: name → value
    J::Object(o.iter().map(|(k, v)| (k.clone(), plain(v))).collect())
}

/// The fields of an event (name → value, as the outbox keeps them) as plain JSON, each value
/// by [`plain`]: the names are the event's own, never read as the marks of a kind of value.
pub fn plain_fields(fields: &J) -> J {
    match fields {
        J::Object(o) => J::Object(o.iter().map(|(k, v)| (k.clone(), plain(v))).collect()),
        other => plain(other),
    }
}

/// `quadra-<commit>-<id>`: the same on every attempt of an event, and different for an event
/// that reuses the number of another after the database was restored to an earlier point (its
/// commit is not the same).
pub fn idempotency_key(due: &Due) -> String {
    let commit = due.commit.get(..16).unwrap_or(&due.commit);
    format!("quadra-{commit}-{}", due.id)
}

/// The text of a field of an event, as the outbox keeps it.
fn text_field(fields: &J, name: &str) -> Option<String> {
    match plain(fields.get(name)?) {
        J::String(s) => Some(s),
        J::Null => None,
        other => Some(other.to_string()),
    }
}

/// What the block of an `outbound … as e:` built for an event.
#[derive(Clone, Debug, PartialEq)]
pub enum Built {
    Http {
        method: String,
        path: String,
        body: Option<String>,
        content_type: Option<String>,
    },
    Email {
        to: String,
        subject: String,
        body: String,
        html: Option<String>,
    },
    /// `write(path, content)`: a file; `insert(table, row)`: a row of an exchange table
    Put {
        file: bool,
        path: String,
        body: Vec<u8>,
    },
}

/// A pull of an `inbound` to make: a `GET` of `url` with the connection's authentication.
#[derive(Clone, Debug)]
pub struct Pull {
    pub inbound: String,
    /// a connection that is not HTTP (files, a database, a mailbox): the job that fetches,
    /// for the server or the agent that runs it; then `url` and the rest are not used
    pub job: Option<JobPull>,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub creds: Credentials,
    pub timeout: Duration,
}

/// The fetch of a pull of a connection that is not HTTP.
#[derive(Clone, Debug)]
pub struct JobPull {
    pub job: crate::transport::Job,
    pub agent: Option<String>,
    pub env: BTreeMap<String, String>,
    /// the tokens of its OAuth2 consent (a mailbox of Gmail, Microsoft 365): the server's only
    pub consent: Option<crate::oauth::Use>,
}

/// What a pull brought: an HTTP answer (status, body, `Retry-After`), or what a job fetched.
#[derive(Clone, Debug)]
pub enum PullResult {
    Http(Result<(u16, String, Option<u64>), String>),
    Job(crate::transport::Outcome),
}

/// Makes a pull: its HTTP request, or its job (here, or by its agent).
pub async fn pull(
    client: &reqwest::Client,
    p: &Pull,
    public_only: bool,
    agents: Option<&crate::agents::Agents>,
) -> PullResult {
    match &p.job {
        None => PullResult::Http(fetch(client, p, public_only).await),
        Some(j) => PullResult::Job(match (&j.agent, agents) {
            (None, _) => {
                crate::transport::run_with(&j.job, &j.env, public_only, j.consent.as_ref()).await
            }
            (Some(a), Some(q)) => q.run(a, &j.job).await,
            (Some(a), None) => {
                crate::transport::Outcome::Failed(crate::transport::Failed::Waiting(format!(
                    "the agent `{a}` runs it: this server has no agents"
                )))
            }
        }),
    }
}

/// What proves who sends a request, beyond its headers: an OAuth2 token asked for it, the
/// client's certificate (mutual TLS), the authority its server's certificate is signed by, a
/// signature of its body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    pub oauth: Option<OAuth>,
    /// the client's certificate and private key, PEM
    pub identity: Option<String>,
    /// a private authority, PEM: then the only one trusted
    pub ca: Option<String>,
    /// the secret and how the signature is written
    pub sign: Option<(String, Signature)>,
    /// OAuth2 with a user's consent: the tokens kept for the connection (`crate::oauth`)
    pub consent: Option<Box<crate::oauth::Use>>,
}

/// OAuth2 client credentials, as a connection says them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OAuth {
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub scope: Option<String>,
    pub in_body: bool,
}

/// The base url, the headers of authentication, the credentials and the timeout of an HTTP
/// connection.
pub type Parts = (String, Vec<(String, String)>, Credentials, Duration);

/// The parts of an HTTP connection, with the values where the app runs.
fn http_parts(
    manifest: &Manifest,
    env: &BTreeMap<String, String>,
    connection: &str,
) -> Result<Parts, String> {
    let value = |name: &str| -> Result<String, String> {
        manifest
            .value(env, name)
            .map(|s| s.to_string())
            .ok_or_else(|| format!("the variable {name} is not set where the app runs"))
    };
    let setting = |s: &Setting| -> Result<String, String> {
        match s {
            Setting::Literal(l) => Ok(l.clone()),
            Setting::Var(v) => value(v),
        }
    };
    let conn = manifest
        .connection(connection)
        .ok_or_else(|| format!("quadra.toml has no connection `{connection}`"))?;
    let ConnectionKind::Http {
        url,
        auth,
        signing_secret,
        signature,
        client_certificate,
        server_ca,
        timeout_s,
        ..
    } = &conn.kind
    else {
        return Err(format!("`{connection}` is not an HTTP connection"));
    };
    let mut headers = Vec::new();
    let mut creds = Credentials::default();
    match auth {
        HttpAuth::None => {}
        HttpAuth::Bearer(v) => {
            headers.push(("Authorization".into(), format!("Bearer {}", value(v)?)))
        }
        HttpAuth::ApiKey { header, key } => headers.push((header.clone(), value(key)?)),
        HttpAuth::Basic { user, password } => {
            use base64::Engine;
            let token = base64::engine::general_purpose::STANDARD.encode(format!(
                "{}:{}",
                setting(user)?,
                value(password)?
            ));
            headers.push(("Authorization".into(), format!("Basic {token}")));
        }
        HttpAuth::OAuth2 {
            token_url,
            client_id,
            client_secret,
            scope,
            in_body,
        } => {
            creds.oauth = Some(OAuth {
                token_url: setting(token_url)?,
                client_id: setting(client_id)?,
                client_secret: value(client_secret)?,
                scope: scope.clone(),
                in_body: *in_body,
            })
        }
        HttpAuth::Consent(c) => {
            // the tokens themselves are the engine's to add (`Engine::consents`)
            creds.consent = Some(Box::new(crate::oauth::Use {
                key: String::new(),
                connection: connection.to_string(),
                token_url: setting(&c.token_url)?,
                client_id: setting(&c.client_id)?,
                client_secret: value(&c.client_secret)?,
                in_body: c.in_body,
                stored: None,
                problem: None,
            }))
        }
    }
    if let Some(s) = signing_secret {
        creds.sign = Some((value(s)?, signature.clone()));
    }
    creds.identity = client_certificate.as_deref().map(value).transpose()?;
    creds.ca = server_ca.as_deref().map(value).transpose()?;
    Ok((
        setting(url)?.trim_end_matches('/').to_string(),
        headers,
        creds,
        Duration::from_secs(*timeout_s),
    ))
}

/// The base url, the headers of authentication, the credentials and the timeout of an HTTP
/// connection: what a pull sends.
pub fn connection_request(
    manifest: &Manifest,
    env: &BTreeMap<String, String>,
    connection: &str,
) -> Result<Parts, String> {
    let (base, auth, creds, timeout) = http_parts(manifest, env, connection)?;
    let mut headers = vec![
        ("User-Agent".to_string(), "Quadra".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
    ];
    headers.extend(auth);
    Ok((base, headers, creds, timeout))
}

/// What came of a pull: the status, the body (up to 16 MB), `Retry-After` in seconds.
pub async fn fetch(
    client: &reqwest::Client,
    pull: &Pull,
    public_only: bool,
) -> Result<(u16, String, Option<u64>), String> {
    let parsed = reqwest::Url::parse(&pull.url).map_err(|e| format!("{}: {e}", pull.url))?;
    if public_only {
        if let Some(host) = parsed.host_str() {
            if host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .is_ok()
            {
                addresses(host, 0, true).await?;
            }
        }
    }
    let client = client_for(client, &pull.creds, public_only)?;
    let mut req = client.get(parsed).timeout(pull.timeout);
    for (k, v) in &pull.headers {
        req = req.header(k, v);
    }
    if let Some(o) = &pull.creds.oauth {
        let t = token(&client, o, public_only, pull.timeout)
            .await
            .map_err(|f| f.text())?;
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let mut bearer = None;
    if let Some(u) = &pull.creds.consent {
        let t = crate::oauth::bearer(u, &client, public_only, pull.timeout)
            .await
            .map_err(|f| f.text().to_string())?;
        req = req.header("Authorization", format!("Bearer {t}"));
        bearer = Some(t);
    }
    let again = req.try_clone();
    let mut resp = req
        .send()
        .await
        .map_err(|e| format!("{}: {}", host_of(&pull.url), root_cause(&e)))?;
    // the token refused: refreshed, and asked again once
    if let (Some(u), Some(t), Some(again), 401) =
        (&pull.creds.consent, &bearer, again, resp.status().as_u16())
    {
        let t = crate::oauth::refused(u, t, &client, public_only, pull.timeout)
            .await
            .map_err(|f| f.text().to_string())?;
        resp = again
            .header("Authorization", format!("Bearer {t}"))
            .send()
            .await
            .map_err(|e| format!("{}: {}", host_of(&pull.url), root_cause(&e)))?;
    }
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let body = read_up_to(resp, 16 << 20).await;
    Ok((status, body, retry_after))
}

/// What a block's last expression gave (`Interp::request`), as a request.
pub fn built_of(v: &quadra_runtime::V) -> Result<Built, String> {
    use quadra_runtime::V;
    let V::Tuple(parts) = v else {
        return Err(format!("the block ended with {v}, not a request"));
    };
    let get = |k: &str| -> Option<String> {
        parts
            .iter()
            .find(|(n, _)| n.as_deref() == Some(k))
            .and_then(|(_, v)| match v {
                V::None => None,
                V::Text(t) => Some(t.to_string()),
                other => Some(other.to_string()),
            })
    };
    let method = get("method").ok_or("the block did not end with a request")?;
    if method == "WRITE" || method == "INSERT" {
        return Ok(Built::Put {
            file: method == "WRITE",
            path: get("path").unwrap_or_default(),
            // the bytes of a file of rows come a character per byte
            body: match get("encoding").as_deref() {
                Some("bytes") => get("body")
                    .unwrap_or_default()
                    .chars()
                    .map(|c| c as u32 as u8)
                    .collect(),
                _ => get("body").unwrap_or_default().into_bytes(),
            },
        });
    }
    if method == "EMAIL" {
        return Ok(Built::Email {
            to: get("to").ok_or("`email(…)` without `to`")?,
            subject: get("subject").unwrap_or_default(),
            body: get("body").unwrap_or_default(),
            html: get("html"),
        });
    }
    Ok(Built::Http {
        method,
        path: get("path").unwrap_or_default(),
        body: get("body"),
        content_type: get("content_type"),
    })
}

/// The plan of a due event of the outbound `(connection, path)`; `built`, what its block built
/// (`Err`: the block failed, and the event waits for a fixed deploy).
pub fn plan(
    manifest: &Manifest,
    env: &BTreeMap<String, String>,
    due: Due,
    connection: &str,
    path: Option<&str>,
    built: Option<Result<Built, String>>,
) -> Plan {
    let base = |send| Plan {
        id: due.id,
        event: due.event.clone(),
        connection: connection.to_string(),
        attempts: due.attempts,
        created_at: due.created_at.clone(),
        fields: due.fields.clone(),
        retry: Retry::default(),
        limits: manifest
            .connection(connection)
            .map(|c| c.limits)
            .unwrap_or_default(),
        batch: Vec::new(),
        send,
    };
    let value = |name: &str| -> Result<String, String> {
        manifest
            .value(env, name)
            .map(|s| s.to_string())
            .ok_or_else(|| format!("the variable {name} is not set where the app runs"))
    };
    let setting = |s: &Setting| -> Result<String, String> {
        match s {
            Setting::Literal(l) => Ok(l.clone()),
            Setting::Var(v) => value(v),
        }
    };
    let Some(conn) = manifest.connection(connection) else {
        return base(Send::Broken(format!(
            "quadra.toml has no connection `{connection}`"
        )));
    };
    let built = match built {
        Some(Err(e)) => {
            return base(Send::Broken(format!(
                "the block of the outbound failed: {e} (fixed by a deploy)"
            )))
        }
        Some(Ok(b)) => Some(b),
        None => None,
    };
    let send = (|| -> Result<Send, String> {
        match &conn.kind {
            // an HTTP service of another network: the agent makes the request, with its values
            ConnectionKind::Http { .. } if conn.agent.is_some() => {
                let (method, path, body, content_type) = match &built {
                    Some(Built::Http {
                        method,
                        path: p,
                        body,
                        content_type,
                    }) => (
                        method.clone(),
                        format!("{}{}", path.unwrap_or(""), p),
                        body.clone().unwrap_or_default(),
                        content_type.clone(),
                    ),
                    Some(_) => return Err("only a request goes through an HTTP connection".into()),
                    None => (
                        "POST".to_string(),
                        path.unwrap_or("").to_string(),
                        json!({
                            "id": due.id,
                            "event": due.event,
                            "tx": due.tx,
                            "created_at": format!("{}Z", due.created_at),
                            "data": plain_fields(&due.fields),
                        })
                        .to_string(),
                        Some("application/json".to_string()),
                    ),
                };
                let headers = vec![
                    (
                        "Content-Type".to_string(),
                        content_type.unwrap_or_else(|| "application/json".to_string()),
                    ),
                    ("Quadra-Event".to_string(), due.event.clone()),
                    ("Quadra-Delivery".to_string(), due.id.to_string()),
                    ("Idempotency-Key".to_string(), idempotency_key(&due)),
                ];
                Ok(Send::Job {
                    job: crate::transport::Job::Put {
                        conn: crate::transport::Conn::of(manifest, connection)?,
                        put: crate::transport::Put::Http {
                            method,
                            path,
                            headers,
                            body,
                        },
                    },
                    agent: conn.agent.clone(),
                    env: env.clone(),
                })
            }
            ConnectionKind::Http { .. } => {
                let (base, auth, creds, timeout) = http_parts(manifest, env, connection)?;
                // the request its block built, or the envelope of the event
                let (method, path, body, content_type) = match &built {
                    Some(Built::Http {
                        method,
                        path: p,
                        body,
                        content_type,
                    }) => (
                        method.clone(),
                        format!("{}{}", path.unwrap_or(""), p),
                        body.clone().unwrap_or_default(),
                        content_type.clone(),
                    ),
                    Some(Built::Put { .. }) => {
                        return Err(
                            "`write` and `insert` are for file and database connections".into()
                        )
                    }
                    Some(Built::Email { .. }) => {
                        return Err("an email cannot go through an HTTP connection".into())
                    }
                    None => (
                        "POST".to_string(),
                        path.unwrap_or("").to_string(),
                        json!({
                            "id": due.id,
                            "event": due.event,
                            "tx": due.tx,
                            "created_at": format!("{}Z", due.created_at),
                            "data": plain_fields(&due.fields),
                        })
                        .to_string(),
                        Some("application/json".to_string()),
                    ),
                };
                if !path.is_empty()
                    && (!path.starts_with('/')
                        || path.contains("..")
                        || path.contains("://")
                        || path.chars().any(|c| c.is_whitespace() || c.is_control()))
                {
                    return Err(format!("the path `{path}` is not a path of the connection's url (from `/`, no spaces, no `..`)"));
                }
                let url = format!("{base}{path}");
                let mut headers = vec![
                    (
                        "Content-Type".to_string(),
                        content_type.unwrap_or_else(|| "application/json".to_string()),
                    ),
                    ("User-Agent".to_string(), "Quadra".to_string()),
                    ("Quadra-Event".to_string(), due.event.clone()),
                    ("Quadra-Delivery".to_string(), due.id.to_string()),
                    ("Idempotency-Key".to_string(), idempotency_key(&due)),
                ];
                headers.extend(auth);
                Ok(Send::Http {
                    method,
                    url,
                    headers,
                    body,
                    creds,
                    timeout,
                })
            }
            ConnectionKind::Smtp {
                url,
                user,
                password,
                from,
                insecure,
            } => {
                let login = match (user, password) {
                    (Some(u), Some(p)) => Some((setting(u)?, value(p)?)),
                    _ => None,
                };
                let field = |n: &str| {
                    text_field(&due.fields, n)
                        .ok_or_else(|| format!("the event has no `{n}`: an email needs it"))
                };
                // what its block built, or the event's `to`, `subject` and `body`
                let (to, subject, body, html) = match &built {
                    Some(Built::Email {
                        to,
                        subject,
                        body,
                        html,
                    }) => (to.clone(), subject.clone(), body.clone(), html.clone()),
                    Some(Built::Http { .. }) | Some(Built::Put { .. }) => {
                        return Err("only an email goes through an SMTP connection".into())
                    }
                    None => (field("to")?, field("subject")?, field("body")?, None),
                };
                let url = setting(url)?;
                Ok(Send::Smtp {
                    plain: *insecure && url.starts_with("smtp://"),
                    url,
                    login,
                    from: setting(from)?,
                    to,
                    subject,
                    body,
                    html,
                })
            }
            // a file, a row: a job for whoever runs the connection (the server, an agent)
            ConnectionKind::Files { .. } | ConnectionKind::Database { .. } => {
                let Some(Built::Put { file, path, body }) = &built else {
                    return Err(format!(
                        "`{connection}` takes {}: the block ends with it",
                        if conn.pulls() == "files" {
                            "`write(path, content)`"
                        } else {
                            "`insert(table, record)`"
                        }
                    ));
                };
                let put = if *file {
                    if path.is_empty() || path.contains("..") || path.starts_with('/') {
                        return Err(format!("the path `{path}` is a path in the connection's folder: no `..`, not from `/`"));
                    }
                    crate::transport::Put::File {
                        path: path.clone(),
                        body: body.clone(),
                    }
                } else {
                    crate::transport::Put::Row {
                        table: path.clone(),
                        row: serde_json::from_slice(body).map_err(|e| e.to_string())?,
                        key: idempotency_key(&due),
                    }
                };
                Ok(Send::Job {
                    job: crate::transport::Job::Put {
                        conn: crate::transport::Conn::of(manifest, connection)?,
                        put,
                    },
                    agent: conn.agent.clone(),
                    env: env.clone(),
                })
            }
            ConnectionKind::Mail { .. } => Err(format!(
                "`{connection}` is a mailbox read over IMAP: nothing is sent through it"
            )),
        }
    })();
    base(send.unwrap_or_else(Send::Broken))
}

/// A duration said as a person would: `1 day`, `3 hours`, `90 s`.
pub fn human(s: u64) -> String {
    match s {
        s if s % 86_400 == 0 => format!("{} day{}", s / 86_400, if s == 86_400 { "" } else { "s" }),
        s if s % 3600 == 0 => format!("{} hour{}", s / 3600, if s == 3600 { "" } else { "s" }),
        s if s % 60 == 0 => format!("{} minute{}", s / 60, if s == 60 { "" } else { "s" }),
        s => format!("{s} s"),
    }
}

/// When the `window "02:00" .. "04:00"` of an outbound next opens after `now` (UTC), in the
/// project's zone (`zone`, an IANA name; UTC when empty): `None` when it is open now.
pub fn window_opens(
    zone: &str,
    window: &quadra_ir::Expr,
    now: chrono::NaiveDateTime,
) -> Option<chrono::NaiveDateTime> {
    use chrono::TimeZone;
    let quadra_ir::ExprKind::Binary { lhs, rhs, .. } = &window.kind else {
        return None;
    };
    let time = |e: &quadra_ir::Expr| match &e.kind {
        quadra_ir::ExprKind::Lit(quadra_ir::Lit::Str(s)) => {
            chrono::NaiveTime::parse_from_str(s, "%H:%M").ok()
        }
        _ => None,
    };
    let (from, to) = (time(lhs)?, time(rhs)?);
    let tz: chrono_tz::Tz = if zone.is_empty() {
        chrono_tz::UTC
    } else {
        zone.parse().unwrap_or(chrono_tz::UTC)
    };
    let local = tz.from_utc_datetime(&now).naive_local();
    let t = local.time();
    let open = if from <= to {
        t >= from && t < to
    } else {
        t >= from || t < to
    };
    if open {
        return None;
    }
    // the next time it opens: today at `from`, or tomorrow
    let day = if t < from {
        local.date()
    } else {
        local.date() + chrono::Duration::days(1)
    };
    let at = day.and_time(from);
    let utc = tz
        .from_local_datetime(&at)
        .earliest()
        .map(|d| d.naive_utc())
        .unwrap_or(at);
    Some(utc)
}

/// HMAC-SHA256 of `message` with `key`, in hex.
pub fn sign(key: &str, message: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes()).expect("any key length");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// The headers that sign `body` with `key` at `t` (unix seconds), written as `how` says:
/// Quadra's own `Quadra-Signature: t=…,v1=…`, or a header of the receiver's (hex or base64,
/// with a prefix, the timestamp in a header of its own and signed with the body).
pub fn signature(key: &str, how: &Signature, body: &str, t: i64) -> Vec<(String, String)> {
    use hmac::{Hmac, Mac};
    let Some(header) = &how.header else {
        return vec![(
            "Quadra-Signature".into(),
            format!("t={t},v1={}", sign(key, &format!("{t}.{body}"))),
        )];
    };
    let message = match how.timestamp_header {
        Some(_) => format!("{t}.{body}"),
        None => body.to_string(),
    };
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes()).expect("any key length");
    mac.update(message.as_bytes());
    let bytes = mac.finalize().into_bytes();
    let text = if how.base64 {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    } else {
        hex::encode(bytes)
    };
    let mut out = vec![(header.clone(), format!("{}{text}", how.prefix))];
    if let Some(h) = &how.timestamp_header {
        out.push((h.clone(), t.to_string()));
    }
    out
}

/// The client for these credentials: the shared one, or (a certificate of the client, a
/// private authority) one of its own, kept for the next requests.
fn client_for(
    shared: &reqwest::Client,
    creds: &Credentials,
    public_only: bool,
) -> Result<reqwest::Client, String> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    if creds.identity.is_none() && creds.ca.is_none() {
        return Ok(shared.clone());
    }
    type Clients = Mutex<HashMap<(Option<String>, Option<String>, bool), reqwest::Client>>;
    static CLIENTS: OnceLock<Clients> = OnceLock::new();
    let key = (creds.identity.clone(), creds.ca.clone(), public_only);
    let clients = CLIENTS.get_or_init(Default::default);
    if let Some(c) = clients.lock().unwrap().get(&key) {
        return Ok(c.clone());
    }
    let mut b = builder(public_only);
    if let Some(pem) = &creds.identity {
        let id = reqwest::Identity::from_pem(pem.as_bytes()).map_err(|e| {
            format!("client_certificate is not a certificate with its private key, PEM: {e}")
        })?;
        b = b.identity(id);
    }
    if let Some(pem) = &creds.ca {
        let ca = reqwest::Certificate::from_pem(pem.as_bytes())
            .map_err(|e| format!("server_ca is not a certificate, PEM: {e}"))?;
        b = b.tls_certs_only([ca]);
    }
    let c = b
        .build()
        .map_err(|e| format!("the TLS settings of the connection: {e}"))?;
    let mut all = clients.lock().unwrap();
    // a value changed: the clients of the old one go
    if all.len() > 64 {
        all.clear();
    }
    all.insert(key, c.clone());
    Ok(c)
}

type Tokens = std::sync::Mutex<std::collections::HashMap<OAuth, (String, std::time::Instant)>>;

fn tokens() -> &'static Tokens {
    static TOKENS: std::sync::OnceLock<Tokens> = std::sync::OnceLock::new();
    TOKENS.get_or_init(Default::default)
}

fn forget_token(o: &OAuth) {
    tokens().lock().unwrap().remove(o);
}

/// A token of OAuth2 client credentials: the one kept while it holds (a minute before it
/// expires), else one asked of the token url.
async fn token(
    client: &reqwest::Client,
    o: &OAuth,
    public_only: bool,
    timeout: Duration,
) -> Result<String, Failure> {
    if let Some((t, until)) = tokens().lock().unwrap().get(o) {
        if *until > std::time::Instant::now() {
            return Ok(t.clone());
        }
    }
    let parsed = reqwest::Url::parse(&o.token_url)
        .map_err(|e| Failure::Waiting(format!("oauth2_token_url {}: {e}", o.token_url)))?;
    if public_only {
        if let Some(host) = parsed.host_str() {
            if host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .is_ok()
            {
                addresses(host, 0, true).await.map_err(Failure::Never)?;
            }
        }
    }
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    let mut form = vec!["grant_type=client_credentials".to_string()];
    if let Some(s) = &o.scope {
        form.push(format!("scope={}", enc(s)));
    }
    let mut req = client
        .post(parsed)
        .timeout(timeout)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .header("User-Agent", "Quadra");
    if o.in_body {
        form.push(format!("client_id={}", enc(&o.client_id)));
        form.push(format!("client_secret={}", enc(&o.client_secret)));
    } else {
        use base64::Engine;
        let b = base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            enc(&o.client_id),
            enc(&o.client_secret)
        ));
        req = req.header("Authorization", format!("Basic {b}"));
    }
    let host = host_of(&o.token_url);
    let resp = req
        .body(form.join("&"))
        .send()
        .await
        .map_err(|e| Failure::Again(format!("the token of {host}: {}", root_cause(&e))))?;
    let status = resp.status();
    let text = read_up_to(resp, 64 << 10).await;
    if !status.is_success() {
        let said: String = text.chars().take(200).collect();
        return Err(Failure::Again(format!(
            "{host} refused the token ({}): {said}",
            status.as_u16()
        )));
    }
    let j: J = serde_json::from_str(&text)
        .map_err(|_| Failure::Again(format!("{host} answered the token with no JSON")))?;
    let t = j["access_token"]
        .as_str()
        .ok_or_else(|| Failure::Again(format!("{host} answered the token with no access_token")))?
        .to_string();
    let lasts = j["expires_in"].as_u64().unwrap_or(3600);
    let until = std::time::Instant::now() + Duration::from_secs(lasts.saturating_sub(60).max(30));
    tokens()
        .lock()
        .unwrap()
        .insert(o.clone(), (t.clone(), until));
    Ok(t)
}

/// Whether an address is on the public internet: not private, loopback, link-local (the
/// clouds' metadata, `169.254.169.254`), shared (100.64/10), multicast, documentation or
/// reserved.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_broadcast()
                || v.is_documentation()
                || v.is_unspecified()
                || v.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] & 0xfe) == 18)
                || o[0] >= 240)
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v.segments();
            !(v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || (s[0] == 0x64 && s[1] == 0xff9b)
                || s[0] == 0x2002
                || (s[0] == 0x2001 && s[1] == 0))
        }
    }
}

/// The addresses of `host` that may be connected to (all of them, or only public ones).
pub(crate) async fn addresses(
    host: &str,
    port: u16,
    public_only: bool,
) -> Result<Vec<SocketAddr>, String> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let all: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| format!("cannot resolve {host}: {e}"))?
            .collect(),
    };
    let ok: Vec<SocketAddr> = all
        .into_iter()
        .filter(|a| !public_only || is_public(a.ip()))
        .collect();
    if ok.is_empty() {
        return Err(format!(
            "{host} is not a public address: connections only go to the internet"
        ));
    }
    Ok(ok)
}

/// Resolves names for the HTTP client, keeping only public addresses (`public_only`).
struct PublicResolver;

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = addresses(&host, 0, true).await.map_err(
                |e| -> Box<dyn std::error::Error + std::marker::Send + Sync> { e.into() },
            )?;
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The client every HTTP delivery of a host goes through: no redirects, no proxy from the
/// environment, and with `public_only` only public addresses.
pub fn http_client(public_only: bool) -> reqwest::Client {
    builder(public_only).build().expect("an HTTP client")
}

fn builder(public_only: bool) -> reqwest::ClientBuilder {
    let mut b = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(10));
    if public_only {
        b = b.dns_resolver(std::sync::Arc::new(PublicResolver));
    }
    b
}

impl Failure {
    fn text(self) -> String {
        match self {
            Failure::Again(e) | Failure::Later(e, _) | Failure::Never(e) | Failure::Waiting(e) => e,
        }
    }
}

impl From<crate::transport::Failed> for Failure {
    fn from(f: crate::transport::Failed) -> Failure {
        match f {
            crate::transport::Failed::Again(e) => Failure::Again(e),
            crate::transport::Failed::Never(e) => Failure::Never(e),
            crate::transport::Failed::Waiting(e) => Failure::Waiting(e),
        }
    }
}

/// What failed, and whether it may work later.
enum Failure {
    Again(String),
    /// try again, not before this long (the receiver said so: `Retry-After`)
    Later(String, Duration),
    Never(String),
    /// it cannot be sent as configured: it waits for the configuration, without a time limit
    Waiting(String),
}

/// Runs a plan: what came of it, given when it was first emitted (after a day it is not tried
/// again).
pub async fn run(
    plan: &Plan,
    client: &reqwest::Client,
    public_only: bool,
    agents: Option<&crate::agents::Agents>,
) -> (Outcome, Attempt, Option<String>) {
    let started = std::time::Instant::now();
    let mut seen = Attempt {
        connection: plan.connection.clone(),
        ..Default::default()
    };
    let mut answer: Option<String> = None;
    let result = match &plan.send {
        Send::Broken(why) => Err(Failure::Waiting(why.clone())),
        Send::Job { job, agent, env } => {
            let outcome = match agent {
                None => crate::transport::run(job, env, public_only).await,
                Some(a) => match agents {
                    Some(q) => q.run(a, job).await,
                    None => crate::transport::Outcome::Failed(crate::transport::Failed::Waiting(
                        format!("the agent `{a}` runs it: this server has no agents"),
                    )),
                },
            };
            match outcome {
                crate::transport::Outcome::Answered {
                    status,
                    body,
                    retry_after,
                } => {
                    seen.status = Some(status);
                    let short: String = body.chars().take(500).collect();
                    seen.excerpt = (!short.trim().is_empty()).then_some(short);
                    if (200..300).contains(&status) {
                        answer = Some(body);
                        Ok(())
                    } else {
                        let text: String = body.chars().take(200).collect();
                        let via = agent.as_deref().unwrap_or("the server");
                        Err(judge(
                            status,
                            retry_after.map(Duration::from_secs),
                            format!(
                                "answered {status} (through the agent `{via}`): {}",
                                text.trim()
                            ),
                        ))
                    }
                }
                crate::transport::Outcome::Put | crate::transport::Outcome::Fetched(_) => Ok(()),
                crate::transport::Outcome::Failed(f) => Err(match f {
                    crate::transport::Failed::Again(e) => Failure::Again(e),
                    crate::transport::Failed::Never(e) => Failure::Never(e),
                    crate::transport::Failed::Waiting(e) => Failure::Waiting(e),
                }),
            }
        }
        Send::Http {
            method,
            url,
            headers,
            body,
            creds,
            timeout,
        } => match client_for(client, creds, public_only) {
            Err(e) => Err(Failure::Waiting(e)),
            Ok(client) => {
                let mut headers = headers.clone();
                let first = match (&creds.oauth, &creds.consent) {
                    (Some(o), _) => token(&client, o, public_only, *timeout).await.map(Some),
                    (None, Some(u)) => crate::oauth::bearer(u, &client, public_only, *timeout)
                        .await
                        .map(Some)
                        .map_err(Failure::from),
                    (None, None) => Ok(None),
                };
                match first {
                    Err(f) => Err(f),
                    Ok(t) => {
                        let base = headers.len();
                        let t_used = t.clone();
                        if let Some(t) = t {
                            headers.push(("Authorization".into(), format!("Bearer {t}")));
                        }
                        let mut r = http(
                            &client,
                            method,
                            url,
                            &headers,
                            body,
                            creds.sign.as_ref(),
                            *timeout,
                            public_only,
                            &mut seen,
                            &mut answer,
                        )
                        .await;
                        // a token of the user's consent refused: refreshed, and the request sent
                        // again once
                        if let (Some(u), Some(401), Some(old)) =
                            (&creds.consent, seen.status, t_used.as_deref())
                        {
                            match crate::oauth::refused(u, old, &client, public_only, *timeout)
                                .await
                            {
                                Ok(t) => {
                                    headers.truncate(base);
                                    headers.push(("Authorization".into(), format!("Bearer {t}")));
                                    seen.status = None;
                                    seen.excerpt = None;
                                    r = http(
                                        &client,
                                        method,
                                        url,
                                        &headers,
                                        body,
                                        creds.sign.as_ref(),
                                        *timeout,
                                        public_only,
                                        &mut seen,
                                        &mut answer,
                                    )
                                    .await;
                                }
                                Err(f) => r = Err(Failure::from(f)),
                            }
                        }
                        // the token refused (it expired early, it was revoked): a new one, and
                        // the request sent again once
                        if let (Some(o), Some(401)) = (&creds.oauth, seen.status) {
                            forget_token(o);
                            match token(&client, o, public_only, *timeout).await {
                                Ok(t) => {
                                    headers.truncate(base);
                                    headers.push(("Authorization".into(), format!("Bearer {t}")));
                                    seen.status = None;
                                    seen.excerpt = None;
                                    r = http(
                                        &client,
                                        method,
                                        url,
                                        &headers,
                                        body,
                                        creds.sign.as_ref(),
                                        *timeout,
                                        public_only,
                                        &mut seen,
                                        &mut answer,
                                    )
                                    .await;
                                    if seen.status == Some(401) {
                                        forget_token(o);
                                    }
                                }
                                Err(f) => r = Err(f),
                            }
                        }
                        r
                    }
                }
            }
        },
        Send::Smtp {
            url,
            login,
            from,
            to,
            subject,
            body,
            html,
            plain,
        } => {
            smtp(
                url,
                login.as_ref(),
                from,
                to,
                subject,
                body,
                html.as_deref(),
                *plain,
                public_only,
                &mut seen,
            )
            .await
        }
    };
    seen.duration_ms = started.elapsed().as_millis() as u64;
    (outcome(plan, result), seen, answer)
}

/// What a result means for the event: delivered, tried again when, or failed for good.
fn outcome(plan: &Plan, result: Result<(), Failure>) -> Outcome {
    match result {
        Ok(()) => Outcome::Delivered,
        Err(Failure::Never(e)) => Outcome::Failed { error: e },
        Err(f) => {
            let now = chrono::Utc::now().naive_utc();
            let (e, at_least, ages) = match f {
                Failure::Again(e) => (e, None, true),
                Failure::Later(e, d) => (e, Some(d), true),
                // a configuration to fix (a value missing where it runs) is no reason to give up
                Failure::Waiting(e) => (e, None, false),
                Failure::Never(_) => unreachable!(),
            };
            let first =
                chrono::NaiveDateTime::parse_from_str(&plan.created_at, "%Y-%m-%dT%H:%M:%S")
                    .unwrap_or(now);
            let r = plan.retry;
            let limit = chrono::Duration::try_seconds(r.for_s.min(10 * 365 * 86_400) as i64)
                .unwrap_or(chrono::Duration::days(3650));
            if ages && now - first > limit {
                return Outcome::Failed {
                    error: format!("{e} (tried for {})", human(r.for_s)),
                };
            }
            if ages && r.times.is_some_and(|n| plan.attempts + 1 >= n) {
                return Outcome::Failed {
                    error: format!("{e} (tried {} times)", plan.attempts + 1),
                };
            }
            // a jitter of its own, the same for the same event and attempt
            let jitter =
                ((plan.id.wrapping_mul(2654435761) ^ plan.attempts) % 1000) as f64 / 1000.0;
            let mut wait = quadra_store::deliveries::backoff_between(
                plan.attempts + 1,
                jitter,
                r.first_s,
                r.max_s,
            );
            if let Some(d) = at_least {
                wait = wait.max(d.min(Duration::from_secs(3600)));
            }
            let at = now + chrono::Duration::from_std(wait).unwrap_or_default();
            Outcome::Retry {
                at: at.format("%Y-%m-%dT%H:%M:%S").to_string(),
                error: e,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn http(
    client: &reqwest::Client,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &str,
    signed: Option<&(String, Signature)>,
    timeout: Duration,
    public_only: bool,
    seen: &mut Attempt,
    answer: &mut Option<String>,
) -> Result<(), Failure> {
    let parsed = reqwest::Url::parse(url).map_err(|e| Failure::Never(format!("{url}: {e}")))?;
    // an address written as such does not go through the resolver
    if public_only {
        if let Some(host) = parsed.host_str() {
            if host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .is_ok()
            {
                addresses(host, 0, true).await.map_err(Failure::Never)?;
            }
        }
    }
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| Failure::Never(format!("`{method}` is not an HTTP method")))?;
    let mut req = client.request(method, parsed).timeout(timeout);
    if !body.is_empty() {
        req = req.body(body.to_string());
    }
    for (k, v) in headers {
        req = req.header(k, v);
    }
    if let Some((key, how)) = signed {
        for (k, v) in signature(key, how, body, chrono::Utc::now().timestamp()) {
            req = req.header(k, v);
        }
    }
    let resp = req.send().await.map_err(|e| {
        let msg = if e.is_timeout() {
            format!(
                "no answer from {} within {}s",
                host_of(url),
                timeout.as_secs()
            )
        } else {
            format!("{}: {}", host_of(url), root_cause(&e))
        };
        Failure::Again(msg)
    })?;
    let status = resp.status();
    seen.status = Some(status.as_u16());
    if status.is_success() {
        // the whole answer (up to 1 MB) for `reply`, its beginning for the log
        let text = read_up_to(resp, 1 << 20).await;
        let short: String = text.chars().take(500).collect();
        seen.excerpt = (!short.trim().is_empty()).then_some(short);
        *answer = Some(text);
        return Ok(());
    }
    // `Retry-After` in seconds (a date is read as no hint)
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let text = excerpt(resp, 500).await;
    seen.excerpt = (!text.trim().is_empty()).then(|| text.clone());
    let text: String = text.chars().take(200).collect();
    let code = status.as_u16();
    let msg = format!(
        "{} answered {code}{}",
        host_of(url),
        if text.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", text.trim())
        }
    );
    Err(judge(code, retry_after, msg))
}

/// What an answer that is not a success means for the event: tried again (when), or failed.
fn judge(code: u16, retry_after: Option<Duration>, msg: String) -> Failure {
    match code {
        300..=399 => Failure::Never(format!(
            "{msg} (redirects are not followed: give the final url)"
        )),
        429 | 503 if retry_after.is_some() => Failure::Later(msg, retry_after.unwrap()),
        408 | 429 => Failure::Again(msg),
        // credentials refused: a token to renew, not a reason to drop the event
        401 | 403 => Failure::Again(format!(
            "{msg} (the connection's credentials were refused: check them)"
        )),
        500..=599 => Failure::Again(msg),
        _ => Failure::Never(msg),
    }
}

/// A request of an HTTP connection run by an agent (`transport::Put::Http`, `Ask::Http`): the
/// connection's url, authentication and signature with the values where it runs. Its status,
/// its body (up to 16 MB) and `Retry-After`; the server judges them.
pub(crate) async fn job_http(
    c: &crate::transport::Resolved,
    method: &str,
    path: &str,
    extra: &[(String, String)],
    body: &str,
    public_only: bool,
) -> Result<(u16, String, Option<u64>), crate::transport::Failed> {
    use crate::transport::Failed;
    let (base, auth, creds, timeout) =
        http_parts(&c.manifest, &c.env, &c.connection.name).map_err(Failed::Waiting)?;
    if path.contains("..") || path.contains("://") || (!path.is_empty() && !path.starts_with('/')) {
        return Err(Failed::Never(format!(
            "`{path}` is not a path of the connection's url"
        )));
    }
    let url = format!("{base}{path}");
    let parsed = reqwest::Url::parse(&url).map_err(|e| Failed::Never(format!("{url}: {e}")))?;
    let shared = http_client(public_only);
    let client = client_for(&shared, &creds, public_only).map_err(Failed::Waiting)?;
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| Failed::Never(format!("`{method}` is not an HTTP method")))?;
    let mut req = client.request(method, parsed).timeout(timeout);
    req = req.header("User-Agent", "Quadra");
    for (k, v) in auth.iter().chain(extra) {
        req = req.header(k, v);
    }
    if let Some(o) = &creds.oauth {
        let t = token(&client, o, public_only, timeout)
            .await
            .map_err(|f| Failed::Again(f.text()))?;
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    if let Some((key, how)) = &creds.sign {
        for (k, v) in signature(key, how, body, chrono::Utc::now().timestamp()) {
            req = req.header(k, v);
        }
    }
    if !body.is_empty() {
        req = req.body(body.to_string());
    }
    let resp = req
        .send()
        .await
        .map_err(|e| Failed::Again(format!("{}: {}", host_of(&url), root_cause(&e))))?;
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    Ok((status, read_up_to(resp, 16 << 20).await, retry_after))
}

/// The first `chars` characters of a response's body, reading no more than 64 KB of it: a huge
/// or endless answer does not fill the node's memory.
async fn excerpt(resp: reqwest::Response, chars: usize) -> String {
    read_up_to(resp, 64 << 10)
        .await
        .chars()
        .take(chars)
        .collect()
}

/// A response's body, reading no more than `max` bytes of it.
async fn read_up_to(mut resp: reqwest::Response, max: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < max {
        match resp.chunk().await {
            Ok(Some(c)) => buf.extend_from_slice(&c),
            _ => break,
        }
    }
    buf.truncate(max);
    String::from_utf8_lossy(&buf).to_string()
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| url.to_string())
}

fn root_cause(e: &dyn std::error::Error) -> String {
    let mut cur: &dyn std::error::Error = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

#[allow(clippy::too_many_arguments)]
async fn smtp(
    url: &str,
    login: Option<&(String, String)>,
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
    html: Option<&str>,
    plain: bool,
    public_only: bool,
    seen: &mut Attempt,
) -> Result<(), Failure> {
    use lettre::transport::smtp::client::{Tls, TlsParameters};
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
    let (wrapper, rest) = if let Some(r) = url.strip_prefix("smtps://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("smtp://") {
        (false, r)
    } else {
        return Err(Failure::Never(format!(
            "{url} is not an smtp:// or smtps:// url"
        )));
    };
    let hostport = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse().unwrap()),
        _ => (hostport.to_string(), if wrapper { 465 } else { 587 }),
    };
    let message = Message::builder()
        .from(
            from.parse()
                .map_err(|e| Failure::Never(format!("the sender {from}: {e}")))?,
        )
        .to(to
            .parse()
            .map_err(|e| Failure::Never(format!("the address {to}: {e}")))?)
        .subject(subject);
    let message = match html {
        None => message
            .header(lettre::message::header::ContentType::TEXT_PLAIN)
            .body(body.to_string()),
        Some(h) => message.multipart(lettre::message::MultiPart::alternative_plain_html(
            body.to_string(),
            h.to_string(),
        )),
    }
    .map_err(|e| Failure::Never(format!("the email: {e}")))?;
    // connect to an address checked here, with the certificate checked for the name
    let addr = addresses(&host, port, public_only)
        .await
        .map_err(Failure::Again)?
        .remove(0);
    let tls =
        TlsParameters::new(host.clone()).map_err(|e| Failure::Never(format!("{host}: {e}")))?;
    let mut b = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(addr.ip().to_string())
        .port(port)
        .timeout(Some(Duration::from_secs(30)))
        .tls(if plain {
            Tls::None
        } else if wrapper {
            Tls::Wrapper(tls)
        } else {
            Tls::Required(tls)
        });
    if let Some((u, p)) = login {
        b = b.credentials(lettre::transport::smtp::authentication::Credentials::new(
            u.clone(),
            p.clone(),
        ));
    }
    match b.build().send(message).await {
        Ok(r) => {
            seen.status = Some(u16::from(r.code()));
            seen.excerpt = Some(r.message().collect::<Vec<_>>().join(" "));
            Ok(())
        }
        Err(e) if e.is_permanent() => Err(Failure::Never(format!("{host}: {e}"))),
        Err(e) => Err(Failure::Again(format!("{host}: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_metadata_addresses_are_not_public() {
        for a in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.100.100.100",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            assert!(!is_public(a.parse().unwrap()), "{a}");
        }
        for a in [
            "1.1.1.1",
            "8.8.8.8",
            "2606:4700:4700::1111",
            "::ffff:1.1.1.1",
        ] {
            assert!(is_public(a.parse().unwrap()), "{a}");
        }
    }

    #[test]
    fn the_data_of_a_webhook_is_plain_json() {
        let history = json!({
            "n": 3, "rate": {"d": "2.50"}, "on": {"date": "2026-11-27"}, "at": {"dt": "2026-11-27T10:00:00"},
            "total": {"money": "12.50", "cur": "EUR"}, "w": {"qty": "2", "unit": "kg"},
            "order": {"ref": 7, "ent": "Order"}, "tags": {"list": ["a", {"d": "1.5"}]}, "state": {"variant": "Shipped", "fields": []},
            "note": null, "ok": true,
        });
        assert_eq!(
            plain(&history),
            json!({
                "n": 3, "rate": "2.50", "on": "2026-11-27", "at": "2026-11-27T10:00:00Z",
                "total": {"amount": "12.50", "currency": "EUR"}, "w": {"quantity": "2", "unit": "kg"},
                "order": {"entity": "Order", "id": 7}, "tags": ["a", "1.5"], "state": "Shipped",
                "note": null, "ok": true,
            })
        );
    }

    #[test]
    fn the_names_of_the_fields_are_never_read_as_marks() {
        // fields named like the marks of the history's encoding: `date`, `money` and `cur`
        let fields = json!({ "date": {"date": "2026-11-27"}, "money": {"d": "3.5"}, "cur": "EUR" });
        assert_eq!(
            plain_fields(&fields),
            json!({ "date": "2026-11-27", "money": "3.5", "cur": "EUR" })
        );
        let one = json!({ "d": {"d": "2.5"} });
        assert_eq!(plain_fields(&one), json!({ "d": "2.5" }));
    }

    #[tokio::test]
    async fn a_configuration_to_fix_waits_without_a_time_limit() {
        let plan = Plan {
            id: 3,
            event: "E".into(),
            connection: "c".into(),
            attempts: 40,
            created_at: "2020-01-01T00:00:00".into(),
            fields: J::Null,
            retry: Retry::default(),
            limits: Default::default(),
            batch: Vec::new(),
            send: Send::Broken("the variable TOKEN is not set where the app runs".into()),
        };
        let client = http_client(false);
        match run(&plan, &client, false, None).await.0 {
            Outcome::Retry { error, .. } => assert!(error.contains("TOKEN"), "{error}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_window_opens_at_its_time_of_day_in_the_project_zone() {
        use quadra_ir::{BinOp, Expr, ExprKind, Lit};
        let span =
            quadra_ir::Span::new(quadra_sema::FileId(0), quadra_syntax::TextRange::default());
        let t = |s: &str| {
            Box::new(Expr {
                kind: ExprKind::Lit(Lit::Str(s.into())),
                span,
            })
        };
        let window = |a: &str, b: &str| Expr {
            kind: ExprKind::Binary {
                op: BinOp::Range,
                lhs: t(a),
                rhs: t(b),
            },
            span,
        };
        let at = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").unwrap();
        // 02:00–04:00 in Rome (UTC+1 in November): 01:00–03:00 UTC
        let w = window("02:00", "04:00");
        assert_eq!(
            window_opens("Europe/Rome", &w, at("2026-11-27T01:30")),
            None
        );
        assert_eq!(
            window_opens("Europe/Rome", &w, at("2026-11-27T00:10")),
            Some(at("2026-11-27T01:00"))
        );
        assert_eq!(
            window_opens("Europe/Rome", &w, at("2026-11-27T10:00")),
            Some(at("2026-11-28T01:00"))
        );
        // across midnight: 22:00–06:00 UTC
        let night = window("22:00", "06:00");
        assert_eq!(window_opens("", &night, at("2026-11-27T23:00")), None);
        assert_eq!(window_opens("", &night, at("2026-11-27T05:00")), None);
        assert_eq!(
            window_opens("", &night, at("2026-11-27T12:00")),
            Some(at("2026-11-27T22:00"))
        );
    }

    #[test]
    fn the_signature_is_hmac_sha256() {
        // RFC 4231, test case 2
        assert_eq!(
            sign("Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn a_plan_carries_the_connection_and_the_event() {
        let m = quadra_sema::manifest::parse(
            "[vars]\nTOKEN = { secret = true }\nKEY = { secret = true }\n[connections.shop]\nkind = \"http\"\nurl = \"https://shop.example.com/api/\"\nbearer = { var = \"TOKEN\" }\nsigning_secret = { var = \"KEY\" }\n",
        )
        .unwrap();
        let due = Due {
            id: 7,
            tx: 3,
            event: "OrderShipped".into(),
            fields: json!({"number": "A1"}),
            created_at: "2026-11-27T10:00:00".into(),
            attempts: 0,
            commit: "0123456789abcdef0123".into(),
        };
        let mut env = BTreeMap::new();
        let p = plan(&m, &env, due.clone(), "shop", Some("/hooks"), None);
        assert!(
            matches!(&p.send, Send::Broken(e) if e.contains("TOKEN")),
            "{:?}",
            p.send
        );
        env.insert("TOKEN".into(), "t0k".into());
        env.insert("KEY".into(), "k3y".into());
        let Send::Http {
            url,
            headers,
            body,
            creds,
            ..
        } = plan(&m, &env, due, "shop", Some("/hooks"), None).send
        else {
            panic!()
        };
        assert_eq!(url, "https://shop.example.com/api/hooks");
        assert!(headers.contains(&("Authorization".into(), "Bearer t0k".into())));
        assert!(headers.contains(&("Idempotency-Key".into(), "quadra-0123456789abcdef-7".into())));
        assert_eq!(creds.sign.map(|s| s.0).as_deref(), Some("k3y"));
        let b: J = serde_json::from_str(&body).unwrap();
        assert_eq!(b["data"]["number"], "A1");
        assert_eq!(b["event"], "OrderShipped");
    }
}
