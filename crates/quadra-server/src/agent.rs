//! `quadra agent`: the on-premise agent, run in the network of the systems it reaches
//! (databases, SFTP servers, cash registers behind a firewall). It opens connections only to
//! the Quadra server, never the other way: it asks for its jobs (held open until there is one),
//! runs each with the credentials of its own environment (the server never has them), and
//! sends back what came of them.

use crate::transport::{self, Job, Outcome};
use serde_json::{json, Value as J};
use std::collections::BTreeMap;
use std::time::Duration;

/// The version an agent says it runs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where and as whom an agent runs.
#[derive(Clone, Debug)]
pub struct Options {
    /// the app's address (`https://shop.quadra.run`, `http://erp.local:8080`)
    pub server: String,
    pub key: String,
    /// the values of the variables of the connections it runs (its environment, its `.env`)
    pub env: BTreeMap<String, String>,
    /// jobs at once
    pub concurrency: usize,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .user_agent(format!("quadra-agent/{VERSION}"))
        .build()
        .expect("an HTTP client")
}

/// Registers with a one-time code: the agent's name and its key.
pub fn register(server: &str, code: &str) -> Result<(String, String), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let r = client()
            .post(format!("{}/v1/agent/hello", server.trim_end_matches('/')))
            .header("Content-Type", "application/json")
            .body(json!({ "code": code, "version": VERSION }).to_string())
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("{server}: {e}"))?;
        let status = r.status();
        let b: J = serde_json::from_slice(&r.bytes().await.unwrap_or_default()).unwrap_or(J::Null);
        if !status.is_success() {
            return Err(b["error"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| format!("the server answered {status}")));
        }
        match (b["name"].as_str(), b["key"].as_str()) {
            (Some(n), Some(k)) => Ok((n.to_string(), k.to_string())),
            _ => Err("the server answered without a key".into()),
        }
    })
}

/// Runs until the agent is revoked (an error) or `stop` says so: asks for its jobs, runs them,
/// sends what came of them. A server that cannot be reached is asked again, waiting longer each
/// time up to a minute.
pub fn run(
    o: Options,
    log: impl Fn(&str) + Send + Sync + 'static,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let log = std::sync::Arc::new(log);
    rt.block_on(async move {
        let http = client();
        let base = o.server.trim_end_matches('/').to_string();
        let limit = std::sync::Arc::new(tokio::sync::Semaphore::new(o.concurrency.max(1)));
        let mut wait = Duration::from_secs(1);
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let r = http
                .get(format!("{base}/v1/agent/jobs?wait=20"))
                .bearer_auth(&o.key)
                .header("Quadra-Agent", VERSION)
                .timeout(Duration::from_secs(40))
                .send()
                .await;
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    log(&format!("the server cannot be reached ({e}): again in {} s", wait.as_secs()));
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(Duration::from_secs(60));
                    continue;
                }
            };
            let status = r.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err("this agent was revoked, or its key is not valid here: register it again (a new code)".into());
            }
            let b: J =
                serde_json::from_slice(&r.bytes().await.unwrap_or_default()).unwrap_or(J::Null);
            if !status.is_success() {
                log(&format!("the server answered {status}: again in {} s", wait.as_secs()));
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(60));
                continue;
            }
            wait = Duration::from_secs(1);
            // a sleeping app: asked again when it says
            if let Some(s) = b["retry_after_s"].as_u64() {
                tokio::time::sleep(Duration::from_secs(s.clamp(1, 300))).await;
                continue;
            }
            for j in b["jobs"].as_array().cloned().unwrap_or_default() {
                let Some(id) = j["id"].as_u64() else { continue };
                let job: Job = match serde_json::from_value(j["job"].clone()) {
                    Ok(job) => job,
                    Err(e) => {
                        log(&format!("job {id}: not understood ({e}): an older agent? `quadra agent upgrade`"));
                        continue;
                    }
                };
                let permit = limit.clone().acquire_owned().await.expect("open");
                let (http, base, key, env, log) = (
                    http.clone(),
                    base.clone(),
                    o.key.clone(),
                    o.env.clone(),
                    log.clone(),
                );
                tokio::spawn(async move {
                    let what = describe(&job);
                    let outcome = transport::run(&job, &env, false).await;
                    match &outcome {
                        Outcome::Failed(f) => log(&format!("{what}: {}", f.text())),
                        Outcome::Fetched(f) => log(&format!("{what}: {} item(s)", f.items.len())),
                        Outcome::Put => log(&format!("{what}: done")),
                        Outcome::Answered { status, .. } => log(&format!("{what}: {status}")),
                    }
                    let body = json!({ "results": [{ "id": id, "outcome": outcome }] });
                    // the answer is sent again a few times: the job waits for it
                    for attempt in 0..5u32 {
                        let sent = http
                            .post(format!("{base}/v1/agent/results"))
                            .bearer_auth(&key)
                            .header("Quadra-Agent", VERSION)
                            .header("Content-Type", "application/json")
                            .body(body.to_string())
                            .timeout(Duration::from_secs(120))
                            .send()
                            .await;
                        if matches!(&sent, Ok(r) if r.status().is_success()) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    }
                    drop(permit);
                });
            }
        }
        Ok(())
    })
}

fn describe(job: &Job) -> String {
    match job {
        Job::Fetch { conn, ask } => match ask {
            transport::Ask::Files { pattern, .. } => format!("{}: files {pattern}", conn.name),
            transport::Ask::Rows { table, .. } => format!("{}: rows of {table}", conn.name),
            transport::Ask::Mail { mailbox, .. } => format!("{}: mailbox {mailbox}", conn.name),
            transport::Ask::Http { path } => format!("{}: GET {path}", conn.name),
        },
        Job::Put { conn, put } => match put {
            transport::Put::File { path, .. } => format!("{}: write {path}", conn.name),
            transport::Put::Row { table, .. } => format!("{}: insert into {table}", conn.name),
            transport::Put::Http { method, path, .. } => format!("{}: {method} {path}", conn.name),
        },
    }
}
