//! The variables and connections a project declares in its `quadra.toml`.
//!
//! ```toml
//! [vars]
//! SHOP_NAME = { type = "Text", default = "Acme" }
//! SHOP_TOKEN = { secret = true }
//! SMTP_PASSWORD = { secret = true }
//!
//! [connections.shop]
//! kind = "http"
//! url = "https://acme.example.com/api"
//! bearer = { var = "SHOP_TOKEN" }
//!
//! [connections.mail]
//! kind = "smtp"
//! url = "smtps://smtp.example.com:465"
//! user = "noreply@acme.example.com"
//! password = { var = "SMTP_PASSWORD" }
//! from = "Acme <noreply@acme.example.com>"
//! ```
//!
//! A variable has a type (`Text` when not said), an optional default and may be `secret`. The
//! code reads a variable that is not secret as `env.NAME`; a secret one it cannot read at all:
//! only a connection uses it, for the credentials it sends. A connection is what the code names
//! to talk to the outside (`outbound Event via shop`): where and how is here, never in the code.
//! The values come from the environment the project runs in (the process's environment or a
//! `.env` file in self-hosting, the environment's values in Quadra Cloud), never from the code.

use std::collections::BTreeMap;
use std::path::Path;

/// The type of a variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarType {
    Text,
    Integer,
    Decimal,
    Boolean,
}

impl VarType {
    pub fn name(self) -> &'static str {
        match self {
            VarType::Text => "Text",
            VarType::Integer => "Integer",
            VarType::Decimal => "Decimal",
            VarType::Boolean => "Boolean",
        }
    }

    /// Whether `value` is a value of this type (as written in the environment).
    pub fn accepts(self, value: &str) -> bool {
        let v = value.trim();
        match self {
            VarType::Text => true,
            VarType::Integer => v.parse::<i64>().is_ok(),
            VarType::Decimal => {
                let d = v.strip_prefix('-').unwrap_or(v);
                let mut parts = d.splitn(2, '.');
                let int = parts.next().unwrap_or("");
                let frac = parts.next();
                !int.is_empty()
                    && int.bytes().all(|b| b.is_ascii_digit())
                    && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
            }
            VarType::Boolean => matches!(v, "true" | "false"),
        }
    }
}

/// A variable of `[vars]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Var {
    pub name: String,
    pub ty: VarType,
    pub default: Option<String>,
    pub secret: bool,
    pub description: Option<String>,
    /// the value in the project's tests (`quadra test`, the editor): they do not read the
    /// environment, so that they give the same result everywhere; without it, the default
    pub test: Option<String>,
}

/// A value of a connection: written in `quadra.toml`, or the value of a variable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Setting {
    Literal(String),
    Var(String),
}

/// How an HTTP connection authenticates its requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpAuth {
    None,
    /// `Authorization: Bearer <token>`
    Bearer(String),
    /// `<header>: <key>`
    ApiKey {
        header: String,
        key: String,
    },
    /// `Authorization: Basic …`
    Basic {
        user: Setting,
        password: String,
    },
    /// OAuth2 client credentials: a token asked of `token_url` (kept until it expires), sent
    /// as `Authorization: Bearer …`
    OAuth2 {
        token_url: Setting,
        client_id: Setting,
        client_secret: String,
        scope: Option<String>,
        /// the client's id and secret in the form (`client_auth = "body"`) rather than as
        /// basic authentication
        in_body: bool,
    },
    /// OAuth2 with a user's consent (`oauth2_grant = "authorization_code"`): an admin grants
    /// it once in the provider's page, the server keeps the tokens and refreshes them
    Consent(Consent),
}

/// OAuth2 with the consent of a user (the *authorization code* grant, with PKCE), as an app of
/// a shop or a mailbox of Gmail or Microsoft 365 asks it: an admin opens
/// `/v1/connections/<name>/authorize`, agrees in the provider's page, and the server keeps the
/// access and refresh tokens it gets back (encrypted, in the tenant's database).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Consent {
    /// the provider's page that asks the user (`oauth2_authorize_url`); parameters of its own
    /// (`access_type=offline`) may be written in it
    pub authorize_url: Setting,
    pub token_url: Setting,
    pub client_id: Setting,
    /// a secret variable
    pub client_secret: String,
    pub scope: Option<String>,
    /// the client's id and secret in the form rather than as basic authentication
    pub in_body: bool,
}

/// How the signature of a request (`signing_secret`) is written. Without `header`, Quadra's
/// own: `Quadra-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<body>">`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Signature {
    pub header: Option<String>,
    /// base64 rather than hex
    pub base64: bool,
    /// written before the signature (`sha256=`)
    pub prefix: String,
    /// the header that carries the unix seconds signed with the body (`"<t>.<body>"`); none:
    /// the body alone is signed
    pub timestamp_header: Option<String>,
}

/// What a connection talks to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionKind {
    /// HTTPS (HTTP only with `insecure = true`): events go as signed JSON webhooks
    Http {
        url: Setting,
        auth: HttpAuth,
        /// the secret variable whose value signs each request (HMAC-SHA256)
        signing_secret: Option<String>,
        /// how the signature is written
        signature: Signature,
        /// the secret variable with the client's certificate and private key, PEM (mutual TLS)
        client_certificate: Option<String>,
        /// the variable with the certificate (PEM) of the authority the server's certificate is
        /// signed by, when it is a private one; then only it is trusted
        server_ca: Option<String>,
        timeout_s: u64,
        insecure: bool,
    },
    /// an SMTP server (`smtps://` on 465, `smtp://` with STARTTLS on 587)
    Smtp {
        url: Setting,
        user: Option<Setting>,
        password: Option<String>,
        from: Setting,
        insecure: bool,
    },
    /// files on a server: SFTP (`sftp://host`, the server's key pinned by `known_host`) or FTP
    /// (`ftps://host`; plain `ftp://` only with `insecure = true`)
    Files {
        protocol: FileProtocol,
        url: Setting,
        user: Setting,
        /// a secret variable: the password
        password: Option<String>,
        /// a secret variable: the private key (OpenSSH or PEM), SFTP only
        private_key: Option<String>,
        /// the server's public key as in `known_hosts` (`ssh-ed25519 AAAA…`): SFTP only, needed
        known_host: Option<Setting>,
        timeout_s: u64,
        insecure: bool,
    },
    /// the database of another system: its tables read with a cursor (`inbound … pull
    /// "ORDERS"`), rows written to exchange tables agreed with it (`insert(…)` in an outbound)
    Database {
        engine: DbEngine,
        /// `postgres://host:5432/db`, `mysql://host:3306/db`, `sqlserver://host:1433/db`; for
        /// ODBC the connection string or the DSN (`DSN=erp`)
        url: Setting,
        user: Option<Setting>,
        password: Option<String>,
        timeout_s: u64,
        /// no TLS (a database on the same network, through an agent)
        insecure: bool,
    },
    /// a mailbox read over IMAP (`imaps://host`), PEC included: each message with its
    /// attachments is a message of an inbound (`inbound … pull "INBOX"`)
    Mail {
        url: Setting,
        user: Setting,
        /// a secret variable: the password (`LOGIN`); none with `oauth`
        password: Option<String>,
        /// OAuth2 with the user's consent instead of a password (`AUTHENTICATE XOAUTH2`:
        /// Gmail, Microsoft 365)
        oauth: Option<Consent>,
        timeout_s: u64,
        insecure: bool,
    },
    /// a printer: raw printing to its port (`tcp://host:9100`, "JetDirect": ZPL, or PDF for a
    /// printer that takes it), or a queue of the system where the connection runs (`lp://NAME`,
    /// CUPS). Only `print(content)` goes to it: nothing is read from a printer
    Printer {
        url: Setting,
        timeout_s: u64,
        /// how many times each job is printed, 1 to 99
        copies: u32,
    },
}

/// Where a printer is: its address (raw printing) or the name of a queue of the system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrinterAt {
    /// `tcp://host[:port]`, 9100 when not said
    Tcp { host: String, port: u16 },
    /// `lp://NAME`: printed with `lp -d NAME`
    Lp { name: String },
}

/// The url of a printer: `tcp://192.168.1.50:9100`, `tcp://zebra.lan`, `lp://Brother_HL_L2350DW`.
pub fn printer_at(url: &str) -> Result<PrinterAt, String> {
    if let Some(rest) = url.strip_prefix("tcp://") {
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.is_empty() || rest.contains('/') || rest.contains('@') || rest.contains('?') {
            return Err("a printer's address is tcp://host or tcp://host:port, nothing after it (tcp://192.168.1.50:9100)".into());
        }
        // `host`, `host:9100`, `[::1]`, `[::1]:9100`
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) if !rest.starts_with('[') || h.ends_with(']') => (
                h,
                p.parse::<u16>()
                    .ok()
                    .filter(|p| *p > 0)
                    .ok_or("the port of a printer is a number from 1 to 65535 (9100)")?,
            ),
            _ => (rest, 9100),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(
                "a printer's address is tcp://host or tcp://host:port (tcp://192.168.1.50:9100)"
                    .into(),
            );
        }
        return Ok(PrinterAt::Tcp {
            host: host.to_string(),
            port,
        });
    }
    if let Some(name) = url.strip_prefix("lp://") {
        if name.is_empty()
            || name.starts_with('-')
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            return Err("a printer of the system is lp://NAME, its name as `lpstat -p` shows it: letters, digits, `_`, `-`, `.` (lp://Brother_HL_L2350DW)".into());
        }
        return Ok(PrinterAt::Lp {
            name: name.to_string(),
        });
    }
    Err("a printer's url is tcp://host:port (raw printing, port 9100) or lp://NAME (a printer of the system, CUPS)".into())
}

/// The protocol of a connection to files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileProtocol {
    Sftp,
    Ftp,
}

/// The kind of database of a connection to another system's database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DbEngine {
    Postgres,
    Mysql,
    SqlServer,
    /// any database with an ODBC driver (DB2 for i, the AS/400): run by an agent, where the
    /// driver is installed
    Odbc,
}

/// A connection of `[connections.<name>]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    pub name: String,
    pub kind: ConnectionKind,
    /// how the messages it sends in (`inbound … via <name>`) prove they come from it
    pub inbound: Option<InboundAuth>,
    /// what the other side accepts: requests per second or minute (`rate = "2/s"`), at once
    /// (`concurrency`), and how many failures of transport in a row pause it (`pause_after`)
    pub limits: Limits,
    /// `location = "agent:milano"`: run by that on-premise agent, in the network of the other
    /// system, which holds its credentials; none: by the server
    pub agent: Option<String>,
    /// the declaration as written in quadra.toml, as an inline table: what an agent is given to
    /// run it
    pub decl: String,
}

/// How a message received from a connection proves where it comes from: a signature of its
/// body with a shared secret, in the way of that sender (`preset`), or a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundAuth {
    /// `quadra`, `hmac`, `shopify`, `stripe`, `github` or `token`
    pub preset: String,
    /// the secret variable
    pub secret: String,
    /// the previous secret, still accepted while the sender switches (rotation)
    pub previous: Option<String>,
    /// the largest body, in bytes
    pub max_bytes: usize,
}

/// The ways a sender signs what it sends.
pub const INBOUND_PRESETS: &[&str] = &["quadra", "hmac", "shopify", "stripe", "github", "token"];

/// The limits of a connection: those of the system on the other side, not of an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// at most this many requests per `per_s` seconds (`None`: no limit)
    pub rate: Option<(u32, u32)>,
    pub concurrency: u32,
    pub pause_after: u32,
}

impl Default for Limits {
    /// No rate, four at once, paused after five failures of transport in a row.
    fn default() -> Self {
        Limits {
            rate: None,
            concurrency: 4,
            pause_after: 5,
        }
    }
}

/// `"2/s"`, `"100/min"`, `"1000/h"`: (requests, seconds).
fn parse_rate(s: &str) -> Option<(u32, u32)> {
    let (n, per) = s.split_once('/')?;
    let n: u32 = n.trim().parse().ok().filter(|n| *n > 0)?;
    let per = match per.trim() {
        "s" | "second" => 1,
        "min" | "minute" => 60,
        "h" | "hour" => 3600,
        _ => return None,
    };
    Some((n, per))
}

impl Connection {
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            ConnectionKind::Http { .. } => "http",
            ConnectionKind::Smtp { .. } => "smtp",
            ConnectionKind::Files {
                protocol: FileProtocol::Sftp,
                ..
            } => "sftp",
            ConnectionKind::Files { .. } => "ftp",
            ConnectionKind::Database { engine, .. } => match engine {
                DbEngine::Postgres => "postgres",
                DbEngine::Mysql => "mysql",
                DbEngine::SqlServer => "sqlserver",
                DbEngine::Odbc => "odbc",
            },
            ConnectionKind::Mail { .. } => "imap",
            ConnectionKind::Printer { .. } => "printer",
        }
    }

    /// What `pull "…"` names on it: a URL path (http), files (`in/*.csv`), a table, a mailbox.
    pub fn pulls(&self) -> &'static str {
        match self.kind {
            ConnectionKind::Http { .. } => "path",
            ConnectionKind::Files { .. } => "files",
            ConnectionKind::Database { .. } => "table",
            ConnectionKind::Mail { .. } => "mailbox",
            ConnectionKind::Smtp { .. } | ConnectionKind::Printer { .. } => "nothing",
        }
    }

    /// The variables it uses.
    pub fn vars(&self) -> Vec<&str> {
        fn setting<'a>(s: &'a Setting, out: &mut Vec<&'a str>) {
            if let Setting::Var(v) = s {
                out.push(v.as_str());
            }
        }
        let mut out = Vec::new();
        match &self.kind {
            ConnectionKind::Http {
                url,
                auth,
                signing_secret,
                client_certificate,
                server_ca,
                ..
            } => {
                setting(url, &mut out);
                match auth {
                    HttpAuth::None => {}
                    HttpAuth::Bearer(v) => out.push(v),
                    HttpAuth::ApiKey { key, .. } => out.push(key),
                    HttpAuth::Basic { user, password } => {
                        setting(user, &mut out);
                        out.push(password);
                    }
                    HttpAuth::OAuth2 {
                        token_url,
                        client_id,
                        client_secret,
                        ..
                    } => {
                        setting(token_url, &mut out);
                        setting(client_id, &mut out);
                        out.push(client_secret);
                    }
                    HttpAuth::Consent(c) => c.vars(&mut out),
                }
                if let Some(s) = signing_secret {
                    out.push(s);
                }
                if let Some(c) = client_certificate {
                    out.push(c);
                }
                if let Some(c) = server_ca {
                    out.push(c);
                }
                if let Some(i) = &self.inbound {
                    out.push(&i.secret);
                    if let Some(p) = &i.previous {
                        out.push(p);
                    }
                }
            }
            ConnectionKind::Smtp {
                url,
                user,
                password,
                from,
                ..
            } => {
                setting(url, &mut out);
                if let Some(u) = user {
                    setting(u, &mut out);
                }
                if let Some(p) = password {
                    out.push(p);
                }
                setting(from, &mut out);
            }
            ConnectionKind::Files {
                url,
                user,
                password,
                private_key,
                known_host,
                ..
            } => {
                setting(url, &mut out);
                setting(user, &mut out);
                out.extend(password.iter().map(String::as_str));
                out.extend(private_key.iter().map(String::as_str));
                if let Some(k) = known_host {
                    setting(k, &mut out);
                }
            }
            ConnectionKind::Database {
                url,
                user,
                password,
                ..
            } => {
                setting(url, &mut out);
                if let Some(u) = user {
                    setting(u, &mut out);
                }
                out.extend(password.iter().map(String::as_str));
            }
            ConnectionKind::Mail {
                url,
                user,
                password,
                oauth,
                ..
            } => {
                setting(url, &mut out);
                setting(user, &mut out);
                out.extend(password.iter().map(String::as_str));
                if let Some(c) = oauth {
                    c.vars(&mut out);
                }
            }
            ConnectionKind::Printer { url, .. } => setting(url, &mut out),
        }
        out
    }

    /// Its OAuth2 with a user's consent, if it has one.
    pub fn consent(&self) -> Option<&Consent> {
        match &self.kind {
            ConnectionKind::Http {
                auth: HttpAuth::Consent(c),
                ..
            } => Some(c),
            ConnectionKind::Mail { oauth, .. } => oauth.as_ref(),
            _ => None,
        }
    }
}

impl Consent {
    fn vars<'a>(&'a self, out: &mut Vec<&'a str>) {
        for s in [&self.authorize_url, &self.token_url, &self.client_id] {
            if let Setting::Var(v) = s {
                out.push(v.as_str());
            }
        }
        out.push(&self.client_secret);
    }
}

/// What `quadra.toml` declares about the outside: variables and connections.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    pub vars: Vec<Var>,
    pub connections: Vec<Connection>,
    /// `[signers.<name>]`: the certificates and keys documents are signed with
    pub signers: Vec<SignerDecl>,
    /// `[integrations] log_days`: how long the log of the deliveries and of the messages
    /// received is kept (their final states stay)
    pub log_days: Option<u32>,
    /// `[files]`: where the files of `File` fields are kept (none: `.quadra/files` in the
    /// project)
    pub files: Option<FilesDecl>,
    /// `[data]`: the project's defaults for how much of its data it keeps
    pub data: DataDecl,
}

/// `[data]`: what the entities and the messages of the project keep when they do not say.
///
/// ```toml
/// [data]
/// history = "none"          # full (the default), none, "last 10", "1 year"
/// inbox = "90 days"         # the messages received, once handled ("forever", "30 days, then archive")
/// outbox = "30 days"        # the events, once delivered
/// archive_after = "2 years" # the history moved to archives by the sweep, with a signed checkpoint
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DataDecl {
    pub history: Option<KeepDecl>,
    pub inbox: Option<TermDecl>,
    pub outbox: Option<TermDecl>,
    pub archive_after: Option<(u32, String)>,
}

/// How much of the history of a record is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeepDecl {
    Full,
    None,
    Last(u32),
    /// the values replaced longer ago than `n` units leave the history
    For(u32, String),
}

/// How long a message is kept once it is closed: for ever, or `n` units, then deleted or
/// archived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermDecl {
    Forever,
    For { n: u32, unit: String, archive: bool },
}

/// The units of time of the terms (`2 years`, `1 year`): the unit in its plural form.
pub fn time_unit(u: &str) -> Option<&'static str> {
    Some(match u {
        "hour" | "hours" => "hours",
        "day" | "days" => "days",
        "week" | "weeks" => "weeks",
        "month" | "months" => "months",
        "year" | "years" => "years",
        _ => return None,
    })
}

/// `"2 years"`: the number and the unit.
fn duration(at: &str, text: &str) -> Result<(u32, String), String> {
    let mut w = text.split_whitespace();
    let (Some(n), Some(u), None) = (w.next(), w.next(), w.next()) else {
        return Err(format!(
            "{at}: `{text}` is not a time (`30 days`, `2 years`)"
        ));
    };
    let n: u32 = n.parse().ok().filter(|n| *n > 0).ok_or(format!(
        "{at}: `{text}` is not a time (`30 days`, `2 years`)"
    ))?;
    let unit = time_unit(u).ok_or(format!(
        "{at}: `{u}` is not a unit of time here: hours, days, weeks, months or years"
    ))?;
    Ok((n, unit.to_string()))
}

/// `history = "full" | "none" | "last 10" | "1 year"`.
pub fn keep_decl(at: &str, text: &str) -> Result<KeepDecl, String> {
    let t = text.trim();
    Ok(match t {
        "full" => KeepDecl::Full,
        "none" => KeepDecl::None,
        _ if t.starts_with("last ") => KeepDecl::Last(
            t[5..]
                .trim()
                .parse()
                .ok()
                .filter(|n| *n > 0)
                .ok_or(format!(
                    "{at}: `{t}`: `last` is followed by how many versions (`last 10`)"
                ))?,
        ),
        _ => {
            let (n, u) = duration(at, t).map_err(|_| {
                format!("{at}: `{t}` is not a history: full, none, `last 10` or a time (`1 year`)")
            })?;
            KeepDecl::For(n, u)
        }
    })
}

/// `inbox = "forever" | "90 days" | "30 days, then archive"`.
pub fn term_decl(at: &str, text: &str) -> Result<TermDecl, String> {
    let t = text.trim();
    if t == "forever" {
        return Ok(TermDecl::Forever);
    }
    let (time, archive) = match t.split_once(',') {
        Some((time, rest)) if rest.trim() == "then archive" => (time, true),
        Some(_) => {
            return Err(format!(
                "{at}: `{t}`: after the time comes only `, then archive`"
            ))
        }
        None => (t, false),
    };
    let (n, unit) = duration(at, time)?;
    Ok(TermDecl::For { n, unit, archive })
}

fn data(v: &toml::Value) -> Result<DataDecl, String> {
    let at = "quadra.toml, [data]";
    let t = v.as_table().ok_or(format!("{at}: [data] is a table"))?;
    let mut d = DataDecl::default();
    for (k, v) in t {
        let text = v
            .as_str()
            .ok_or(format!("{at}: {k} is text (`{k} = \"…\"`)"))?;
        let at = format!("{at}, {k}");
        match k.as_str() {
            "history" => d.history = Some(keep_decl(&at, text)?),
            "inbox" => d.inbox = Some(term_decl(&at, text)?),
            "outbox" => d.outbox = Some(term_decl(&at, text)?),
            "archive_after" => d.archive_after = Some(duration(&at, text)?),
            other => {
                return Err(format!(
                    "quadra.toml, [data]: `{other}` is not a setting of the data (history, inbox, outbox, archive_after)"
                ))
            }
        }
    }
    Ok(d)
}

/// `[files]`: an S3 compatible bucket for the files of `File` fields, and a CDN in front of
/// it.
///
/// ```toml
/// [files]
/// url = "s3://acme-files/prod"
/// endpoint = "https://fsn1.your-objectstorage.com"
/// region = "eu-central"
/// key = { var = "FILES_KEY" }
/// secret = { var = "FILES_SECRET" }
/// cdn = "https://files.acme.example.com"
/// cdn_secret = { var = "CDN_TOKEN_KEY" }
/// ```
///
/// `url` may also be a folder of this machine (`file:/srv/acme/files`, or relative to the
/// project). The keys are always secret variables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilesDecl {
    pub url: Setting,
    /// the S3 endpoint (AWS when not said)
    pub endpoint: Option<Setting>,
    pub region: Option<Setting>,
    /// secret variables: the access key and its secret
    pub key: Option<String>,
    pub secret: Option<String>,
    /// downloads through a CDN: its address, and the secret variable its tokens are signed with
    pub cdn: Option<Setting>,
    pub cdn_secret: Option<String>,
}

impl Default for Setting {
    fn default() -> Self {
        Setting::Literal(String::new())
    }
}

/// `[signers.company]`: a certificate and its key (a PKCS#12 file, in base64, and its
/// password: PAdES, CAdES, XAdES), or an OpenPGP key (`pgp_key`, `passphrase`) to sign with,
/// or the public key of someone files are encrypted for (`pgp_public`). Every credential comes
/// from a secret variable; `reason` and `location` go in a PDF's signature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SignerDecl {
    pub name: String,
    pub p12: Option<String>,
    pub password: Option<String>,
    pub pgp_key: Option<String>,
    pub passphrase: Option<String>,
    pub pgp_public: Option<Setting>,
    pub reason: Option<String>,
    pub location: Option<String>,
}

impl Manifest {
    pub fn signer(&self, name: &str) -> Option<&SignerDecl> {
        self.signers.iter().find(|s| s.name == name)
    }

    /// The days the log of the integrations is kept: 90 when not said.
    pub fn log_days(&self) -> u32 {
        self.log_days.unwrap_or(90)
    }

    pub fn var(&self, name: &str) -> Option<&Var> {
        self.vars.iter().find(|v| v.name == name)
    }

    pub fn connection(&self, name: &str) -> Option<&Connection> {
        self.connections.iter().find(|c| c.name == name)
    }

    /// The variables that have no value in `values` and no default.
    pub fn missing(&self, values: &BTreeMap<String, String>) -> Vec<&Var> {
        self.vars
            .iter()
            .filter(|v| v.default.is_none() && !values.contains_key(&v.name))
            .filter(|v| !self.only_for_agents(&v.name))
            .collect()
    }

    /// Whether the variable is used only by connections an agent runs (`location =
    /// "agent:…"`): its value is the agent's, and the server need not have it.
    pub fn only_for_agents(&self, name: &str) -> bool {
        let users: Vec<&Connection> = self
            .connections
            .iter()
            .filter(|c| c.vars().contains(&name))
            .collect();
        !users.is_empty() && users.iter().all(|c| c.agent.is_some())
    }

    /// Checks the values of an environment: every variable without a default has one, and each
    /// is of its variable's type. Values of names not declared are ignored.
    pub fn check_values(&self, values: &BTreeMap<String, String>) -> Result<(), String> {
        let missing: Vec<&str> = self
            .missing(values)
            .iter()
            .map(|v| v.name.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "{} not set: {} (declared in quadra.toml, [vars], without a default)",
                if missing.len() == 1 {
                    "a variable is"
                } else {
                    "variables are"
                },
                missing.join(", ")
            ));
        }
        for v in &self.vars {
            if let Some(value) = values.get(&v.name) {
                if !v.ty.accepts(value) {
                    return Err(format!(
                        "the value of {} is not {} {}",
                        v.name,
                        if v.ty == VarType::Integer { "an" } else { "a" },
                        v.ty.name()
                    ));
                }
            }
        }
        Ok(())
    }

    /// The values the project's tests run with: each variable's `test` value.
    pub fn test_values(&self) -> BTreeMap<String, String> {
        self.vars
            .iter()
            .filter_map(|v| Some((v.name.clone(), v.test.clone()?)))
            .collect()
    }

    /// The value of a variable: from `values`, else its default.
    pub fn value<'a>(
        &'a self,
        values: &'a BTreeMap<String, String>,
        name: &str,
    ) -> Option<&'a str> {
        values
            .get(name)
            .map(|s| s.as_str())
            .or_else(|| self.var(name)?.default.as_deref())
    }
}

/// Whether a name is a variable's name: upper case letters, digits and `_`, from a letter.
pub fn is_var_name(n: &str) -> bool {
    n.starts_with(|c: char| c.is_ascii_uppercase())
        && n.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn is_connection_name(n: &str) -> bool {
    n.starts_with(|c: char| c.is_ascii_lowercase())
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The manifest of the project at `root` (empty when it has no quadra.toml), or what is wrong
/// with it.
pub fn read(root: &Path) -> Result<Manifest, String> {
    match std::fs::read_to_string(root.join("quadra.toml")) {
        Ok(text) => parse(&text),
        Err(_) => Ok(Manifest::default()),
    }
}

/// The manifest declared by the text of a quadra.toml.
pub fn parse(text: &str) -> Result<Manifest, String> {
    let doc: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| format!("quadra.toml: {}", e.message()))?;
    let mut m = Manifest::default();
    if let Some(vars) = doc.get("vars") {
        let vars = vars
            .as_table()
            .ok_or("quadra.toml: [vars] is a table of variables")?;
        for (name, decl) in vars {
            m.vars.push(var(name, decl)?);
        }
    }
    if let Some(i) = doc.get("integrations") {
        let t = i
            .as_table()
            .ok_or("quadra.toml: [integrations] is a table")?;
        for (k, v) in t {
            match k.as_str() {
                "log_days" => {
                    m.log_days = Some(
                        v.as_integer()
                            .filter(|d| (1..=3650).contains(d))
                            .ok_or("quadra.toml, [integrations]: log_days is a number of days, from 1 to 3650")?
                            as u32,
                    )
                }
                other => {
                    return Err(format!(
                        "quadra.toml, [integrations]: `{other}` is not a setting of the integrations (log_days)"
                    ))
                }
            }
        }
    }
    if let Some(signers) = doc.get("signers") {
        let t = signers
            .as_table()
            .ok_or("quadra.toml: [signers] is a table of signers ([signers.name])")?;
        for (name, decl) in t {
            let at = format!("quadra.toml, [signers.{name}]");
            if !is_connection_name(name) {
                return Err(format!(
                    "{at}: a signer's name is in lower case letters, digits and `_` (`company`)"
                ));
            }
            let d = decl
                .as_table()
                .ok_or(format!("{at}: a signer is a table"))?;
            let mut sd = SignerDecl {
                name: name.clone(),
                ..Default::default()
            };
            for (k, v) in d {
                match k.as_str() {
                    "p12" => sd.p12 = Some(credential(&m, &at, k, v)?),
                    "password" => sd.password = Some(credential(&m, &at, k, v)?),
                    "pgp_key" => sd.pgp_key = Some(credential(&m, &at, k, v)?),
                    "passphrase" => sd.passphrase = Some(credential(&m, &at, k, v)?),
                    "pgp_public" => sd.pgp_public = Some(setting(&m, &at, k, v)?),
                    "reason" | "location" => {
                        let t = v.as_str().ok_or(format!("{at}: {k} is text"))?.to_string();
                        if k == "reason" { sd.reason = Some(t) } else { sd.location = Some(t) }
                    }
                    other => return Err(format!("{at}: `{other}` is not a setting of a signer (p12, password, pgp_key, passphrase, pgp_public, reason, location)")),
                }
            }
            let kinds = [
                sd.p12.is_some(),
                sd.pgp_key.is_some(),
                sd.pgp_public.is_some(),
            ];
            if kinds.iter().filter(|k| **k).count() != 1 {
                return Err(format!("{at}: a signer has one of `p12` (a certificate, with its `password`), `pgp_key` (with its `passphrase`, if any) or `pgp_public`"));
            }
            if sd.p12.is_some() && sd.password.is_none() {
                return Err(format!("{at}: the `p12` file opens with its `password`"));
            }
            m.signers.push(sd);
        }
    }
    if let Some(f) = doc.get("files") {
        m.files = Some(files(&m, f)?);
    }
    if let Some(d) = doc.get("data") {
        m.data = data(d)?;
    }
    if let Some(conns) = doc.get("connections") {
        let conns = conns
            .as_table()
            .ok_or("quadra.toml: [connections] is a table of connections ([connections.name])")?;
        for (name, decl) in conns {
            m.connections.push(connection(&m, name, decl)?);
        }
    }
    Ok(m)
}

/// `[files]`: `url` (`s3://bucket/prefix` or `file:folder`), and for S3 `endpoint`,
/// `region`, `key`, `secret`; `cdn` with `cdn_secret`.
fn files(m: &Manifest, v: &toml::Value) -> Result<FilesDecl, String> {
    let at = "quadra.toml, [files]";
    let t = v.as_table().ok_or(format!("{at}: [files] is a table"))?;
    let mut d = FilesDecl::default();
    let mut url = None;
    for (k, v) in t {
        match k.as_str() {
            "url" => url = Some(setting(m, at, k, v)?),
            "endpoint" => d.endpoint = Some(setting(m, at, k, v)?),
            "region" => d.region = Some(setting(m, at, k, v)?),
            "key" => d.key = Some(credential(m, at, k, v)?),
            "secret" => d.secret = Some(credential(m, at, k, v)?),
            "cdn" => d.cdn = Some(setting(m, at, k, v)?),
            "cdn_secret" => d.cdn_secret = Some(credential(m, at, k, v)?),
            other => {
                return Err(format!(
                    "{at}: `{other}` is not a setting of the files (url, endpoint, region, key, secret, cdn, cdn_secret)"
                ))
            }
        }
    }
    d.url = url.ok_or(format!(
        "{at}: url says where the files are: `s3://bucket/prefix`, or a folder (`file:/srv/files`)"
    ))?;
    if let Setting::Literal(u) = &d.url {
        if let Some(rest) = u.strip_prefix("s3://") {
            if rest.split('/').next().unwrap_or("").is_empty() {
                return Err(format!(
                    "{at}: url `{u}` has no bucket (`s3://bucket/prefix`)"
                ));
            }
        } else if !u.starts_with("file:") {
            return Err(format!(
                "{at}: url `{u}` is `s3://bucket/prefix` or a folder (`file:/srv/files`)"
            ));
        }
    }
    let s3 = !matches!(&d.url, Setting::Literal(u) if u.starts_with("file:"));
    if s3 && (d.key.is_none() || d.secret.is_none()) {
        return Err(format!(
            "{at}: a bucket is reached with `key` and `secret` (secret variables: `key = {{ var = \"FILES_KEY\" }}`)"
        ));
    }
    if d.cdn.is_some() != d.cdn_secret.is_some() {
        return Err(format!(
            "{at}: `cdn` goes with `cdn_secret`, the secret variable its download tokens are signed with"
        ));
    }
    Ok(d)
}

fn var(name: &str, decl: &toml::Value) -> Result<Var, String> {
    let at = format!("quadra.toml, [vars] {name}");
    if !is_var_name(name) {
        return Err(format!(
            "{at}: a variable's name is in capitals, digits and `_` (`SHOP_TOKEN`)"
        ));
    }
    let t = decl.as_table().ok_or(format!(
        "{at}: a variable is a table: `{name} = {{ type = \"Text\" }}`"
    ))?;
    for k in t.keys() {
        if !["type", "default", "secret", "description", "test"].contains(&k.as_str()) {
            return Err(format!(
                "{at}: `{k}` is not a key of a variable (type, default, secret, description, test)"
            ));
        }
    }
    let ty = match t.get("type").map(|v| v.as_str()) {
        None => VarType::Text,
        Some(Some("Text")) => VarType::Text,
        Some(Some("Integer")) => VarType::Integer,
        Some(Some("Decimal")) => VarType::Decimal,
        Some(Some("Boolean")) => VarType::Boolean,
        Some(_) => {
            return Err(format!(
                "{at}: the type is one of Text, Integer, Decimal, Boolean"
            ))
        }
    };
    let secret = match t.get("secret") {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or(format!("{at}: secret is true or false"))?,
    };
    let single = |key: &str| -> Result<Option<String>, String> {
        Ok(match t.get(key) {
            None => None,
            Some(toml::Value::String(s)) => Some(s.clone()),
            Some(toml::Value::Integer(i)) => Some(i.to_string()),
            Some(toml::Value::Float(f)) => Some(f.to_string()),
            Some(toml::Value::Boolean(b)) => Some(b.to_string()),
            Some(_) => return Err(format!("{at}: the {key} is a single value")),
        })
    };
    let default = single("default")?;
    let test = single("test")?;
    if let Some(d) = &test {
        if !ty.accepts(d) {
            return Err(format!("{at}: the test value `{d}` is not a {}", ty.name()));
        }
    }
    if secret && default.is_some() {
        return Err(format!(
            "{at}: a secret has no default in quadra.toml (the file is in the repository): give it in the environment"
        ));
    }
    if let Some(d) = &default {
        if !ty.accepts(d) {
            return Err(format!("{at}: the default `{d}` is not a {}", ty.name()));
        }
    }
    let description = match t.get("description") {
        None => None,
        Some(v) => Some(
            v.as_str()
                .ok_or(format!("{at}: the description is text"))?
                .to_string(),
        ),
    };
    Ok(Var {
        name: name.to_string(),
        ty,
        default,
        secret,
        description,
        test,
    })
}

/// A setting of a connection: text, or `{ var = "NAME" }`.
fn setting(m: &Manifest, at: &str, key: &str, v: &toml::Value) -> Result<Setting, String> {
    match v {
        toml::Value::String(s) => Ok(Setting::Literal(s.clone())),
        toml::Value::Table(t) => {
            let name = t
                .get("var")
                .and_then(|v| v.as_str())
                .filter(|_| t.len() == 1)
                .ok_or(format!(
                    "{at}: {key} is text or `{{ var = \"NAME\" }}`, a variable of [vars]"
                ))?;
            if m.var(name).is_none() {
                return Err(format!(
                    "{at}: {key} uses the variable {name}, which [vars] does not declare"
                ));
            }
            Ok(Setting::Var(name.to_string()))
        }
        _ => Err(format!(
            "{at}: {key} is text or `{{ var = \"NAME\" }}`, a variable of [vars]"
        )),
    }
}

/// The kinds of connection.
const KINDS: &str =
    "\"http\", \"smtp\", \"sftp\", \"ftp\", \"postgres\", \"mysql\", \"sqlserver\", \"odbc\", \"imap\", \"printer\"";

/// What the other side accepts (`rate`, `concurrency`, `pause_after`).
fn limits_of(at: &str, t: &toml::Table) -> Result<Limits, String> {
    let mut limits = Limits::default();
    if let Some(v) = t.get("rate") {
        limits.rate = Some(v.as_str().and_then(parse_rate).ok_or(format!(
            "{at}: rate is requests per second, minute or hour: \"2/s\", \"100/min\""
        ))?);
    }
    if let Some(v) = t.get("concurrency") {
        limits.concurrency =
            v.as_integer()
                .filter(|n| (1..=64).contains(n))
                .ok_or(format!("{at}: concurrency is from 1 to 64"))? as u32;
    }
    if let Some(v) = t.get("pause_after") {
        limits.pause_after =
            v.as_integer()
                .filter(|n| (1..=1000).contains(n))
                .ok_or(format!("{at}: pause_after is from 1 to 1000"))? as u32;
    }
    Ok(limits)
}

/// A printer: `url` (`tcp://host:9100` or `lp://NAME`), `timeout`, `copies`.
fn printer(m: &Manifest, at: &str, t: &toml::Table) -> Result<ConnectionKind, String> {
    const KEYS: &[&str] = &[
        "kind",
        "url",
        "timeout",
        "copies",
        "rate",
        "concurrency",
        "pause_after",
        "location",
    ];
    for k in t.keys() {
        if !KEYS.contains(&k.as_str()) {
            return Err(format!(
                "{at}: `{k}` is not a key of a printer connection ({})",
                KEYS[1..].join(", ")
            ));
        }
    }
    let url = setting(
        m,
        at,
        "url",
        t.get("url").ok_or(format!(
            "{at}: url is needed: tcp://host:9100 (raw printing) or lp://NAME (a printer of the system)"
        ))?,
    )?;
    if let Setting::Literal(u) = &url {
        printer_at(u).map_err(|e| format!("{at}: {e}"))?;
    }
    let timeout_s = match t.get("timeout") {
        None => 30,
        Some(v) => v
            .as_str()
            .and_then(parse_seconds)
            .filter(|s| (1..=600).contains(s))
            .ok_or(format!(
                "{at}: timeout is a duration from 1 to 600 seconds (\"30s\")"
            ))?,
    };
    let copies = match t.get("copies") {
        None => 1,
        Some(v) => v
            .as_integer()
            .filter(|n| (1..=99).contains(n))
            .ok_or(format!("{at}: copies is from 1 to 99"))? as u32,
    };
    Ok(ConnectionKind::Printer {
        url,
        timeout_s,
        copies,
    })
}

/// A connection to files, a database or a mailbox.
fn other_kind(
    m: &Manifest,
    at: &str,
    kind: &str,
    t: &toml::Table,
) -> Result<ConnectionKind, String> {
    let common = [
        "kind",
        "url",
        "user",
        "password",
        "timeout",
        "insecure",
        "rate",
        "concurrency",
        "pause_after",
        "location",
    ];
    let extra: &[&str] = match kind {
        "sftp" => &["private_key", "known_host"],
        "imap" => &[
            "oauth2_grant",
            "oauth2_authorize_url",
            "oauth2_token_url",
            "client_id",
            "client_secret",
            "scope",
            "client_auth",
        ],
        _ => &[],
    };
    for k in t.keys() {
        if !common.contains(&k.as_str()) && !extra.contains(&k.as_str()) {
            let mut all: Vec<&str> = common[1..].to_vec();
            all.extend(extra);
            return Err(format!(
                "{at}: `{k}` is not a key of a {kind} connection ({})",
                all.join(", ")
            ));
        }
    }
    let insecure = match t.get("insecure") {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or(format!("{at}: insecure is true or false"))?,
    };
    let url = setting(
        m,
        at,
        "url",
        t.get("url").ok_or(format!("{at}: url is needed"))?,
    )?;
    let schemes: &[&str] = match kind {
        "sftp" => &["sftp://"],
        "ftp" if insecure => &["ftps://", "ftp://"],
        "ftp" => &["ftps://"],
        "postgres" => &["postgres://", "postgresql://"],
        "mysql" => &["mysql://"],
        "sqlserver" => &["sqlserver://"],
        "imap" if insecure => &["imaps://", "imap://"],
        "imap" => &["imaps://"],
        _ => &[],
    };
    if let Setting::Literal(u) = &url {
        if !schemes.is_empty() && !schemes.iter().any(|s| u.starts_with(s)) {
            let plain = match kind {
                "ftp" => " (plain ftp:// only with `insecure = true`)",
                "imap" => " (plain imap:// only with `insecure = true`)",
                _ => "",
            };
            return Err(format!(
                "{at}: url starts with {}{plain}",
                schemes.join(" or ")
            ));
        }
        if u.contains('@') && kind != "odbc" {
            return Err(format!("{at}: the url has no credentials: they are `user` and `password` (a secret variable)"));
        }
    }
    let user = t
        .get("user")
        .map(|v| setting(m, at, "user", v))
        .transpose()?;
    let password = t
        .get("password")
        .map(|v| credential(m, at, "password", v))
        .transpose()?;
    let timeout_s = match t.get("timeout") {
        None => 30,
        Some(v) => v
            .as_str()
            .and_then(parse_seconds)
            .filter(|s| (1..=600).contains(s))
            .ok_or(format!(
                "{at}: timeout is a duration from 1 to 600 seconds (\"30s\")"
            ))?,
    };
    Ok(match kind {
        "sftp" | "ftp" => {
            let user = user.ok_or(format!("{at}: user is needed"))?;
            let private_key = t
                .get("private_key")
                .map(|v| credential(m, at, "private_key", v))
                .transpose()?;
            let known_host = t
                .get("known_host")
                .map(|v| setting(m, at, "known_host", v))
                .transpose()?;
            if kind == "sftp" {
                if known_host.is_none() {
                    return Err(format!("{at}: known_host is needed: the server's public key as in known_hosts (`ssh-keyscan -t ed25519 host`), so that no one else can pose as the server"));
                }
                if password.is_none() && private_key.is_none() {
                    return Err(format!("{at}: a password or a private_key is needed"));
                }
            } else if password.is_none() {
                return Err(format!("{at}: password is needed"));
            }
            ConnectionKind::Files {
                protocol: if kind == "sftp" {
                    FileProtocol::Sftp
                } else {
                    FileProtocol::Ftp
                },
                url,
                user,
                password,
                private_key,
                known_host,
                timeout_s,
                insecure,
            }
        }
        "imap" => {
            let oauth = match t.get("oauth2_grant").map(|v| v.as_str()) {
                None if t.contains_key("oauth2_authorize_url") || t.contains_key("oauth2_token_url") => {
                    return Err(format!("{at}: a mailbox takes OAuth2 with the user's consent: oauth2_grant = \"authorization_code\""))
                }
                None => None,
                Some(Some("authorization_code")) => Some(consent(m, at, t, insecure)?),
                _ => {
                    return Err(format!(
                        "{at}: a mailbox takes OAuth2 with the user's consent only: oauth2_grant = \"authorization_code\" (Gmail, Microsoft 365)"
                    ))
                }
            };
            if oauth.is_none() {
                for k in ["client_id", "client_secret", "scope", "client_auth"] {
                    if t.contains_key(k) {
                        return Err(format!(
                            "{at}: {k} is for OAuth2: with oauth2_grant = \"authorization_code\""
                        ));
                    }
                }
            }
            match (&password, &oauth) {
                (None, None) => {
                    return Err(format!(
                    "{at}: a password is needed, or OAuth2 (oauth2_grant = \"authorization_code\")"
                ))
                }
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "{at}: one authentication only: a password, or OAuth2"
                    ))
                }
                _ => {}
            }
            ConnectionKind::Mail {
                url,
                user: user.ok_or(format!("{at}: user is needed"))?,
                password,
                oauth,
                timeout_s,
                insecure,
            }
        }
        _ => ConnectionKind::Database {
            engine: match kind {
                "postgres" => DbEngine::Postgres,
                "mysql" => DbEngine::Mysql,
                "sqlserver" => DbEngine::SqlServer,
                _ => DbEngine::Odbc,
            },
            url,
            user,
            password,
            timeout_s,
            insecure,
        },
    })
}

/// OAuth2 with the consent of a user: `oauth2_authorize_url`, `oauth2_token_url`, `client_id`,
/// `client_secret`, `scope`, `client_auth`.
fn consent(m: &Manifest, at: &str, t: &toml::Table, insecure: bool) -> Result<Consent, String> {
    let url = |key: &str| -> Result<Setting, String> {
        let s = setting(
            m,
            at,
            key,
            t.get(key)
                .ok_or(format!("{at}: OAuth2 with the user's consent needs {key}"))?,
        )?;
        if let Setting::Literal(u) = &s {
            if !(u.starts_with("https://") || insecure && u.starts_with("http://")) {
                return Err(format!("{at}: {key} starts with https://"));
            }
        }
        Ok(s)
    };
    let authorize_url = url("oauth2_authorize_url")?;
    let token_url = url("oauth2_token_url")?;
    let client_id = setting(
        m,
        at,
        "client_id",
        t.get("client_id")
            .ok_or(format!("{at}: OAuth2 needs client_id"))?,
    )?;
    let client_secret = credential(
        m,
        at,
        "client_secret",
        t.get("client_secret")
            .ok_or(format!("{at}: OAuth2 needs client_secret"))?,
    )?;
    let scope = match t.get("scope") {
        None => None,
        Some(v) => Some(
            v.as_str()
                .ok_or(format!(
                    "{at}: scope is text (\"read_orders write_orders\")"
                ))?
                .to_string(),
        ),
    };
    let in_body = match t.get("client_auth").map(|v| v.as_str()) {
        None | Some(Some("basic")) => false,
        Some(Some("body")) => true,
        _ => {
            return Err(format!(
                "{at}: client_auth is \"basic\" (the default) or \"body\""
            ))
        }
    };
    // the tokens stay where the consent was given: an agent would need them handed over
    if t.contains_key("location") {
        return Err(format!(
            "{at}: OAuth2 with the user's consent runs on the server only: its tokens are not handed to an on-premise agent (remove location, or use a password or a token the agent holds)"
        ));
    }
    Ok(Consent {
        authorize_url,
        token_url,
        client_id,
        client_secret,
        scope,
        in_body,
    })
}

/// A credential of a connection: always `{ var = "NAME" }` with a secret variable.
fn credential(m: &Manifest, at: &str, key: &str, v: &toml::Value) -> Result<String, String> {
    match setting(m, at, key, v) {
        Ok(Setting::Var(name)) if m.var(&name).is_some_and(|v| v.secret) => Ok(name),
        Ok(Setting::Var(name)) => Err(format!(
            "{at}: {key} is a credential: {name} must be declared `secret = true`"
        )),
        Ok(Setting::Literal(_)) => Err(format!(
            "{at}: {key} is a credential: not written in quadra.toml (it is in the repository) but `{{ var = \"NAME\" }}`, a secret variable"
        )),
        Err(e) => Err(e),
    }
}

fn connection(m: &Manifest, name: &str, decl: &toml::Value) -> Result<Connection, String> {
    let at = format!("quadra.toml, [connections.{name}]");
    if !is_connection_name(name) {
        return Err(format!(
            "{at}: a connection's name is in lower case letters, digits and `_` (`shop`, `mail`)"
        ));
    }
    let t = decl
        .as_table()
        .ok_or(format!("{at}: a connection is a table"))?;
    let kind = t
        .get("kind")
        .and_then(|k| k.as_str())
        .ok_or(format!("{at}: kind is needed: {KINDS}"))?;
    // `location = "agent:milano"`: an on-premise agent runs it
    let agent = match t.get("location") {
        None => None,
        Some(v) => Some(
            v.as_str()
                .and_then(|l| l.strip_prefix("agent:"))
                .filter(|n| is_connection_name(n))
                .ok_or(format!(
                    "{at}: location is \"agent:<name>\": the on-premise agent that runs it (`agent:milano`)"
                ))?
                .to_string(),
        ),
    };
    // inline, so that it reads back as one value (`name = { kind = …, url = { var = … } }`)
    let decl = toml::Value::Table(t.clone()).to_string();
    // a printer takes one job at a time: one at once, unless said
    if kind == "printer" {
        let kind = printer(m, &at, t)?;
        let mut limits = limits_of(&at, t)?;
        if !t.contains_key("concurrency") {
            limits.concurrency = 1;
        }
        return Ok(Connection {
            name: name.to_string(),
            kind,
            inbound: None,
            limits,
            agent,
            decl,
        });
    }
    if matches!(
        kind,
        "sftp" | "ftp" | "postgres" | "mysql" | "sqlserver" | "odbc" | "imap"
    ) {
        let kind = other_kind(m, &at, kind, t)?;
        return Ok(Connection {
            name: name.to_string(),
            kind,
            inbound: None,
            limits: limits_of(&at, t)?,
            agent,
            decl,
        });
    }
    let allowed: &[&str] = match kind {
        "http" => &[
            "kind",
            "url",
            "bearer",
            "api_key",
            "api_key_header",
            "user",
            "password",
            "signing_secret",
            "signature_header",
            "signature_encoding",
            "signature_prefix",
            "signature_timestamp_header",
            "oauth2_token_url",
            "oauth2_grant",
            "oauth2_authorize_url",
            "client_id",
            "client_secret",
            "scope",
            "client_auth",
            "client_certificate",
            "server_ca",
            "timeout",
            "insecure",
            "rate",
            "concurrency",
            "pause_after",
            "inbound_signature",
            "inbound_secret",
            "inbound_secret_previous",
            "inbound_max_kb",
            "location",
        ],
        "smtp" => &[
            "kind",
            "url",
            "user",
            "password",
            "from",
            "insecure",
            "rate",
            "concurrency",
            "pause_after",
            "location",
        ],
        _ => return Err(format!("{at}: kind `{kind}` is not known ({KINDS})")),
    };
    for k in t.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(format!(
                "{at}: `{k}` is not a key of a {kind} connection ({})",
                allowed[1..].join(", ")
            ));
        }
    }
    let insecure = match t.get("insecure") {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or(format!("{at}: insecure is true or false"))?,
    };
    let url = setting(
        m,
        &at,
        "url",
        t.get("url").ok_or(format!("{at}: url is needed"))?,
    )?;
    let schemes: &[&str] = match kind {
        "http" if insecure => &["https://", "http://"],
        "http" => &["https://"],
        _ if insecure => &["smtps://", "smtp://"],
        _ => &["smtps://", "smtp://"],
    };
    if let Setting::Literal(u) = &url {
        if !schemes.iter().any(|s| u.starts_with(s)) {
            return Err(format!(
                "{at}: url starts with {}{}",
                schemes.join(" or "),
                if kind == "http" && !insecure {
                    " (plain http only with `insecure = true`)"
                } else {
                    ""
                }
            ));
        }
    }
    let user = t
        .get("user")
        .map(|v| setting(m, &at, "user", v))
        .transpose()?;
    let password = t
        .get("password")
        .map(|v| credential(m, &at, "password", v))
        .transpose()?;
    let kind = if kind == "http" {
        let bearer = t
            .get("bearer")
            .map(|v| credential(m, &at, "bearer", v))
            .transpose()?;
        let api_key = t
            .get("api_key")
            .map(|v| credential(m, &at, "api_key", v))
            .transpose()?;
        let header = match t.get("api_key_header") {
            None => "X-Api-Key".to_string(),
            Some(v) => v
                .as_str()
                .filter(|h| {
                    !h.is_empty()
                        && h.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                })
                .ok_or(format!("{at}: api_key_header is the name of a header"))?
                .to_string(),
        };
        let grant = match t.get("oauth2_grant").map(|v| v.as_str()) {
            None | Some(Some("client_credentials")) => false,
            Some(Some("authorization_code")) => true,
            _ => {
                return Err(format!(
                    "{at}: oauth2_grant is \"client_credentials\" (the default) or \"authorization_code\" (the user's consent)"
                ))
            }
        };
        if !grant && t.contains_key("oauth2_authorize_url") {
            return Err(format!(
                "{at}: oauth2_authorize_url is for the user's consent: oauth2_grant = \"authorization_code\""
            ));
        }
        let oauth = match t.get("oauth2_token_url") {
            _ if grant => Some(HttpAuth::Consent(consent(m, &at, t, insecure)?)),
            None => {
                for k in ["client_id", "client_secret", "scope", "client_auth"] {
                    if t.contains_key(k) {
                        return Err(format!("{at}: {k} is for OAuth2: with oauth2_token_url"));
                    }
                }
                None
            }
            Some(u) => {
                let token_url = setting(m, &at, "oauth2_token_url", u)?;
                if let Setting::Literal(u) = &token_url {
                    if !(u.starts_with("https://") || insecure && u.starts_with("http://")) {
                        return Err(format!("{at}: oauth2_token_url starts with https://"));
                    }
                }
                let client_id = setting(
                    m,
                    &at,
                    "client_id",
                    t.get("client_id")
                        .ok_or(format!("{at}: OAuth2 needs client_id"))?,
                )?;
                let client_secret = credential(
                    m,
                    &at,
                    "client_secret",
                    t.get("client_secret")
                        .ok_or(format!("{at}: OAuth2 needs client_secret"))?,
                )?;
                let scope = match t.get("scope") {
                    None => None,
                    Some(v) => Some(
                        v.as_str()
                            .ok_or(format!(
                                "{at}: scope is text (\"orders.read orders.write\")"
                            ))?
                            .to_string(),
                    ),
                };
                let in_body = match t.get("client_auth").map(|v| v.as_str()) {
                    None | Some(Some("basic")) => false,
                    Some(Some("body")) => true,
                    _ => {
                        return Err(format!(
                            "{at}: client_auth is \"basic\" (the default) or \"body\""
                        ))
                    }
                };
                Some(HttpAuth::OAuth2 {
                    token_url,
                    client_id,
                    client_secret,
                    scope,
                    in_body,
                })
            }
        };
        let others = bearer.is_some() || api_key.is_some() || user.is_some() || password.is_some();
        let auth = match (bearer, api_key, user, password) {
            _ if oauth.is_some() && others => {
                return Err(format!(
                    "{at}: one authentication only: bearer, api_key, user and password, or OAuth2"
                ))
            }
            _ if oauth.is_some() => oauth.expect("checked"),
            (None, None, None, None) => HttpAuth::None,
            (Some(b), None, None, None) => HttpAuth::Bearer(b),
            (None, Some(k), None, None) => HttpAuth::ApiKey { header, key: k },
            (None, None, Some(u), Some(p)) => HttpAuth::Basic {
                user: u,
                password: p,
            },
            (None, None, Some(_), None) | (None, None, None, Some(_)) => {
                return Err(format!(
                    "{at}: basic authentication needs user and password"
                ))
            }
            _ => {
                return Err(format!(
                    "{at}: one authentication only: bearer, api_key, or user and password"
                ))
            }
        };
        let signing_secret = t
            .get("signing_secret")
            .map(|v| credential(m, &at, "signing_secret", v))
            .transpose()?;
        let header_name = |k: &str| -> Result<Option<String>, String> {
            match t.get(k) {
                None => Ok(None),
                Some(v) => v
                    .as_str()
                    .filter(|h| {
                        !h.is_empty()
                            && h.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    })
                    .map(|h| Some(h.to_string()))
                    .ok_or(format!("{at}: {k} is the name of a header")),
            }
        };
        let signature = Signature {
            header: header_name("signature_header")?,
            base64: match t.get("signature_encoding").map(|v| v.as_str()) {
                None | Some(Some("hex")) => false,
                Some(Some("base64")) => true,
                _ => {
                    return Err(format!(
                        "{at}: signature_encoding is \"hex\" (the default) or \"base64\""
                    ))
                }
            },
            prefix: match t.get("signature_prefix") {
                None => String::new(),
                Some(v) => v
                    .as_str()
                    .filter(|p| p.len() <= 40 && !p.chars().any(|c| c.is_control()))
                    .ok_or(format!(
                        "{at}: signature_prefix is short text (\"sha256=\")"
                    ))?
                    .to_string(),
            },
            timestamp_header: header_name("signature_timestamp_header")?,
        };
        if signing_secret.is_none() && t.keys().any(|k| k.starts_with("signature_")) {
            return Err(format!(
                "{at}: the signature_… settings say how signing_secret signs: it is needed"
            ));
        }
        if signature.header.is_none()
            && (signature.base64
                || !signature.prefix.is_empty()
                || signature.timestamp_header.is_some())
        {
            return Err(format!(
                "{at}: a signature of your own has a signature_header (without it, Quadra-Signature as Quadra writes it)"
            ));
        }
        let client_certificate = t
            .get("client_certificate")
            .map(|v| credential(m, &at, "client_certificate", v))
            .transpose()?;
        let server_ca = match t.get("server_ca") {
            None => None,
            Some(v) => match setting(m, &at, "server_ca", v)? {
                Setting::Var(name) => Some(name),
                Setting::Literal(_) => {
                    return Err(format!(
                        "{at}: server_ca is `{{ var = \"NAME\" }}`, a variable with the certificate (PEM)"
                    ))
                }
            },
        };
        let timeout_s = match t.get("timeout") {
            None => 10,
            Some(v) => v
                .as_str()
                .and_then(parse_seconds)
                .filter(|s| (1..=120).contains(s))
                .ok_or(format!(
                    "{at}: timeout is a duration from 1 to 120 seconds (\"10s\")"
                ))?,
        };
        ConnectionKind::Http {
            url,
            auth,
            signing_secret,
            signature,
            client_certificate,
            server_ca,
            timeout_s,
            insecure,
        }
    } else {
        let from = setting(
            m,
            &at,
            "from",
            t.get("from")
                .ok_or(format!("{at}: from is needed: the sender"))?,
        )?;
        if user.is_some() != password.is_some() {
            return Err(format!("{at}: user and password go together"));
        }
        ConnectionKind::Smtp {
            url,
            user,
            password,
            from,
            insecure,
        }
    };
    let limits = limits_of(&at, t)?;
    let inbound = match t.get("inbound_signature") {
        None => {
            if t.contains_key("inbound_secret") {
                return Err(format!(
                    "{at}: inbound_secret goes with inbound_signature (one of {})",
                    INBOUND_PRESETS.join(", ")
                ));
            }
            None
        }
        Some(v) => {
            let preset = v
                .as_str()
                .filter(|p| INBOUND_PRESETS.contains(p))
                .ok_or(format!(
                    "{at}: inbound_signature is one of {}",
                    INBOUND_PRESETS.join(", ")
                ))?
                .to_string();
            let secret = credential(
                m,
                &at,
                "inbound_secret",
                t.get("inbound_secret").ok_or(format!(
                    "{at}: inbound_secret is needed: the secret the sender signs with"
                ))?,
            )?;
            let previous = t
                .get("inbound_secret_previous")
                .map(|v| credential(m, &at, "inbound_secret_previous", v))
                .transpose()?;
            let max_bytes = match t.get("inbound_max_kb") {
                None => 1 << 20,
                Some(v) => {
                    v.as_integer()
                        .filter(|n| (1..=16_384).contains(n))
                        .ok_or(format!("{at}: inbound_max_kb is from 1 to 16384"))?
                        as usize
                        * 1024
                }
            };
            Some(InboundAuth {
                preset,
                secret,
                previous,
                max_bytes,
            })
        }
    };
    Ok(Connection {
        name: name.to_string(),
        kind,
        inbound,
        limits,
        agent,
        decl,
    })
}

/// `10s`, `2m`: seconds.
fn parse_seconds(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('s') {
        n.trim().parse().ok()
    } else if let Some(n) = s.strip_suffix('m') {
        n.trim().parse::<u64>().ok().map(|n| n * 60)
    } else {
        None
    }
}

/// The values of a `.env` file: `NAME=value` lines, `#` comments, values optionally in quotes.
pub fn parse_dotenv(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        let v = if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"'))
                || (v.starts_with('\'') && v.ends_with('\'')))
        {
            v[1..v.len() - 1].to_string()
        } else {
            v.split(" #").next().unwrap_or("").trim().to_string()
        };
        out.insert(k.trim().to_string(), v);
    }
    out
}

/// The values of the variables a project declares, as `quadra serve` and `quadra dev` read
/// them: the process's environment, then the project's `.env` file for what it does not have.
pub fn values_from_env(root: &Path, m: &Manifest) -> BTreeMap<String, String> {
    let file = std::fs::read_to_string(root.join(".env"))
        .map(|t| parse_dotenv(&t))
        .unwrap_or_default();
    let mut out = BTreeMap::new();
    for v in &m.vars {
        if let Ok(value) = std::env::var(&v.name) {
            out.insert(v.name.clone(), value);
        } else if let Some(value) = file.get(&v.name) {
            out.insert(v.name.clone(), value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
[project]
name = "shop"

[vars]
SHOP_NAME = { type = "Text", default = "Acme" }
VAT = { type = "Decimal", default = "0.22" }
SHOP_TOKEN = { secret = true }
HOOK_SECRET = { secret = true }
SMTP_PASSWORD = { secret = true }

[connections.shop]
kind = "http"
url = "https://acme.example.com/api"
bearer = { var = "SHOP_TOKEN" }
signing_secret = { var = "HOOK_SECRET" }
timeout = "5s"

[connections.mail]
kind = "smtp"
url = "smtps://smtp.example.com:465"
user = "noreply@acme.example.com"
password = { var = "SMTP_PASSWORD" }
from = "Acme <noreply@acme.example.com>"
"#;

    #[test]
    fn files_databases_and_mailboxes_are_connections_and_an_agent_may_run_them() {
        let vars =
            "[vars]\nPASS = { secret = true }\nKEY = { secret = true }\nHOST = {}\nPLAIN = {}\n\n";
        let ok = |c: &str| {
            parse(&format!("{vars}[connections.x]\n{c}"))
                .map(|m| m.connection("x").unwrap().clone())
        };
        let sftp = ok("kind = \"sftp\"\nurl = \"sftp://files.example.com\"\nuser = \"q\"\nprivate_key = { var = \"KEY\" }\nknown_host = \"ssh-ed25519 AAAA\"\nlocation = \"agent:milano\"\n").unwrap();
        assert_eq!(
            (sftp.kind_name(), sftp.pulls(), sftp.agent.as_deref()),
            ("sftp", "files", Some("milano"))
        );
        assert_eq!(sftp.vars(), ["KEY"]);
        assert!(sftp.decl.contains("known_host"));
        let pg = ok("kind = \"postgres\"\nurl = \"postgres://db:5432/erp\"\nuser = \"q\"\npassword = { var = \"PASS\" }\n").unwrap();
        assert_eq!((pg.kind_name(), pg.pulls()), ("postgres", "table"));
        let odbc = ok("kind = \"odbc\"\nurl = \"DSN=AS400\"\nuser = \"q\"\npassword = { var = \"PASS\" }\nlocation = \"agent:sede\"\n").unwrap();
        assert_eq!(odbc.kind_name(), "odbc");
        let imap = ok("kind = \"imap\"\nurl = \"imaps://imap.pec.example.it\"\nuser = \"q\"\npassword = { var = \"PASS\" }\n").unwrap();
        assert_eq!(imap.pulls(), "mailbox");
        let bad = |c: &str| ok(c).unwrap_err();
        assert!(bad(
            "kind = \"sftp\"\nurl = \"sftp://h\"\nuser = \"q\"\npassword = { var = \"PASS\" }\n"
        )
        .contains("known_host is needed"));
        assert!(bad(
            "kind = \"ftp\"\nurl = \"ftp://h\"\nuser = \"q\"\npassword = { var = \"PASS\" }\n"
        )
        .contains("insecure = true"));
        assert!(
            bad("kind = \"postgres\"\nurl = \"postgres://u:p@db/erp\"\n")
                .contains("no credentials")
        );
        assert!(bad(
            "kind = \"imap\"\nurl = \"imaps://h\"\nuser = \"q\"\npassword = { var = \"PLAIN\" }\n"
        )
        .contains("secret = true"));
        assert!(bad("kind = \"sftp\"\nurl = \"sftp://h\"\nuser = \"q\"\npassword = { var = \"PASS\" }\nknown_host = \"x\"\nlocation = \"milano\"\n").contains("agent:<name>"));
        assert!(bad("kind = \"gopher\"\nurl = \"x\"\n").contains("not known"));
    }

    #[test]
    fn a_printer_is_a_connection_by_its_address_or_its_queue() {
        let vars = "[vars]\nPRINTER = {}\n\n";
        let ok = |c: &str| {
            parse(&format!("{vars}[connections.x]\n{c}"))
                .map(|m| m.connection("x").unwrap().clone())
        };
        let zebra = ok("kind = \"printer\"\nurl = \"tcp://192.168.1.50:9100\"\nlocation = \"agent:backoffice\"\ntimeout = \"10s\"\n").unwrap();
        assert_eq!(
            (zebra.kind_name(), zebra.pulls(), zebra.agent.as_deref()),
            ("printer", "nothing", Some("backoffice"))
        );
        assert_eq!(
            zebra.kind,
            ConnectionKind::Printer {
                url: Setting::Literal("tcp://192.168.1.50:9100".into()),
                timeout_s: 10,
                copies: 1,
            }
        );
        // one job at a time, unless said
        assert_eq!(zebra.limits.concurrency, 1);
        let office = ok(
            "kind = \"printer\"\nurl = \"lp://Brother_HL_L2350DW\"\ncopies = 2\nconcurrency = 2\n",
        )
        .unwrap();
        assert!(matches!(
            office.kind,
            ConnectionKind::Printer {
                copies: 2,
                timeout_s: 30,
                ..
            }
        ));
        assert_eq!(office.limits.concurrency, 2);
        let by_var = ok("kind = \"printer\"\nurl = { var = \"PRINTER\" }\n").unwrap();
        assert_eq!(by_var.vars(), ["PRINTER"]);
        assert_eq!(
            printer_at("tcp://zebra.lan").unwrap(),
            PrinterAt::Tcp {
                host: "zebra.lan".into(),
                port: 9100
            }
        );
        assert_eq!(
            printer_at("tcp://[fe80::1]:9101").unwrap(),
            PrinterAt::Tcp {
                host: "fe80::1".into(),
                port: 9101
            }
        );
        assert_eq!(
            printer_at("lp://Office.2").unwrap(),
            PrinterAt::Lp {
                name: "Office.2".into()
            }
        );
        let bad = |c: &str| ok(c).unwrap_err();
        assert!(
            bad("kind = \"printer\"\nurl = \"http://192.168.1.50\"\n").contains("tcp://host:port")
        );
        assert!(
            bad("kind = \"printer\"\nurl = \"tcp://192.168.1.50:printer\"\n").contains("the port")
        );
        assert!(
            bad("kind = \"printer\"\nurl = \"tcp://h:9100/queue\"\n").contains("nothing after it")
        );
        assert!(bad("kind = \"printer\"\nurl = \"lp://my printer\"\n").contains("lpstat"));
        assert!(bad("kind = \"printer\"\nurl = \"lp://-o\"\n").contains("lpstat"));
        assert!(bad("kind = \"printer\"\n").contains("url is needed"));
        assert!(bad("kind = \"printer\"\nurl = \"tcp://h\"\ncopies = 0\n")
            .contains("copies is from 1 to 99"));
        assert!(bad("kind = \"printer\"\nurl = \"tcp://h\"\ncopies = 100\n")
            .contains("copies is from 1 to 99"));
        assert!(
            bad("kind = \"printer\"\nurl = \"tcp://h\"\ntimeout = \"20min\"\n").contains("timeout")
        );
        let unknown = bad("kind = \"printer\"\nurl = \"tcp://h\"\nuser = \"q\"\n");
        assert!(
            unknown.contains("`user` is not a key of a printer connection"),
            "{unknown}"
        );
        assert!(
            bad("kind = \"printer\"\nurl = \"tcp://h\"\nlocation = \"backoffice\"\n")
                .contains("agent:<name>")
        );
    }

    #[test]
    fn oauth2_with_the_users_consent_is_read_for_http_and_mailboxes() {
        let vars = "[vars]\nID = {}\nSECRET = { secret = true }\nPASS = { secret = true }\n\n";
        let ok = |c: &str| {
            parse(&format!("{vars}[connections.x]\n{c}"))
                .map(|m| m.connection("x").unwrap().clone())
        };
        let shop = ok("kind = \"http\"\nurl = \"https://acme.myshopify.com/admin/api\"\noauth2_grant = \"authorization_code\"\noauth2_authorize_url = \"https://acme.myshopify.com/admin/oauth/authorize\"\noauth2_token_url = \"https://acme.myshopify.com/admin/oauth/access_token\"\nclient_id = { var = \"ID\" }\nclient_secret = { var = \"SECRET\" }\nscope = \"read_orders\"\nclient_auth = \"body\"\n").unwrap();
        let c = shop.consent().unwrap();
        assert_eq!(c.scope.as_deref(), Some("read_orders"));
        assert!(c.in_body);
        assert_eq!(c.client_id, Setting::Var("ID".into()));
        let mut used = shop.vars();
        used.sort();
        assert_eq!(used, ["ID", "SECRET"]);
        let gmail = ok("kind = \"imap\"\nurl = \"imaps://imap.gmail.com\"\nuser = \"ops@acme.example.com\"\noauth2_grant = \"authorization_code\"\noauth2_authorize_url = \"https://accounts.google.com/o/oauth2/v2/auth?access_type=offline&prompt=consent\"\noauth2_token_url = \"https://oauth2.googleapis.com/token\"\nclient_id = \"123.apps.googleusercontent.com\"\nclient_secret = { var = \"SECRET\" }\nscope = \"https://mail.google.com/\"\n").unwrap();
        assert!(gmail.consent().is_some());
        assert!(matches!(
            gmail.kind,
            ConnectionKind::Mail { password: None, .. }
        ));
        assert_eq!(gmail.vars(), ["SECRET"]);
        // a password mailbox has none
        let pec = ok("kind = \"imap\"\nurl = \"imaps://imap.pec.example.it\"\nuser = \"q\"\npassword = { var = \"PASS\" }\n").unwrap();
        assert!(pec.consent().is_none());
        let bad = |c: &str| ok(c).unwrap_err();
        assert!(bad(
            "kind = \"http\"\nurl = \"https://a\"\noauth2_authorize_url = \"https://a/auth\"\n"
        )
        .contains("oauth2_grant = \"authorization_code\""));
        assert!(bad("kind = \"http\"\nurl = \"https://a\"\noauth2_grant = \"authorization_code\"\noauth2_token_url = \"https://a/t\"\nclient_id = \"x\"\nclient_secret = { var = \"SECRET\" }\n").contains("needs oauth2_authorize_url"));
        assert!(
            bad("kind = \"http\"\nurl = \"https://a\"\noauth2_grant = \"implicit\"\n")
                .contains("oauth2_grant is")
        );
        assert!(bad("kind = \"http\"\nurl = \"https://a\"\noauth2_grant = \"authorization_code\"\noauth2_authorize_url = \"https://a/auth\"\noauth2_token_url = \"https://a/t\"\nclient_id = \"x\"\nclient_secret = { var = \"SECRET\" }\nbearer = { var = \"SECRET\" }\n").contains("one authentication only"));
        assert!(bad("kind = \"http\"\nurl = \"https://a\"\noauth2_grant = \"authorization_code\"\noauth2_authorize_url = \"https://a/auth\"\noauth2_token_url = \"https://a/t\"\nclient_id = \"x\"\nclient_secret = { var = \"SECRET\" }\nlocation = \"agent:milano\"\n").contains("not handed to an on-premise agent"));
        assert!(bad("kind = \"imap\"\nurl = \"imaps://h\"\nuser = \"q\"\n")
            .contains("a password is needed, or OAuth2"));
        assert!(bad("kind = \"imap\"\nurl = \"imaps://h\"\nuser = \"q\"\noauth2_grant = \"client_credentials\"\n").contains("consent only"));
        assert!(bad("kind = \"imap\"\nurl = \"imaps://h\"\nuser = \"q\"\npassword = { var = \"PASS\" }\nclient_id = \"x\"\n").contains("for OAuth2"));
        assert!(bad("kind = \"imap\"\nurl = \"imaps://h\"\nuser = \"q\"\npassword = { var = \"PASS\" }\noauth2_grant = \"authorization_code\"\noauth2_authorize_url = \"https://a/auth\"\noauth2_token_url = \"https://a/t\"\nclient_id = \"x\"\nclient_secret = { var = \"SECRET\" }\n").contains("one authentication only"));
    }

    #[test]
    fn the_log_of_the_integrations_is_kept_as_said() {
        assert_eq!(parse("").unwrap().log_days(), 90);
        assert_eq!(
            parse("[integrations]\nlog_days = 30\n").unwrap().log_days(),
            30
        );
        assert!(parse("[integrations]\nlog_days = 0\n")
            .unwrap_err()
            .contains("from 1 to 3650"));
        assert!(parse("[integrations]\nkeep = 1\n")
            .unwrap_err()
            .contains("not a setting"));
    }

    #[test]
    fn oauth2_certificates_and_signatures_are_read_and_checked() {
        let vars = "[vars]\nID = {}\nSECRET = { secret = true }\nPEM = { secret = true }\nCA = {}\nHOOK = { secret = true }\n\n";
        let m = parse(&format!("{vars}[connections.erp]\nkind = \"http\"\nurl = \"https://erp.example.com\"\noauth2_token_url = \"https://login.example.com/token\"\nclient_id = {{ var = \"ID\" }}\nclient_secret = {{ var = \"SECRET\" }}\nscope = \"orders\"\nclient_auth = \"body\"\nclient_certificate = {{ var = \"PEM\" }}\nserver_ca = {{ var = \"CA\" }}\nsigning_secret = {{ var = \"HOOK\" }}\nsignature_header = \"X-Sig\"\nsignature_encoding = \"base64\"\n")).unwrap();
        let ConnectionKind::Http {
            auth,
            signature,
            client_certificate,
            server_ca,
            ..
        } = &m.connection("erp").unwrap().kind
        else {
            panic!()
        };
        assert_eq!(
            auth,
            &HttpAuth::OAuth2 {
                token_url: Setting::Literal("https://login.example.com/token".into()),
                client_id: Setting::Var("ID".into()),
                client_secret: "SECRET".into(),
                scope: Some("orders".into()),
                in_body: true,
            }
        );
        assert_eq!(signature.header.as_deref(), Some("X-Sig"));
        assert!(signature.base64);
        assert_eq!(client_certificate.as_deref(), Some("PEM"));
        assert_eq!(server_ca.as_deref(), Some("CA"));
        let mut used = m.connection("erp").unwrap().vars();
        used.sort();
        assert_eq!(used, ["CA", "HOOK", "ID", "PEM", "SECRET"]);
        let bad = |extra: &str| {
            parse(&format!("{vars}[connections.erp]\nkind = \"http\"\nurl = \"https://erp.example.com\"\n{extra}")).unwrap_err()
        };
        assert!(bad("client_id = \"x\"\n").contains("for OAuth2"));
        assert!(bad("oauth2_token_url = \"https://l/t\"\nclient_id = \"x\"\nclient_secret = { var = \"SECRET\" }\nbearer = { var = \"SECRET\" }\n").contains("one authentication only"));
        assert!(bad("oauth2_token_url = \"https://l/t\"\nclient_id = \"x\"\nclient_secret = { var = \"ID\" }\n").contains("secret = true"));
        assert!(bad("signature_header = \"X-Sig\"\n").contains("signing_secret"));
        assert!(
            bad("signing_secret = { var = \"HOOK\" }\nsignature_prefix = \"v1=\"\n")
                .contains("signature_header")
        );
        assert!(bad("client_certificate = \"-----BEGIN\"\n").contains("credential"));
        assert!(bad("server_ca = \"-----BEGIN\"\n").contains("server_ca"));
    }

    #[test]
    fn variables_and_connections_are_read() {
        let m = parse(OK).unwrap();
        assert_eq!(m.vars.len(), 5);
        assert_eq!(m.var("VAT").unwrap().ty, VarType::Decimal);
        assert!(m.var("SHOP_TOKEN").unwrap().secret);
        let shop = m.connection("shop").unwrap();
        assert_eq!(
            shop.kind,
            ConnectionKind::Http {
                url: Setting::Literal("https://acme.example.com/api".into()),
                auth: HttpAuth::Bearer("SHOP_TOKEN".into()),
                signing_secret: Some("HOOK_SECRET".into()),
                signature: Signature::default(),
                client_certificate: None,
                server_ca: None,
                timeout_s: 5,
                insecure: false,
            }
        );
        assert_eq!(m.connection("mail").unwrap().kind_name(), "smtp");
        assert_eq!(m.connection("mail").unwrap().limits, Limits::default());
        let l = parse("[connections.x]\nkind = \"http\"\nurl = \"https://a\"\nrate = \"100/min\"\nconcurrency = 2\npause_after = 3\n").unwrap();
        assert_eq!(
            l.connections[0].limits,
            Limits {
                rate: Some((100, 60)),
                concurrency: 2,
                pause_after: 3
            }
        );
        assert!(
            parse("[connections.x]\nkind = \"http\"\nurl = \"https://a\"\nrate = \"fast\"\n")
                .unwrap_err()
                .contains("rate")
        );
        assert_eq!(m.connection("mail").unwrap().vars(), vec!["SMTP_PASSWORD"]);
    }

    #[test]
    fn what_is_wrong_is_said() {
        let bad = |extra: &str| {
            parse(&format!(
                "[vars]\nTOKEN = {{ secret = true }}\nPLAIN = {{}}\n{extra}"
            ))
            .unwrap_err()
        };
        assert!(
            bad("[connections.x]\nkind = \"http\"\nurl = \"http://a\"\n")
                .contains("insecure = true")
        );
        assert!(
            bad("[connections.x]\nkind = \"http\"\nurl = \"https://a\"\nbearer = \"abc\"\n")
                .contains("credential")
        );
        assert!(bad(
            "[connections.x]\nkind = \"http\"\nurl = \"https://a\"\nbearer = { var = \"PLAIN\" }\n"
        )
        .contains("secret = true"));
        assert!(bad(
            "[connections.x]\nkind = \"http\"\nurl = \"https://a\"\nbearer = { var = \"NOPE\" }\n"
        )
        .contains("does not declare"));
        assert!(bad("[connections.x]\nkind = \"gopher\"\nurl = \"x\"\n").contains("not known"));
        assert!(
            bad("[connections.Shop]\nkind = \"http\"\nurl = \"https://a\"\n")
                .contains("lower case")
        );
        assert!(parse("[vars]\nlower = {}\n")
            .unwrap_err()
            .contains("capitals"));
        assert!(parse("[vars]\nX = { secret = true, default = \"a\" }\n")
            .unwrap_err()
            .contains("no default"));
        assert!(
            parse("[vars]\nX = { type = \"Integer\", default = \"a\" }\n")
                .unwrap_err()
                .contains("not a Integer")
        );
    }

    #[test]
    fn values_are_checked_against_the_declarations() {
        let m = parse(OK).unwrap();
        let mut values = BTreeMap::new();
        let e = m.check_values(&values).unwrap_err();
        assert!(
            e.contains("SHOP_TOKEN") && e.contains("SMTP_PASSWORD") && !e.contains("VAT"),
            "{e}"
        );
        for n in ["SHOP_TOKEN", "HOOK_SECRET", "SMTP_PASSWORD"] {
            values.insert(n.to_string(), "x".to_string());
        }
        m.check_values(&values).unwrap();
        values.insert("VAT".into(), "abc".into());
        assert!(m
            .check_values(&values)
            .unwrap_err()
            .contains("not a Decimal"));
        values.insert("VAT".into(), "0.10".into());
        assert_eq!(m.value(&values, "VAT"), Some("0.10"));
        assert_eq!(m.value(&values, "SHOP_NAME"), Some("Acme"));
    }

    #[test]
    fn dotenv_lines() {
        let v = parse_dotenv("# x\nA=1\nexport B = \"two words\"\nC='q' \nD=v # note\n\nE\n");
        assert_eq!(v.get("A").unwrap(), "1");
        assert_eq!(v.get("B").unwrap(), "two words");
        assert_eq!(v.get("C").unwrap(), "q");
        assert_eq!(v.get("D").unwrap(), "v");
        assert!(!v.contains_key("E"));
    }

    #[test]
    fn data_settings() {
        let m = parse("[data]\nhistory = \"last 10\"\ninbox = \"30 days, then archive\"\noutbox = \"forever\"\narchive_after = \"2 years\"\n").unwrap();
        assert_eq!(m.data.history, Some(KeepDecl::Last(10)));
        assert_eq!(
            m.data.inbox,
            Some(TermDecl::For {
                n: 30,
                unit: "days".into(),
                archive: true
            })
        );
        assert_eq!(m.data.outbox, Some(TermDecl::Forever));
        assert_eq!(m.data.archive_after, Some((2, "years".into())));
        assert_eq!(
            parse("[data]\nhistory = \"1 year\"\n")
                .unwrap()
                .data
                .history,
            Some(KeepDecl::For(1, "years".into()))
        );
        for (bad, says) in [
            ("[data]\nhistory = \"some\"\n", "is not a history"),
            (
                "[data]\ninbox = \"3 fortnights\"\n",
                "is not a unit of time",
            ),
            (
                "[data]\nkeep = \"1 year\"\n",
                "is not a setting of the data",
            ),
            (
                "[data]\noutbox = \"30 days, then delete\"\n",
                "then archive",
            ),
        ] {
            let e = parse(bad).unwrap_err();
            assert!(e.contains(says), "{bad}: {e}");
        }
    }

    #[test]
    fn the_files_settings() {
        let m = parse("[vars]\nK = { secret = true }\nS = { secret = true }\nC = { secret = true }\n\n[files]\nurl = \"s3://acme/prod\"\nendpoint = \"http://localhost:9000\"\nregion = \"us-east-1\"\nkey = { var = \"K\" }\nsecret = { var = \"S\" }\ncdn = \"https://files.acme.example.com\"\ncdn_secret = { var = \"C\" }\n").unwrap();
        let f = m.files.unwrap();
        assert_eq!(f.url, Setting::Literal("s3://acme/prod".into()));
        assert_eq!(
            (f.key.as_deref(), f.secret.as_deref()),
            (Some("K"), Some("S"))
        );
        assert_eq!(f.cdn_secret.as_deref(), Some("C"));
        assert!(parse("[files]\nurl = \"file:/srv/files\"\n")
            .unwrap()
            .files
            .is_some());
        for (toml, says) in [
            ("[files]\nurl = \"s3://acme\"\n", "with `key` and `secret`"),
            (
                "[files]\nurl = \"ftp://x\"\n",
                "is `s3://bucket/prefix` or a folder",
            ),
            ("[files]\nurl = \"s3:///p\"\n", "has no bucket"),
            (
                "[files]\nendpoint = \"x\"\n",
                "url says where the files are",
            ),
            (
                "[files]\nurl = \"file:x\"\nbucket = 1\n",
                "`bucket` is not a setting of the files",
            ),
            (
                "[vars]\nK = {}\n[files]\nurl = \"s3://a\"\nkey = { var = \"K\" }\n",
                "K must be declared `secret = true`",
            ),
            (
                "[files]\nurl = \"file:x\"\ncdn = \"https://c\"\n",
                "`cdn` goes with `cdn_secret`",
            ),
        ] {
            let e = parse(toml).unwrap_err();
            assert!(e.contains(says), "{toml}: {e}");
        }
    }
}
