//! The language server over stdio (`quadra lsp`) with interface files: a `.qdi` file gets a
//! diagnostic as soon as a `.qd` file changes a field it uses; find references and rename go
//! from the code to the interface files; hover, definition and completion in a `.qdi` file.

use serde_json::{json, Value as J};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "tests") {
                continue;
            }
            copy_dir(&p, &to.join(e.file_name()));
        } else if matches!(p.extension().and_then(|x| x.to_str()), Some("qd" | "toml" | "qdi")) {
            std::fs::copy(&p, to.join(e.file_name())).unwrap();
        }
    }
}

struct Lsp {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<J>,
    next: u64,
}

impl Lsp {
    fn start() -> Lsp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_quadra")).arg("lsp").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout: ChildStdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(n) = line.strip_prefix("Content-Length:") {
                        len = n.trim().parse().unwrap_or(0);
                    }
                }
                let mut buf = vec![0; len];
                if r.read_exact(&mut buf).is_err() {
                    return;
                }
                if let Ok(j) = serde_json::from_slice::<J>(&buf) {
                    if tx.send(j).is_err() {
                        return;
                    }
                }
            }
        });
        Lsp { child, stdin, rx, next: 1 }
    }

    fn send(&mut self, j: J) {
        let s = j.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{s}", s.len()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: J) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn request(&mut self, method: &str, params: J) -> J {
        let id = self.next;
        self.next += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.wait(|m| m["id"] == json!(id)).map(|m| m["result"].clone()).unwrap_or_else(|| panic!("no answer to {method}"))
    }

    fn wait(&mut self, f: impl Fn(&J) -> bool) -> Option<J> {
        let end = Instant::now() + Duration::from_secs(30);
        while Instant::now() < end {
            if let Ok(m) = self.rx.recv_timeout(Duration::from_millis(200)) {
                if f(&m) {
                    return Some(m);
                }
            }
        }
        None
    }

    /// The diagnostics published for `uri` next that satisfy `f`.
    fn diagnostics(&mut self, uri: &str, f: impl Fn(&[J]) -> bool) -> Vec<J> {
        let m = self
            .wait(|m| m["method"] == json!("textDocument/publishDiagnostics") && m["params"]["uri"] == json!(uri) && f(m["params"]["diagnostics"].as_array().map(Vec::as_slice).unwrap_or(&[])))
            .unwrap_or_else(|| panic!("no diagnostics for {uri}"));
        m["params"]["diagnostics"].as_array().cloned().unwrap_or_default()
    }
}

impl Drop for Lsp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn uri(p: &Path) -> String {
    format!("file://{}", p.display())
}

#[test]
fn interface_files_in_the_language_server() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/stress");
    let dir: PathBuf = std::env::temp_dir().join(format!("quadra-lsp-qdi-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&src, &dir);
    let dir = std::fs::canonicalize(&dir).unwrap();
    let cockpit = dir.join("interfaces/sales/cockpit.qdi");
    let sales = dir.join("sales.qd");
    let (cockpit_uri, sales_uri) = (uri(&cockpit), uri(&sales));
    let cockpit_text = std::fs::read_to_string(&cockpit).unwrap();
    let sales_text = std::fs::read_to_string(&sales).unwrap();

    let mut l = Lsp::start();
    l.request("initialize", json!({ "processId": null, "rootUri": uri(&dir), "capabilities": {} }));
    l.notify("initialized", json!({}));
    l.notify("textDocument/didOpen", json!({ "textDocument": { "uri": cockpit_uri, "languageId": "qdi", "version": 1, "text": cockpit_text } }));
    let d = l.diagnostics(&cockpit_uri, |_| true);
    assert!(d.is_empty(), "{d:?}");

    // the code changes: the interface file is wrong at once, where it uses the field
    let renamed = sales_text.replace("o.total", "o.amount_total").replace("    total: Money<EUR> = lines", "    amount_total: Money<EUR> = lines");
    l.notify("textDocument/didOpen", json!({ "textDocument": { "uri": sales_uri, "languageId": "quadra", "version": 1, "text": renamed } }));
    let d = l.diagnostics(&cockpit_uri, |d| !d.is_empty());
    assert!(d.iter().any(|x| x["range"]["start"]["line"] == json!(24) && x["message"].as_str().unwrap().contains("no field `total`")), "{d:?}");
    // and right again when the code is
    l.notify("textDocument/didChange", json!({ "textDocument": { "uri": sales_uri, "version": 2 }, "contentChanges": [{ "text": sales_text }] }));
    let d = l.diagnostics(&cockpit_uri, |d| d.is_empty());
    assert!(d.is_empty());

    // `total` of Order in the code: its references include the interface files
    let line = sales_text.lines().position(|x| x.starts_with("    total: Money<EUR> = lines")).unwrap();
    let refs = l.request("textDocument/references", json!({ "textDocument": { "uri": sales_uri }, "position": { "line": line, "character": 5 }, "context": { "includeDeclaration": true } }));
    let in_cockpit = refs.as_array().unwrap().iter().filter(|r| r["uri"] == json!(cockpit_uri)).count();
    assert!(in_cockpit >= 8, "{refs}");
    // rename: the code and the interface files
    let edit = l.request("textDocument/rename", json!({ "textDocument": { "uri": sales_uri }, "position": { "line": line, "character": 5 }, "newName": "amount_total" }));
    let edits = edit["changes"][&cockpit_uri].as_array().cloned().unwrap_or_default();
    assert!(edits.len() >= 8 && edits.iter().all(|e| e["newText"] == json!("amount_total")), "{edit}");
    assert!(edit["changes"][&sales_uri].as_array().is_some_and(|e| !e.is_empty()));

    // in the interface file: hover and definition of `total`, completion
    let (cl, cc) = cockpit_text.lines().enumerate().find_map(|(k, x)| x.find("o.total").map(|c| (k, c + 2))).unwrap();
    let hover = l.request("textDocument/hover", json!({ "textDocument": { "uri": cockpit_uri }, "position": { "line": cl, "character": cc } }));
    assert!(hover["contents"]["value"].as_str().unwrap().contains("total"), "{hover}");
    let def = l.request("textDocument/definition", json!({ "textDocument": { "uri": cockpit_uri }, "position": { "line": cl, "character": cc } }));
    assert_eq!(def["uri"], json!(sales_uri), "{def}");
    assert_eq!(def["range"]["start"]["line"], json!(line));
    let (fl, fc) = cockpit_text.lines().enumerate().find_map(|(k, x)| x.find("fields: number").map(|c| (k, c + 8))).unwrap();
    let items = l.request("textDocument/completion", json!({ "textDocument": { "uri": cockpit_uri }, "position": { "line": fl, "character": fc + 3 } }));
    assert!(items.as_array().unwrap().iter().any(|i| i["label"] == json!("number")), "{items}");
    drop(l);
    let _ = std::fs::remove_dir_all(&dir);
}
