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

use quadra_sema::manifest::{ConnectionKind, HttpAuth, Manifest, Setting};
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
    pub send: Send,
}

#[derive(Clone, Debug)]
pub enum Send {
    Http {
        url: String,
        headers: Vec<(String, String)>,
        body: String,
        /// the value that signs it (`signing_secret`)
        sign: Option<String>,
        timeout: Duration,
    },
    Smtp {
        url: String,
        login: Option<(String, String)>,
        from: String,
        to: String,
        subject: String,
        body: String,
        /// `smtp://` without TLS (`insecure = true`: a local test server)
        plain: bool,
    },
    /// it cannot be sent as configured (a value missing where it runs): tried again later,
    /// once the configuration is fixed and deployed
    Broken(String),
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

/// The plan of a due event of the outbound `(connection, path)`.
pub fn plan(
    manifest: &Manifest,
    env: &BTreeMap<String, String>,
    due: Due,
    connection: &str,
    path: Option<&str>,
) -> Plan {
    let base = |send| Plan {
        id: due.id,
        event: due.event.clone(),
        connection: connection.to_string(),
        attempts: due.attempts,
        created_at: due.created_at.clone(),
        fields: due.fields.clone(),
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
    let send = (|| -> Result<Send, String> {
        match &conn.kind {
            ConnectionKind::Http {
                url,
                auth,
                signing_secret,
                timeout_s,
                ..
            } => {
                let url = format!(
                    "{}{}",
                    setting(url)?.trim_end_matches('/'),
                    path.unwrap_or("")
                );
                let body = json!({
                    "id": due.id,
                    "event": due.event,
                    "tx": due.tx,
                    "created_at": format!("{}Z", due.created_at),
                    "data": plain_fields(&due.fields),
                })
                .to_string();
                let mut headers = vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("User-Agent".to_string(), "Quadra".to_string()),
                    ("Quadra-Event".to_string(), due.event.clone()),
                    ("Quadra-Delivery".to_string(), due.id.to_string()),
                    ("Idempotency-Key".to_string(), idempotency_key(&due)),
                ];
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
                }
                let sign = signing_secret.as_deref().map(value).transpose()?;
                Ok(Send::Http {
                    url,
                    headers,
                    body,
                    sign,
                    timeout: Duration::from_secs(*timeout_s),
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
                let url = setting(url)?;
                Ok(Send::Smtp {
                    plain: *insecure && url.starts_with("smtp://"),
                    url,
                    login,
                    from: setting(from)?,
                    to: field("to")?,
                    subject: field("subject")?,
                    body: field("body")?,
                })
            }
        }
    })();
    base(send.unwrap_or_else(Send::Broken))
}

/// HMAC-SHA256 of `message` with `key`, in hex.
pub fn sign(key: &str, message: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes()).expect("any key length");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
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
async fn addresses(host: &str, port: u16, public_only: bool) -> Result<Vec<SocketAddr>, String> {
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
    let mut b = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(10));
    if public_only {
        b = b.dns_resolver(std::sync::Arc::new(PublicResolver));
    }
    b.build().expect("an HTTP client")
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
pub async fn run(plan: &Plan, client: &reqwest::Client, public_only: bool) -> (Outcome, Attempt) {
    let started = std::time::Instant::now();
    let mut seen = Attempt {
        connection: plan.connection.clone(),
        ..Default::default()
    };
    let result = match &plan.send {
        Send::Broken(why) => Err(Failure::Waiting(why.clone())),
        Send::Http {
            url,
            headers,
            body,
            sign: key,
            timeout,
        } => {
            http(
                client,
                url,
                headers,
                body,
                key.as_deref(),
                *timeout,
                public_only,
                &mut seen,
            )
            .await
        }
        Send::Smtp {
            url,
            login,
            from,
            to,
            subject,
            body,
            plain,
        } => {
            smtp(
                url,
                login.as_ref(),
                from,
                to,
                subject,
                body,
                *plain,
                public_only,
                &mut seen,
            )
            .await
        }
    };
    seen.duration_ms = started.elapsed().as_millis() as u64;
    (outcome(plan, result), seen)
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
            if ages && now - first > chrono::Duration::hours(24) {
                return Outcome::Failed {
                    error: format!("{e} (tried for a day)"),
                };
            }
            // a jitter of its own, the same for the same event and attempt
            let jitter =
                ((plan.id.wrapping_mul(2654435761) ^ plan.attempts) % 1000) as f64 / 1000.0;
            let mut wait = quadra_store::deliveries::backoff(plan.attempts + 1, jitter);
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

async fn http(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &str,
    key: Option<&str>,
    timeout: Duration,
    public_only: bool,
    seen: &mut Attempt,
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
    let mut req = client.post(parsed).timeout(timeout).body(body.to_string());
    for (k, v) in headers {
        req = req.header(k, v);
    }
    if let Some(key) = key {
        let t = chrono::Utc::now().timestamp();
        req = req.header(
            "Quadra-Signature",
            format!("t={t},v1={}", sign(key, &format!("{t}.{body}"))),
        );
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
        let text = excerpt(resp, 500).await;
        seen.excerpt = (!text.trim().is_empty()).then_some(text);
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
    if status.is_redirection() {
        return Err(Failure::Never(format!(
            "{msg} (redirects are not followed: give the final url)"
        )));
    }
    match code {
        429 | 503 if retry_after.is_some() => Err(Failure::Later(msg, retry_after.unwrap())),
        408 | 429 => Err(Failure::Again(msg)),
        // credentials refused: a token to renew, not a reason to drop the event
        401 | 403 => Err(Failure::Again(format!(
            "{msg} (the connection's credentials were refused: check them)"
        ))),
        _ if status.is_server_error() => Err(Failure::Again(msg)),
        _ => Err(Failure::Never(msg)),
    }
}

/// The first `chars` characters of a response's body, reading no more than 64 KB of it: a huge
/// or endless answer does not fill the node's memory.
async fn excerpt(mut resp: reqwest::Response, chars: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < 64 << 10 {
        match resp.chunk().await {
            Ok(Some(c)) => buf.extend_from_slice(&c),
            _ => break,
        }
    }
    String::from_utf8_lossy(&buf).chars().take(chars).collect()
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
        .subject(subject)
        .header(lettre::message::header::ContentType::TEXT_PLAIN)
        .body(body.to_string())
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
            send: Send::Broken("the variable TOKEN is not set where the app runs".into()),
        };
        let client = http_client(false);
        match run(&plan, &client, false).await.0 {
            Outcome::Retry { error, .. } => assert!(error.contains("TOKEN"), "{error}"),
            other => panic!("{other:?}"),
        }
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
        let p = plan(&m, &env, due.clone(), "shop", Some("/hooks"));
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
            sign,
            ..
        } = plan(&m, &env, due, "shop", Some("/hooks")).send
        else {
            panic!()
        };
        assert_eq!(url, "https://shop.example.com/api/hooks");
        assert!(headers.contains(&("Authorization".into(), "Bearer t0k".into())));
        assert!(headers.contains(&("Idempotency-Key".into(), "quadra-0123456789abcdef-7".into())));
        assert_eq!(sign.as_deref(), Some("k3y"));
        let b: J = serde_json::from_str(&body).unwrap();
        assert_eq!(b["data"]["number"], "A1");
        assert_eq!(b["event"], "OrderShipped");
    }
}
