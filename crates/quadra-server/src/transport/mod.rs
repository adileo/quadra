//! The systems that do not speak HTTP: files on SFTP and FTP servers (`files`), the databases
//! of other systems (`db`), mailboxes read over IMAP (`mail`). A pull asks a connection for its
//! new messages ([`Ask`]), an outbound puts something there ([`Put`]).
//!
//! A job carries its connection as it is declared ([`Conn`]: the `[connections.<name>]` table
//! and the declarations of the variables it uses, never their values), so that whoever runs it
//! resolves the values itself: the server with its environment, or the on-premise agent named
//! by `location = "agent:<name>"` with its own, in the network of the other system. The agent
//! runs these same functions (`quadra agent`).

pub mod db;
pub mod files;
pub mod mail;

use base64::Engine as _;
use quadra_sema::manifest::{Connection, ConnectionKind, Manifest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A connection as a job carries it: its declaration, and the declarations of the variables it
/// uses (`[vars]`), so that it can be read again where it runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conn {
    pub name: String,
    /// `[vars]` (the variables it uses) and `[connections.<name>]`, as TOML
    pub toml: String,
}

impl Conn {
    /// The connection `name` of a manifest, as a job carries it.
    pub fn of(manifest: &Manifest, name: &str) -> Result<Conn, String> {
        let c = manifest
            .connection(name)
            .ok_or_else(|| format!("quadra.toml has no connection `{name}`"))?;
        let mut vars = String::from("[vars]\n");
        for v in c.vars() {
            if let Some(var) = manifest.var(v) {
                vars.push_str(&format!("{} = {{ secret = {} }}\n", var.name, var.secret));
            }
        }
        Ok(Conn {
            name: name.to_string(),
            toml: format!("{vars}\n[connections]\n{name} = {}\n", c.decl),
        })
    }

    /// The connection read again, and its values from `env` (where it runs).
    pub fn open(&self, env: &BTreeMap<String, String>) -> Result<Resolved, String> {
        let manifest = quadra_sema::manifest::parse(&self.toml)?;
        let connection = manifest
            .connection(&self.name)
            .cloned()
            .ok_or_else(|| format!("no connection `{}` in the job", self.name))?;
        Ok(Resolved {
            manifest,
            connection,
            env: env.clone(),
        })
    }
}

/// A connection with the values of its variables where it runs.
pub struct Resolved {
    pub manifest: Manifest,
    pub connection: Connection,
    pub env: BTreeMap<String, String>,
}

impl Resolved {
    /// The value of a setting (`url`, `user`): written in quadra.toml, or its variable's here.
    pub fn setting(&self, s: &quadra_sema::manifest::Setting) -> Result<String, String> {
        match s {
            quadra_sema::manifest::Setting::Literal(l) => Ok(l.clone()),
            quadra_sema::manifest::Setting::Var(v) => self.value(v),
        }
    }

    /// The value of a variable here (a credential).
    pub fn value(&self, name: &str) -> Result<String, String> {
        self.manifest
            .value(&self.env, name)
            .map(|s| s.to_string())
            .ok_or_else(|| {
                format!(
                    "the variable {name} of `{}` is not set where it runs",
                    self.connection.name
                )
            })
    }
}

/// What a pull asks of a connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ask", rename_all = "snake_case")]
pub enum Ask {
    /// the files matching `pattern` (`in/chiusure/*.csv`: a folder and a glob), oldest first;
    /// those of `settle` first moved to `processed/` or `rejected/` (with their report)
    Files {
        pattern: String,
        settle: Vec<Settle>,
        limit: u32,
    },
    /// the rows of `table` whose `column` is after `after`, in its order, at most `limit`; each
    /// a JSON object of its columns, its cursor the value of `column`
    Rows {
        table: String,
        column: Option<String>,
        after: Option<String>,
        limit: u32,
    },
    /// the messages of `mailbox` not taken yet; those of `settle` moved or flagged
    Mail {
        mailbox: String,
        settle: Vec<Settle>,
        limit: u32,
    },
}

/// A message taken before, now handled: where it goes on the other side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settle {
    /// its name there (the file's path, the message's id)
    pub name: String,
    /// done (`processed/`), else rejected (`rejected/`)
    pub ok: bool,
    /// what was wrong, for whoever looks there (a `<file>.report.txt` next to it)
    pub report: Option<String>,
}

/// A message a pull brought.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// its name there: the file's path, the message's id, the row's cursor
    pub name: String,
    /// its bytes, as base64 in JSON
    #[serde(with = "b64")]
    pub body: Vec<u8>,
    /// where the next pull starts after it (a row's cursor)
    pub cursor: Option<String>,
}

/// What a pull brought, and which messages it settled there.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fetched {
    pub items: Vec<Item>,
    pub settled: Vec<String>,
}

/// What an outbound puts on a connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "put", rename_all = "snake_case")]
pub enum Put {
    /// a file at `path` (written whole, then renamed into place: never seen half written)
    File {
        path: String,
        #[serde(with = "b64")]
        body: Vec<u8>,
    },
    /// a row in an exchange table: its columns as a JSON object; `key`, the idempotency key,
    /// lets the table refuse it again
    Row {
        table: String,
        row: serde_json::Value,
        key: String,
    },
}

/// Why a job failed, and whether trying again may help.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Failed {
    /// try again later (the server is down, a timeout)
    Again(String),
    /// it will not work as it is (refused, malformed)
    Never(String),
    /// it cannot run as configured (a value missing, a driver not installed): it waits
    Waiting(String),
}

impl Failed {
    pub fn text(&self) -> &str {
        match self {
            Failed::Again(e) | Failed::Never(e) | Failed::Waiting(e) => e,
        }
    }
}

/// A job for whoever runs a connection: the server, or an agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum Job {
    Fetch { conn: Conn, ask: Ask },
    Put { conn: Conn, put: Put },
}

/// What came of a job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Fetched(Fetched),
    Put,
    Failed(Failed),
}

/// Runs a job here, with the values of `env`. `public_only`: no address of a private network
/// (Quadra Cloud); an agent runs them in its own network.
pub async fn run(job: &Job, env: &BTreeMap<String, String>, public_only: bool) -> Outcome {
    match job {
        Job::Fetch { conn, ask } => match conn.open(env) {
            Err(e) => Outcome::Failed(Failed::Waiting(e)),
            Ok(c) => match fetch(&c, ask, public_only).await {
                Ok(f) => Outcome::Fetched(f),
                Err(f) => Outcome::Failed(f),
            },
        },
        Job::Put { conn, put } => match conn.open(env) {
            Err(e) => Outcome::Failed(Failed::Waiting(e)),
            Ok(c) => match send(&c, put, public_only).await {
                Ok(()) => Outcome::Put,
                Err(f) => Outcome::Failed(f),
            },
        },
    }
}

async fn fetch(c: &Resolved, ask: &Ask, public_only: bool) -> Result<Fetched, Failed> {
    match (&c.connection.kind, ask) {
        (ConnectionKind::Files { .. }, Ask::Files { .. }) => {
            files::fetch(c, ask, public_only).await
        }
        (ConnectionKind::Database { .. }, Ask::Rows { .. }) => db::fetch(c, ask, public_only).await,
        (ConnectionKind::Mail { .. }, Ask::Mail { .. }) => mail::fetch(c, ask, public_only).await,
        _ => Err(Failed::Never(format!(
            "`{}` ({}) is not pulled that way",
            c.connection.name,
            c.connection.kind_name()
        ))),
    }
}

async fn send(c: &Resolved, put: &Put, public_only: bool) -> Result<(), Failed> {
    match (&c.connection.kind, put) {
        (ConnectionKind::Files { .. }, Put::File { .. }) => files::put(c, put, public_only).await,
        (ConnectionKind::Database { .. }, Put::Row { .. }) => db::put(c, put, public_only).await,
        _ => Err(Failed::Never(format!(
            "`{}` ({}) does not take that",
            c.connection.name,
            c.connection.kind_name()
        ))),
    }
}

/// Bytes as base64 in JSON.
mod b64 {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let t = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(t)
            .map_err(serde::de::Error::custom)
    }
}

/// The text of a body for the inbox: as it is when it is UTF-8, else base64 (`true`).
pub fn body_text(body: &[u8]) -> (String, bool) {
    match std::str::from_utf8(body) {
        Ok(t) => (t.to_string(), false),
        Err(_) => (base64::engine::general_purpose::STANDARD.encode(body), true),
    }
}

/// The bytes of a body kept in the inbox (`base64`: as [`body_text`] wrote it).
pub fn body_bytes(text: &str, base64: bool) -> Vec<u8> {
    if base64 {
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .unwrap_or_default()
    } else {
        text.as_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_carries_its_connection_without_values_and_is_read_again_where_it_runs() {
        let m = quadra_sema::manifest::parse("[vars]\nPASS = { secret = true }\nHOST = {}\n\n[connections.erp]\nkind = \"sftp\"\nurl = { var = \"HOST\" }\nuser = \"quadra\"\npassword = { var = \"PASS\" }\nknown_host = \"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl\"\nlocation = \"agent:milano\"\n").unwrap();
        let conn = Conn::of(&m, "erp").unwrap();
        assert!(!conn.toml.contains("s3cret"));
        let job = Job::Fetch {
            conn: conn.clone(),
            ask: Ask::Files {
                pattern: "in/*.csv".into(),
                settle: vec![],
                limit: 100,
            },
        };
        let back: Job = serde_json::from_str(&serde_json::to_string(&job).unwrap()).unwrap();
        assert_eq!(back, job);
        let mut env = BTreeMap::new();
        env.insert("PASS".to_string(), "s3cret".to_string());
        env.insert("HOST".to_string(), "sftp://files.example.com".to_string());
        let r = conn.open(&env).unwrap();
        assert_eq!(r.connection.agent.as_deref(), Some("milano"));
        assert_eq!(r.value("PASS").unwrap(), "s3cret");
        let ConnectionKind::Files { url, .. } = &r.connection.kind else {
            panic!()
        };
        assert_eq!(r.setting(url).unwrap(), "sftp://files.example.com");
        // bytes travel as base64
        let item = Item {
            name: "a.txt".into(),
            body: vec![0xC1, 0xC2, 0x00, 0xFF],
            cursor: None,
        };
        let j = serde_json::to_string(&item).unwrap();
        assert!(j.contains("\"wcIA/w==\""), "{j}");
        assert_eq!(serde_json::from_str::<Item>(&j).unwrap(), item);
        assert_eq!(body_text(&[0xC1]), ("wQ==".to_string(), true));
        assert_eq!(body_bytes("wQ==", true), vec![0xC1]);
    }
}
