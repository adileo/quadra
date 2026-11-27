//! A bucket of the S3 of development (`deploy/compose.dev.yml` of the cloud: localhost:9000,
//! key `quadra`, secret `quadra-dev-secret`, bucket `quadra`): objects kept, read, copied and
//! removed, and the signed URLs used without credentials. Skipped, and said, when it is not
//! running.

use quadra_blobs::{Blobs, Files};
use quadra_sema::manifest::parse;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const ENDPOINT: &str = "127.0.0.1:9000";

fn reachable() -> bool {
    TcpStream::connect_timeout(&ENDPOINT.parse().unwrap(), Duration::from_millis(500)).is_ok()
}

/// A request to the URL (http only), its status, headers and body.
fn http(method: &str, url: &str, body: &[u8]) -> (u16, String, Vec<u8>) {
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let mut s = TcpStream::connect(host).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let head = format!(
        "{method} /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head, raw[split + 4..].to_vec())
}

fn files(prefix: &str) -> std::sync::Arc<Files> {
    let m = parse(&format!(
        "[vars]\nK = {{ secret = true }}\nS = {{ secret = true }}\n\n[files]\nurl = \"s3://quadra/{prefix}\"\nendpoint = \"http://{ENDPOINT}\"\nkey = {{ var = \"K\" }}\nsecret = {{ var = \"S\" }}\n"
    ))
    .unwrap();
    let mut values = BTreeMap::new();
    values.insert("K".to_string(), "quadra".to_string());
    values.insert("S".to_string(), "quadra-dev-secret".to_string());
    quadra_blobs::open(std::path::Path::new("."), &m, &values).unwrap()
}

#[test]
fn a_bucket_keeps_objects_and_signs_urls() {
    if !reachable() {
        eprintln!("skipped: no S3 at {ENDPOINT} (docker compose -f deploy/compose.dev.yml up -d s3 s3-init, in the cloud repository)");
        return;
    }
    let prefix = format!("test-blobs-{}", std::process::id());
    let f = files(&prefix);
    let b: &dyn Blobs = f.blobs.as_ref();
    assert_eq!(b.path_of("files/x"), format!("{prefix}/files/x"));
    b.put("uploads/a", b"%PDF-1.4 hello", "", "").unwrap();
    assert_eq!(b.head("uploads/a").unwrap(), Some(14));
    assert_eq!(b.get("uploads/a").unwrap().unwrap(), b"%PDF-1.4 hello");
    b.copy("uploads/a", "files/h").unwrap();
    b.delete("uploads/a").unwrap();
    b.delete("uploads/a").unwrap();
    assert_eq!(b.get("uploads/a").unwrap(), None);
    assert_eq!(b.head("uploads/a").unwrap(), None);

    // an upload with the signed URL, no credentials (its size is checked when the server
    // reads it); the URL for another key is refused
    let put = b.put_url("uploads/b", 5, Duration::from_secs(60)).unwrap();
    assert_eq!(put.method, "PUT");
    let (s, _, _) = http("PUT", &put.url.replace("uploads/b", "uploads/c"), b"hello");
    assert!(s >= 400);
    let (s, head, _) = http("PUT", &put.url, b"hello");
    assert_eq!(s, 200, "{head}");
    assert_eq!(b.get("uploads/b").unwrap().unwrap(), b"hello");

    // a download with the type and disposition asked for
    let url = b
        .get_url(
            "uploads/b",
            Duration::from_secs(60),
            "application/pdf",
            "inline; filename=\"a.pdf\"",
        )
        .unwrap();
    let (s, head, body) = http("GET", &url, b"");
    assert_eq!(s, 200, "{head}");
    assert_eq!(body, b"hello");
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("content-type: application/pdf"), "{head}");
    assert!(
        lower.contains("content-disposition: inline; filename=\"a.pdf\""),
        "{head}"
    );
    // the same URL changed: refused
    let (s, _, _) = http("GET", &url.replace("inline", "attachment"), b"");
    assert!(s >= 400);

    // a file kept under its hash, with its type and disposition
    f.keep("h2", b"\x89PNG....", "image/png", "p.png").unwrap();
    let url = f.download_url("h2", "image/png", "p.png", true).unwrap();
    let (s, head, _) = http("GET", &url, b"");
    assert_eq!(s, 200, "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-disposition: attachment"),
        "{head}"
    );
    for k in ["uploads/b", "files/h", "files/h2"] {
        b.delete(k).unwrap();
    }
}
