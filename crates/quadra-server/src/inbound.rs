//! Messages received by an `inbound` (`inbound ShopOrder via shop path "/orders" as m: …`):
//! how a sender proves a message comes from it, by the preset of its connection
//! (`inbound_signature` in quadra.toml), with the secret of `inbound_secret` (and, while it
//! switches, `inbound_secret_previous`).
//!
//! | Preset | What the sender adds |
//! |---|---|
//! | `quadra` | `Quadra-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<body>">` (what Quadra's own webhooks send) |
//! | `stripe` | `Stripe-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<body>">` |
//! | `hmac` | `X-Signature: <hex HMAC-SHA256 of the body>` (`sha256=` in front is accepted) |
//! | `shopify` | `X-Shopify-Hmac-Sha256: <base64 HMAC-SHA256 of the body>` |
//! | `github` | `X-Hub-Signature-256: sha256=<hex HMAC-SHA256 of the body>` |
//! | `token` | `Authorization: Bearer <secret>` |
//!
//! A timestamped signature older (or newer) than five minutes is refused: a recorded message
//! sent again later is not taken. Every comparison is in constant time.

use hmac::{Hmac, Mac};

/// How far a timestamp of a signature may be from now.
const TOLERANCE_S: i64 = 300;

fn mac(key: &str, message: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes()).expect("any key length");
    m.update(message);
    m.finalize().into_bytes().to_vec()
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn header<'h>(headers: &'h [(String, String)], name: &str) -> Option<&'h str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Whether `body` with `headers` is signed by one of `secrets` in the way of `preset`, at
/// `now` (unix seconds). The error says what is missing or wrong, never a secret.
pub fn verify(
    preset: &str,
    secrets: &[&str],
    headers: &[(String, String)],
    body: &[u8],
    now: i64,
) -> Result<(), String> {
    use base64::Engine;
    let any = |f: &dyn Fn(&str) -> bool| secrets.iter().any(|s| f(s));
    let timestamped = |name: &str| -> Result<(), String> {
        let h = header(headers, name).ok_or(format!("no {name} header"))?;
        let mut t = None;
        let mut sigs = Vec::new();
        for part in h.split(',') {
            match part.trim().split_once('=') {
                Some(("t", v)) => t = v.parse::<i64>().ok(),
                Some(("v1", v)) => sigs.push(v.to_string()),
                _ => {}
            }
        }
        let t = t.ok_or(format!("{name} has no t=<unix seconds>"))?;
        if (now - t).abs() > TOLERANCE_S {
            return Err(format!("{name} is more than {TOLERANCE_S} s from now: a message sent again later is not taken"));
        }
        let mut signed = format!("{t}.").into_bytes();
        signed.extend_from_slice(body);
        let ok = any(&|s| {
            let want = hex::encode(mac(s, &signed));
            sigs.iter().any(|v| same(v.as_bytes(), want.as_bytes()))
        });
        if ok {
            Ok(())
        } else {
            Err(format!("the signature of {name} does not match"))
        }
    };
    let hex_of = |name: &str, prefix: &str| -> Result<(), String> {
        let h = header(headers, name).ok_or(format!("no {name} header"))?;
        let got = h.trim().strip_prefix(prefix).unwrap_or(h.trim());
        let ok = any(&|s| same(got.as_bytes(), hex::encode(mac(s, body)).as_bytes()));
        if ok {
            Ok(())
        } else {
            Err(format!("the signature of {name} does not match"))
        }
    };
    match preset {
        "quadra" => timestamped("Quadra-Signature"),
        "stripe" => timestamped("Stripe-Signature"),
        "hmac" => hex_of("X-Signature", "sha256="),
        "github" => hex_of("X-Hub-Signature-256", "sha256="),
        "shopify" => {
            let h = header(headers, "X-Shopify-Hmac-Sha256")
                .ok_or("no X-Shopify-Hmac-Sha256 header")?;
            let ok = any(&|s| {
                let want = base64::engine::general_purpose::STANDARD.encode(mac(s, body));
                same(h.trim().as_bytes(), want.as_bytes())
            });
            if ok {
                Ok(())
            } else {
                Err("the signature of X-Shopify-Hmac-Sha256 does not match".into())
            }
        }
        "token" => {
            let h = header(headers, "Authorization").ok_or("no Authorization header")?;
            let got = h.trim().strip_prefix("Bearer ").unwrap_or("");
            if any(&|s| same(got.as_bytes(), s.as_bytes())) {
                Ok(())
            } else {
                Err("the token does not match".into())
            }
        }
        other => Err(format!("`{other}` is not a way of signing Quadra knows")),
    }
}

/// The hex SHA-256 of a body: what tells two messages apart when the inbound has no key.
pub fn hash(body: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(k: &str, v: &str) -> Vec<(String, String)> {
        vec![(k.to_string(), v.to_string())]
    }

    #[test]
    fn each_preset_checks_its_signature() {
        let body = b"{\"id\":1}";
        let now = 1_800_000_000;
        let hex_sig = hex::encode(mac("s3cret", body));
        assert!(verify("hmac", &["s3cret"], &h("X-Signature", &hex_sig), body, now).is_ok());
        assert!(verify(
            "hmac",
            &["s3cret"],
            &h("X-Signature", &format!("sha256={hex_sig}")),
            body,
            now
        )
        .is_ok());
        assert!(verify("hmac", &["other"], &h("X-Signature", &hex_sig), body, now).is_err());
        assert!(verify(
            "github",
            &["s3cret"],
            &h("X-Hub-Signature-256", &format!("sha256={hex_sig}")),
            body,
            now
        )
        .is_ok());
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(mac("s3cret", body));
        assert!(verify(
            "shopify",
            &["s3cret"],
            &h("X-Shopify-Hmac-Sha256", &b64),
            body,
            now
        )
        .is_ok());
        // timestamped: within five minutes only
        let signed = |t: i64| {
            format!(
                "t={t},v1={}",
                hex::encode(mac("s3cret", format!("{t}.{{\"id\":1}}").as_bytes()))
            )
        };
        assert!(verify(
            "quadra",
            &["s3cret"],
            &h("Quadra-Signature", &signed(now - 10)),
            body,
            now
        )
        .is_ok());
        assert!(verify(
            "stripe",
            &["s3cret"],
            &h("Stripe-Signature", &signed(now)),
            body,
            now
        )
        .is_ok());
        assert!(verify(
            "quadra",
            &["s3cret"],
            &h("Quadra-Signature", &signed(now - 400)),
            body,
            now
        )
        .unwrap_err()
        .contains("300 s"));
        // a changed body, a missing header
        assert!(verify(
            "quadra",
            &["s3cret"],
            &h("Quadra-Signature", &signed(now)),
            b"{\"id\":2}",
            now
        )
        .is_err());
        assert!(verify("hmac", &["s3cret"], &[], body, now)
            .unwrap_err()
            .contains("no X-Signature"));
        // the previous secret is still accepted during a rotation
        assert!(verify(
            "hmac",
            &["new", "s3cret"],
            &h("X-Signature", &hex_sig),
            body,
            now
        )
        .is_ok());
        assert!(verify(
            "token",
            &["tok"],
            &h("Authorization", "Bearer tok"),
            body,
            now
        )
        .is_ok());
        assert!(verify(
            "token",
            &["tok"],
            &h("Authorization", "Bearer nope"),
            body,
            now
        )
        .is_err());
    }
}
