//! A server killed during a delivery: the event is sent again when it starts, with the same
//! `Idempotency-Key`, and delivered; the receiver sees at most a duplicate it can recognise.

use quadra_store::Conn;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Seen = Arc<Mutex<Vec<(HashMap<String, String>, String)>>>;

fn read_request(s: &mut TcpStream) -> (HashMap<String, String>, String) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = s.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return (HashMap::new(), String::new());
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some((h, b)) = text.split_once("\r\n\r\n") {
            let headers: HashMap<String, String> = h
                .lines()
                .skip(1)
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                .collect();
            let len: usize = headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if b.len() >= len {
                return (headers, b[..len].to_string());
            }
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait(what: &str, secs: u64, ok: impl Fn() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(
            start.elapsed() < Duration::from_secs(secs),
            "waited for {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_server_killed_during_a_delivery_sends_it_again_with_the_same_key() {
    // the receiver: the first request is never answered, the others at once
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let rport = l.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let g = seen.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let g = g.clone();
            std::thread::spawn(move || {
                let (h, b) = read_request(&mut s);
                let first = {
                    let mut all = g.lock().unwrap();
                    all.push((h, b));
                    all.len() == 1
                };
                if first {
                    std::thread::sleep(Duration::from_secs(60));
                }
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
            });
        }
    });
    let dir = std::env::temp_dir().join(format!("quadra-killed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("quadra.toml"),
        format!("[project]\nname = \"k\"\n\n[vars]\nHOOK_SECRET = {{ secret = true }}\n\n[connections.hooks]\nkind = \"http\"\nurl = \"http://127.0.0.1:{rport}\"\ninsecure = true\ntimeout = \"30s\"\nsigning_secret = {{ var = \"HOOK_SECRET\" }}\ninbound_signature = \"hmac\"\ninbound_secret = {{ var = \"HOOK_SECRET\" }}\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("main.qd"),
        "entity Order:\n    number: Text\n\nrecord ShopOrder from json:\n    id: Text\n    number: Text at \"name\"\n\nevent OrderPlaced(order: Order)\n\ninbound OrderIn via hooks path \"/orders\" as m: ShopOrder:\n    key m.id\n    let o = Order(number=m.number)\n    emit OrderPlaced(order=o)\n\noutbound OrderPlaced via hooks path \"/orders\"\n",
    )
    .unwrap();
    let db = dir.join("data.db");
    let url = format!("sqlite:{}", db.display());
    let checked = quadra_analysis::check(&quadra_sema::Project::load(&dir).unwrap());
    quadra_store::ops::init(
        &url,
        checked.model.as_ref().unwrap(),
        &checked.report,
        chrono::Utc::now().naive_utc(),
    )
    .unwrap();
    let start = |port: u16| {
        std::process::Command::new(env!("CARGO_BIN_EXE_quadra"))
            .arg("serve")
            .arg(&dir)
            .args(["--url", &url, "--port", &port.to_string()])
            .env("HOOK_SECRET", "s3cret")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    };
    let up = |port: u16| {
        wait("the server", 30, || {
            TcpStream::connect(("127.0.0.1", port)).is_ok()
        })
    };
    let state = || -> Vec<String> {
        Conn::open(&url)
            .unwrap()
            .query("SELECT \"state\" FROM \"_quadra_outbox\"", &[])
            .unwrap()
            .into_iter()
            .map(|r| r[0].clone().unwrap_or_default())
            .collect()
    };
    let port = free_port();
    let mut server = start(port);
    up(port);
    // an order comes in, its event goes out, and the server dies while it waits for the answer
    let body = "{\"id\": \"1\", \"name\": \"K1\"}";
    let sig = quadra_server::deliver::sign("s3cret", body);
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(format!("POST /in/hooks/orders HTTP/1.1\r\nHost: k.test\r\nConnection: close\r\nX-Signature: {sig}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 202"), "{raw}");
    wait("the first attempt", 30, || !seen.lock().unwrap().is_empty());
    server.kill().unwrap();
    server.wait().unwrap();
    assert_eq!(state(), ["pending"]);
    // started again: sent again, delivered
    let port = free_port();
    let mut server = start(port);
    up(port);
    wait("the delivery", 60, || state() == ["delivered"]);
    server.kill().unwrap();
    let _ = server.wait();
    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].0["idempotency-key"], got[1].0["idempotency-key"]);
    assert_eq!(got[0].1, got[1].1);
    let _ = std::fs::remove_dir_all(&dir);
}
