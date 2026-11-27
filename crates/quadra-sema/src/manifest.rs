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
                }
                if let Some(s) = signing_secret {
                    out.push(s);
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
        }
        out
    }
}

/// What `quadra.toml` declares about the outside: variables and connections.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    pub vars: Vec<Var>,
    pub connections: Vec<Connection>,
}

impl Manifest {
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
            .collect()
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
        .ok_or(format!("{at}: kind is needed: \"http\" or \"smtp\""))?;
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
            "timeout",
            "insecure",
            "rate",
            "concurrency",
            "pause_after",
            "inbound_signature",
            "inbound_secret",
            "inbound_secret_previous",
            "inbound_max_kb",
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
        ],
        _ => {
            return Err(format!(
                "{at}: kind `{kind}` is not known (\"http\", \"smtp\")"
            ))
        }
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
        let auth = match (bearer, api_key, user, password) {
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
        assert!(bad("[connections.x]\nkind = \"ftp\"\nurl = \"x\"\n").contains("not known"));
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
}
