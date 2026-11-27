//! Files of `File` fields over HTTP: an upload asked for, its bytes put to the signed URL,
//! named by a transaction, downloaded through a signed URL; with the folder of the project
//! and with the S3 of development (`deploy/compose.dev.yml` of the cloud, skipped when it is
//! not running). A file of a type the field does not take, or too large, is refused with why;
//! a user who may not read the field gets `403`; the bytes are never in the database.

use crate::deliver::serve;
use quadra_sema::Project;
use quadra_server::{Config, HostOptions, Limits, Outside};
use quadra_store::{auth, ops, Conn};
use serde_json::{json, Value as J};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CODE: &str = r#"from expenses import Expense
from docs import Slip

role Clerk:
    read, create, edit Expense

role Auditor:
    read Expense
    hide Expense.receipt

attach Slip to Expense.slip

seed:
    User(login="clara", roles={Clerk})
    User(login="otto", roles={Auditor})
"#;

const EXPENSES: &str = "entity Expense:\n    title: Text\n    receipt: File<pdf, image>? max 1 KB\n    photos: [File<image>]\n    slip: File?\n";

const DOCS: &str = "from expenses import Expense\n\ndocument Slip(e: Expense):\n    text e.title\n";

/// A small PDF, with a mark to look for in the database.
const PDF: &[u8] = b"%PDF-1.4\n% QUADRA-MARK-0451\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n";

fn png(n: u8) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    b.extend_from_slice(&[n; 40]);
    b
}

fn exe() -> Vec<u8> {
    let mut b = b"MZ\x90\0\x03\0\0\0\x04\0\0\0".to_vec();
    b.extend_from_slice(&[0x41; 100]);
    b
}

/// An HTTP answer: status, headers (lower case), body.
struct Answer {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> J {
        serde_json::from_slice(&self.body).unwrap_or(J::Null)
    }
    fn header(&self, h: &str) -> &str {
        self.headers.get(h).map(String::as_str).unwrap_or("")
    }
}

/// A request to `url`: a path of the server on `port`, or an absolute `http://` URL.
fn http(port: u16, method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Answer {
    let (host, path) = match url.strip_prefix("http://") {
        Some(rest) => {
            let (h, p) = rest.split_once('/').unwrap();
            (h.to_string(), format!("/{p}"))
        }
        None => (format!("127.0.0.1:{port}"), url.to_string()),
    };
    let mut s = TcpStream::connect(&host).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let headers: HashMap<String, String> = head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let mut rest = raw[split + 4..].to_vec();
    if headers
        .get("transfer-encoding")
        .is_some_and(|t| t.contains("chunked"))
    {
        let mut out = Vec::new();
        let mut r = &rest[..];
        while let Some(i) = r.windows(2).position(|w| w == b"\r\n") {
            let n = usize::from_str_radix(String::from_utf8_lossy(&r[..i]).trim(), 16).unwrap_or(0);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&r[i + 2..i + 2 + n]);
            r = &r[i + 2 + n + 2..];
        }
        rest = out;
    }
    Answer {
        status,
        headers,
        body: rest,
    }
}

fn now() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

/// The project in `dir` with these `[files]` settings, its database initialized; the tokens
/// of clara (Clerk) and otto (Auditor).
fn project(dir: &Path, files: &str, env: BTreeMap<String, String>) -> (Config, String, String) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("quadra.toml"),
        format!("[project]\nname = \"u\"\n\n{files}"),
    )
    .unwrap();
    std::fs::write(dir.join("main.qd"), CODE).unwrap();
    std::fs::write(dir.join("expenses.qd"), EXPENSES).unwrap();
    std::fs::write(dir.join("docs.qdp"), DOCS).unwrap();
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
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    ops::init(&url, m, r, now()).unwrap();
    let mut db = quadra_store::Database::open(&url, m, r).unwrap();
    let mut interp = ops::loaded(&mut db, m, now()).unwrap();
    let id_of = |i: &mut quadra_runtime::Interp, login: &str| {
        i.ids("User", None)
            .into_iter()
            .find(|id| {
                i.store
                    .read(*id, "login", None)
                    .is_some_and(|v| v.to_string() == login)
            })
            .unwrap()
    };
    let (clara, otto) = (id_of(&mut interp, "clara"), id_of(&mut interp, "otto"));
    let mut conn = Conn::open(&url).unwrap();
    let (ct, _) = auth::create(&mut conn, "token", clara, "t", false, now(), None).unwrap();
    let (ot, _) = auth::create(&mut conn, "token", otto, "t", false, now(), None).unwrap();
    let cfg = Config {
        outside: Outside {
            env,
            deliver: false,
        },
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
    (cfg, ct, ot)
}

struct Client {
    port: u16,
    bearer: String,
}

impl Client {
    fn call(&self, method: &str, path: &str, body: Option<&J>, key: Option<&str>) -> Answer {
        let mut h = vec![("Authorization", self.bearer.as_str())];
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        if !body.is_empty() {
            h.push(("Content-Type", "application/json"));
        }
        if let Some(k) = key {
            h.push(("Idempotency-Key", k));
        }
        http(self.port, method, path, &h, body.as_bytes())
    }

    /// An upload asked for and its bytes put to its URL: the value to write (`upload:<id>`).
    fn upload(&self, field: &str, name: &str, bytes: &[u8]) -> String {
        let a = self.call(
            "POST",
            "/v1/uploads",
            Some(
                &json!({ "entity": "Expense", "field": field, "name": name, "size": bytes.len() }),
            ),
            None,
        );
        assert_eq!(a.status, 200, "{}", a.json());
        let u = a.json();
        assert_eq!(u["method"], "PUT");
        assert!(u["expires_at"].as_str().unwrap().ends_with('Z'), "{u}");
        let put = http(self.port, "PUT", u["url"].as_str().unwrap(), &[], bytes);
        assert_eq!(put.status, 200, "{}", String::from_utf8_lossy(&put.body));
        u["value"].as_str().unwrap().to_string()
    }

    fn tx(&self, ops: J, key: &str) -> Answer {
        self.call("POST", "/v1/tx", Some(&json!({ "ops": ops })), Some(key))
    }

    fn tx_on(&self, base: u64, ops: J, key: &str) -> Answer {
        self.call(
            "POST",
            "/v1/tx",
            Some(&json!({ "base": base, "ops": ops })),
            Some(key),
        )
    }

    /// A download: the answer of the server (302), then of where it sends.
    fn download(&self, path: &str) -> (Answer, Answer) {
        let first = self.call("GET", path, None, None);
        assert_eq!(first.status, 302, "{path}: {}", first.json());
        assert_eq!(first.header("cache-control"), "private, no-store");
        let to = first.header("location").to_string();
        let second = http(self.port, "GET", &to, &[], b"");
        (first, second)
    }
}

fn sha256(b: &[u8]) -> String {
    quadra_blobs::sha256_hex(b)
}

/// Upload, write, download and the refusals, on the project served at `port`.
fn flow(port: u16, clara: &str, otto: &str, dir: &Path) {
    let c = Client {
        port,
        bearer: format!("Bearer {clara}"),
    };
    let o = Client {
        port,
        bearer: format!("Bearer {otto}"),
    };

    // a PDF uploaded and named by a transaction: the field has its hash
    let value = c.upload("receipt", "scan.pdf", PDF);
    assert!(value.starts_with("upload:"), "{value}");
    let a = c.tx(
        json!([{ "op": "create", "entity": "Expense", "ref": "e", "values": { "title": "Taxi", "receipt": value } }]),
        "k1",
    );
    assert_eq!(a.status, 200, "{}", a.json());
    let id = a.json()["created"]["e"].as_u64().unwrap();
    let rec = c.call("GET", &format!("/v1/records/Expense/{id}"), None, None);
    assert_eq!(rec.status, 200);
    let base = rec.json()["base"].as_u64().unwrap();
    assert_eq!(
        rec.json()["cells"]["receipt"]["v"],
        json!(format!("sha256:{}", sha256(PDF)))
    );
    // the same request again: answered as it was
    let again = c.tx(
        json!([{ "op": "create", "entity": "Expense", "ref": "e", "values": { "title": "Taxi", "receipt": value } }]),
        "k1",
    );
    assert_eq!(again.status, 200, "{}", again.json());
    assert_eq!(again.json()["replayed"], json!(true));

    // downloaded: shown in the page, then downloaded with ?download
    let path = format!("/v1/records/Expense/{id}/files/receipt");
    let (_, got) = c.download(&path);
    assert_eq!(got.status, 200);
    assert_eq!(got.body, PDF);
    assert_eq!(got.header("content-type"), "application/pdf");
    assert!(
        got.header("content-disposition")
            .starts_with("inline; filename=\"scan.pdf\""),
        "{:?}",
        got.headers
    );
    let (_, got) = c.download(&format!("{path}?download"));
    assert!(got.header("content-disposition").starts_with("attachment;"));
    let info = c.call("GET", &format!("{path}?info"), None, None).json();
    assert_eq!(
        (
            info["name"].as_str(),
            info["type"].as_str(),
            info["size"].as_u64()
        ),
        (
            Some("scan.pdf"),
            Some("application/pdf"),
            Some(PDF.len() as u64)
        )
    );
    // a user who may not read the field
    let denied = o.call("GET", &path, None, None);
    assert_eq!(denied.status, 403, "{}", denied.json());
    assert!(denied.json()["error"]
        .as_str()
        .unwrap()
        .contains("cannot read Expense.receipt"));
    // nor write it
    let a = o.call(
        "POST",
        "/v1/uploads",
        Some(&json!({ "entity": "Expense", "field": "receipt", "name": "a.pdf", "size": 10 })),
        None,
    );
    assert_eq!(a.status, 403, "{}", a.json());

    // an upload is used once
    let a = c.tx(
        json!([{ "op": "create", "entity": "Expense", "values": { "title": "Again", "receipt": value } }]),
        "k2",
    );
    assert_eq!(a.status, 422, "{}", a.json());
    assert!(a.json()["error"].as_str().unwrap().contains("already used"));

    // a type the field does not take, by its name: refused before the upload
    let a = c.call(
        "POST",
        "/v1/uploads",
        Some(
            &json!({ "entity": "Expense", "field": "receipt", "name": "report.exe", "size": 100 }),
        ),
        None,
    );
    assert_eq!(a.status, 422);
    assert_eq!(
        a.json()["error"],
        "report.exe: the file is an application/x-msdownload, but Expense.receipt accepts pdf, image"
    );
    // a program renamed `.pdf`: its bytes tell, when the transaction names it
    let fake = c.upload("receipt", "report.pdf", &exe());
    let a = c.tx(
        json!([{ "op": "create", "entity": "Expense", "values": { "title": "Fake", "receipt": fake } }]),
        "k3",
    );
    assert_eq!(a.status, 422, "{}", a.json());
    assert_eq!(
        a.json()["error"],
        "report.pdf: the file is an application/x-msdownload, but Expense.receipt accepts pdf, image"
    );
    // larger than `max 1 KB`
    let a = c.call(
        "POST",
        "/v1/uploads",
        Some(&json!({ "entity": "Expense", "field": "receipt", "name": "big.pdf", "size": 5000 })),
        None,
    );
    assert_eq!(a.status, 422);
    assert_eq!(
        a.json()["error"],
        "big.pdf: the file is 4.9 KB, but Expense.receipt takes files up to 1 KB"
    );
    // a field takes an upload or a file it has, never a hash written by the client
    let other = format!("sha256:{}", "b".repeat(64));
    let a = c.tx_on(
        base,
        json!([{ "op": "update", "entity": "Expense", "id": id, "values": { "receipt": other } }]),
        "k4",
    );
    assert_eq!(a.status, 422, "{}", a.json());
    assert!(a.json()["error"]
        .as_str()
        .unwrap()
        .contains("Expense.receipt takes a file uploaded"));

    // a list of files: each by its place
    let (p1, p2) = (png(1), png(2));
    let (u1, u2) = (
        c.upload("photos", "one.png", &p1),
        c.upload("photos", "two.png", &p2),
    );
    let a = c.tx_on(
        base,
        json!([{ "op": "update", "entity": "Expense", "id": id, "values": { "photos": [u1, u2] } }]),
        "k5",
    );
    assert_eq!(a.status, 200, "{}", a.json());
    let base = a.json()["base"].as_u64().unwrap();
    let (_, got) = c.download(&format!("/v1/records/Expense/{id}/files/photos/1"));
    assert_eq!(got.body, p2);
    assert_eq!(got.header("content-type"), "image/png");
    assert_eq!(
        c.call(
            "GET",
            &format!("/v1/records/Expense/{id}/files/photos"),
            None,
            None
        )
        .status,
        400
    );
    assert_eq!(
        c.call(
            "GET",
            &format!("/v1/records/Expense/{id}/files/photos/5"),
            None,
            None
        )
        .status,
        404
    );
    // the files it has are kept as they are, a new one added
    let p3 = png(3);
    let u3 = c.upload("photos", "three.png", &p3);
    let keep = format!("sha256:{}", sha256(&p1));
    let a = c.tx_on(
        base,
        json!([{ "op": "update", "entity": "Expense", "id": id, "values": { "photos": [keep, u3] } }]),
        "k6",
    );
    assert_eq!(a.status, 200, "{}", a.json());
    let (_, got) = c.download(&format!("/v1/records/Expense/{id}/files/photos/1"));
    assert_eq!(got.body, p3);

    // the same file twice: kept once
    let v = c.upload("receipt", "scan-again.pdf", PDF);
    let a = c.tx(
        json!([{ "op": "create", "entity": "Expense", "values": { "title": "Same", "receipt": v } }]),
        "k7",
    );
    assert_eq!(a.status, 200, "{}", a.json());
    let rows = Conn::open(&format!("sqlite:{}", dir.join("data.db").display()))
        .unwrap()
        .query(
            "SELECT \"hash\", \"name\", \"type\", CAST(\"size\" AS TEXT) FROM \"_quadra_files\" WHERE \"hash\" = ?text",
            &[quadra_store::conn::Sql::Text(sha256(PDF))],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][2].as_deref(), Some("application/pdf"));

    // the document of `attach`: in the store too, its hash in the record
    let start = Instant::now();
    let slip = loop {
        let r = c
            .call("GET", &format!("/v1/records/Expense/{id}"), None, None)
            .json();
        if let Some(v) = r["cells"]["slip"]["v"].as_str() {
            break v.to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(60), "no slip: {r}");
        std::thread::sleep(Duration::from_millis(200));
    };
    let (_, got) = c.download(&format!("/v1/records/Expense/{id}/files/slip"));
    assert!(got.body.starts_with(b"%PDF"));
    assert_eq!(slip, format!("sha256:{}", sha256(&got.body)));
}

/// The bytes of the database and its log: no file is in them.
fn assert_not_in_database(dir: &Path) {
    let mark = b"QUADRA-MARK-0451";
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_file()
            && p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("data.db")
        {
            let b = std::fs::read(&p).unwrap();
            assert!(
                !b.windows(mark.len()).any(|w| w == mark),
                "the PDF is in {}",
                p.display()
            );
        }
    }
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("quadra-uploads-{name}-{}", std::process::id()))
}

#[test]
fn files_go_to_the_folder_of_the_project_through_signed_urls() {
    let dir = temp("local");
    let (cfg, clara, otto) = project(&dir, "", BTreeMap::new());
    // an upload of two days ago, never used: removed when the server starts its passes
    let old = "0ld0000000000000000000000000000a";
    let folder = dir.join(".quadra/files");
    std::fs::create_dir_all(folder.join("uploads")).unwrap();
    std::fs::write(folder.join("uploads").join(old), b"stale").unwrap();
    let two_days_ago = (now() - chrono::Duration::days(2))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    quadra_store::files::add_upload(
        &mut Conn::open(&cfg.url).unwrap(),
        &quadra_store::files::Upload {
            id: old.into(),
            entity: "Expense".into(),
            field: "receipt".into(),
            user_id: 1,
            name: "old.pdf".into(),
            size: 5,
            created_at: two_days_ago.clone(),
            expires_at: two_days_ago,
            used: false,
            hash: None,
        },
    )
    .unwrap();
    let (_host, _rt, port) = serve(HostOptions::default(), cfg.clone());
    flow(port, &clara, &otto, &dir);
    assert_not_in_database(&dir);
    // the bytes are in the folder, under their hash
    assert_eq!(
        std::fs::read(folder.join("files").join(sha256(PDF))).unwrap(),
        PDF
    );
    assert!(!folder.join("uploads").join(old).exists());
    assert!(
        quadra_store::files::upload(&mut Conn::open(&cfg.url).unwrap(), old)
            .unwrap()
            .is_none()
    );
    // a signed URL changed, or of another key: refused
    let c = Client {
        port,
        bearer: format!("Bearer {clara}"),
    };
    let u = c
        .call(
            "POST",
            "/v1/uploads",
            Some(&json!({ "entity": "Expense", "field": "receipt", "name": "x.pdf", "size": 5 })),
            None,
        )
        .json();
    let url = u["url"].as_str().unwrap();
    assert_eq!(http(port, "PUT", url, &[], b"12345678").status, 413);
    assert_eq!(http(port, "PUT", url, &[], b"123").status, 400);
    assert_eq!(
        http(
            port,
            "PUT",
            &url.replace("size=5", "size=8"),
            &[],
            b"12345678"
        )
        .status,
        403
    );
    assert_eq!(
        http(
            port,
            "GET",
            &format!("/v1/blobs/files/{}", sha256(PDF)),
            &[],
            b""
        )
        .status,
        403
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn files_go_to_an_s3_bucket_through_signed_urls() {
    let endpoint = "127.0.0.1:9000";
    if TcpStream::connect_timeout(&endpoint.parse().unwrap(), Duration::from_millis(500)).is_err() {
        eprintln!("skipped: no S3 at {endpoint} (docker compose -f deploy/compose.dev.yml up -d s3 s3-init, in the cloud repository)");
        return;
    }
    let dir = temp("s3");
    let prefix = format!("test-uploads-{}", std::process::id());
    let files = format!("[vars]\nFILES_KEY = {{ secret = true }}\nFILES_SECRET = {{ secret = true }}\n\n[files]\nurl = \"s3://quadra/{prefix}\"\nendpoint = \"http://{endpoint}\"\nregion = \"us-east-1\"\nkey = {{ var = \"FILES_KEY\" }}\nsecret = {{ var = \"FILES_SECRET\" }}\n");
    let mut env = BTreeMap::new();
    env.insert("FILES_KEY".to_string(), "quadra".to_string());
    env.insert("FILES_SECRET".to_string(), "quadra-dev-secret".to_string());
    let (cfg, clara, otto) = project(&dir, &files, env);
    let web = dir.join("web");
    std::fs::create_dir_all(&web).unwrap();
    std::fs::write(web.join("index.html"), "<!doctype html><title>t</title>").unwrap();
    let options = HostOptions {
        web: Some(web),
        ..Default::default()
    };
    let (_host, _rt, port) = serve(options, cfg.clone());
    // the URLs are the bucket's: the bytes never pass through the server
    let c = Client {
        port,
        bearer: format!("Bearer {clara}"),
    };
    let u = c
        .call(
            "POST",
            "/v1/uploads",
            Some(&json!({ "entity": "Expense", "field": "receipt", "name": "x.pdf", "size": 5 })),
            None,
        )
        .json();
    assert!(
        u["url"]
            .as_str()
            .unwrap()
            .starts_with(&format!("http://{endpoint}/quadra/{prefix}/uploads/")),
        "{u}"
    );
    flow(port, &clara, &otto, &dir);
    assert_not_in_database(&dir);
    assert!(!dir.join(".quadra/files/files").exists());
    // the web client may reach the bucket
    let page = http(port, "GET", "/", &[], b"");
    assert!(page.header("content-type").starts_with("text/html"));
    let csp = page.header("content-security-policy");
    for d in [
        "connect-src 'self'",
        "img-src 'self' data:",
        "frame-src 'self'",
    ] {
        assert!(csp.contains(&format!("{d} http://{endpoint}")), "{csp}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
