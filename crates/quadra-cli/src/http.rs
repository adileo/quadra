//! HTTPS for the commands that talk to the network (`quadra cloud …`, `quadra upgrade`), with
//! rustls: no system library needed.

use serde_json::Value;
use std::io::Read;
use std::time::Duration;

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .user_agent(format!(
            "quadra/{} ({})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS
        ))
        .build()
        .into()
}

/// A request with a JSON body or none; the answer's status and JSON (`null` if it is not JSON).
pub fn json(
    method: &str,
    url: &str,
    bearer: Option<&str>,
    body: Option<&Value>,
) -> Result<(u16, Value), String> {
    let a = agent();
    let auth = bearer.map(|t| format!("Bearer {t}"));
    let version = env!("CARGO_PKG_VERSION");
    let res = match method {
        "GET" | "DELETE" => {
            let mut r = if method == "GET" {
                a.get(url)
            } else {
                a.delete(url)
            };
            r = r.header("x-quadra-version", version);
            if let Some(h) = &auth {
                r = r.header("authorization", h);
            }
            r.call()
        }
        _ => {
            let mut r = if method == "PATCH" {
                a.patch(url)
            } else {
                a.post(url)
            };
            r = r.header("x-quadra-version", version);
            if let Some(h) = &auth {
                r = r.header("authorization", h);
            }
            match body {
                Some(b) => r.send_json(b),
                None => r.send_empty(),
            }
        }
    };
    let mut res = res.map_err(|e| format!("cannot reach {}: {e}", host(url)))?;
    let status = res.status().as_u16();
    let text = res.body_mut().read_to_string().unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

/// A .tar.gz POSTed, with a token: the answer's status and JSON.
pub fn upload(url: &str, bearer: &str, body: Vec<u8>) -> Result<(u16, Value), String> {
    let mut res = agent()
        .post(url)
        .header("x-quadra-version", env!("CARGO_PKG_VERSION"))
        .header("authorization", &format!("Bearer {bearer}"))
        .header("content-type", "application/gzip")
        .send(&body[..])
        .map_err(|e| format!("cannot reach {}: {e}", host(url)))?;
    let status = res.status().as_u16();
    let text = res.body_mut().read_to_string().unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

pub fn get_json(url: &str) -> Result<(u16, Value), String> {
    json("GET", url, None, None)
}

/// The body of a GET that must succeed (a download).
pub fn get_bytes(url: &str) -> Result<Vec<u8>, String> {
    let mut res = agent()
        .get(url)
        .call()
        .map_err(|e| format!("cannot reach {}: {e}", host(url)))?;
    if res.status().as_u16() != 200 {
        return Err(format!("{url}: HTTP {}", res.status().as_u16()));
    }
    let mut out = Vec::new();
    res.body_mut()
        .with_config()
        .limit(512 * 1024 * 1024)
        .reader()
        .read_to_end(&mut out)
        .map_err(|e| format!("{url}: {e}"))?;
    Ok(out)
}

fn host(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or(url)
}
