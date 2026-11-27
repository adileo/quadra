//! The type of a file, from its bytes: never from its name or from what the browser said. A
//! program renamed `report.pdf` is still a program. The name only chooses among the kinds of
//! plain text, which bytes cannot tell apart (a `.csv` is `text/csv`).

/// The type of a file with these bytes (and this name, for text only).
pub fn detect(bytes: &[u8], name: &str) -> &'static str {
    let b = bytes;
    let at = |i: usize, s: &[u8]| b.len() >= i + s.len() && &b[i..i + s.len()] == s;
    if b.len() >= 5 && b[..b.len().min(1024)].windows(5).any(|w| w == b"%PDF-") {
        return "application/pdf";
    }
    if at(0, &[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg";
    }
    if at(0, b"\x89PNG\r\n\x1a\n") {
        return "image/png";
    }
    if at(0, b"GIF87a") || at(0, b"GIF89a") {
        return "image/gif";
    }
    if at(0, b"RIFF") {
        if at(8, b"WEBP") {
            return "image/webp";
        }
        if at(8, b"WAVE") {
            return "audio/wav";
        }
        if at(8, b"AVI ") {
            return "video/x-msvideo";
        }
    }
    if at(4, b"ftyp") && b.len() >= 12 {
        return match &b[8..12] {
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" => "image/heic",
            b"mif1" | b"msf1" | b"heif" => "image/heif",
            b"avif" | b"avis" => "image/avif",
            b"qt  " => "video/quicktime",
            b"M4A " | b"M4B " => "audio/mp4",
            [b'3', b'g', ..] => "video/3gpp",
            _ => "video/mp4",
        };
    }
    if at(0, &[0x1A, 0x45, 0xDF, 0xA3]) {
        let head = &b[..b.len().min(64)];
        return if head.windows(4).any(|w| w == b"webm") {
            "video/webm"
        } else {
            "video/x-matroska"
        };
    }
    if at(0, &[0, 0, 1, 0xBA]) || at(0, &[0, 0, 1, 0xB3]) {
        return "video/mpeg";
    }
    if at(0, b"ID3") {
        return "audio/mpeg";
    }
    if b.len() >= 2 && b[0] == 0xFF && b[1] & 0xF6 == 0xF0 {
        return "audio/aac";
    }
    if b.len() >= 2 && b[0] == 0xFF && b[1] & 0xE0 == 0xE0 && (b[1] >> 1) & 3 != 0 {
        return "audio/mpeg";
    }
    if at(0, b"OggS") {
        return "audio/ogg";
    }
    if at(0, b"fLaC") {
        return "audio/flac";
    }
    // programs
    if at(0, b"MZ") {
        return "application/x-msdownload";
    }
    if at(0, b"\x7fELF") {
        return "application/x-executable";
    }
    if [
        [0xFE, 0xED, 0xFA, 0xCE],
        [0xFE, 0xED, 0xFA, 0xCF],
        [0xCE, 0xFA, 0xED, 0xFE],
        [0xCF, 0xFA, 0xED, 0xFE],
        [0xCA, 0xFE, 0xBA, 0xBE],
    ]
    .iter()
    .any(|m| at(0, m))
    {
        return "application/x-mach-binary";
    }
    if at(0, &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        return compound(b);
    }
    if at(0, b"{\\rtf") {
        return "application/rtf";
    }
    if at(0, b"PK\x03\x04") || at(0, b"PK\x05\x06") || at(0, b"PK\x07\x08") {
        return zip(b);
    }
    if at(0, &[0x1F, 0x8B]) {
        return "application/gzip";
    }
    if at(0, b"7z\xBC\xAF\x27\x1C") {
        return "application/x-7z-compressed";
    }
    if at(0, b"Rar!\x1A\x07") {
        return "application/vnd.rar";
    }
    if at(0, b"II*\0") || at(0, b"MM\0*") {
        return "image/tiff";
    }
    if at(0, b"BM") && b.len() > 14 && b[6..10] == [0, 0, 0, 0] {
        return "image/bmp";
    }
    if is_text(b) {
        return text(b, name);
    }
    "application/octet-stream"
}

/// A file of the old Office formats (a compound file): a document of Word, a workbook of
/// Excel, a presentation, by the name of its main stream (in UTF-16 in its directory).
fn compound(b: &[u8]) -> &'static str {
    let utf16 = |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect() };
    let has = |s: &str| {
        let n = utf16(s);
        b.windows(n.len()).any(|w| w == n.as_slice())
    };
    if has("WordDocument") {
        "application/msword"
    } else if has("Workbook") || has("Book") {
        "application/vnd.ms-excel"
    } else if has("PowerPoint Document") {
        "application/vnd.ms-powerpoint"
    } else {
        "application/x-ole-storage"
    }
}

/// A zip: a document of Office (`word/`, `xl/`, `ppt/`) or of OpenDocument (its `mimetype`
/// entry), a Java or Android package, or just a zip.
fn zip(b: &[u8]) -> &'static str {
    // OpenDocument and EPUB: the first entry is `mimetype`, stored, with the type as its content
    if b.len() > 30 && &b[..4] == b"PK\x03\x04" {
        let n = u16::from_le_bytes([b[26], b[27]]) as usize;
        let extra = u16::from_le_bytes([b[28], b[29]]) as usize;
        if b.get(30..30 + n) == Some(b"mimetype") {
            let start = 30 + n + extra;
            let size = u32::from_le_bytes([b[18], b[19], b[20], b[21]]) as usize;
            let content = &b[start.min(b.len())..(start + size.min(100)).min(b.len())];
            const KNOWN: &[&str] = &[
                "application/vnd.oasis.opendocument.text",
                "application/vnd.oasis.opendocument.spreadsheet",
                "application/vnd.oasis.opendocument.presentation",
                "application/vnd.oasis.opendocument.graphics",
                "application/epub+zip",
            ];
            if let Some(k) = KNOWN.iter().find(|k| content == k.as_bytes()) {
                return k;
            }
        }
    }
    let names = zip_names(b);
    let any = |p: &str| names.iter().any(|n| n.starts_with(p));
    let types = names.iter().any(|n| n == "[Content_Types].xml");
    if types && any("word/") {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    } else if types && any("xl/") {
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    } else if types && any("ppt/") {
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
    } else if names.iter().any(|n| n == "AndroidManifest.xml") {
        "application/vnd.android.package-archive"
    } else if names.iter().any(|n| n == "META-INF/MANIFEST.MF") {
        "application/java-archive"
    } else {
        "application/zip"
    }
}

/// The names of a zip's entries: from its central directory (at the end), else from the
/// headers of its entries one after the other.
fn zip_names(b: &[u8]) -> Vec<String> {
    let u16_at = |i: usize| {
        b.get(i..i + 2)
            .map(|s| u16::from_le_bytes([s[0], s[1]]) as usize)
    };
    let u32_at = |i: usize| {
        b.get(i..i + 4)
            .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
    };
    let mut out = Vec::new();
    // the end of the central directory: in the last 64 KB (its comment is at most that long)
    let from = b.len().saturating_sub(65_557);
    let eocd = (from..b.len().saturating_sub(21))
        .rev()
        .find(|&i| &b[i..i + 4] == b"PK\x05\x06");
    if let Some(e) = eocd {
        if let (Some(count), Some(mut p)) = (u16_at(e + 10), u32_at(e + 16)) {
            for _ in 0..count.min(10_000) {
                if b.get(p..p + 4) != Some(b"PK\x01\x02") {
                    break;
                }
                let (Some(n), Some(x), Some(c)) = (u16_at(p + 28), u16_at(p + 30), u16_at(p + 32))
                else {
                    break;
                };
                if let Some(name) = b.get(p + 46..p + 46 + n) {
                    out.push(String::from_utf8_lossy(name).to_string());
                }
                p += 46 + n + x + c;
            }
        }
    }
    if out.is_empty() {
        let mut p = 0;
        while out.len() < 10_000 && b.get(p..p + 4) == Some(b"PK\x03\x04") {
            let (Some(size), Some(n), Some(x)) = (u32_at(p + 18), u16_at(p + 26), u16_at(p + 28))
            else {
                break;
            };
            if let Some(name) = b.get(p + 30..p + 30 + n) {
                out.push(String::from_utf8_lossy(name).to_string());
            }
            // sizes kept after the data (bit 3): the next header cannot be found this way
            if size == 0 && u16_at(p + 6).is_some_and(|f| f & 8 != 0) {
                break;
            }
            p += 30 + n + x + size;
        }
    }
    out
}

/// Text: no NUL and no control characters but tabs, line ends and form feeds, in the first
/// 8 KB. Any encoding where those are as in ASCII (UTF-8, Latin-1, Windows-1252).
fn is_text(b: &[u8]) -> bool {
    !b.is_empty()
        && b[..b.len().min(8192)]
            .iter()
            .all(|&c| c >= 0x20 && c != 0x7F || matches!(c, b'\t' | b'\n' | b'\r' | 0x0C))
}

/// The kind of a text: markup a browser would run (HTML, SVG) by its content, then the plain
/// kinds by the name's extension.
fn text(b: &[u8], name: &str) -> &'static str {
    let head = String::from_utf8_lossy(&b[..b.len().min(2048)]).to_ascii_lowercase();
    let start = head.trim_start_matches('\u{feff}').trim_start();
    if start.starts_with("<svg") || (start.starts_with("<?xml") && head.contains("<svg")) {
        return "image/svg+xml";
    }
    if [
        "<!doctype html",
        "<html",
        "<head",
        "<body",
        "<script",
        "<iframe",
    ]
    .iter()
    .any(|t| start.starts_with(t))
    {
        return "text/html";
    }
    if start.starts_with("<?xml") {
        return "application/xml";
    }
    if start.starts_with("#!") {
        return "text/x-shellscript";
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("csv") => "text/csv",
        Some("tsv") => "text/tab-separated-values",
        Some("md" | "markdown") => "text/markdown",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        _ => "text/plain",
    }
}

/// The type a name suggests, for what an upload declares before its bytes are seen: the
/// bytes decide in the end.
pub fn guess(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "svg" => "image/svg+xml",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "doc" => "application/msword",
        "odt" => "application/vnd.oasis.opendocument.text",
        "rtf" => "application/rtf",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xls" => "application/vnd.ms-excel",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "csv" => "text/csv",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "zip" => "application/zip",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "exe" | "dll" | "msi" => "application/x-msdownload",
        "html" | "htm" => "text/html",
        _ => return None,
    })
}

/// Whether a browser may show a file of this type in the page (`inline`): PDFs and images,
/// never SVG (it runs scripts) nor HTML. Anything else is downloaded.
pub fn inline(mime: &str) -> bool {
    mime == "application/pdf" || (mime.starts_with("image/") && mime != "image/svg+xml")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zip of entries with these names (stored, empty), with its central directory.
    fn zip_of(names: &[&str], first: Option<(&str, &str)>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        if let Some((n, c)) = first {
            entries.push((n.to_string(), c.as_bytes().to_vec()));
        }
        entries.extend(names.iter().map(|n| (n.to_string(), Vec::new())));
        for (name, data) in &entries {
            let offset = out.len() as u32;
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            central.extend_from_slice(b"PK\x01\x02");
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]);
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let at = out.len() as u32;
        let size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    #[test]
    fn documents_and_images_by_their_bytes() {
        assert_eq!(detect(b"%PDF-1.7\n...", "x.bin"), "application/pdf");
        assert_eq!(
            detect(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10], "a"),
            "image/jpeg"
        );
        assert_eq!(detect(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR", "a"), "image/png");
        assert_eq!(detect(b"GIF89a\x01\0", "a"), "image/gif");
        assert_eq!(detect(b"RIFF\x10\0\0\0WEBPVP8 ", "a"), "image/webp");
        assert_eq!(detect(b"\0\0\0\x18ftypheic\0\0\0\0", "a"), "image/heic");
        assert_eq!(detect(b"\0\0\0\x18ftypisom\0\0\0\0", "a"), "video/mp4");
        assert_eq!(
            detect(b"\0\0\0\x14ftypqt  \0\0\0\0", "a"),
            "video/quicktime"
        );
        assert_eq!(detect(b"ID3\x04\0\0\0\0", "a"), "audio/mpeg");
        assert_eq!(detect(b"RIFF\x10\0\0\0WAVEfmt ", "a"), "audio/wav");
        assert_eq!(detect(b"{\\rtf1\\ansi", "a"), "application/rtf");
    }

    #[test]
    fn a_program_renamed_is_still_a_program() {
        let mut exe = b"MZ\x90\0\x03\0\0\0".to_vec();
        exe.extend_from_slice(&[0; 64]);
        assert_eq!(detect(&exe, "report.pdf"), "application/x-msdownload");
        assert_eq!(
            detect(b"\x7fELF\x02\x01\x01", "photo.jpg"),
            "application/x-executable"
        );
        assert_eq!(
            detect(&[0xCF, 0xFA, 0xED, 0xFE, 7, 0], "a.png"),
            "application/x-mach-binary"
        );
        assert_eq!(
            detect(b"#!/bin/sh\nrm -rf /\n", "notes.txt"),
            "text/x-shellscript"
        );
    }

    #[test]
    fn office_documents_by_the_entries_of_their_zip() {
        let docx = zip_of(
            &["[Content_Types].xml", "_rels/.rels", "word/document.xml"],
            None,
        );
        assert_eq!(
            detect(&docx, "x.zip"),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        );
        let xlsx = zip_of(&["[Content_Types].xml", "xl/workbook.xml"], None);
        assert_eq!(
            detect(&xlsx, "x"),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        );
        let odt = zip_of(
            &["content.xml"],
            Some(("mimetype", "application/vnd.oasis.opendocument.text")),
        );
        assert_eq!(detect(&odt, "x"), "application/vnd.oasis.opendocument.text");
        let ods = zip_of(
            &["content.xml"],
            Some(("mimetype", "application/vnd.oasis.opendocument.spreadsheet")),
        );
        assert_eq!(
            detect(&ods, "x"),
            "application/vnd.oasis.opendocument.spreadsheet"
        );
        let jar = zip_of(&["META-INF/MANIFEST.MF", "a/B.class"], None);
        assert_eq!(detect(&jar, "report.docx"), "application/java-archive");
        // a zip named like a document is a zip: the entries are what count
        let plain = zip_of(&["readme.txt", "word/x"], None);
        assert_eq!(detect(&plain, "report.docx"), "application/zip");
    }

    #[test]
    fn old_office_documents_by_their_main_stream() {
        let mut doc = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
        doc.extend(vec![0; 512]);
        let mut xls = doc.clone();
        let mut msi = doc.clone();
        doc.extend("WordDocument".encode_utf16().flat_map(|u| u.to_le_bytes()));
        xls.extend("Workbook".encode_utf16().flat_map(|u| u.to_le_bytes()));
        msi.extend(
            "SummaryInformation"
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes()),
        );
        assert_eq!(detect(&doc, "a"), "application/msword");
        assert_eq!(detect(&xls, "a"), "application/vnd.ms-excel");
        assert_eq!(detect(&msi, "setup.doc"), "application/x-ole-storage");
    }

    #[test]
    fn text_is_told_apart_by_its_markup_then_by_its_name() {
        assert_eq!(detect(b"a;b\n1;2\n", "data.csv"), "text/csv");
        assert_eq!(detect(b"a;b\n1;2\n", "data.txt"), "text/plain");
        assert_eq!(detect("caff\u{e8}\n".as_bytes(), "n"), "text/plain");
        // Windows-1252 text (an Excel export) is text too
        assert_eq!(detect(b"caff\xe8;1\r\n", "x.csv"), "text/csv");
        assert_eq!(
            detect(b"<svg xmlns='http://www.w3.org/2000/svg'/>", "a.png"),
            "image/svg+xml"
        );
        assert_eq!(
            detect(b"<?xml version='1.0'?>\n<svg/>", "a.xml"),
            "image/svg+xml"
        );
        assert_eq!(
            detect(b"<!DOCTYPE html><script>x</script>", "a.txt"),
            "text/html"
        );
        assert_eq!(
            detect(b"\0\x01\x02garbage", "a.txt"),
            "application/octet-stream"
        );
        assert_eq!(detect(b"", "a.txt"), "application/octet-stream");
    }

    #[test]
    fn inline_only_for_pdfs_and_images() {
        assert!(inline("application/pdf") && inline("image/png"));
        assert!(!inline("image/svg+xml") && !inline("text/html") && !inline("text/plain"));
        assert_eq!(guess("Report.PDF"), Some("application/pdf"));
        assert_eq!(guess("report.exe"), Some("application/x-msdownload"));
        assert_eq!(guess("noext"), None);
    }
}
