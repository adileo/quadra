//! PEM blocks (RFC 7468) and base64 helpers.

use base64::Engine as _;

/// One `-----BEGIN LABEL-----` block, decoded.
pub(crate) struct Block {
    pub label: String,
    pub der: Vec<u8>,
}

/// Every PEM block in `text`, in order. Text outside the blocks is ignored.
pub(crate) fn blocks(text: &str, what: &str) -> Result<Vec<Block>, String> {
    let mut out = Vec::new();
    let mut lines = text.lines().enumerate();
    while let Some((n, line)) = lines.next() {
        let line = line.trim();
        let Some(label) = line
            .strip_prefix("-----BEGIN ")
            .and_then(|l| l.strip_suffix("-----"))
        else {
            continue;
        };
        let end = format!("-----END {label}-----");
        let mut body = String::new();
        let mut closed = false;
        for (_, l) in lines.by_ref() {
            let l = l.trim();
            if l == end {
                closed = true;
                break;
            }
            // Encapsulated headers (e.g. "Proc-Type:") are not base64.
            if l.contains(':') {
                continue;
            }
            body.push_str(l);
        }
        if !closed {
            return Err(format!(
                "{what}: the PEM block starting on line {} has no `{end}` line",
                n + 1
            ));
        }
        let der = b64_decode(&body).map_err(|e| {
            format!(
                "{what}: the PEM block `{label}` on line {} is not valid base64: {e}",
                n + 1
            )
        })?;
        out.push(Block {
            label: label.to_string(),
            der,
        });
    }
    Ok(out)
}

pub(crate) fn b64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decodes base64, ignoring whitespace (as in XML and PEM).
pub(crate) fn b64_decode(text: &str) -> Result<Vec<u8>, String> {
    let clean: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(clean.as_bytes())
        .map_err(|e| e.to_string())
}
