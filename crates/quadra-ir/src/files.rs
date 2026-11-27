//! The files a `File` field takes: `File<pdf, image>`, `File<"application/zip">`, with
//! `max 10 MB`. A family names the types of a kind of document; a string names one type
//! exactly. `File` without arguments takes every file.

use crate::Span;
use serde::Serialize;

/// The families of files and their types, as the server recognizes them from the bytes.
pub const FAMILIES: &[(&str, &[&str])] = &[
    ("pdf", &["application/pdf"]),
    (
        "image",
        &[
            "image/jpeg",
            "image/png",
            "image/webp",
            "image/gif",
            "image/heic",
            "image/heif",
        ],
    ),
    (
        "word",
        &[
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/msword",
            "application/vnd.oasis.opendocument.text",
            "application/rtf",
        ],
    ),
    (
        "excel",
        &[
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "application/vnd.ms-excel",
            "application/vnd.oasis.opendocument.spreadsheet",
            "text/csv",
        ],
    ),
    ("text", &["text/plain", "text/markdown", "text/csv"]),
    ("zip", &["application/zip"]),
    (
        "video",
        &[
            "video/mp4",
            "video/quicktime",
            "video/webm",
            "video/x-matroska",
            "video/x-msvideo",
            "video/mpeg",
            "video/3gpp",
        ],
    ),
    (
        "audio",
        &[
            "audio/mpeg",
            "audio/wav",
            "audio/ogg",
            "audio/flac",
            "audio/mp4",
            "audio/aac",
        ],
    ),
];

/// The names of the families, for messages: `pdf, image, word, …`.
pub fn family_names() -> String {
    FAMILIES
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The types of a family, if it is one.
pub fn family(name: &str) -> Option<&'static [&'static str]> {
    FAMILIES.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

/// What a `File` field takes.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct FileSpec {
    /// families (`pdf`) and exact types (`"application/zip"`, written with its quotes), with
    /// where each is written; none: every file
    pub accept: Vec<(String, Span)>,
    /// `max 10 MB`: the largest file, in bytes (`Some(0)`: a size that could not be read,
    /// refused by the checker), and where it is written
    pub max: Option<(u64, Span)>,
}

impl FileSpec {
    /// The families and types it takes, as written (`pdf, image, "application/zip"`).
    pub fn accepts_text(&self) -> String {
        self.accept
            .iter()
            .map(|(a, _)| a.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Whether it takes a file of this type (as recognized from its bytes).
    pub fn accepts(&self, mime: &str) -> bool {
        self.accept.is_empty()
            || self.accept.iter().any(|(a, _)| match exact(a) {
                Some(t) => t.eq_ignore_ascii_case(mime),
                None => family(a).is_some_and(|ts| ts.contains(&mime)),
            })
    }

    /// The largest file, in bytes.
    pub fn max_bytes(&self) -> Option<u64> {
        self.max.map(|(n, _)| n)
    }
}

/// The type an accepted entry names exactly (`"application/zip"`), if it is one.
pub fn exact(entry: &str) -> Option<&str> {
    entry.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
}

/// A size in bytes from `max 10 MB` (`B`, `KB`, `MB`, `GB`, powers of 1024).
pub fn size_bytes(number: &str, unit: &str) -> Option<u64> {
    let n: f64 = number.replace('_', "").parse().ok()?;
    let k: f64 = match unit {
        "B" => 1.0,
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let b = n * k;
    (b.is_finite() && b >= 1.0 && b < 1e15).then_some(b.round() as u64)
}

/// A size as people read it: `512 B`, `12.4 KB`, `10 MB`.
pub fn show_size(bytes: u64) -> String {
    let units = [("GB", 1u64 << 30), ("MB", 1 << 20), ("KB", 1 << 10)];
    for (u, k) in units {
        if bytes >= k {
            let v = bytes as f64 / k as f64;
            return if (v - v.round()).abs() < 0.05 {
                format!("{} {u}", v.round() as u64)
            } else {
                format!("{v:.1} {u}")
            };
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(entries: &[&str]) -> FileSpec {
        let span = Span::new(quadra_sema::FileId(0), Default::default());
        FileSpec {
            accept: entries.iter().map(|e| (e.to_string(), span)).collect(),
            max: None,
        }
    }

    #[test]
    fn families_and_exact_types() {
        let s = spec(&["pdf", "image"]);
        assert!(s.accepts("application/pdf"));
        assert!(s.accepts("image/png"));
        assert!(!s.accepts("application/x-msdownload"));
        assert!(!s.accepts("image/svg+xml"));
        let z = spec(&["\"application/zip\""]);
        assert!(z.accepts("application/zip"));
        assert!(!z.accepts("application/pdf"));
        assert!(spec(&[]).accepts("application/x-msdownload"));
        assert_eq!(s.accepts_text(), "pdf, image");
    }

    #[test]
    fn sizes() {
        assert_eq!(size_bytes("10", "MB"), Some(10 << 20));
        assert_eq!(size_bytes("1.5", "KB"), Some(1536));
        assert_eq!(size_bytes("2", "GB"), Some(2 << 30));
        assert_eq!(size_bytes("0", "MB"), None);
        assert_eq!(size_bytes("3", "TB"), None);
        assert_eq!(show_size(10 << 20), "10 MB");
        assert_eq!(show_size(13002342), "12.4 MB");
        assert_eq!(show_size(500), "500 B");
    }
}
