//! Mailboxes read over IMAP (`kind = "imap"`), PEC included: `inbound PecIn via pec as m:
//! PecMail:` with `pull "INBOX" every 2 minutes`.
//!
//! A pull first **settles** the messages handled for good since the last one: each is marked
//! `\Seen` and moved to `Quadra/processed` (done, skipped, superseded) or `Quadra/rejected`
//! (rejected), folders created when missing, with the server's own hierarchy separator
//! (`Quadra.processed` on a server that uses `.`). `UID MOVE` when the server has it (RFC
//! 6851), else `UID COPY`, `\Deleted` and `UID EXPUNGE` (RFC 4315), or a plain `EXPUNGE` on a
//! server without UIDPLUS. A message no longer there (moved by someone else) counts as settled.
//!
//! Then it **takes** the messages after its cursor, oldest first: the cursor is the
//! mailbox's `UIDVALIDITY` and the last UID taken (`1700000000:0000000042`), kept by the engine
//! with the messages in one write, so a restart neither loses nor takes again what it took. A
//! message is fetched with `BODY.PEEK[]`: taking it does not mark it read; only settling does.
//! If the server renumbers the mailbox (a new `UIDVALIDITY`), the messages still there are taken
//! again: an inbound with `key m.message_id` sees them as duplicates.
//!
//! Each message is an item named `<uidvalidity>:<uid>:<message-id>` (what settling finds it
//! by: the UID, or the Message-ID once the UIDs changed), its body a JSON document of the mail
//! ([`document`]): headers, text, HTML, attachments, and for a PEC what its `daticert.xml`
//! certifies and the message it carries. The PEC's signature (`smime.p7s`) is not verified:
//! the mailbox is the provider's, reached over TLS, and the provider is who signed it.
//!
//! Limits: a message larger than 32 MiB is taken with its headers only (`truncated: true`,
//! no bodies or attachments); an attachment larger than 16 MiB, or past 24 MiB of attachments
//! in one mail, has no `content` (`truncated: true`, its size and SHA-256 still given). A pull
//! takes at most `limit` messages and about 64 MiB; the rest come at the next one.

use super::{Ask, Failed, Fetched, Item, Resolved, Settle};
use base64::Engine as _;
use mail_parser::{MessageParser, MimeHeaders, PartType};
use quadra_sema::manifest::ConnectionKind;
use serde_json::{json, Value as J};
use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

/// A message larger than this is taken with its headers only.
pub const MAX_MESSAGE: u32 = 32 << 20;
/// An attachment larger than this has no `content`.
pub const MAX_ATTACHMENT: usize = 16 << 20;
/// The attachments of one mail carry at most this much `content` together.
pub const MAX_CONTENT: usize = 24 << 20;
/// A pull stops taking messages past this many bytes (at least one is taken).
const MAX_PULL: u64 = 64 << 20;

/// The folders settled messages go to, in `Quadra`.
pub const PROCESSED: &str = "processed";
pub const REJECTED: &str = "rejected";
const PARENT: &str = "Quadra";

pub async fn fetch(
    c: &Resolved,
    ask: &Ask,
    public_only: bool,
    consent: Option<&crate::oauth::Use>,
) -> Result<Fetched, Failed> {
    let Ask::Mail {
        mailbox,
        settle,
        limit,
        after,
    } = ask
    else {
        return Err(Failed::Never("not a pull of a mailbox".into()));
    };
    let ConnectionKind::Mail {
        url,
        user,
        password,
        oauth,
        timeout_s,
        insecure,
    } = &c.connection.kind
    else {
        return Err(Failed::Never("not a mailbox".into()));
    };
    let name = &c.connection.name;
    let url = c.setting(url).map_err(Failed::Waiting)?;
    let user = c.setting(user).map_err(Failed::Waiting)?;
    let (tls, host, port) = address(&url, *insecure).map_err(Failed::Waiting)?;
    let timeout = Duration::from_secs(*timeout_s);
    let auth = match (password, oauth) {
        (Some(p), _) => Auth::Password(c.value(p).map_err(Failed::Waiting)?),
        (None, Some(_)) => {
            let u = consent.ok_or_else(|| {
                Failed::Waiting(format!(
                    "`{name}` uses OAuth2 with the user's consent: only the server has its tokens"
                ))
            })?;
            let client = crate::deliver::http_client(public_only);
            Auth::Token(crate::oauth::bearer(u, &client, public_only, timeout).await?)
        }
        (None, None) => return Err(Failed::Waiting(format!("`{name}` has no password"))),
    };
    let job = Job {
        host: &host,
        port,
        tls,
        user: &user,
        mailbox,
        settle,
        limit: *limit as usize,
        after: after.as_deref(),
        public_only,
        timeout,
    };
    match job.bounded(&auth).await {
        Ok(f) => Ok(f),
        Err(Refused::Other(f)) => Err(f),
        // the token refused: refreshed once (it may have been revoked or expired early)
        Err(Refused::Login(why)) => match (&auth, consent) {
            (Auth::Token(t), Some(u)) => {
                let client = crate::deliver::http_client(public_only);
                let t = crate::oauth::refused(u, t, &client, public_only, timeout).await?;
                match job.bounded(&Auth::Token(t)).await {
                    Ok(f) => Ok(f),
                    Err(Refused::Other(f)) => Err(f),
                    Err(Refused::Login(why)) => Err(Failed::Waiting(format!(
                        "{host} refused the token of `{user}` ({why}): check the scope the consent gave (Gmail: https://mail.google.com/; Microsoft 365: https://outlook.office.com/IMAP.AccessAsUser.All offline_access)"
                    ))),
                }
            }
            _ => Err(Failed::Never(format!(
                "{host} refused the login of `{user}` ({why}): check the user and the password"
            ))),
        },
    }
}

/// `imaps://host[:993]` (TLS), `imap://host[:143]` (plain, with `insecure = true` only).
fn address(url: &str, insecure: bool) -> Result<(bool, String, u16), String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("imaps://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("imap://") {
        if !insecure {
            return Err("plain imap:// only with `insecure = true`".into());
        }
        (false, r)
    } else {
        return Err(format!("{url}: the url of a mailbox starts with imaps://"));
    };
    let hostport = rest.split('/').next().unwrap_or("");
    let default = if tls { 993 } else { 143 };
    let (host, port) = match hostport.rsplit_once(':') {
        // `[::1]` alone has colons but no port
        Some((h, p)) if !hostport.starts_with('[') || h.ends_with(']') => match p.parse() {
            Ok(p) => (h, p),
            Err(_) => (hostport, default),
        },
        _ => (hostport, default),
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if host.is_empty() {
        return Err(format!("{url}: no host"));
    }
    Ok((tls, host, port))
}

enum Auth {
    Password(String),
    Token(String),
}

/// Why a conversation stopped: the login refused, or anything else.
enum Refused {
    Login(String),
    Other(Failed),
}

impl From<Failed> for Refused {
    fn from(f: Failed) -> Refused {
        Refused::Other(f)
    }
}

/// `AUTHENTICATE XOAUTH2` (Gmail, Microsoft 365): the user and the token, once; an empty answer
/// to the error the server sends before it refuses.
struct XOAuth2 {
    user: String,
    token: String,
    sent: bool,
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;
    fn process(&mut self, _challenge: &[u8]) -> String {
        if self.sent {
            return String::new();
        }
        self.sent = true;
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
    }
}

struct Job<'a> {
    host: &'a str,
    port: u16,
    tls: bool,
    user: &'a str,
    mailbox: &'a str,
    settle: &'a [Settle],
    limit: usize,
    after: Option<&'a str>,
    public_only: bool,
    timeout: Duration,
}

fn again(host: &str, e: impl std::fmt::Display) -> Failed {
    Failed::Again(format!("{host}: {e}"))
}

impl Job<'_> {
    /// The whole conversation within a bound of its own: a server that stops answering midway.
    async fn bounded(&self, auth: &Auth) -> Result<Fetched, Refused> {
        let budget = (self.timeout * 20).max(Duration::from_secs(200));
        match tokio::time::timeout(budget, self.run(auth)).await {
            Ok(r) => r,
            Err(_) => Err(Failed::Again(format!(
                "{}: no end within {}s",
                self.host,
                budget.as_secs()
            ))
            .into()),
        }
    }

    async fn run(&self, auth: &Auth) -> Result<Fetched, Refused> {
        let host = self.host;
        let addrs = crate::deliver::addresses(host, self.port, self.public_only)
            .await
            .map_err(|e| {
                if e.contains("not a public address") {
                    Failed::Never(e)
                } else {
                    Failed::Again(e)
                }
            })?;
        let secs = self.timeout.as_secs();
        let tcp = tokio::time::timeout(self.timeout, tokio::net::TcpStream::connect(&addrs[..]))
            .await
            .map_err(|_| Failed::Again(format!("{host}: no connection within {secs}s")))?
            .map_err(|e| again(host, e))?;
        if !self.tls {
            return self.session(tcp, auth).await;
        }
        let config = tls_config().map_err(|e| Failed::Waiting(format!("TLS: {e}")))?;
        let server = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|e| Failed::Never(format!("{host}: {e}")))?;
        let stream = tokio::time::timeout(
            self.timeout,
            tokio_rustls::TlsConnector::from(config).connect(server, tcp),
        )
        .await
        .map_err(|_| Failed::Again(format!("{host}: no TLS handshake within {secs}s")))?
        .map_err(|e| again(host, format!("TLS: {e}")))?;
        self.session(stream, auth).await
    }

    async fn session<T>(&self, stream: T, auth: &Auth) -> Result<Fetched, Refused>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + std::fmt::Debug + Send,
    {
        let host = self.host;
        let mut client = async_imap::Client::new(stream);
        let greeting = tokio::time::timeout(self.timeout, client.read_response())
            .await
            .map_err(|_| {
                Failed::Again(format!(
                    "{host}: no greeting within {}s",
                    self.timeout.as_secs()
                ))
            })?
            .map_err(|e| again(host, e))?;
        if greeting.is_none() {
            return Err(again(host, "the server closed the connection").into());
        }
        let login = match auth {
            Auth::Password(p) => client.login(self.user, p).await,
            Auth::Token(t) => {
                client
                    .authenticate(
                        "XOAUTH2",
                        XOAuth2 {
                            user: self.user.to_string(),
                            token: t.clone(),
                            sent: false,
                        },
                    )
                    .await
            }
        };
        let mut s = match login {
            Ok(s) => s,
            Err((async_imap::error::Error::No(why), _))
            | Err((async_imap::error::Error::Bad(why), _)) => {
                return Err(Refused::Login(said(&why)))
            }
            Err((e, _)) => return Err(again(host, e).into()),
        };
        let r = self.work(&mut s).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), s.logout()).await;
        r.map_err(Refused::Other)
    }

    async fn work<T>(&self, s: &mut async_imap::Session<T>) -> Result<Fetched, Failed>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + std::fmt::Debug + Send,
    {
        use futures_util::TryStreamExt;
        let host = self.host;
        let caps = s.capabilities().await.map_err(|e| again(host, e))?;
        let (can_move, uidplus) = (caps.has_str("MOVE"), caps.has_str("UIDPLUS"));
        let mb = s.select(self.mailbox).await.map_err(|e| match e {
            async_imap::error::Error::No(why) => Failed::Never(format!(
                "{host}: the mailbox `{}` cannot be opened: {}",
                self.mailbox,
                said(&why)
            )),
            e => again(host, e),
        })?;
        let validity = mb.uid_validity.unwrap_or(0);

        // 1. what was handled goes to Quadra/processed or Quadra/rejected
        let mut settled = Vec::new();
        if !self.settle.is_empty() {
            let delim = {
                let names: Vec<_> = s
                    .list(Some(""), None)
                    .await
                    .map_err(|e| again(host, e))?
                    .try_collect()
                    .await
                    .map_err(|e| again(host, e))?;
                names
                    .first()
                    .and_then(|n| n.delimiter().map(str::to_string))
                    .unwrap_or_else(|| "/".to_string())
            };
            // the UID of each in this numbering
            let mut targets: BTreeMap<bool, Vec<(String, u32)>> = BTreeMap::new();
            for st in self.settle {
                let mut parts = st.name.splitn(3, ':');
                let v: Option<u32> = parts.next().and_then(|v| v.parse().ok());
                let uid: Option<u32> = parts.next().and_then(|u| u.parse().ok());
                let mid = parts.next().unwrap_or("");
                let uid = match (v, uid) {
                    (Some(v), Some(u)) if v == validity => Some(u),
                    _ if !mid.is_empty() => s
                        .uid_search(format!("HEADER Message-ID {}", quoted(&format!("<{mid}>"))))
                        .await
                        .map_err(|e| again(host, e))?
                        .into_iter()
                        .min(),
                    _ => None,
                };
                match uid {
                    Some(u) => targets.entry(st.ok).or_default().push((st.name.clone(), u)),
                    // renumbered and without a Message-ID: nothing to find it by
                    None => settled.push(st.name.clone()),
                }
            }
            for (ok, list) in targets {
                // only those still here (someone may have moved them)
                let here: HashSet<u32> = s
                    .uid_search(format!("UID {}", uid_set(list.iter().map(|(_, u)| *u))))
                    .await
                    .map_err(|e| again(host, e))?;
                let present: Vec<u32> = list
                    .iter()
                    .map(|(_, u)| *u)
                    .filter(|u| here.contains(u))
                    .collect();
                if !present.is_empty() {
                    let folder =
                        format!("{PARENT}{delim}{}", if ok { PROCESSED } else { REJECTED });
                    // created when missing (an error here is that it exists: the move tells)
                    let _ = s.create(&folder).await;
                    let set = uid_set(present.iter().copied());
                    store(s, &set, "+FLAGS.SILENT (\\Seen)", host).await?;
                    if can_move {
                        s.uid_mv(&set, &folder)
                            .await
                            .map_err(|e| again(host, format!("moving to {folder}: {e}")))?;
                    } else {
                        s.uid_copy(&set, &folder)
                            .await
                            .map_err(|e| again(host, format!("copying to {folder}: {e}")))?;
                        store(s, &set, "+FLAGS.SILENT (\\Deleted)", host).await?;
                        if uidplus {
                            let st = s.uid_expunge(&set).await.map_err(|e| again(host, e))?;
                            let _: Vec<_> = st.try_collect().await.map_err(|e| again(host, e))?;
                        } else {
                            let st = s.expunge().await.map_err(|e| again(host, e))?;
                            let _: Vec<_> = st.try_collect().await.map_err(|e| again(host, e))?;
                        }
                    }
                }
                settled.extend(list.into_iter().map(|(n, _)| n));
            }
        }

        // 2. the messages after the cursor
        let start = match self.after.and_then(cursor) {
            Some((v, u)) if v == validity => u.saturating_add(1),
            _ => 1,
        };
        let mut uids: Vec<u32> = s
            .uid_search(format!("UID {start}:* NOT DELETED"))
            .await
            .map_err(|e| again(host, e))?
            .into_iter()
            // `n:*` always names the last message, even one below n
            .filter(|u| *u >= start)
            .collect();
        uids.sort_unstable();
        uids.truncate(self.limit.max(1));
        let mut items = Vec::new();
        if uids.is_empty() {
            return Ok(Fetched { items, settled });
        }
        let sizes: BTreeMap<u32, u32> = {
            let st = s
                .uid_fetch(uid_set(uids.iter().copied()), "(UID RFC822.SIZE)")
                .await
                .map_err(|e| again(host, e))?;
            let all: Vec<_> = st.try_collect().await.map_err(|e| again(host, e))?;
            all.iter()
                .filter_map(|f| Some((f.uid?, f.size.unwrap_or(0))))
                .collect()
        };
        let mut taken: u64 = 0;
        for uid in uids {
            let Some(&size) = sizes.get(&uid) else {
                continue;
            };
            let whole = size <= MAX_MESSAGE;
            if !items.is_empty() && taken + size.min(MAX_MESSAGE) as u64 > MAX_PULL {
                break;
            }
            let query = if whole {
                "(UID BODY.PEEK[])"
            } else {
                "(UID BODY.PEEK[HEADER])"
            };
            let st = s
                .uid_fetch(uid.to_string(), query)
                .await
                .map_err(|e| again(host, e))?;
            let all: Vec<_> = st.try_collect().await.map_err(|e| again(host, e))?;
            let Some(f) = all.iter().find(|f| f.uid == Some(uid)) else {
                continue;
            };
            let raw = if whole { f.body() } else { f.header() }.unwrap_or_default();
            taken += raw.len() as u64;
            let mut doc = document(raw);
            if !whole {
                doc["truncated"] = json!(true);
                doc["size"] = json!(size);
            }
            let mid = doc["message_id"].as_str().unwrap_or("").to_string();
            items.push(Item {
                name: format!("{validity}:{uid}:{mid}"),
                body: serde_json::to_vec(&doc).expect("a mail as JSON"),
                cursor: Some(format!("{validity:010}:{uid:010}")),
            });
        }
        Ok(Fetched { items, settled })
    }
}

/// `UID STORE`, its answers read.
async fn store<T>(
    s: &mut async_imap::Session<T>,
    set: &str,
    what: &str,
    host: &str,
) -> Result<(), Failed>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + std::fmt::Debug + Send,
{
    use futures_util::TryStreamExt;
    let st = s.uid_store(set, what).await.map_err(|e| again(host, e))?;
    let _: Vec<_> = st.try_collect().await.map_err(|e| again(host, e))?;
    Ok(())
}

/// What the server said with a `NO` or a `BAD` (the client writes `code: …, info: Some("…")`).
fn said(why: &str) -> String {
    why.split_once("info: Some(\"")
        .and_then(|(_, rest)| rest.rsplit_once("\")"))
        .map(|(text, _)| text.replace("\\\"", "\""))
        .unwrap_or_else(|| why.to_string())
}

/// The cursor written by a pull: (UIDVALIDITY, the last UID taken).
fn cursor(c: &str) -> Option<(u32, u32)> {
    let (v, u) = c.split_once(':')?;
    Some((v.parse().ok()?, u.parse().ok()?))
}

/// `1,2,5`.
fn uid_set(uids: impl Iterator<Item = u32>) -> String {
    uids.map(|u| u.to_string()).collect::<Vec<_>>().join(",")
}

/// A string as IMAP quotes it.
fn quoted(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace(['\r', '\n'], "")
    )
}

/// TLS with the system's trusted authorities.
fn tls_config() -> Result<std::sync::Arc<rustls::ClientConfig>, String> {
    use rustls_platform_verifier::BuilderVerifierExt;
    type Config = Result<std::sync::Arc<rustls::ClientConfig>, String>;
    static C: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_platform_verifier()
            .map_err(|e| e.to_string())
            .map(|b| std::sync::Arc::new(b.with_no_client_auth()))
    })
    .clone()
}

/// The JSON document of a mail, as an inbound reads it (`record … from json`):
///
/// ```json
/// {"message_id": "…", "from": {"name": "…", "address": "…"}, "to": [{…}], "cc": [], "reply_to": [],
///  "subject": "…", "date": "2026-11-27T09:14:03Z", "text": "…", "html": null,
///  "attachments": [{"name", "content_type", "size", "sha256", "content", "text", "truncated"}],
///  "pec": null | {"type", "error", "error_detail", "id", "message_id", "sender", "recipients",
///                 "reply_to", "subject", "provider", "date", "receipt", "delivered_to", "original"}}
/// ```
///
/// Message-ids are written without `<` `>`; instants in UTC. The same bytes always give the
/// same document (what tells a duplicate). Bytes that are not a mail at all give a document
/// with `"error"`.
pub fn document(raw: &[u8]) -> J {
    match MessageParser::default().parse(raw) {
        Some(m) => mail(&m, true),
        None => json!({ "error": "not an email message", "size": raw.len() }),
    }
}

/// The parts of a PEC that `pec` reads, left out of its attachments.
const PEC_PARTS: &[&str] = &["daticert.xml", "postacert.eml", "smime.p7s"];

fn mail(m: &mail_parser::Message<'_>, outer: bool) -> J {
    let addr = |a: &mail_parser::Addr<'_>| json!({ "name": a.name(), "address": a.address() });
    let list = |a: Option<&mail_parser::Address<'_>>| -> J {
        J::Array(a.map(|a| a.iter().map(addr).collect()).unwrap_or_default())
    };
    let header = |n: &str| m.header_raw(n).map(|v| v.trim().to_string());
    let named = |name: &str| {
        m.attachments().find(|p| {
            p.attachment_name()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
    };
    // a PEC: a transport envelope (`X-Trasporto`) or a receipt (`X-Ricevuta`), with its daticert
    let pec_header = header("X-Ricevuta").or_else(|| header("X-Trasporto"));
    let daticert = named("daticert.xml");
    let is_pec = outer && (pec_header.is_some() || daticert.is_some());
    let pec = if is_pec {
        let mut p = daticert
            .map(|d| daticert_of(part_bytes(d)))
            .unwrap_or_else(|| json!({ "type": pec_header }));
        // the message carried (`posta-certificata`, a complete `avvenuta-consegna`)
        let original = m
            .attachments()
            .find(|p| {
                p.is_message()
                    && p.attachment_name()
                        .is_none_or(|n| n.eq_ignore_ascii_case("postacert.eml"))
            })
            .and_then(|p| p.message())
            .map(|o| mail(o, false));
        p["original"] = original.unwrap_or(J::Null);
        p
    } else {
        J::Null
    };
    let text = m
        .text_part(0)
        .filter(|p| p.is_text() && !p.is_text_html())
        .and_then(|p| p.text_contents())
        .map(str::to_string);
    let html = m
        .html_part(0)
        .filter(|p| p.is_text_html())
        .and_then(|p| p.text_contents())
        .map(str::to_string);
    let mut budget = MAX_CONTENT;
    let attachments: Vec<J> = m
        .attachments()
        .enumerate()
        .filter(|(_, p)| {
            !is_pec
                || !p
                    .attachment_name()
                    .is_some_and(|n| PEC_PARTS.iter().any(|x| n.eq_ignore_ascii_case(x)))
        })
        .map(|(i, p)| attachment(p, i, &mut budget))
        .collect();
    let mut doc = json!({
        "message_id": m.message_id().map(bare),
        "from": m.from().and_then(|a| a.first()).map(addr),
        "to": list(m.to()),
        "cc": list(m.cc()),
        "reply_to": list(m.reply_to()),
        "subject": m.subject(),
        "date": m.date().map(|d| iso(d.to_timestamp())),
        "text": text,
        "html": html,
        "attachments": attachments,
    });
    if outer {
        doc["pec"] = pec;
    }
    doc
}

/// A message-id without its `<` `>` and spaces.
fn bare(id: &str) -> String {
    id.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_string()
}

fn iso(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// The bytes of a part as sent (decoded from base64 or quoted-printable; text in UTF-8).
fn part_bytes<'a>(p: &'a mail_parser::MessagePart<'_>) -> &'a [u8] {
    match &p.body {
        PartType::Text(t) | PartType::Html(t) => t.as_bytes(),
        PartType::Binary(b) | PartType::InlineBinary(b) => b.as_ref(),
        PartType::Message(m) => m.raw_message(),
        PartType::Multipart(_) => &[],
    }
}

fn attachment(p: &mail_parser::MessagePart<'_>, i: usize, budget: &mut usize) -> J {
    use sha2::Digest;
    let bytes = part_bytes(p);
    let content_type = p
        .content_type()
        .map(|c| match c.subtype() {
            Some(s) => format!("{}/{}", c.ctype(), s),
            None => c.ctype().to_string(),
        })
        .unwrap_or_else(|| "application/octet-stream".into())
        .to_ascii_lowercase();
    let name = p.attachment_name().map(str::to_string).unwrap_or_else(|| {
        if p.is_message() {
            format!("message-{}.eml", i + 1)
        } else {
            format!("attachment-{}", i + 1)
        }
    });
    let fits = bytes.len() <= MAX_ATTACHMENT && bytes.len() <= *budget;
    if fits {
        *budget -= bytes.len();
    }
    let lower = name.to_ascii_lowercase();
    let textual = content_type.starts_with("text/")
        || content_type.ends_with("/xml")
        || content_type.ends_with("+xml")
        || content_type.ends_with("/json")
        || content_type == "message/rfc822"
        || [".xml", ".txt", ".csv", ".json", ".eml"]
            .iter()
            .any(|e| lower.ends_with(e));
    let text = if fits && textual {
        std::str::from_utf8(bytes)
            .ok()
            .map(|t| t.trim_start_matches('\u{feff}').to_string())
    } else {
        None
    };
    json!({
        "name": name,
        "content_type": content_type,
        "size": bytes.len(),
        "sha256": hex::encode(sha2::Sha256::digest(bytes)),
        "content": fits.then(|| base64::engine::general_purpose::STANDARD.encode(bytes)),
        "text": text,
        "truncated": !fits,
    })
}

/// What a PEC's `daticert.xml` certifies (DPR 68/2005 and the technical rules of DM 2 November
/// 2005): its type (`posta-certificata`, `accettazione`, `avvenuta-consegna`,
/// `non-accettazione`, `errore-consegna`, …), the error, the provider's id of the message and
/// the Message-ID of the message certified, the sender, the recipients, the subject, the
/// provider, the date (in UTC), the kind of receipt and to whom it was delivered. Its DTD is
/// named, never read.
pub fn daticert_of(xml: &[u8]) -> J {
    use quick_xml::events::Event;
    let text = String::from_utf8_lossy(xml);
    let mut r = quick_xml::Reader::from_str(&text);
    let mut path: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut out = json!({
        "type": null, "error": null, "error_detail": null, "id": null, "message_id": null,
        "sender": null, "recipients": [], "reply_to": null, "subject": null, "provider": null,
        "date": null, "receipt": null, "delivered_to": null,
    });
    let mut recipient: Option<String> = None;
    let (mut zone, mut day, mut time) = (String::new(), String::new(), String::new());
    let attr = |e: &quick_xml::events::BytesStart<'_>, name: &str| -> Option<String> {
        e.attributes()
            .flatten()
            .find(|a| a.key.local_name().as_ref() == name)
            .and_then(|a| {
                a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .ok()
                    .map(|v| v.to_string())
            })
    };
    loop {
        match r.read_event() {
            Ok(ev @ (Event::Start(_) | Event::Empty(_))) => {
                let (Event::Start(e) | Event::Empty(e)) = &ev else {
                    unreachable!()
                };
                let e = e.clone();
                let name = e.local_name().as_ref().to_string();
                match name.as_str() {
                    "postacert" => {
                        out["type"] = json!(attr(&e, "tipo"));
                        out["error"] = json!(attr(&e, "errore"));
                    }
                    "destinatari" => recipient = attr(&e, "tipo"),
                    "data" => zone = attr(&e, "zona").unwrap_or_default(),
                    "ricevuta" => out["receipt"] = json!(attr(&e, "tipo")),
                    _ => {}
                }
                buf.clear();
                // an empty element (`<ricevuta tipo="completa"/>`) has no end
                if matches!(ev, Event::Start(_)) {
                    path.push(name);
                }
            }
            Ok(Event::Text(t)) => buf.push_str(&t),
            Ok(Event::CData(t)) => buf.push_str(&t),
            Ok(Event::GeneralRef(g)) => match g.resolve_char_ref() {
                Ok(Some(c)) => buf.push(c),
                _ => buf.push_str(match &*g {
                    "lt" => "<",
                    "gt" => ">",
                    "amp" => "&",
                    "quot" => "\"",
                    "apos" => "'",
                    _ => "",
                }),
            },
            Ok(Event::End(_)) => {
                let name = path.pop().unwrap_or_default();
                let v = buf.trim().to_string();
                buf.clear();
                if v.is_empty() {
                    continue;
                }
                match name.as_str() {
                    "mittente" => out["sender"] = json!(v),
                    "destinatari" => out["recipients"]
                        .as_array_mut()
                        .expect("a list")
                        .push(json!({ "address": v, "type": recipient.take() })),
                    "risposte" => out["reply_to"] = json!(v),
                    "oggetto" => out["subject"] = json!(v),
                    "gestore-emittente" => out["provider"] = json!(v),
                    "giorno" => day = v,
                    "ora" => time = v,
                    "identificativo" => out["id"] = json!(bare(&v)),
                    "msgid" => out["message_id"] = json!(bare(&v)),
                    "consegna" => out["delivered_to"] = json!(v),
                    "errore-esteso" => out["error_detail"] = json!(v),
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    // `27/11/2026` `10:14:03` in the zone `+0100`: an instant, in UTC
    let offset = {
        let z = zone.trim();
        let n = |a: usize, b: usize| z.get(a..b).and_then(|d| d.parse::<i64>().ok());
        match (z.len(), n(1, 3), n(3, 5)) {
            (5, Some(h), Some(m)) => (h * 3600 + m * 60) * if z.starts_with('-') { -1 } else { 1 },
            _ => 0,
        }
    };
    out["date"] = json!(chrono::NaiveDateTime::parse_from_str(
        &format!("{day} {time}"),
        "%d/%m/%Y %H:%M:%S"
    )
    .ok()
    .map(|local| iso(local.and_utc().timestamp() - offset)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/it/mail")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn a_plain_mail_is_its_headers_bodies_and_attachments() {
        let d = document(&fixture("plain.eml"));
        assert_eq!(d["message_id"], "20261127.4711@shop.example.com");
        assert_eq!(d["from"]["address"], "orders@shop.example.com");
        assert_eq!(d["from"]["name"], "Shop Orders");
        assert_eq!(d["to"][0]["address"], "acme@example.com");
        assert_eq!(d["subject"], "Order 1042");
        assert_eq!(d["date"], "2026-11-27T09:14:03Z");
        assert!(d["text"].as_str().unwrap().contains("Thank you"));
        assert!(d["html"].as_str().unwrap().contains("<b>Thank you</b>"));
        assert_eq!(d["pec"], J::Null);
        let a = &d["attachments"][0];
        assert_eq!(a["name"], "order-1042.csv");
        assert_eq!(a["content_type"], "text/csv");
        assert_eq!(a["text"], "sku;qty\nA-1;2\n");
        assert_eq!(a["size"], 14);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(a["content"].as_str().unwrap())
                .unwrap(),
            b"sku;qty\nA-1;2\n"
        );
        assert_eq!(a["truncated"], false);
        assert_eq!(a["sha256"].as_str().unwrap().len(), 64);
        // the same bytes, the same document
        assert_eq!(document(&fixture("plain.eml")), d);
    }

    #[test]
    fn a_pec_carries_what_its_daticert_certifies_and_the_original_message() {
        let d = document(&fixture("posta-certificata.eml"));
        let p = &d["pec"];
        assert_eq!(p["type"], "posta-certificata");
        assert_eq!(p["error"], "nessuno");
        assert_eq!(p["sender"], "fatture@pec.fornitore.example.it");
        assert_eq!(p["recipients"][0]["address"], "acme@pec.example.it");
        assert_eq!(p["recipients"][0]["type"], "certificato");
        assert_eq!(p["subject"], "Fattura 2026/118");
        assert_eq!(p["provider"], "Example PEC S.p.A.");
        assert_eq!(p["date"], "2026-11-27T09:14:03Z");
        assert_eq!(
            p["message_id"],
            "FAT2026118.20261127@mail.fornitore.example.it"
        );
        assert_eq!(
            p["id"],
            "opec21031.20261127101403.04711.06.1.1@pec.example.it"
        );
        // the PEC's own parts are read, not listed
        assert_eq!(d["attachments"], json!([]));
        assert_eq!(d["message_id"], p["id"]);
        let o = &p["original"];
        assert_eq!(
            o["message_id"],
            "FAT2026118.20261127@mail.fornitore.example.it"
        );
        assert_eq!(o["subject"], "Fattura 2026/118");
        assert_eq!(o["from"]["address"], "fatture@pec.fornitore.example.it");
        assert_eq!(o["attachments"][0]["name"], "IT01234567890_00118.xml");
        assert!(o["attachments"][0]["text"]
            .as_str()
            .unwrap()
            .contains("<Numero>2026/118</Numero>"));
        assert!(o.get("pec").is_none());

        let a = document(&fixture("accettazione.eml"));
        assert_eq!(a["pec"]["type"], "accettazione");
        assert_eq!(a["pec"]["message_id"], "PEC-OUT-77@acme.example.it");
        assert_eq!(a["pec"]["original"], J::Null);
        let c = document(&fixture("avvenuta-consegna.eml"));
        assert_eq!(c["pec"]["type"], "avvenuta-consegna");
        assert_eq!(c["pec"]["receipt"], "completa");
        assert_eq!(c["pec"]["delivered_to"], "clienti@pec.cliente.example.it");
        assert_eq!(c["pec"]["message_id"], "PEC-OUT-77@acme.example.it");
        assert_eq!(c["pec"]["date"], "2026-11-27T09:20:11Z");
        assert_eq!(
            c["pec"]["original"]["message_id"],
            "PEC-OUT-77@acme.example.it"
        );
    }

    #[test]
    fn huge_attachments_keep_their_size_and_hash_but_not_their_content() {
        let big = "x".repeat(MAX_ATTACHMENT + 1);
        let raw = format!("From: a@example.com\r\nTo: b@example.com\r\nSubject: big\r\nMessage-ID: <big@example.com>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n--b\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"big.bin\"\r\n\r\n{big}\r\n--b--\r\n");
        let d = document(raw.as_bytes());
        let a = &d["attachments"][0];
        assert_eq!(a["truncated"], true);
        assert_eq!(a["content"], J::Null);
        assert!(a["size"].as_u64().unwrap() > MAX_ATTACHMENT as u64);
        assert_eq!(d["text"], "see attached");
    }

    #[test]
    fn urls_cursors_and_sets() {
        assert_eq!(
            address("imaps://imap.example.com", false).unwrap(),
            (true, "imap.example.com".into(), 993)
        );
        assert_eq!(
            address("imap://127.0.0.1:3143", true).unwrap(),
            (false, "127.0.0.1".into(), 3143)
        );
        assert_eq!(
            address("imaps://[::1]", false).unwrap(),
            (true, "::1".into(), 993)
        );
        assert!(address("imap://h", false).is_err());
        assert_eq!(cursor("1700000000:0000000042"), Some((1700000000, 42)));
        assert_eq!(uid_set([1, 5, 9].into_iter()), "1,5,9");
        assert_eq!(quoted("<a\"b@x>"), "\"<a\\\"b@x>\"");
        assert_eq!(
            said(
                "code: None, info: Some(\"[AUTHENTICATIONFAILED] Invalid credentials (Failure)\")"
            ),
            "[AUTHENTICATIONFAILED] Invalid credentials (Failure)"
        );
        assert_eq!(said("refused"), "refused");
    }
}
