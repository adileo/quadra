//! Files pulled from SFTP and FTP servers and written to them: a file of rows comes in once,
//! even across a restart, and ends in `processed/` (or `rejected/`) with its report; a file
//! of rows is applied whole (`atomic per file`) or row by row (`atomic per row`); an outbound
//! writes a file whole, in the encoding of its record. The servers run in the test: an SFTP
//! server over a folder (russh and russh-sftp) and an FTP server (libunftp).

use quadra_sema::Project;
use quadra_server::{Config, Host, HostOptions, Limits, Outside, TenantResolver, TenantSpec};
use quadra_store::{auth, ops, Conn};
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::PrivateKey;
use russh::server::{Auth, ChannelOpenHandle, Msg, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---- an SFTP server over a folder ---------------------------------------------------------

struct Sftp {
    port: u16,
    /// its key, as a line of known_hosts
    known_host: String,
    /// renames refused (a server whose folder cannot be written)
    refuse_moves: Arc<AtomicBool>,
}

const PASSWORD: &str = "s3cret";

/// An SFTP server on a free port, serving `root`, for the user `quadra` with [`PASSWORD`];
/// `seed` makes its key.
fn sftp_server(root: &Path, seed: u8) -> Sftp {
    let key = PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
    let known_host = key.public_key().to_openssh().unwrap();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let port = l.local_addr().unwrap().port();
    let refuse_moves = Arc::new(AtomicBool::new(false));
    let (root, refuse) = (root.to_path_buf(), refuse_moves.clone());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let config = Arc::new(russh::server::Config {
                keys: vec![key],
                auth_rejection_time: Duration::from_millis(10),
                auth_rejection_time_initial: Some(Duration::ZERO),
                ..Default::default()
            });
            let l = tokio::net::TcpListener::from_std(l).unwrap();
            loop {
                let Ok((s, _)) = l.accept().await else {
                    continue;
                };
                let handler = SshSession {
                    root: root.clone(),
                    refuse: refuse.clone(),
                    channels: Arc::default(),
                };
                let config = config.clone();
                tokio::spawn(async move {
                    if let Ok(session) = russh::server::run_stream(config, s, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
    });
    Sftp {
        port,
        known_host,
        refuse_moves,
    }
}

struct SshSession {
    root: PathBuf,
    refuse: Arc<AtomicBool>,
    channels: Arc<tokio::sync::Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl russh::server::Handler for SshSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == "quadra" && password == PASSWORD {
            Auth::Accept
        } else {
            Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            }
        })
    }

    async fn auth_publickey(
        &mut self,
        _: &str,
        _: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(Auth::Reject {
            proceed_with_methods: None,
            partial_success: false,
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.close(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let channel = self.channels.lock().await.remove(&id);
        match (name, channel) {
            ("sftp", Some(channel)) => {
                session.channel_success(id)?;
                let fs = FsSftp {
                    root: self.root.clone(),
                    refuse: self.refuse.clone(),
                    handles: HashMap::new(),
                    next: 0,
                };
                tokio::spawn(russh_sftp::server::run(channel.into_stream(), fs));
            }
            _ => session.channel_failure(id)?,
        }
        Ok(())
    }
}

enum Open {
    File(std::fs::File),
    Dir(Option<Vec<File>>),
}

/// The SFTP requests over a folder of this machine.
struct FsSftp {
    root: PathBuf,
    refuse: Arc<AtomicBool>,
    handles: HashMap<String, Open>,
    next: u64,
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

fn io(e: std::io::Error) -> StatusCode {
    match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

impl FsSftp {
    fn path(&self, p: &str) -> PathBuf {
        let p = p.trim_start_matches('/');
        if p.is_empty() || p == "." {
            self.root.clone()
        } else {
            self.root.join(p)
        }
    }

    fn handle(&mut self, open: Open) -> String {
        self.next += 1;
        let h = format!("h{}", self.next);
        self.handles.insert(h.clone(), open);
        h
    }
}

impl russh_sftp::server::Handler for FsSftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _: u32, _: HashMap<String, String>) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let f = std::fs::OpenOptions::new()
            .read(pflags.contains(OpenFlags::READ))
            .write(pflags.contains(OpenFlags::WRITE))
            .create(pflags.contains(OpenFlags::CREATE))
            .truncate(pflags.contains(OpenFlags::TRUNCATE))
            .open(self.path(&filename))
            .map_err(io)?;
        Ok(Handle {
            id,
            handle: self.handle(Open::File(f)),
        })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle);
        Ok(ok(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let Some(Open::File(f)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        f.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut buf = vec![0; len as usize];
        let n = f.read(&mut buf).map_err(io)?;
        if n == 0 {
            return Err(StatusCode::Eof);
        }
        buf.truncate(n);
        Ok(Data { id, data: buf })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let Some(Open::File(f)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        f.seek(SeekFrom::Start(offset)).map_err(io)?;
        f.write_all(&data).map_err(io)?;
        Ok(ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let m = std::fs::metadata(self.path(&path)).map_err(io)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&m),
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let dir = self.path(&path);
        if !dir.is_dir() {
            return Err(StatusCode::NoSuchFile);
        }
        let files = std::fs::read_dir(dir)
            .map_err(io)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                Some(File::new(
                    e.file_name().to_string_lossy(),
                    FileAttributes::from(&m),
                ))
            })
            .collect();
        Ok(Handle {
            id,
            handle: self.handle(Open::Dir(Some(files))),
        })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        match self.handles.get_mut(&handle) {
            Some(Open::Dir(files)) => match files.take() {
                Some(files) => Ok(Name { id, files }),
                None => Err(StatusCode::Eof),
            },
            _ => Err(StatusCode::Failure),
        }
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        std::fs::remove_file(self.path(&filename)).map_err(io)?;
        Ok(ok(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _: FileAttributes,
    ) -> Result<Status, Self::Error> {
        std::fs::create_dir(self.path(&path)).map_err(io)?;
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![File::dummy(format!(
                "/{}",
                path.trim_start_matches(['.', '/'])
            ))],
        })
    }

    async fn rename(&mut self, id: u32, from: String, to: String) -> Result<Status, Self::Error> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(StatusCode::PermissionDenied);
        }
        let to = self.path(&to);
        if to.exists() {
            return Err(StatusCode::Failure);
        }
        std::fs::rename(self.path(&from), to).map_err(io)?;
        Ok(ok(id))
    }
}

// ---- an FTP server over a folder ----------------------------------------------------------

/// An FTP server (plain, any user) on a free port, serving `root`.
fn ftp_server(root: &Path) -> u16 {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let server = libunftp::ServerBuilder::new(Box::new(move || {
                unftp_sbe_fs::Filesystem::new(root.clone()).unwrap()
            }))
            .passive_ports(41000..=41999)
            .build()
            .unwrap();
            server.listen(format!("127.0.0.1:{port}")).await.unwrap();
        });
    });
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    port
}

// ---- the app ------------------------------------------------------------------------------

struct One(TenantSpec);

impl TenantResolver for One {
    fn resolve(&self, _: &str) -> Option<TenantSpec> {
        Some(self.0.clone())
    }
}

fn now() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("quadra-files-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The project in `dir` with the connection `pos` (its settings) and `code`; its database
/// initialized; a token.
fn project(dir: &Path, connection: &str, code: &str) -> (Config, String) {
    let app = dir.join("app");
    let _ = std::fs::remove_dir_all(&app);
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("quadra.toml"),
        format!("[project]\nname = \"f\"\n\n[vars]\nPOS_PASSWORD = {{ secret = true }}\n\n[connections.pos]\n{connection}password = {{ var = \"POS_PASSWORD\" }}\ntimeout = \"5s\"\n"),
    )
    .unwrap();
    std::fs::write(app.join("main.qd"), code).unwrap();
    let url = format!("sqlite:{}", app.join("data.db").display());
    let checked = quadra_analysis::check(&Project::load(&app).unwrap());
    assert!(
        checked.model.is_some(),
        "{:?}",
        checked
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
    ops::init(
        &url,
        checked.model.as_ref().unwrap(),
        &checked.report,
        now(),
    )
    .unwrap();
    let (token, _) = auth::create(
        &mut Conn::open(&url).unwrap(),
        "token",
        1,
        "test",
        false,
        now(),
        None,
    )
    .unwrap();
    let mut env = BTreeMap::new();
    env.insert("POS_PASSWORD".to_string(), PASSWORD.to_string());
    let cfg = Config {
        outside: Outside { env, deliver: true },
        root: app.clone(),
        url,
        port: 0,
        dev_login: false,
        secure_cookies: false,
        cache_mb: 16,
        prewarm_days: None,
        web: None,
        sweep_every: 0,
        editors: vec![],
        watch: false,
        limits: Limits::default(),
        engines: Default::default(),
    };
    (cfg, token)
}

fn sftp_connection(s: &Sftp) -> String {
    format!(
        "kind = \"sftp\"\nurl = \"sftp://127.0.0.1:{}\"\nuser = \"quadra\"\nknown_host = \"{}\"\n",
        s.port, s.known_host
    )
}

struct Running {
    host: Host,
    rt: tokio::runtime::Runtime,
    port: u16,
}

impl Running {
    /// Stops the tenant and the host.
    fn stop(self) {
        let h = self.host.clone();
        self.rt.block_on(async move { h.stop("f").await });
        self.rt.shutdown_timeout(Duration::from_secs(5));
    }
}

/// The host serving the app, its tenant woken.
fn serve(cfg: Config, token: &str) -> Running {
    let host = Host::new(
        HostOptions::default(),
        Arc::new(One(TenantSpec {
            id: "f".into(),
            config: cfg,
        })),
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let h = host.clone();
    rt.spawn(async move {
        h.serve(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .unwrap()
    });
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let r = Running { host, rt, port };
    wake(r.port, token);
    r
}

/// A request that starts the tenant (and its pulls).
fn wake(port: u16, token: &str) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let req = format!("GET /v1/me HTTP/1.1\r\nHost: f.test\r\nConnection: close\r\nAuthorization: Bearer {token}\r\n\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
}

fn sql(cfg: &Config, q: &str) -> Vec<Vec<Option<String>>> {
    Conn::open(&cfg.url).unwrap().query(q, &[]).unwrap()
}

fn count(cfg: &Config, table: &str) -> usize {
    sql(
        cfg,
        &format!("SELECT CAST(COUNT(*) AS TEXT) FROM \"{table}\""),
    )[0][0]
        .as_deref()
        .unwrap()
        .parse()
        .unwrap()
}

/// (state, error, report, settled) of each message of the inbox.
fn inbox(cfg: &Config) -> Vec<(String, Option<String>, Option<String>, bool)> {
    sql(cfg, "SELECT \"state\", \"error\", \"report\", \"settled_at\" FROM \"_quadra_inbox\" ORDER BY \"id\"")
        .into_iter()
        .map(|r| (r[0].clone().unwrap(), r[1].clone(), r[2].clone(), r[3].is_some()))
        .collect()
}

fn pull_error(cfg: &Config) -> Option<String> {
    sql(cfg, "SELECT \"error\" FROM \"_quadra_cursors\"")
        .into_iter()
        .next()
        .and_then(|r| r[0].clone())
}

fn until(what: &str, secs: u64, ok: impl Fn() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(
            start.elapsed() < Duration::from_secs(secs),
            "waited for {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

const SALES: &str = "entity Sale:\n    store: Code\n    sku: Code\n    qty: Int\n    amount: Decimal\n\nrecord PosRow from csv(sep=\";\"):\n    store: Code at \"NEGOZIO\"\n    sku: Code at \"SKU\"\n    qty: Int at \"QTA\"\n    amount: Decimal at \"IMPORTO\" decimals 2\n\n";

fn closing(atomic: &str) -> String {
    format!("{SALES}inbound Closing via pos as r: PosRow:\n    pull \"in/chiusure/*.csv\" every 1 minutes\n{atomic}    if r.qty == 0:\n        fail \"a quantity of zero\"\n    let s = Sale(store=r.store, sku=r.sku, qty=r.qty, amount=r.amount)\n")
}

/// A closing of `n` rows; the rows of `zero` have a quantity of zero, those of `bad` cannot be
/// read.
fn rows(n: usize, zero: &[usize], bad: &[usize]) -> String {
    let mut out = String::from("NEGOZIO;SKU;QTA;IMPORTO\n");
    for i in 1..=n {
        let qty = if zero.contains(&i) { 0 } else { 1 + i % 5 };
        if bad.contains(&i) {
            out.push_str(&format!("MI01;A-{i};many;1.00\n"));
        } else {
            out.push_str(&format!("MI01;A-{i};{qty};{}.{:02}\n", i % 100, i % 97));
        }
    }
    out
}

fn put_file(root: &Path, path: &str, body: &[u8]) {
    let p = root.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

// ---- the tests ----------------------------------------------------------------------------

#[test]
fn a_file_comes_in_once_across_a_restart_and_ends_in_processed() {
    // the servers of the test hold sockets: not beside the test that counts them
    let _alone = super::host::MEMORY
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = temp("restart");
    let root = dir.join("sftp");
    put_file(
        &root,
        "in/chiusure/2026-11-27.csv",
        rows(50, &[], &[]).as_bytes(),
    );
    put_file(&root, "in/chiusure/notes.txt", b"not a closing");
    let server = sftp_server(&root, 1);
    // the server cannot move files: the file is handled, then stays where it is
    server.refuse_moves.store(true, Ordering::SeqCst);
    let (cfg, token) = project(&dir, &sftp_connection(&server), &closing(""));
    let first = serve(cfg.clone(), &token);
    until("the rows", 30, || count(&cfg, "sale") == 50);
    until("the move refused", 30, || {
        pull_error(&cfg).is_some_and(|e| e.contains("moving in/chiusure/2026-11-27.csv"))
    });
    assert!(root.join("in/chiusure/2026-11-27.csv").exists());
    first.stop();

    // started again, the server fixed, the next pull due: the same file is listed again,
    // recognised, not handled again, and moved
    server.refuse_moves.store(false, Ordering::SeqCst);
    sql(&cfg, "UPDATE \"_quadra_cursors\" SET \"next_at\" = NULL");
    let second = serve(cfg.clone(), &token);
    until("the file moved", 30, || {
        root.join("in/chiusure/processed/2026-11-27.csv").exists()
    });
    until("settled", 10, || inbox(&cfg).iter().all(|m| m.3));
    assert_eq!(count(&cfg, "sale"), 50);
    let messages = inbox(&cfg);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0].0, "done");
    assert!(!root.join("in/chiusure/2026-11-27.csv").exists());
    assert!(!root
        .join("in/chiusure/processed/2026-11-27.csv.report.txt")
        .exists());
    assert!(root.join("in/chiusure/notes.txt").exists());
    let origin = sql(&cfg, "SELECT \"origin\" FROM \"_quadra_inbox\"");
    assert_eq!(origin[0][0].as_deref(), Some("in/chiusure/2026-11-27.csv"));

    // the same file sent again: not handled again, moved like the first (beside it)
    put_file(
        &root,
        "in/chiusure/2026-11-27.csv",
        rows(50, &[], &[]).as_bytes(),
    );
    sql(&cfg, "UPDATE \"_quadra_cursors\" SET \"next_at\" = NULL");
    wake(second.port, &token);
    until("moved again", 30, || {
        !root.join("in/chiusure/2026-11-27.csv").exists()
            && std::fs::read_dir(root.join("in/chiusure/processed"))
                .unwrap()
                .count()
                == 2
    });
    assert_eq!(count(&cfg, "sale"), 50);
    second.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_with_bad_rows_is_rejected_whole_with_its_report() {
    // the servers of the test hold sockets: not beside the test that counts them
    let _alone = super::host::MEMORY
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = temp("per-file");
    let root = dir.join("sftp");
    put_file(
        &root,
        "in/chiusure/a.csv",
        rows(20, &[], &[4, 9]).as_bytes(),
    );
    put_file(&root, "in/chiusure/b.csv", rows(20, &[12], &[]).as_bytes());
    put_file(&root, "in/chiusure/c.csv", rows(20, &[], &[]).as_bytes());
    let server = sftp_server(&root, 2);
    let (cfg, token) = project(
        &dir,
        &sftp_connection(&server),
        &closing("    atomic per file\n"),
    );
    let r = serve(cfg.clone(), &token);
    until("the files moved", 40, || {
        root.join("in/chiusure/rejected/a.csv").exists()
            && root.join("in/chiusure/rejected/b.csv").exists()
            && root.join("in/chiusure/processed/c.csv").exists()
    });
    // only the good file is in
    assert_eq!(count(&cfg, "sale"), 20);
    let a = std::fs::read_to_string(root.join("in/chiusure/rejected/a.csv.report.txt")).unwrap();
    assert_eq!(
        a,
        "PosRow: the file is not applied, no row was written: 2 rows of 20 cannot be read\nline 5, column `QTA`: an integer was expected, not `many`\nline 10, column `QTA`: an integer was expected, not `many`\n"
    );
    let b = std::fs::read_to_string(root.join("in/chiusure/rejected/b.csv.report.txt")).unwrap();
    assert!(b.contains("no row was written"), "{b}");
    assert!(b.contains("line 13: a quantity of zero"), "{b}");
    assert!(!root.join("in/chiusure/processed/c.csv.report.txt").exists());
    r.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_applied_row_by_row_keeps_the_good_rows_and_reports_the_others() {
    // the servers of the test hold sockets: not beside the test that counts them
    let _alone = super::host::MEMORY
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = temp("per-row");
    let root = dir.join("sftp");
    // 1200 rows: three chunks; a row that cannot be read, three whose code fails
    put_file(
        &root,
        "in/chiusure/a.csv",
        rows(1200, &[3, 700, 1200], &[42]).as_bytes(),
    );
    // every row fails: rejected
    put_file(&root, "in/chiusure/b.csv", rows(2, &[1, 2], &[]).as_bytes());
    let server = sftp_server(&root, 3);
    let (cfg, token) = project(
        &dir,
        &sftp_connection(&server),
        &closing("    atomic per row\n"),
    );
    let r = serve(cfg.clone(), &token);
    until("the files moved", 60, || {
        root.join("in/chiusure/processed/a.csv").exists()
            && root.join("in/chiusure/rejected/b.csv").exists()
    });
    assert_eq!(count(&cfg, "sale"), 1196);
    let a = std::fs::read_to_string(root.join("in/chiusure/processed/a.csv.report.txt")).unwrap();
    assert_eq!(
        a,
        "PosRow: 4 rows of 1200 refused, the others are in\nline 4: a quantity of zero\nline 43, column `QTA`: an integer was expected, not `many`\nline 701: a quantity of zero\nline 1201: a quantity of zero\n"
    );
    let b = std::fs::read_to_string(root.join("in/chiusure/rejected/b.csv.report.txt")).unwrap();
    assert!(
        b.starts_with("PosRow: every row was refused (2 rows)\n"),
        "{b}"
    );
    let states: Vec<String> = inbox(&cfg).into_iter().map(|m| m.0).collect();
    assert_eq!(states, ["done", "rejected"]);
    // committed in chunks of 500 rows, each row once
    let causes = sql(&cfg, "SELECT \"cause\" FROM \"_quadra_commits\" WHERE \"cause\" LIKE '%inbound:Closing#1/%' ORDER BY \"tx\"");
    assert!(causes.len() <= 8, "{causes:?}");
    assert!(
        causes[0][0]
            .as_deref()
            .unwrap()
            .contains("inbound:Closing#1/lines 2-3 handled"),
        "{causes:?}"
    );

    // stopped halfway (its chunks committed, the message not yet marked): started again, the
    // rows committed are not run again
    r.stop();
    sql(&cfg, "UPDATE \"_quadra_inbox\" SET \"state\" = 'received', \"settled_at\" = NULL, \"report\" = NULL WHERE \"id\" = 1");
    let again = serve(cfg.clone(), &token);
    until("handled again", 30, || {
        inbox(&cfg)[0].0 == "done" && inbox(&cfg)[0].2.is_some()
    });
    assert_eq!(count(&cfg, "sale"), 1196);
    again.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_sftp_server_with_another_key_is_refused() {
    // the servers of the test hold sockets: not beside the test that counts them
    let _alone = super::host::MEMORY
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = temp("key");
    let root = dir.join("sftp");
    put_file(&root, "in/chiusure/a.csv", rows(2, &[], &[]).as_bytes());
    let server = sftp_server(&root, 4);
    let other = sftp_server(&dir.join("other"), 5);
    let pinned = Sftp {
        port: server.port,
        known_host: other.known_host.clone(),
        refuse_moves: Arc::default(),
    };
    let (cfg, token) = project(&dir, &sftp_connection(&pinned), &closing(""));
    let r = serve(cfg.clone(), &token);
    until("the pull refused", 20, || pull_error(&cfg).is_some());
    let e = pull_error(&cfg).unwrap();
    assert!(e.contains("not the one of known_host"), "{e}");
    assert!(e.contains("SHA256:"), "{e}");
    assert_eq!(count(&cfg, "sale"), 0);
    r.stop();

    // the right key and a wrong password: refused, said so
    let (cfg, token) = project(
        &dir,
        &sftp_connection(&server).replace("user = \"quadra\"", "user = \"nobody\""),
        &closing(""),
    );
    let r = serve(cfg.clone(), &token);
    until("the login refused", 20, || pull_error(&cfg).is_some());
    let e = pull_error(&cfg).unwrap();
    assert!(e.contains("refused the password of `nobody`"), "{e}");
    r.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

const EXPORT: &str = "entity Sale:\n    store: Code\n    sku: Code\n    qty: Int\n    amount: Decimal\n\non Sale created as s:\n    if s.qty < 0:\n        emit Exported(store=s.store, sku=s.sku, qty=s.qty, amount=s.amount)\n\nevent Exported(store: Code, sku: Code, qty: Int, amount: Decimal)\n\nrecord HostRow from fixed(encoding=\"ibm-1144\"):\n    store: Code at \"1..4\"\n    sku: Text at \"5..12\"\n    qty: Int at \"13..15\" zoned\n    amount: Decimal at \"16..19\" packed decimals 2\n\noutbound Exported via pos as e:\n    write(f\"out/{e.store}/{e.sku}.dat\", [HostRow(store=e.store, sku=e.sku, qty=e.qty, amount=e.amount)])\n";

/// Creates a sale and exports it, through the API.
fn export(port: u16, token: &str, sku: &str) {
    let body = serde_json::json!({ "ops": [
        { "op": "create", "entity": "Sale", "values": { "store": "MI01", "sku": sku, "qty": -3, "amount": "1234.56" } }
    ] })
    .to_string();
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let req = format!("POST /v1/tx HTTP/1.1\r\nHost: f.test\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: export-{sku}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
}

/// The bytes of the record written for `export`: in EBCDIC 1144, a quantity zoned and an
/// amount packed.
fn exported(sku: &str) -> Vec<u8> {
    let mut v: Vec<u8> = vec![0xD4, 0xC9, 0xF0, 0xF1]; // MI01
    let mut s: Vec<u8> = sku
        .chars()
        .map(|c| match c {
            'A' => 0xC1,
            '-' => 0x60,
            d => 0xF0 + d.to_digit(10).unwrap() as u8,
        })
        .collect();
    s.resize(8, 0x40);
    v.extend(s);
    v.extend([0xF0, 0xF0, 0xD3]); // -3
    v.extend([0x01, 0x23, 0x45, 0x6C]); // 1234.56
    v
}

#[test]
fn an_outbound_writes_a_file_whole_in_the_encoding_of_its_record() {
    // the servers of the test hold sockets: not beside the test that counts them
    let _alone = super::host::MEMORY
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = temp("write");
    let root = dir.join("sftp");
    std::fs::create_dir_all(&root).unwrap();
    let server = sftp_server(&root, 6);
    let (cfg, token) = project(&dir, &sftp_connection(&server), EXPORT);
    let r = serve(cfg.clone(), &token);
    export(r.port, &token, "A-1");
    until("the file written", 30, || {
        root.join("out/MI01/A-1.dat").exists()
    });
    assert_eq!(
        std::fs::read(root.join("out/MI01/A-1.dat")).unwrap(),
        exported("A-1")
    );
    // no file left half written
    let names: Vec<String> = std::fs::read_dir(root.join("out/MI01"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, ["A-1.dat"]);
    r.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ftp_pulls_a_file_and_writes_one() {
    // the servers of the test hold sockets: not beside the test that counts them
    let _alone = super::host::MEMORY
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = temp("ftp");
    let root = dir.join("ftp");
    put_file(&root, "in/chiusure/a.csv", rows(30, &[], &[]).as_bytes());
    let port = ftp_server(&root);
    let connection = format!(
        "kind = \"ftp\"\nurl = \"ftp://127.0.0.1:{port}\"\ninsecure = true\nuser = \"quadra\"\n"
    );
    // the closing comes in, and a sale with a negative quantity goes out
    let code = format!(
        "{}{}",
        EXPORT,
        closing("").split("inbound Closing").nth(1).map(|i| format!("record PosRow from csv(sep=\";\"):\n    store: Code at \"NEGOZIO\"\n    sku: Code at \"SKU\"\n    qty: Int at \"QTA\"\n    amount: Decimal at \"IMPORTO\" decimals 2\n\ninbound Closing{i}")).unwrap()
    );
    let (cfg, token) = project(&dir, &connection, &code);
    let r = serve(cfg.clone(), &token);
    until("the file moved", 40, || {
        root.join("in/chiusure/processed/a.csv").exists()
    });
    assert_eq!(count(&cfg, "sale"), 30);
    export(r.port, &token, "A-2");
    until("the file written", 30, || {
        root.join("out/MI01/A-2.dat").exists()
    });
    assert_eq!(
        std::fs::read(root.join("out/MI01/A-2.dat")).unwrap(),
        exported("A-2")
    );
    r.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cash register's closing of 100,000 rows, pulled over SFTP and handled in one transaction
/// (`atomic per file`), then another row by row: how long each takes, from the moment the
/// pull starts. `cargo test --release -p quadra-server --test it closing_of_100000 -- --ignored --nocapture`
#[test]
#[ignore]
fn a_closing_of_100000_rows_comes_in_fast() {
    // the servers of the test hold sockets: not beside the test that counts them
    let _alone = super::host::MEMORY
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for atomic in ["file", "row"] {
        let dir = temp(&format!("large-{atomic}"));
        let root = dir.join("sftp");
        let file = rows(100_000, &[], &[]);
        put_file(&root, "in/chiusure/big.csv", file.as_bytes());
        // old enough not to be looked at twice
        let old = std::time::SystemTime::now() - Duration::from_secs(600);
        std::fs::File::options()
            .write(true)
            .open(root.join("in/chiusure/big.csv"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let server = sftp_server(&root, 7);
        let (cfg, token) = project(
            &dir,
            &sftp_connection(&server),
            &closing(&format!("    atomic per {atomic}\n")),
        );
        let start = Instant::now();
        let r = serve(cfg.clone(), &token);
        until("the rows", 600, || {
            inbox(&cfg).first().is_some_and(|m| m.0 != "received")
        });
        let handled = start.elapsed();
        assert_eq!(count(&cfg, "sale"), 100_000, "{:?}", inbox(&cfg));
        until("the file moved", 60, || {
            root.join("in/chiusure/processed/big.csv").exists()
        });
        println!(
            "100,000 rows ({} KB), atomic per {atomic}: handled in {:.1} s, moved in {:.1} s",
            file.len() / 1024,
            handled.as_secs_f64(),
            start.elapsed().as_secs_f64()
        );
        assert!(handled < Duration::from_secs(30), "{handled:?}");
        r.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
