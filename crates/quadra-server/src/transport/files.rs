//! Files on a server: SFTP (`sftp://host`, its key pinned by `known_host`) or FTP (`ftps://host`,
//! TLS asked with `AUTH TLS` on the same port; plain `ftp://` only with `insecure = true`), in
//! passive mode and binary.
//!
//! A pull (`Ask::Files`) first settles the files handled since the last one: each is moved to
//! the folder `processed/` (handled) or `rejected/` next to it, created when missing, with its
//! report as `<file>.report.txt` beside it (written before the move, so that a file is never
//! moved without it). A file already there takes the time as a suffix (`a.csv.20261127-101403`),
//! never overwriting another. Then the folder of the pattern is listed: the files whose name
//! matches its last part (`*`, `?`; `in/chiusure/*.csv`), oldest first by modification time, at
//! most `limit`, each read whole as an item named by its path (`in/chiusure/a.csv`).
//!
//! A file still being written is left for the next pull: one modified in the last minute is
//! listed again after two seconds, and taken only if its size and time did not change. (A
//! server whose clock is off by more than a minute costs those two seconds at each pull.)
//! Files whose name starts with `.` are never taken: a sender may upload under such a name
//! and rename when done.
//!
//! A put (`Put::File`) writes `.<name>.quadra-<n>.part` in the same folder (created when
//! missing), then renames it into place: no one sees the file half written.
//!
//! The url's path, when it has one (`sftp://host/data`), is the folder the paths start from;
//! else they start from the folder the user logs in to. With `public_only`, the server's name
//! must resolve to public addresses, and FTP's data connections go to the address of its
//! control connection whatever the server announces.

use super::{Ask, Failed, Fetched, Item, Put, Resolved, Settle};
use quadra_sema::manifest::{ConnectionKind, FileProtocol};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a file must be unmodified to be taken without looking at it twice.
const STEADY_S: i64 = 60;
/// How long between the two looks at a file modified recently.
const SECOND_LOOK: Duration = Duration::from_secs(2);

pub async fn fetch(c: &Resolved, ask: &Ask, public_only: bool) -> Result<Fetched, Failed> {
    let Ask::Files {
        pattern,
        settle,
        limit,
    } = ask
    else {
        return Err(Failed::Never("not a pull of files".into()));
    };
    let (dir, glob) = match pattern.rsplit_once('/') {
        Some((d, g)) => (d.to_string(), g.to_string()),
        None => (String::new(), pattern.clone()),
    };
    let mut remote = Remote::open(c, public_only).await?;
    let result = async {
        let mut settled = Vec::new();
        for s in settle {
            remote.settle(s).await?;
            settled.push(s.name.clone());
        }
        let items = remote.take(&dir, &glob, *limit as usize).await?;
        Ok(Fetched { items, settled })
    }
    .await;
    remote.close().await;
    result
}

pub async fn put(c: &Resolved, put: &Put, public_only: bool) -> Result<(), Failed> {
    let Put::File { path, body } = put else {
        return Err(Failed::Never("a file connection takes files".into()));
    };
    if path.is_empty() || path.split('/').any(|p| p == "..") || path.starts_with('/') {
        return Err(Failed::Never(format!(
            "the path `{path}` is a path in the connection's folder: no `..`, not from `/`"
        )));
    }
    let mut remote = Remote::open(c, public_only).await?;
    let r = remote.write_into_place(path, body).await;
    remote.close().await;
    r
}

// ---- where, and who ---------------------------------------------------------------------

/// The server of a connection: its host, port, and the folder the paths start from.
struct Target {
    protocol: FileProtocol,
    tls: bool,
    host: String,
    port: u16,
    base: String,
}

fn target(c: &Resolved) -> Result<Target, Failed> {
    let ConnectionKind::Files {
        protocol,
        url,
        insecure,
        ..
    } = &c.connection.kind
    else {
        return Err(Failed::Never("not a file connection".into()));
    };
    let url = c.setting(url).map_err(Failed::Waiting)?;
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| Failed::Waiting(format!("`{url}` is not a url (sftp://host)")))?;
    let (tls, default_port) = match (protocol, scheme) {
        (FileProtocol::Sftp, "sftp") => (false, 22),
        (FileProtocol::Ftp, "ftps") => (true, 21),
        (FileProtocol::Ftp, "ftp") if *insecure => (false, 21),
        (FileProtocol::Ftp, "ftp") => {
            return Err(Failed::Waiting(format!(
                "`{url}`: plain ftp:// only with `insecure = true`"
            )))
        }
        _ => {
            return Err(Failed::Waiting(format!(
                "`{url}`: a {} connection is {}",
                c.connection.kind_name(),
                if *protocol == FileProtocol::Sftp {
                    "sftp://host"
                } else {
                    "ftps://host (ftp:// with insecure = true)"
                }
            )))
        }
    };
    let (authority, base) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{}", p.trim_end_matches('/'))),
        None => (rest, String::new()),
    };
    let base = if base == "/" { String::new() } else { base };
    // `host`, `host:2222`, `[::1]`, `[::1]:2222`
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !authority.starts_with('[') || h.ends_with(']') => {
            let port = p
                .parse()
                .map_err(|_| Failed::Waiting(format!("`{url}`: the port is not a number")))?;
            (h, port)
        }
        _ => (authority, default_port),
    };
    Ok(Target {
        protocol: *protocol,
        tls,
        host: host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
        port,
        base,
    })
}

fn timeout_of(c: &Resolved) -> Duration {
    match &c.connection.kind {
        ConnectionKind::Files { timeout_s, .. } => Duration::from_secs(*timeout_s),
        _ => Duration::from_secs(30),
    }
}

/// A connection to the server's port, through the addresses its name resolves to (public
/// ones only with `public_only`).
async fn tcp(
    t: &Target,
    public_only: bool,
    limit: Duration,
) -> Result<tokio::net::TcpStream, Failed> {
    let addrs = crate::deliver::addresses(&t.host, t.port, public_only)
        .await
        .map_err(|e| {
            if public_only && e.contains("not a public address") {
                Failed::Never(e)
            } else {
                Failed::Again(e)
            }
        })?;
    let mut last = String::new();
    for a in addrs {
        match tokio::time::timeout(limit, tokio::net::TcpStream::connect(a)).await {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) => last = format!("{}:{}: {e}", t.host, t.port),
            Err(_) => last = format!("{}:{}: no answer in {}s", t.host, t.port, limit.as_secs()),
        }
    }
    Err(Failed::Again(last))
}

// ---- the two protocols ------------------------------------------------------------------

type Ftp = suppaftp::tokio::ImplAsyncFtpStream<suppaftp::tokio::AsyncRustlsStream>;

enum Session {
    Sftp {
        sftp: russh_sftp::client::SftpSession,
        ssh: russh::client::Handle<HostKey>,
    },
    Ftp(Box<Ftp>),
}

/// A file in a listing.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    name: String,
    size: u64,
    /// unix seconds, when the server says
    mtime: Option<i64>,
}

struct Remote {
    session: Session,
    base: String,
    limit: Duration,
    name: String,
}

/// Checks the server's key against `known_host`, and remembers the one it showed.
struct HostKey {
    expected: russh::keys::PublicKey,
    shown: Arc<std::sync::Mutex<Option<String>>>,
}

impl russh::client::Handler for HostKey {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            *self.shown.lock().unwrap() = Some("a certificate".into());
            return Ok(false);
        };
        let same = key.key_data() == self.expected.key_data();
        if !same {
            *self.shown.lock().unwrap() = Some(format!(
                "{} {}",
                key.algorithm(),
                key.fingerprint(russh::keys::HashAlg::Sha256)
            ));
        }
        Ok(same)
    }
}

/// The key of a `known_hosts` line (`ssh-ed25519 AAAA… [comment]`, with or without the host
/// in front).
fn known_key(line: &str) -> Result<russh::keys::PublicKey, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let at = words
        .iter()
        .position(|w| w.starts_with("ssh-") || w.starts_with("ecdsa-") || w.starts_with("sk-"))
        .ok_or("known_host is a line of known_hosts: `ssh-ed25519 AAAA…` (`ssh-keyscan -t ed25519 host`)")?;
    let b64 = words
        .get(at + 1)
        .ok_or("known_host has no key after its type")?;
    let key = russh::keys::parse_public_key_base64(b64)
        .map_err(|e| format!("known_host is not a key: {e}"))?;
    if key.algorithm().as_str() != words[at] {
        return Err(format!(
            "known_host says {} but its key is {}",
            words[at],
            key.algorithm()
        ));
    }
    Ok(key)
}

fn sftp_err(what: &str, e: russh_sftp::client::error::Error) -> Failed {
    Failed::Again(format!("{what}: {e}"))
}

fn ftp_err(what: &str, e: suppaftp::FtpError) -> Failed {
    let text = match &e {
        suppaftp::FtpError::UnexpectedResponse(r) => format!(
            "the server answered {}: {}",
            r.status.code(),
            String::from_utf8_lossy(&r.body).trim()
        ),
        other => other.to_string(),
    };
    Failed::Again(format!("{what}: {text}"))
}

impl Remote {
    async fn open(c: &Resolved, public_only: bool) -> Result<Remote, Failed> {
        let t = target(c)?;
        let limit = timeout_of(c);
        let ConnectionKind::Files {
            user,
            password,
            private_key,
            known_host,
            ..
        } = &c.connection.kind
        else {
            return Err(Failed::Never("not a file connection".into()));
        };
        let user = c.setting(user).map_err(Failed::Waiting)?;
        let password = password
            .as_ref()
            .map(|v| c.value(v))
            .transpose()
            .map_err(Failed::Waiting)?;
        let name = format!("{}:{}", t.host, t.port);
        let late = |what: &str| {
            Failed::Again(format!(
                "{name}: no answer in {}s ({what})",
                limit.as_secs()
            ))
        };
        let session = match t.protocol {
            FileProtocol::Sftp => {
                let known = known_host
                    .as_ref()
                    .ok_or_else(|| Failed::Waiting("an sftp connection needs known_host".into()))
                    .and_then(|k| c.setting(k).map_err(Failed::Waiting))?;
                let expected = known_key(&known).map_err(Failed::Waiting)?;
                let key = private_key
                    .as_ref()
                    .map(|v| c.value(v))
                    .transpose()
                    .map_err(Failed::Waiting)?
                    .map(|k| {
                        russh::keys::decode_secret_key(&k, None).map_err(|e| {
                            Failed::Waiting(format!(
                                "the private_key of `{}` cannot be read: {e}",
                                c.connection.name
                            ))
                        })
                    })
                    .transpose()?;
                let stream = tcp(&t, public_only, limit).await?;
                let shown = Arc::new(std::sync::Mutex::new(None));
                let algorithm = expected.algorithm();
                let mut config = russh::client::Config {
                    inactivity_timeout: Some(limit * 4),
                    ..Default::default()
                };
                // the server shows its key of the kind pinned, not another of its keys
                let kinds: Vec<russh::keys::Algorithm> = if algorithm.clone().is_rsa() {
                    vec![
                        russh::keys::Algorithm::Rsa {
                            hash: Some(russh::keys::HashAlg::Sha512),
                        },
                        russh::keys::Algorithm::Rsa {
                            hash: Some(russh::keys::HashAlg::Sha256),
                        },
                        russh::keys::Algorithm::Rsa { hash: None },
                    ]
                } else {
                    vec![algorithm]
                };
                config.preferred.key = std::borrow::Cow::Owned(kinds);
                let handler = HostKey {
                    expected,
                    shown: shown.clone(),
                };
                let connected = tokio::time::timeout(
                    limit,
                    russh::client::connect_stream(Arc::new(config), stream, handler),
                )
                .await
                .map_err(|_| late("the SSH handshake"))?;
                let mut ssh = match connected {
                    Ok(h) => h,
                    Err(e) => {
                        if let Some(k) = shown.lock().unwrap().clone() {
                            return Err(Failed::Never(format!(
                                "{name} showed the key {k}, not the one of known_host: refused (someone may pose as the server; if its key changed, update known_host)"
                            )));
                        }
                        return Err(Failed::Again(format!("{name}: {e}")));
                    }
                };
                let auth = match &key {
                    Some(k) => {
                        let hash = if k.algorithm().is_rsa() {
                            ssh.best_supported_rsa_hash().await.ok().flatten().flatten()
                        } else {
                            None
                        };
                        let k = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(k.clone()), hash);
                        tokio::time::timeout(limit, ssh.authenticate_publickey(&user, k)).await
                    }
                    None => {
                        tokio::time::timeout(
                            limit,
                            ssh.authenticate_password(&user, password.clone().unwrap_or_default()),
                        )
                        .await
                    }
                }
                .map_err(|_| late("the login"))?
                .map_err(|e| Failed::Again(format!("{name}: {e}")))?;
                if !auth.success() {
                    return Err(Failed::Never(format!(
                        "{name} refused the {} of `{user}`",
                        if key.is_some() { "key" } else { "password" }
                    )));
                }
                let open = async {
                    let channel = ssh.channel_open_session().await?;
                    channel.request_subsystem(true, "sftp").await?;
                    Ok::<_, russh::Error>(channel)
                };
                let channel = tokio::time::timeout(limit, open)
                    .await
                    .map_err(|_| late("the SFTP channel"))?
                    .map_err(|e| Failed::Again(format!("{name}: SFTP: {e}")))?;
                let sftp = tokio::time::timeout(
                    limit,
                    russh_sftp::client::SftpSession::new(channel.into_stream()),
                )
                .await
                .map_err(|_| late("SFTP"))?
                .map_err(|e| sftp_err(&format!("{name}: SFTP"), e))?;
                sftp.set_timeout(limit.as_secs().max(1));
                Session::Sftp { sftp, ssh }
            }
            FileProtocol::Ftp => {
                let stream = tcp(&t, public_only, limit).await?;
                let ftp = async {
                    let mut ftp = Ftp::connect_with_stream(stream)
                        .await
                        .map_err(|e| ftp_err(&name, e))?;
                    if t.tls {
                        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config()?));
                        ftp = ftp
                            .into_secure(
                                suppaftp::tokio::AsyncRustlsConnector::from(connector),
                                &t.host,
                            )
                            .await
                            .map_err(|e| ftp_err(&format!("{name}: TLS"), e))?;
                    }
                    // the data connections go where the control one goes
                    ftp.set_passive_nat_workaround(true);
                    match ftp
                        .login(user.as_str(), password.as_deref().unwrap_or_default())
                        .await
                    {
                        Ok(()) => {}
                        Err(suppaftp::FtpError::UnexpectedResponse(r))
                            if r.status == suppaftp::Status::NotLoggedIn =>
                        {
                            return Err(Failed::Never(format!(
                                "{name} refused the password of `{user}`"
                            )))
                        }
                        Err(e) => return Err(ftp_err(&name, e)),
                    }
                    ftp.transfer_type(suppaftp::types::FileType::Binary)
                        .await
                        .map_err(|e| ftp_err(&name, e))?;
                    Ok(ftp)
                };
                let ftp = tokio::time::timeout(limit, ftp)
                    .await
                    .map_err(|_| late("the login"))??;
                Session::Ftp(Box::new(ftp))
            }
        };
        Ok(Remote {
            session,
            base: t.base,
            limit,
            name,
        })
    }

    async fn close(self) {
        match self.session {
            Session::Sftp { sftp, ssh } => {
                let _ = sftp.close().await;
                let _ = ssh
                    .disconnect(russh::Disconnect::ByApplication, "", "en")
                    .await;
            }
            Session::Ftp(mut ftp) => {
                let _ = ftp.quit().await;
            }
        }
    }

    /// A path of the connection on the server.
    fn full(&self, path: &str) -> String {
        match (self.base.as_str(), path) {
            ("", p) => p.to_string(),
            (b, "") => b.to_string(),
            (b, p) => format!("{b}/{p}"),
        }
    }

    /// `f` within the time limit (`times` the connection's `timeout`).
    async fn timed<T>(
        limit: Duration,
        name: &str,
        what: &str,
        f: impl std::future::Future<Output = Result<T, Failed>>,
    ) -> Result<T, Failed> {
        tokio::time::timeout(limit, f).await.map_err(|_| {
            Failed::Again(format!(
                "{name}: no answer in {}s ({what})",
                limit.as_secs()
            ))
        })?
    }

    /// The files of a folder (not the folders).
    async fn list(&mut self, dir: &str) -> Result<Vec<Entry>, Failed> {
        let path = self.full(dir);
        let shown = if dir.is_empty() { "." } else { dir };
        let what = format!("listing {shown}");
        let (limit, name) = (self.limit, self.name.clone());
        let f = async {
            match &mut self.session {
                Session::Sftp { sftp, .. } => {
                    let p = if path.is_empty() {
                        ".".to_string()
                    } else {
                        path.clone()
                    };
                    let entries = sftp
                        .read_dir(p)
                        .await
                        .map_err(|e| sftp_err(&format!("{name}: {what}"), e))?;
                    Ok(entries
                        .filter(|e| e.file_type().is_file())
                        .map(|e| {
                            let m = e.metadata();
                            Entry {
                                name: e.file_name().to_string(),
                                size: m.size.unwrap_or(0),
                                mtime: m.mtime.map(i64::from),
                            }
                        })
                        .collect())
                }
                Session::Ftp(ftp) => {
                    let p = (!path.is_empty()).then_some(path.as_str());
                    let lines = match ftp.mlsd(p).await {
                        Ok(l) => l,
                        // an older server: LIST
                        Err(_) => ftp
                            .list(p)
                            .await
                            .map_err(|e| ftp_err(&format!("{name}: {what}"), e))?,
                    };
                    Ok(lines
                        .iter()
                        .filter_map(|l| {
                            suppaftp::list::ListParser::parse_mlsd(l)
                                .or_else(|_| suppaftp::list::ListParser::parse_posix(l))
                                .or_else(|_| suppaftp::list::ListParser::parse_dos(l))
                                .ok()
                        })
                        .filter(|f| f.is_file())
                        .map(|f| Entry {
                            name: f.name().to_string(),
                            size: f.size() as u64,
                            mtime: f
                                .modified()
                                .duration_since(UNIX_EPOCH)
                                .ok()
                                .map(|d| d.as_secs() as i64)
                                .filter(|s| *s > 0),
                        })
                        .collect())
                }
            }
        };
        Self::timed(limit, &name, &format!("listing {shown}"), f).await
    }

    /// Whether a file or folder is there.
    async fn exists(&mut self, path: &str) -> Result<bool, Failed> {
        let full = self.full(path);
        let (limit, name) = (self.limit, self.name.clone());
        let f = async {
            match &mut self.session {
                Session::Sftp { sftp, .. } => sftp
                    .try_exists(full)
                    .await
                    .map_err(|e| sftp_err(&format!("{name}: {path}"), e)),
                Session::Ftp(ftp) => {
                    // a file has a size; a folder can be entered
                    if ftp.size(&full).await.is_ok() {
                        return Ok(true);
                    }
                    let back = ftp.pwd().await.ok();
                    let there = ftp.cwd(&full).await.is_ok();
                    if let (true, Some(b)) = (there, back) {
                        let _ = ftp.cwd(&b).await;
                    }
                    Ok(there)
                }
            }
        };
        Self::timed(limit, &name, path, f).await
    }

    /// The folder and those above it, when missing.
    async fn mkdirs(&mut self, dir: &str) -> Result<(), Failed> {
        let mut at = String::new();
        for part in dir.split('/').filter(|p| !p.is_empty()) {
            at = if at.is_empty() {
                part.to_string()
            } else {
                format!("{at}/{part}")
            };
            if self.exists(&at).await? {
                continue;
            }
            let full = self.full(&at);
            let (limit, name) = (self.limit, self.name.clone());
            let what = format!("{name}: making the folder {at}");
            let f = async {
                match &mut self.session {
                    Session::Sftp { sftp, .. } => {
                        sftp.create_dir(full).await.map_err(|e| sftp_err(&what, e))
                    }
                    Session::Ftp(ftp) => ftp.mkdir(&full).await.map_err(|e| ftp_err(&what, e)),
                }
            };
            Self::timed(limit, &name, "making a folder", f).await?;
        }
        Ok(())
    }

    async fn read(&mut self, path: &str) -> Result<Vec<u8>, Failed> {
        let full = self.full(path);
        let (limit, name) = (self.limit, self.name.clone());
        let f = async {
            match &mut self.session {
                Session::Sftp { sftp, .. } => sftp
                    .read(full)
                    .await
                    .map_err(|e| sftp_err(&format!("{name}: reading {path}"), e)),
                Session::Ftp(ftp) => {
                    use tokio::io::AsyncReadExt;
                    let mut s = ftp
                        .retr_as_stream(&full)
                        .await
                        .map_err(|e| ftp_err(&format!("{name}: reading {path}"), e))?;
                    let mut buf = Vec::new();
                    s.read_to_end(&mut buf)
                        .await
                        .map_err(|e| Failed::Again(format!("{name}: reading {path}: {e}")))?;
                    s.finish()
                        .await
                        .map_err(|e| ftp_err(&format!("{name}: reading {path}"), e))?;
                    Ok(buf)
                }
            }
        };
        // a whole file: more time than a command
        Self::timed(limit * 10, &name, &format!("reading {path}"), f).await
    }

    async fn write(&mut self, path: &str, body: &[u8]) -> Result<(), Failed> {
        let full = self.full(path);
        let (limit, name) = (self.limit, self.name.clone());
        let f = async {
            match &mut self.session {
                Session::Sftp { sftp, .. } => {
                    use tokio::io::AsyncWriteExt;
                    let what = format!("{name}: writing {path}");
                    let mut f = sftp.create(full).await.map_err(|e| sftp_err(&what, e))?;
                    f.write_all(body)
                        .await
                        .map_err(|e| Failed::Again(format!("{what}: {e}")))?;
                    f.close()
                        .await
                        .map_err(|e| Failed::Again(format!("{what}: {e}")))
                }
                Session::Ftp(ftp) => {
                    let mut r = body;
                    ftp.put_file(&full, &mut r)
                        .await
                        .map(|_| ())
                        .map_err(|e| ftp_err(&format!("{name}: writing {path}"), e))
                }
            }
        };
        Self::timed(limit * 10, &name, &format!("writing {path}"), f).await
    }

    async fn remove(&mut self, path: &str) -> Result<(), Failed> {
        let full = self.full(path);
        let (limit, name) = (self.limit, self.name.clone());
        let what = format!("{name}: removing {path}");
        let f = async {
            match &mut self.session {
                Session::Sftp { sftp, .. } => {
                    sftp.remove_file(full).await.map_err(|e| sftp_err(&what, e))
                }
                Session::Ftp(ftp) => ftp.rm(&full).await.map_err(|e| ftp_err(&what, e)),
            }
        };
        Self::timed(limit, &name, "removing a file", f).await
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<(), Failed> {
        let (f, t) = (self.full(from), self.full(to));
        let (limit, name) = (self.limit, self.name.clone());
        let what = format!("{name}: moving {from} to {to}");
        let r = async {
            match &mut self.session {
                Session::Sftp { sftp, .. } => {
                    sftp.rename(f, t).await.map_err(|e| sftp_err(&what, e))
                }
                Session::Ftp(ftp) => ftp.rename(&f, &t).await.map_err(|e| ftp_err(&what, e)),
            }
        };
        Self::timed(limit, &name, "a rename", r).await
    }

    /// Moves a file handled to `processed/` or `rejected/` next to it, with its report.
    async fn settle(&mut self, s: &Settle) -> Result<(), Failed> {
        let (dir, file) = match s.name.rsplit_once('/') {
            Some((d, f)) => (d.to_string(), f.to_string()),
            None => (String::new(), s.name.clone()),
        };
        // moved already (a pull that stopped before saying so)
        if !self.exists(&s.name).await? {
            return Ok(());
        }
        let folder = if s.ok { "processed" } else { "rejected" };
        let into = if dir.is_empty() {
            folder.to_string()
        } else {
            format!("{dir}/{folder}")
        };
        self.mkdirs(&into).await?;
        let mut to = format!("{into}/{file}");
        if self.exists(&to).await? {
            to = format!("{to}.{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
        }
        if let Some(r) = &s.report {
            self.write(&format!("{to}.report.txt"), r.as_bytes())
                .await?;
        }
        self.rename(&s.name, &to).await
    }

    /// The files of `dir` matching `glob`, oldest first, at most `limit`, read whole; those
    /// still being written left for the next pull.
    async fn take(&mut self, dir: &str, glob: &str, limit: usize) -> Result<Vec<Item>, Failed> {
        let mut files: Vec<Entry> = self
            .list(dir)
            .await?
            .into_iter()
            .filter(|e| !e.name.starts_with('.') && matches(glob, &e.name))
            .collect();
        files.sort_by(|a, b| (a.mtime, &a.name).cmp(&(b.mtime, &b.name)));
        files.truncate(limit);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let young = |e: &Entry| e.mtime.is_none_or(|m| now - m < STEADY_S);
        if files.iter().any(young) {
            tokio::time::sleep(SECOND_LOOK).await;
            let again = self.list(dir).await?;
            files.retain(|e| !young(e) || again.contains(e));
        }
        let mut items = Vec::with_capacity(files.len());
        for e in files {
            let name = if dir.is_empty() {
                e.name.clone()
            } else {
                format!("{dir}/{}", e.name)
            };
            let body = self.read(&name).await?;
            items.push(Item {
                name,
                body,
                cursor: None,
            });
        }
        Ok(items)
    }

    /// Writes a file under a hidden name in its folder, then renames it into place.
    async fn write_into_place(&mut self, path: &str, body: &[u8]) -> Result<(), Failed> {
        let (dir, file) = match path.rsplit_once('/') {
            Some((d, f)) => (d.to_string(), f.to_string()),
            None => (String::new(), path.to_string()),
        };
        if !dir.is_empty() {
            self.mkdirs(&dir).await?;
        }
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let part = format!(".{file}.quadra-{n}.part");
        let part = if dir.is_empty() {
            part
        } else {
            format!("{dir}/{part}")
        };
        self.write(&part, body).await?;
        if let Err(e) = self.rename(&part, path).await {
            // a server that does not rename over a file: the old one goes first
            if self.exists(path).await.unwrap_or(false) {
                self.remove(path).await?;
                self.rename(&part, path).await?;
            } else {
                let _ = self.remove(&part).await;
                return Err(e);
            }
        }
        Ok(())
    }
}

/// The configuration of TLS for FTPS: the authorities the system trusts.
fn tls_config() -> Result<rustls::ClientConfig, Failed> {
    use rustls_platform_verifier::ConfigVerifierExt;
    rustls::ClientConfig::with_platform_verifier().map_err(|e| Failed::Waiting(format!("TLS: {e}")))
}

/// Whether a file name matches a pattern of `*` (any characters) and `?` (one).
fn matches(glob: &str, name: &str) -> bool {
    let (g, n): (Vec<char>, Vec<char>) = (glob.chars().collect(), name.chars().collect());
    let (mut gi, mut ni) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ni < n.len() {
        if gi < g.len() && (g[gi] == '?' || g[gi] == n[ni]) {
            gi += 1;
            ni += 1;
        } else if gi < g.len() && g[gi] == '*' {
            star = Some(gi);
            mark = ni;
            gi += 1;
        } else if let Some(s) = star {
            gi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    g[gi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_matches_the_name_of_a_file() {
        assert!(matches("*.csv", "chiusura-01.csv"));
        assert!(matches("*", "a"));
        assert!(matches("POS??_*.txt", "POS01_2026.txt"));
        assert!(!matches("*.csv", "a.csv.part"));
        assert!(!matches("POS??_*.txt", "POS1_2026.txt"));
        assert!(matches("a.csv", "a.csv"));
        assert!(!matches("a.csv", "b.csv"));
    }

    #[test]
    fn a_known_hosts_line_gives_the_key() {
        let k = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
        assert!(known_key(k).is_ok());
        assert!(known_key(&format!("files.example.com {k} root@x")).is_ok());
        assert!(known_key(
            "ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl"
        )
        .unwrap_err()
        .contains("says ssh-rsa"));
        assert!(known_key("AAAA").is_err());
    }
}
