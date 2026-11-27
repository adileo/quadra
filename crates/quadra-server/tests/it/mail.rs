//! Mailboxes over IMAP, PEC included, and OAuth2 with a user's consent: against a fake IMAP
//! server and a fake OAuth2 provider in this process.
//!
//! - a PEC received creates the invoice it carries, its receipts are kept and linked to the
//!   message they certify, and everything handled is moved out of the mailbox at the next pull
//! - settling moves or copies, finds a message again by its Message-ID once the mailbox was
//!   renumbered, and the cursor takes only what is new
//! - the consent: the redirect to the provider, the callback, the exchange with PKCE, tokens
//!   sealed in the database, refreshed when they expire and on a 401, a revoked one saying so;
//!   and a mailbox read with `AUTHENTICATE XOAUTH2`

use base64::Engine as _;
use quadra_sema::Project;
use quadra_server::transport::{self, Ask, Conn, Job, Outcome, Settle};
use quadra_server::{Config, Host, HostOptions, Limits, Outside, TenantResolver, TenantSpec};
use quadra_store::{auth, ops};
use serde_json::{json, Value as J};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/it/mail")
            .join(name),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------------------------
// a fake IMAP server: enough of RFC 3501 for the client (LOGIN, AUTHENTICATE XOAUTH2, SELECT,
// LIST, CREATE, UID SEARCH, UID FETCH, UID STORE, UID MOVE or UID COPY and UID EXPUNGE)

#[derive(Default)]
struct Mailbox {
    validity: u32,
    next: u32,
    msgs: Vec<(u32, BTreeSet<String>, Vec<u8>)>,
}

impl Mailbox {
    fn add(&mut self, raw: Vec<u8>, flags: BTreeSet<String>) {
        if self.next == 0 {
            self.next = 1;
        }
        self.msgs.push((self.next, flags, raw));
        self.next += 1;
    }
}

#[derive(Default)]
struct Imap {
    boxes: BTreeMap<String, Mailbox>,
    can_move: bool,
    user: String,
    password: Option<String>,
    /// the access tokens accepted by `AUTHENTICATE XOAUTH2`
    tokens: Vec<String>,
    /// the commands received, without their tags
    log: Vec<String>,
}

type ImapState = Arc<Mutex<Imap>>;

fn imap_server(state: ImapState) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let state = state.clone();
            std::thread::spawn(move || imap_session(s, state));
        }
    });
    port
}

/// The words of a command, quoted strings unquoted.
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' if quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ' ' if !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The UIDs of a set (`1,3:5,7:*`) among `uids`.
fn in_set(set: &str, uid: u32, max: u32) -> bool {
    set.split(',').any(|r| {
        let (a, b) = r.split_once(':').unwrap_or((r, r));
        let n = |x: &str| {
            if x == "*" {
                max
            } else {
                x.parse().unwrap_or(0)
            }
        };
        let (a, b) = (n(a), n(b));
        uid >= a.min(b) && uid <= a.max(b)
    })
}

fn imap_session(s: TcpStream, state: ImapState) {
    let mut w = s.try_clone().unwrap();
    let mut r = BufReader::new(s);
    let caps = |st: &Imap| {
        format!(
            "IMAP4rev1 UIDPLUS AUTH=XOAUTH2{}",
            if st.can_move { " MOVE" } else { "" }
        )
    };
    let greet = format!(
        "* OK [CAPABILITY {}] fake IMAP ready\r\n",
        caps(&state.lock().unwrap())
    );
    let _ = w.write_all(greet.as_bytes());
    let mut selected: Option<String> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let l = line.trim_end().to_string();
        let (tag, rest) = l.split_once(' ').unwrap_or((&l, ""));
        let tag = tag.to_string();
        let ws = words(rest);
        let mut st = state.lock().unwrap();
        st.log.push(rest.to_string());
        let mut out = String::new();
        let cmd: Vec<String> = ws.iter().map(|w| w.to_ascii_uppercase()).collect();
        let ok = |out: &mut String, what: &str| out.push_str(&format!("{tag} OK {what}\r\n"));
        match cmd.first().map(String::as_str) {
            Some("CAPABILITY") => {
                out.push_str(&format!("* CAPABILITY {}\r\n", caps(&st)));
                ok(&mut out, "done");
            }
            Some("LOGIN") => {
                if ws.get(1) == Some(&st.user) && ws.get(2) == st.password.as_ref() {
                    ok(&mut out, "logged in");
                } else {
                    out.push_str(&format!(
                        "{tag} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n"
                    ));
                }
            }
            Some("AUTHENTICATE") if cmd.get(1).map(String::as_str) == Some("XOAUTH2") => {
                drop(st);
                let _ = w.write_all(b"+ \r\n");
                let mut answer = String::new();
                r.read_line(&mut answer).unwrap();
                let said = base64::engine::general_purpose::STANDARD
                    .decode(answer.trim())
                    .map(|b| String::from_utf8_lossy(&b).to_string())
                    .unwrap_or_default();
                let st = state.lock().unwrap();
                let good = st
                    .tokens
                    .iter()
                    .any(|t| said == format!("user={}\x01auth=Bearer {t}\x01\x01", st.user));
                drop(st);
                if good {
                    let _ = w.write_all(format!("{tag} OK authenticated\r\n").as_bytes());
                } else {
                    // the error as Gmail sends it, an empty answer, then the refusal
                    let _ = w.write_all(b"+ eyJzdGF0dXMiOiI0MDAiLCJzY2hlbWVzIjoiQmVhcmVyIn0=\r\n");
                    let mut empty = String::new();
                    r.read_line(&mut empty).unwrap();
                    let _ = w.write_all(
                        format!(
                            "{tag} NO [AUTHENTICATIONFAILED] Invalid credentials (Failure)\r\n"
                        )
                        .as_bytes(),
                    );
                }
                continue;
            }
            Some("SELECT") => {
                let name = ws.get(1).cloned().unwrap_or_default();
                match st.boxes.get(&name) {
                    Some(mb) => {
                        out.push_str(&format!(
                            "* FLAGS (\\Seen \\Deleted)\r\n* {} EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY {}] valid\r\n* OK [UIDNEXT {}] next\r\n",
                            mb.msgs.len(),
                            mb.validity,
                            mb.next.max(1)
                        ));
                        selected = Some(name);
                        out.push_str(&format!("{tag} OK [READ-WRITE] selected\r\n"));
                    }
                    None => out.push_str(&format!("{tag} NO no such mailbox\r\n")),
                }
            }
            Some("LIST") => {
                out.push_str("* LIST (\\Noselect) \"/\" \"\"\r\n");
                ok(&mut out, "listed");
            }
            Some("CREATE") => {
                let name = ws.get(1).cloned().unwrap_or_default();
                if st.boxes.contains_key(&name) {
                    out.push_str(&format!("{tag} NO [ALREADYEXISTS] it exists\r\n"));
                } else {
                    st.boxes.insert(
                        name,
                        Mailbox {
                            validity: 9,
                            next: 1,
                            msgs: vec![],
                        },
                    );
                    ok(&mut out, "created");
                }
            }
            Some("UID") => {
                let sel = selected.clone().unwrap_or_default();
                let sub = cmd.get(1).cloned().unwrap_or_default();
                let set = ws.get(2).cloned().unwrap_or_default();
                let max = st
                    .boxes
                    .get(&sel)
                    .map(|m| m.next.saturating_sub(1))
                    .unwrap_or(0);
                match sub.as_str() {
                    "SEARCH" => {
                        let mb = st.boxes.get(&sel).unwrap();
                        let crit: Vec<String> = ws[2..].to_vec();
                        let hits: Vec<u32> = mb
                            .msgs
                            .iter()
                            .filter(|(uid, flags, raw)| {
                                let mut i = 0;
                                let mut keep = true;
                                while i < crit.len() {
                                    match crit[i].to_ascii_uppercase().as_str() {
                                        "UID" => {
                                            keep &= in_set(&crit[i + 1], *uid, max);
                                            i += 2;
                                        }
                                        "NOT" => {
                                            keep &= !flags.contains("\\Deleted");
                                            i += 2;
                                        }
                                        "HEADER" => {
                                            let want = format!("{}: {}", crit[i + 1], crit[i + 2])
                                                .to_ascii_lowercase();
                                            keep &= String::from_utf8_lossy(raw)
                                                .to_ascii_lowercase()
                                                .contains(&want);
                                            i += 3;
                                        }
                                        _ => i += 1,
                                    }
                                }
                                keep
                            })
                            .map(|(u, _, _)| *u)
                            .collect();
                        out.push_str(&format!(
                            "* SEARCH{}\r\n",
                            hits.iter().map(|u| format!(" {u}")).collect::<String>()
                        ));
                        ok(&mut out, "searched");
                    }
                    "FETCH" => {
                        let mb = st.boxes.get(&sel).unwrap();
                        let what = rest.to_ascii_uppercase();
                        let mut bytes: Vec<u8> = Vec::new();
                        for (i, (uid, _, raw)) in mb.msgs.iter().enumerate() {
                            if !in_set(&set, *uid, max) {
                                continue;
                            }
                            let seq = i + 1;
                            if what.contains("BODY.PEEK[HEADER]") {
                                let end = raw
                                    .windows(4)
                                    .position(|w| w == b"\r\n\r\n")
                                    .map_or(raw.len(), |p| p + 4);
                                bytes.extend(
                                    format!("* {seq} FETCH (UID {uid} BODY[HEADER] {{{end}}}\r\n")
                                        .as_bytes(),
                                );
                                bytes.extend(&raw[..end]);
                                bytes.extend(b")\r\n");
                            } else if what.contains("BODY.PEEK[]") {
                                bytes.extend(
                                    format!(
                                        "* {seq} FETCH (UID {uid} BODY[] {{{}}}\r\n",
                                        raw.len()
                                    )
                                    .as_bytes(),
                                );
                                bytes.extend(raw);
                                bytes.extend(b")\r\n");
                            } else {
                                bytes.extend(
                                    format!(
                                        "* {seq} FETCH (UID {uid} RFC822.SIZE {})\r\n",
                                        raw.len()
                                    )
                                    .as_bytes(),
                                );
                            }
                        }
                        bytes.extend(format!("{tag} OK fetched\r\n").as_bytes());
                        drop(st);
                        let _ = w.write_all(&bytes);
                        continue;
                    }
                    "STORE" => {
                        let flag = if rest.contains("\\Seen") {
                            "\\Seen"
                        } else {
                            "\\Deleted"
                        };
                        let mb = st.boxes.get_mut(&sel).unwrap();
                        for (uid, flags, _) in mb.msgs.iter_mut() {
                            if in_set(&set, *uid, max) {
                                flags.insert(flag.to_string());
                            }
                        }
                        ok(&mut out, "stored");
                    }
                    "MOVE" | "COPY" if sub == "COPY" || st.can_move => {
                        let to = ws.get(3).cloned().unwrap_or_default();
                        if !st.boxes.contains_key(&to) {
                            out.push_str(&format!("{tag} NO [TRYCREATE] no such mailbox\r\n"));
                        } else {
                            let mb = st.boxes.get_mut(&sel).unwrap();
                            let mut moved = Vec::new();
                            let mut expunged = Vec::new();
                            let mut i = 0;
                            while i < mb.msgs.len() {
                                if in_set(&set, mb.msgs[i].0, max) {
                                    if sub == "MOVE" {
                                        let m = mb.msgs.remove(i);
                                        expunged.push(i + 1);
                                        moved.push((m.1, m.2));
                                        continue;
                                    }
                                    moved.push((mb.msgs[i].1.clone(), mb.msgs[i].2.clone()));
                                }
                                i += 1;
                            }
                            let target = st.boxes.get_mut(&to).unwrap();
                            for (mut flags, raw) in moved {
                                flags.remove("\\Deleted");
                                target.add(raw, flags);
                            }
                            for seq in expunged.iter().rev() {
                                out.push_str(&format!("* {seq} EXPUNGE\r\n"));
                            }
                            ok(&mut out, "done");
                        }
                    }
                    "EXPUNGE" => {
                        let mb = st.boxes.get_mut(&sel).unwrap();
                        let mut i = 0;
                        while i < mb.msgs.len() {
                            if in_set(&set, mb.msgs[i].0, max) && mb.msgs[i].1.contains("\\Deleted")
                            {
                                mb.msgs.remove(i);
                                out.push_str(&format!("* {} EXPUNGE\r\n", i + 1));
                                continue;
                            }
                            i += 1;
                        }
                        ok(&mut out, "expunged");
                    }
                    _ => out.push_str(&format!("{tag} BAD not here\r\n")),
                }
            }
            Some("EXPUNGE") => {
                let sel = selected.clone().unwrap_or_default();
                let mb = st.boxes.get_mut(&sel).unwrap();
                mb.msgs.retain(|m| !m.1.contains("\\Deleted"));
                ok(&mut out, "expunged");
            }
            Some("NOOP") => ok(&mut out, "nothing"),
            Some("LOGOUT") => {
                let _ = w.write_all(format!("* BYE\r\n{tag} OK bye\r\n").as_bytes());
                return;
            }
            _ => out.push_str(&format!("{tag} BAD unknown command\r\n")),
        }
        drop(st);
        let _ = w.write_all(out.as_bytes());
    }
}

/// A fake server with the mailbox INBOX holding `msgs`.
fn imap_with(msgs: &[&str], can_move: bool) -> (ImapState, u16) {
    let mut inbox = Mailbox {
        validity: 1700000000,
        next: 1,
        msgs: vec![],
    };
    for m in msgs {
        inbox.add(fixture(m), BTreeSet::new());
    }
    let state = Arc::new(Mutex::new(Imap {
        boxes: BTreeMap::from([("INBOX".to_string(), inbox)]),
        can_move,
        user: "acme@pec.example.it".into(),
        password: Some("s3cret".into()),
        ..Default::default()
    }));
    let port = imap_server(state.clone());
    (state, port)
}

fn subjects(st: &ImapState, mailbox: &str) -> Vec<(String, bool)> {
    let st = st.lock().unwrap();
    st.boxes
        .get(mailbox)
        .map(|m| {
            m.msgs
                .iter()
                .map(|(_, flags, raw)| {
                    let d = quadra_server::transport::mail::document(raw);
                    (
                        d["subject"].as_str().unwrap_or("").to_string(),
                        flags.contains("\\Seen"),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// the jobs, as the server runs them

fn conn(port: u16) -> Conn {
    let m = quadra_sema::manifest::parse(&format!("[vars]\nPEC_PASSWORD = {{ secret = true }}\n\n[connections.pec]\nkind = \"imap\"\nurl = \"imap://127.0.0.1:{port}\"\ninsecure = true\nuser = \"acme@pec.example.it\"\npassword = {{ var = \"PEC_PASSWORD\" }}\ntimeout = \"5s\"\n")).unwrap();
    Conn::of(&m, "pec").unwrap()
}

fn pull(port: u16, settle: Vec<Settle>, after: Option<String>, password: &str) -> Outcome {
    let env = BTreeMap::from([("PEC_PASSWORD".to_string(), password.to_string())]);
    let job = Job::Fetch {
        conn: conn(port),
        ask: Ask::Mail {
            mailbox: "INBOX".into(),
            settle,
            limit: 100,
            after,
        },
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(transport::run(&job, &env, false))
}

#[test]
fn a_mailbox_is_read_after_its_cursor_and_what_was_handled_is_moved_out() {
    let (st, port) = imap_with(
        &["plain.eml", "posta-certificata.eml", "accettazione.eml"],
        true,
    );
    let Outcome::Fetched(f) = pull(port, vec![], None, "s3cret") else {
        panic!()
    };
    assert_eq!(f.items.len(), 3);
    assert_eq!(
        f.items[0].name,
        "1700000000:1:20261127.4711@shop.example.com"
    );
    assert_eq!(f.items[2].cursor.as_deref(), Some("1700000000:0000000003"));
    let doc: J = serde_json::from_slice(&f.items[1].body).unwrap();
    assert_eq!(doc["pec"]["type"], "posta-certificata");
    // taken, not read: only settling marks them
    assert!(subjects(&st, "INBOX").iter().all(|(_, seen)| !seen));
    // after the cursor: nothing new
    let Outcome::Fetched(f2) = pull(port, vec![], f.items[2].cursor.clone(), "s3cret") else {
        panic!()
    };
    assert!(f2.items.is_empty());
    // one done, one rejected: moved with \Seen; a new message comes after the cursor
    st.lock()
        .unwrap()
        .boxes
        .get_mut("INBOX")
        .unwrap()
        .add(fixture("avvenuta-consegna.eml"), BTreeSet::new());
    let settle = vec![
        Settle {
            name: f.items[1].name.clone(),
            ok: true,
            report: None,
        },
        Settle {
            name: f.items[0].name.clone(),
            ok: false,
            report: Some("not a PEC".into()),
        },
    ];
    let Outcome::Fetched(f3) = pull(port, settle, f.items[2].cursor.clone(), "s3cret") else {
        panic!()
    };
    assert_eq!(f3.settled.len(), 2);
    assert_eq!(f3.items.len(), 1);
    assert_eq!(
        f3.items[0].name,
        "1700000000:4:consegna.20261127102011.00042@pec.cliente.example.it"
    );
    assert_eq!(
        subjects(&st, "Quadra/processed"),
        [("POSTA CERTIFICATA: Fattura 2026/118".to_string(), true)]
    );
    assert_eq!(
        subjects(&st, "Quadra/rejected"),
        [("Order 1042".to_string(), true)]
    );
    assert_eq!(subjects(&st, "INBOX").len(), 2);
    assert!(st
        .lock()
        .unwrap()
        .log
        .iter()
        .any(|c| c.starts_with("UID MOVE")));
    // settled again (it is gone): settled all the same
    let settle = vec![Settle {
        name: f.items[1].name.clone(),
        ok: true,
        report: None,
    }];
    let Outcome::Fetched(f4) = pull(port, settle, f3.items[0].cursor.clone(), "s3cret") else {
        panic!()
    };
    assert_eq!(f4.settled, [f.items[1].name.clone()]);
    // renumbered: found again by its Message-ID, and everything is new to the cursor
    st.lock().unwrap().boxes.get_mut("INBOX").unwrap().validity = 1800000000;
    let settle = vec![Settle {
        name: f.items[2].name.clone(),
        ok: true,
        report: None,
    }];
    let Outcome::Fetched(f5) = pull(port, settle, f3.items[0].cursor.clone(), "s3cret") else {
        panic!()
    };
    assert_eq!(f5.settled.len(), 1);
    assert_eq!(subjects(&st, "Quadra/processed").len(), 2);
    assert_eq!(f5.items.len(), 1);
    assert!(f5.items[0].name.starts_with("1800000000:4:"));
    // a wrong password: refused for good, with why
    let Outcome::Failed(e) = pull(port, vec![], None, "wrong") else {
        panic!()
    };
    assert!(matches!(e, transport::Failed::Never(_)), "{e:?}");
    assert!(e.text().contains("refused the login"), "{e:?}");
}

#[test]
fn without_move_a_message_is_copied_flagged_and_expunged() {
    let (st, port) = imap_with(&["plain.eml", "accettazione.eml"], false);
    let Outcome::Fetched(f) = pull(port, vec![], None, "s3cret") else {
        panic!()
    };
    let settle = vec![Settle {
        name: f.items[0].name.clone(),
        ok: true,
        report: None,
    }];
    let Outcome::Fetched(f2) = pull(port, settle, f.items[1].cursor.clone(), "s3cret") else {
        panic!()
    };
    assert_eq!(f2.settled.len(), 1);
    assert_eq!(
        subjects(&st, "Quadra/processed"),
        [("Order 1042".to_string(), true)]
    );
    assert_eq!(
        subjects(&st, "INBOX"),
        [("ACCETTAZIONE: Ordine 77".to_string(), false)]
    );
    let log = st.lock().unwrap().log.clone();
    assert!(log.iter().any(|c| c.starts_with("UID COPY")), "{log:?}");
    assert!(log.iter().any(|c| c.starts_with("UID EXPUNGE")), "{log:?}");
    assert!(!log.iter().any(|c| c.starts_with("UID MOVE")), "{log:?}");
}

// ---------------------------------------------------------------------------------------------
// a whole app: the pass criterion

struct One(TenantSpec);

impl TenantResolver for One {
    fn resolve(&self, _: &str) -> Option<TenantSpec> {
        Some(self.0.clone())
    }
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("quadra-mail-{name}-{}", std::process::id()))
}

/// The project in `dir` with this quadra.toml and code, its database initialized; a token.
fn project(dir: &Path, toml: &str, code: &str, env: BTreeMap<String, String>) -> (Config, String) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("quadra.toml"), toml).unwrap();
    std::fs::write(dir.join("main.qd"), code).unwrap();
    let url = format!("sqlite:{}", dir.join("data.db").display());
    let checked = quadra_analysis::check(&Project::load(dir).unwrap());
    assert!(
        checked.model.is_some(),
        "{:?}",
        checked
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
    let now = chrono::Local::now().naive_local();
    ops::init(&url, checked.model.as_ref().unwrap(), &checked.report, now).unwrap();
    let (token, _) = auth::create(
        &mut quadra_store::Conn::open(&url).unwrap(),
        "token",
        1,
        "test",
        false,
        now,
        None,
    )
    .unwrap();
    let cfg = Config {
        outside: Outside { env, deliver: true },
        root: dir.to_path_buf(),
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

fn serve(cfg: Config) -> (Host, tokio::runtime::Runtime, u16) {
    let host = Host::new(
        HostOptions::default(),
        Arc::new(One(TenantSpec {
            id: "d".into(),
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
    (host, rt, port)
}

/// A request to the host: the status, its headers (lower case) and its body as text.
fn request(
    port: u16,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<J>,
) -> (u16, HashMap<String, String>, String) {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    static KEY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let key = KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!("{method} {path} HTTP/1.1\r\nHost: d.test\r\nConnection: close\r\n{auth}Idempotency-Key: mail-{}-{key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", std::process::id(), body.len());
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, rest) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    (status, headers, rest.to_string())
}

fn call(port: u16, token: &str, method: &str, path: &str, body: Option<J>) -> (u16, J) {
    let (s, _, b) = request(port, Some(token), method, path, body);
    (s, serde_json::from_str(&b).unwrap_or(J::Null))
}

fn query(cfg: &Config, sql: &str) -> Vec<Vec<Option<String>>> {
    quadra_store::Conn::open(&cfg.url)
        .unwrap()
        .query(sql, &[])
        .unwrap()
}

/// Waits (15 s at most) until `ready`.
fn wait_for(what: &str, ready: impl Fn() -> bool) {
    let start = Instant::now();
    while !ready() {
        if start.elapsed() > Duration::from_secs(15) {
            panic!("waited for {what}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

const PEC_CODE: &str = include_str!("mail/pec.qd");

#[test]
fn a_pec_received_creates_its_invoice_and_its_receipts_are_kept_with_their_message() {
    let (st, imap) = imap_with(
        &[
            "posta-certificata.eml",
            "accettazione.eml",
            "avvenuta-consegna.eml",
            "plain.eml",
        ],
        true,
    );
    let dir = temp("pec");
    let toml = format!("[project]\nname = \"pec\"\n\n[vars]\nPEC_PASSWORD = {{ secret = true }}\n\n[connections.pec]\nkind = \"imap\"\nurl = \"imap://127.0.0.1:{imap}\"\ninsecure = true\nuser = \"acme@pec.example.it\"\npassword = {{ var = \"PEC_PASSWORD\" }}\n");
    let env = BTreeMap::from([("PEC_PASSWORD".to_string(), "s3cret".to_string())]);
    let (cfg, token) = project(&dir, &toml, PEC_CODE, env);
    {
        let (_h, _rt, port) = serve(cfg.clone());
        // the PEC this app sent, whose receipts come back
        let (s, v) = call(
            port,
            &token,
            "POST",
            "/v1/tx",
            Some(
                json!({ "ops": [{ "op": "create", "entity": "SentPec", "values": { "message_id": "PEC-OUT-77@acme.example.it", "subject": "Ordine 77" } }] }),
            ),
        );
        assert_eq!(s, 200, "{v}");
        wait_for("the messages handled", || {
            query(&cfg, "SELECT \"state\" FROM \"_quadra_inbox\"")
                .iter()
                .filter(|r| r[0].as_deref() != Some("received"))
                .count()
                == 4
        });
    }
    let states = query(
        &cfg,
        "SELECT \"key\", \"state\", \"error\" FROM \"_quadra_inbox\" ORDER BY \"id\"",
    );
    assert_eq!(states[0][1].as_deref(), Some("done"), "{states:?}");
    assert_eq!(states[1][1].as_deref(), Some("done"), "{states:?}");
    assert_eq!(states[2][1].as_deref(), Some("done"), "{states:?}");
    assert_eq!(states[3][1].as_deref(), Some("rejected"), "{states:?}");
    assert!(states[3][2].as_deref().unwrap().contains("not a PEC"));
    // the invoice, read from the XML the PEC carries
    let inv = query(&cfg, "SELECT \"number\", \"supplier\", CAST(\"total\" AS TEXT), \"pec_id\", \"file_name\" FROM \"invoice\"");
    assert_eq!(inv.len(), 1, "{inv:?}");
    assert_eq!(inv[0][0].as_deref(), Some("2026/118"));
    assert_eq!(inv[0][1].as_deref(), Some("Fornitore Esempio S.r.l."));
    assert!(inv[0][2].as_deref().unwrap().starts_with("1220"));
    assert_eq!(
        inv[0][3].as_deref(),
        Some("opec21031.20261127101403.04711.06.1.1@pec.example.it")
    );
    assert_eq!(inv[0][4].as_deref(), Some("IT01234567890_00118.xml"));
    // the receipts, linked to the PEC they certify by its message id
    let rec = query(&cfg, "SELECT r.\"kind\", r.\"pec_message_id\", s.\"subject\" FROM \"pec_receipt\" r JOIN \"sent_pec\" s ON s.\"id\" = r.\"sent_id\" ORDER BY r.\"id\"");
    assert_eq!(rec.len(), 2, "{rec:?}");
    assert_eq!(rec[0][0].as_deref(), Some("accettazione"));
    assert_eq!(rec[1][0].as_deref(), Some("avvenuta-consegna"));
    assert!(rec
        .iter()
        .all(|r| r[1].as_deref() == Some("PEC-OUT-77@acme.example.it")
            && r[2].as_deref() == Some("Ordine 77")));
    let sent = query(
        &cfg,
        "SELECT \"accepted_at\", \"delivered_at\" FROM \"sent_pec\"",
    );
    assert!(
        sent[0][0]
            .as_deref()
            .unwrap()
            .starts_with("2026-11-27 09:19:00"),
        "{sent:?}"
    );
    assert!(
        sent[0][1]
            .as_deref()
            .unwrap()
            .starts_with("2026-11-27 09:20:11"),
        "{sent:?}"
    );
    // nothing moved yet: the next pull settles (after a restart, from the cursor kept)
    assert_eq!(subjects(&st, "INBOX").len(), 4);
    quadra_store::Conn::open(&cfg.url)
        .unwrap()
        .exec(
            "UPDATE \"_quadra_cursors\" SET \"next_at\" = '2000-01-01T00:00:00'",
            &[],
        )
        .unwrap();
    {
        let (_h, _rt, port) = serve(cfg.clone());
        call(port, &token, "GET", "/v1/me", None);
        wait_for("the messages settled", || subjects(&st, "INBOX").is_empty());
        wait_for("the settlement written", || {
            query(&cfg, "SELECT \"settled_at\" FROM \"_quadra_inbox\"")
                .iter()
                .all(|r| r[0].is_some())
        });
    }
    assert_eq!(subjects(&st, "Quadra/processed").len(), 3);
    assert_eq!(
        subjects(&st, "Quadra/rejected"),
        [("Order 1042".to_string(), true)]
    );
    assert!(subjects(&st, "Quadra/processed")
        .iter()
        .all(|(_, seen)| *seen));
    // taken once: no message fetched twice
    let log = st.lock().unwrap().log.clone();
    let bodies = log.iter().filter(|c| c.contains("BODY.PEEK[]")).count();
    assert_eq!(bodies, 4, "{log:?}");
    assert_eq!(
        query(&cfg, "SELECT COUNT(*) FROM \"invoice\"")[0][0].as_deref(),
        Some("1")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// a fake OAuth2 provider: its token endpoint and an API that wants its access tokens

#[derive(Default)]
struct Provider {
    /// the PKCE challenge of the authorization under way
    challenge: String,
    redirect_uri: String,
    issued: u32,
    /// how long the next access token lasts
    lasts: i64,
    valid_access: Vec<String>,
    valid_refresh: Vec<String>,
    /// requests to the token url: (grant_type, refresh_token)
    grants: Vec<(String, String)>,
    /// the bearer of each request to the API
    api: Vec<String>,
    /// the token given to a mailbox too
    imap: Option<ImapState>,
}

type ProviderState = Arc<Mutex<Provider>>;

fn form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| {
            let mut out = Vec::new();
            let b = v.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' && i + 2 < b.len() + 1 {
                    out.push(u8::from_str_radix(&v[i + 1..i + 3], 16).unwrap());
                    i += 3;
                } else {
                    out.push(if b[i] == b'+' { b' ' } else { b[i] });
                    i += 1;
                }
            }
            (k.to_string(), String::from_utf8(out).unwrap())
        })
        .collect()
}

fn provider(state: ProviderState) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut s in l.incoming().flatten() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head, body) = loop {
                let n = s.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break (String::new(), String::new());
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some((h, b)) = text.split_once("\r\n\r\n") {
                    let len: usize = h
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if b.len() >= len {
                        break (h.to_string(), b[..len].to_string());
                    }
                }
            };
            let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
            let header = |name: &str| {
                head.lines().find_map(|l| {
                    l.split_once(':')
                        .filter(|(k, _)| k.trim().eq_ignore_ascii_case(name))
                        .map(|(_, v)| v.trim().to_string())
                })
            };
            let mut p = state.lock().unwrap();
            let (status, answer) = if path == "/token" {
                let f = form(&body);
                let basic = format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode("quadra-app:app-s3cret")
                );
                let grant = f.get("grant_type").cloned().unwrap_or_default();
                p.grants.push((
                    grant.clone(),
                    f.get("refresh_token").cloned().unwrap_or_default(),
                ));
                let give = |p: &mut Provider| {
                    p.issued += 1;
                    let (a, r) = (format!("AT-{}", p.issued), format!("RT-{}", p.issued));
                    p.valid_access.push(a.clone());
                    p.valid_refresh.push(r.clone());
                    if let Some(imap) = &p.imap {
                        imap.lock().unwrap().tokens.push(a.clone());
                    }
                    (200, json!({ "access_token": a, "refresh_token": r, "expires_in": p.lasts, "token_type": "Bearer", "scope": "read_orders" }).to_string())
                };
                if header("authorization").as_deref() != Some(basic.as_str()) {
                    (401, json!({ "error": "invalid_client" }).to_string())
                } else if grant == "authorization_code" {
                    use sha2::Digest;
                    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
                    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .encode(sha2::Sha256::digest(verifier.as_bytes()));
                    if f.get("code").map(String::as_str) != Some("CODE-1")
                        || challenge != p.challenge
                        || f.get("redirect_uri") != Some(&p.redirect_uri)
                    {
                        (400, json!({ "error": "invalid_grant", "error_description": "bad code, verifier or redirect" }).to_string())
                    } else {
                        give(&mut p)
                    }
                } else if grant == "refresh_token"
                    && p.valid_refresh
                        .contains(f.get("refresh_token").unwrap_or(&String::new()))
                {
                    // rotated: the old one no longer works
                    let old = f["refresh_token"].clone();
                    p.valid_refresh.retain(|r| *r != old);
                    give(&mut p)
                } else {
                    (400, json!({ "error": "invalid_grant", "error_description": "Token has been expired or revoked." }).to_string())
                }
            } else {
                let bearer = header("authorization")
                    .and_then(|a| a.strip_prefix("Bearer ").map(str::to_string))
                    .unwrap_or_default();
                p.api.push(bearer.clone());
                if p.valid_access.contains(&bearer) {
                    (200, "{\"ok\":true}".to_string())
                } else {
                    (401, "{\"error\":\"invalid_token\"}".to_string())
                }
            };
            drop(p);
            let _ = s.write_all(format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len()).as_bytes());
        }
    });
    port
}

fn set_key() -> [u8; 32] {
    // one key for every test of the process
    std::env::set_var(
        quadra_server::oauth::KEY_VAR,
        base64::engine::general_purpose::STANDARD.encode([42u8; 32]),
    );
    [42u8; 32]
}

/// The consent of `connection`: the redirect followed by the admin, the provider's callback.
fn consent(port: u16, token: &str, connection: &str, prov: &ProviderState, provider_port: u16) {
    let (s, h, b) = request(
        port,
        Some(token),
        "GET",
        &format!("/v1/connections/{connection}/authorize"),
        None,
    );
    assert_eq!(s, 302, "{b}");
    let to = h["location"].clone();
    assert!(
        to.starts_with(&format!("http://127.0.0.1:{provider_port}/authorize?")),
        "{to}"
    );
    let q = form(to.split_once('?').unwrap().1);
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], "quadra-app");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(
        q["redirect_uri"],
        format!("http://d.test/oauth/{connection}/callback")
    );
    {
        let mut p = prov.lock().unwrap();
        p.challenge = q["code_challenge"].clone();
        p.redirect_uri = q["redirect_uri"].clone();
    }
    // a state not made by this server is refused
    let (s, _, b) = request(
        port,
        None,
        "GET",
        &format!("/oauth/{connection}/callback?code=CODE-1&state=v1.forged"),
        None,
    );
    assert_eq!(s, 400, "{b}");
    // the provider sends the admin back: no session needed, the state is the proof
    let state: String = q["state"].bytes().map(|b| format!("%{b:02X}")).collect();
    let (s, _, b) = request(
        port,
        None,
        "GET",
        &format!("/oauth/{connection}/callback?code=CODE-1&state={state}"),
        None,
    );
    assert_eq!(s, 200, "{b}");
    assert!(b.contains("is connected"), "{b}");
}

const SHOP: &str = "entity Order:\n    number: Text\n\nevent OrderPlaced(number: Text)\n\non Order created as o:\n    emit OrderPlaced(number=o.number)\n\noutbound OrderPlaced via shop path \"/orders\"\n";

fn place(port: u16, token: &str, number: &str) {
    let (s, v) = call(
        port,
        token,
        "POST",
        "/v1/tx",
        Some(
            json!({ "ops": [{ "op": "create", "entity": "Order", "values": { "number": number } }] }),
        ),
    );
    assert_eq!(s, 200, "{v}");
}

fn outbox(cfg: &Config) -> Vec<Vec<Option<String>>> {
    query(
        cfg,
        "SELECT \"state\", \"error\" FROM \"_quadra_outbox\" ORDER BY \"id\"",
    )
}

#[test]
fn a_connection_with_the_users_consent_keeps_its_tokens_sealed_and_refreshes_them() {
    let key = set_key();
    let prov = Arc::new(Mutex::new(Provider {
        lasts: 1,
        ..Default::default()
    }));
    let pp = provider(prov.clone());
    let dir = temp("consent");
    let toml = format!("[project]\nname = \"c\"\n\n[vars]\nSHOP_SECRET = {{ secret = true }}\n\n[connections.shop]\nkind = \"http\"\nurl = \"http://127.0.0.1:{pp}/api\"\ninsecure = true\noauth2_grant = \"authorization_code\"\noauth2_authorize_url = \"http://127.0.0.1:{pp}/authorize\"\noauth2_token_url = \"http://127.0.0.1:{pp}/token\"\nclient_id = \"quadra-app\"\nclient_secret = {{ var = \"SHOP_SECRET\" }}\nscope = \"read_orders\"\ntimeout = \"3s\"\n");
    let env = BTreeMap::from([("SHOP_SECRET".to_string(), "app-s3cret".to_string())]);
    let (cfg, token) = project(&dir, &toml, SHOP, env);
    let (_h, _rt, port) = serve(cfg.clone());
    let status = |port: u16| -> J {
        let (s, v) = call(port, &token, "GET", "/v1/integrations", None);
        assert_eq!(s, 200, "{v}");
        v["connections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "shop")
            .unwrap()["oauth2"]
            .clone()
    };
    assert_eq!(status(port)["connected"], false);
    assert_eq!(status(port)["authorize"], "/v1/connections/shop/authorize");

    consent(port, &token, "shop", &prov, pp);
    assert_eq!(prov.lock().unwrap().grants[0].0, "authorization_code");
    // the first token lasts a second, the next ones an hour
    prov.lock().unwrap().lasts = 3600;
    let st = status(port);
    assert_eq!(st["connected"], true, "{st}");
    assert_eq!(st["scope"], "read_orders");
    assert_eq!(st["connected_by"], "user:1");
    // sealed: never in the clear, in the database or in what the API shows
    let row = query(
        &cfg,
        "SELECT \"sealed\" FROM \"_quadra_oauth\" WHERE \"connection\" = 'shop'",
    );
    let sealed = row[0][0].clone().unwrap();
    assert!(!sealed.contains("AT-1") && !sealed.contains("RT-1"));
    assert_eq!(
        quadra_server::oauth::Tokens::open(&key, "shop", &sealed)
            .unwrap()
            .access_token,
        "AT-1"
    );
    let (_, all) = call(port, &token, "GET", "/v1/connections", None);
    assert!(
        !all.to_string().contains("AT-") && !all.to_string().contains("RT-"),
        "{all}"
    );

    // it lasts a second: refreshed before the first request
    place(port, &token, "A1");
    wait_for("A1 delivered", || {
        outbox(&cfg)
            .first()
            .is_some_and(|r| r[0].as_deref() == Some("delivered"))
    });
    {
        let p = prov.lock().unwrap();
        assert_eq!(
            p.grants[1],
            ("refresh_token".to_string(), "RT-1".to_string())
        );
        assert_eq!(p.api, ["AT-2"]);
    }
    // the refreshed tokens are written back
    wait_for("the refreshed tokens written", || {
        let s = query(&cfg, "SELECT \"sealed\" FROM \"_quadra_oauth\"")[0][0]
            .clone()
            .unwrap();
        quadra_server::oauth::Tokens::open(&key, "shop", &s)
            .unwrap()
            .access_token
            == "AT-2"
    });

    // the provider forgets the access token early: a 401, refreshed, sent again once
    prov.lock().unwrap().valid_access.clear();
    place(port, &token, "A2");
    wait_for("A2 delivered", || {
        outbox(&cfg)
            .get(1)
            .is_some_and(|r| r[0].as_deref() == Some("delivered"))
    });
    assert_eq!(prov.lock().unwrap().api, ["AT-2", "AT-2", "AT-3"]);

    // the consent revoked: the refresh is refused, and the event waits saying so
    {
        let mut p = prov.lock().unwrap();
        p.valid_access.clear();
        p.valid_refresh.clear();
    }
    place(port, &token, "A3");
    wait_for("A3 refused", || {
        outbox(&cfg).get(2).is_some_and(|r| r[1].is_some())
    });
    let e = outbox(&cfg)[2][1].clone().unwrap();
    assert_eq!(outbox(&cfg)[2][0].as_deref(), Some("pending"));
    assert!(
        e.contains("revoked or expired") && e.contains("/v1/connections/shop/authorize"),
        "{e}"
    );
    assert!(!e.contains("RT-") && !e.contains("AT-"), "{e}");
    wait_for("the error shown", || status(port)["connected"] == false);
    assert!(status(port)["error"].as_str().unwrap().contains("revoked"));

    // disconnected: forgotten
    let (s, v) = call(
        port,
        &token,
        "POST",
        "/v1/connections/shop/disconnect",
        None,
    );
    assert_eq!((s, v["disconnected"].clone()), (200, json!(true)), "{v}");
    assert!(query(&cfg, "SELECT * FROM \"_quadra_oauth\"").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_mailbox_of_gmail_or_microsoft_365_is_read_with_xoauth2_after_the_consent() {
    let key = set_key();
    let (st, imap) = imap_with(&["plain.eml"], true);
    st.lock().unwrap().password = None;
    let prov = Arc::new(Mutex::new(Provider {
        lasts: 3600,
        imap: Some(st.clone()),
        ..Default::default()
    }));
    let pp = provider(prov.clone());
    let dir = temp("xoauth2");
    let toml = format!("[project]\nname = \"x\"\n\n[vars]\nMAIL_SECRET = {{ secret = true }}\n\n[connections.gmail]\nkind = \"imap\"\nurl = \"imap://127.0.0.1:{imap}\"\ninsecure = true\nuser = \"acme@pec.example.it\"\noauth2_grant = \"authorization_code\"\noauth2_authorize_url = \"http://127.0.0.1:{pp}/authorize?access_type=offline&prompt=consent\"\noauth2_token_url = \"http://127.0.0.1:{pp}/token\"\nclient_id = \"quadra-app\"\nclient_secret = {{ var = \"MAIL_SECRET\" }}\nscope = \"https://mail.google.com/\"\ntimeout = \"5s\"\n");
    let code = "entity Note:\n    subject: Text\n\nrecord Mail from json:\n    message_id: Text\n    subject: Text\n\ninbound MailIn via gmail as m: Mail:\n    pull \"INBOX\" every 1 minutes\n    key m.message_id\n    let n = Note(subject=m.subject)\n";
    let env = BTreeMap::from([("MAIL_SECRET".to_string(), "app-s3cret".to_string())]);
    let (cfg, token) = project(&dir, &toml, code, env);
    {
        let (_h, _rt, port) = serve(cfg.clone());
        call(port, &token, "GET", "/v1/me", None);
        // before the consent: the pull waits, saying why
        wait_for("the first pull", || {
            !query(&cfg, "SELECT \"error\" FROM \"_quadra_cursors\"").is_empty()
        });
        let e = query(&cfg, "SELECT \"error\" FROM \"_quadra_cursors\"")[0][0]
            .clone()
            .unwrap();
        assert!(
            e.contains("not connected") && e.contains("/v1/connections/gmail/authorize"),
            "{e}"
        );
        consent(port, &token, "gmail", &prov, pp);
    }
    // the mailbox no longer takes the first token: refreshed once and read with the new one
    st.lock().unwrap().tokens.retain(|t| t != "AT-1");
    quadra_store::Conn::open(&cfg.url)
        .unwrap()
        .exec(
            "UPDATE \"_quadra_cursors\" SET \"next_at\" = '2000-01-01T00:00:00'",
            &[],
        )
        .unwrap();
    {
        let (_h, _rt, port) = serve(cfg.clone());
        call(port, &token, "GET", "/v1/me", None);
        wait_for("the note", || {
            !query(&cfg, "SELECT \"subject\" FROM \"note\"").is_empty()
        });
    }
    assert_eq!(
        query(&cfg, "SELECT \"subject\" FROM \"note\"")[0][0].as_deref(),
        Some("Order 1042")
    );
    let log = st.lock().unwrap().log.clone();
    assert_eq!(
        log.iter()
            .filter(|c| c.starts_with("AUTHENTICATE XOAUTH2"))
            .count(),
        2,
        "{log:?}"
    );
    assert!(!log.iter().any(|c| c.starts_with("LOGIN")));
    let grants = prov.lock().unwrap().grants.clone();
    assert_eq!(
        grants.last().unwrap(),
        &("refresh_token".to_string(), "RT-1".to_string())
    );
    wait_for("the refreshed tokens written", || {
        let s = query(&cfg, "SELECT \"sealed\" FROM \"_quadra_oauth\"")[0][0]
            .clone()
            .unwrap();
        quadra_server::oauth::Tokens::open(&key, "gmail", &s)
            .unwrap()
            .access_token
            == "AT-2"
    });
    let _ = std::fs::remove_dir_all(&dir);
}
