//! Response classification and charset decoding for `web_fetch`.
//!
//! The encoding follows a simplified HTML sniffing order: BOM, the
//! `Content-Type` charset, an in-document declaration (`<meta charset>`,
//! `http-equiv`, XML declaration) in the first 4 KiB, valid UTF-8, then
//! `chardetng` with the host's TLD as a hint. Older GBK/Big5/Shift_JIS sites
//! often declare their charset only in `<meta>` (or not at all) and used to
//! come back as U+FFFD noise.

use std::sync::LazyLock;

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use encoding_rs::{Encoding, UTF_8};
use regex::bytes::Regex;

/// How a decoded body is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BodyKind {
    Html,
    Markdown,
    Text,
}

/// What the `Content-Type` alone says about a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MimeClass {
    Html,
    Markdown,
    Text,
    /// Missing or generic type: decide from the bytes.
    Sniff,
    /// Not text; the description reads "<url> returned {description}".
    Unsupported(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EncodingSource {
    Bom,
    Header,
    Document,
    Utf8,
    Detected,
}

#[derive(Debug)]
pub(super) struct DecodedBody {
    pub kind: BodyKind,
    pub text: String,
    pub encoding: &'static Encoding,
    pub encoding_source: EncodingSource,
}

impl DecodedBody {
    /// Header note for anything other than plain or declared UTF-8.
    pub fn encoding_note(&self) -> Option<String> {
        let source = match self.encoding_source {
            EncodingSource::Detected => "detected",
            EncodingSource::Document => "declared in the document",
            EncodingSource::Header | EncodingSource::Bom | EncodingSource::Utf8 => {
                if self.encoding == UTF_8 {
                    return None;
                }
                "declared"
            }
        };
        Some(format!("decoded as {} ({})", self.encoding.name(), source))
    }
}

/// Media type without parameters, lower-cased.
pub(super) fn mime_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

pub(super) fn classify_mime(mime: &str) -> MimeClass {
    match mime {
        ""
        | "application/octet-stream"
        | "binary/octet-stream"
        | "application/unknown"
        | "unknown/unknown"
        | "application/x-download"
        | "application/force-download" => MimeClass::Sniff,
        "text/html" | "application/xhtml+xml" => MimeClass::Html,
        "text/markdown" | "text/x-markdown" => MimeClass::Markdown,
        "application/pdf" | "application/x-pdf" => MimeClass::Unsupported("a PDF document"),
        _ if mime.starts_with("text/")
            || mime.ends_with("+json")
            || mime.ends_with("+xml")
            || matches!(
                mime,
                "application/json"
                    | "application/xml"
                    | "application/javascript"
                    | "application/x-javascript"
                    | "application/ecmascript"
                    | "application/yaml"
                    | "application/x-yaml"
                    | "application/toml"
                    | "application/x-toml"
                    | "application/sql"
                    | "application/graphql"
                    | "application/x-sh"
                    | "application/x-httpd-php"
                    | "application/csv"
                    | "application/x-ndjson"
            ) =>
        {
            MimeClass::Text
        }
        _ if mime.starts_with("image/") => MimeClass::Unsupported("an image"),
        _ if mime.starts_with("audio/") => MimeClass::Unsupported("audio"),
        _ if mime.starts_with("video/") => MimeClass::Unsupported("a video"),
        _ if mime.starts_with("font/") => MimeClass::Unsupported("a font"),
        _ if is_archive_mime(mime) => MimeClass::Unsupported("an archive"),
        _ => MimeClass::Unsupported("binary content"),
    }
}

fn is_archive_mime(mime: &str) -> bool {
    matches!(
        mime,
        "application/zip"
            | "application/x-zip-compressed"
            | "application/gzip"
            | "application/x-gzip"
            | "application/x-tar"
            | "application/x-7z-compressed"
            | "application/x-rar-compressed"
            | "application/vnd.rar"
            | "application/x-bzip2"
            | "application/x-xz"
            | "application/zstd"
    )
}

/// Binary formats recognised from their first bytes, for responses whose
/// type was missing or generic.
fn sniff_binary(body: &[u8]) -> Option<&'static str> {
    const SIGNATURES: &[(&[u8], &str)] = &[
        (b"%PDF-", "a PDF document"),
        (b"\x89PNG\r\n\x1a\n", "an image"),
        (b"GIF87a", "an image"),
        (b"GIF89a", "an image"),
        (b"\xFF\xD8\xFF", "an image"),
        (b"PK\x03\x04", "a ZIP archive (or Office document)"),
        (b"\x1F\x8B", "gzip-compressed data"),
        (b"Rar!\x1A\x07", "an archive"),
        (b"7z\xBC\xAF\x27\x1C", "an archive"),
        (b"\x7FELF", "an executable"),
        (b"MZ", "an executable"),
        (b"ID3", "audio"),
        (b"fLaC", "audio"),
        (b"OggS", "audio"),
        (b"wOFF", "a font"),
        (b"wOF2", "a font"),
    ];
    if let Some((_, description)) = SIGNATURES
        .iter()
        .find(|(signature, _)| body.starts_with(signature))
    {
        return Some(description);
    }
    if body.len() >= 12 && &body[..4] == b"RIFF" && &body[8..12] == b"WEBP" {
        return Some("an image");
    }
    if body.len() >= 12 && &body[4..8] == b"ftyp" {
        return Some("a video");
    }
    // UTF-16 text is full of NULs but announces itself with a BOM.
    let utf16 = body.starts_with(b"\xFF\xFE") || body.starts_with(b"\xFE\xFF");
    let head = &body[..body.len().min(8192)];
    (!utf16 && head.contains(&0)).then_some("binary content")
}

/// `charset` parameter of a `Content-Type` value.
fn header_charset(content_type: &str) -> Option<&'static Encoding> {
    content_type.split(';').skip(1).find_map(|param| {
        let (name, value) = param.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("charset") {
            return None;
        }
        Encoding::for_label(
            value
                .trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .as_bytes(),
        )
    })
}

static META_CHARSET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<meta\b[^>]*?charset\s*=\s*["']?\s*([A-Za-z0-9_.:\-]+)"#)
        .expect("meta charset pattern")
});
static XML_ENCODING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)^\s*<\?xml\b[^>]*?encoding\s*=\s*["']([A-Za-z0-9_.:\-]+)["']"#)
        .expect("xml encoding pattern")
});

/// Charset declared inside the first 4 KiB of the document.
fn document_charset(body: &[u8]) -> Option<&'static Encoding> {
    let head = &body[..body.len().min(4096)];
    let label = XML_ENCODING
        .captures(head)
        .or_else(|| META_CHARSET.captures(head))?
        .get(1)?
        .as_bytes();
    // An ASCII-readable declaration of UTF-16 means UTF-8 (and
    // x-user-defined means windows-1252), as in the HTML prescan.
    Some(Encoding::for_label(label)?.output_encoding())
}

/// Last DNS label in the form chardetng accepts: lower-case ASCII, no dots.
fn tld_hint(host: Option<&str>) -> Option<Vec<u8>> {
    let label = host?
        .trim_end_matches('.')
        .rsplit('.')
        .next()?
        .to_ascii_lowercase();
    let valid = !label.is_empty()
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !label.bytes().all(|b| b.is_ascii_digit());
    valid.then(|| label.into_bytes())
}

fn detect_encoding(body: &[u8], host: Option<&str>) -> &'static Encoding {
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
    detector.feed(body, true);
    detector.guess(tld_hint(host).as_deref(), Utf8Detection::Allow)
}

fn looks_like_html(text: &str) -> bool {
    let head: String = text
        .chars()
        .take(2048)
        .collect::<String>()
        .to_ascii_lowercase();
    let trimmed = head.trim_start_matches('\u{feff}').trim_start();
    trimmed.starts_with("<!doctype html")
        || trimmed.starts_with("<html")
        || head.contains("<head")
        || head.contains("<body")
}

/// Decodes a successful response body. `Err` carries the "returned ..."
/// description for bodies that turn out to be binary.
pub(super) fn decode(
    body: &[u8],
    content_type: &str,
    host: Option<&str>,
) -> Result<DecodedBody, &'static str> {
    let class = classify_mime(&mime_type(content_type));
    if let MimeClass::Unsupported(description) = class {
        return Err(description);
    }
    if class == MimeClass::Sniff {
        if let Some(description) = sniff_binary(body) {
            return Err(description);
        }
    }

    let (encoding, source, bom_len) = if let Some((encoding, len)) = Encoding::for_bom(body) {
        (encoding, EncodingSource::Bom, len)
    } else if let Some(encoding) = header_charset(content_type) {
        (encoding, EncodingSource::Header, 0)
    } else if let Some(encoding) = (class != MimeClass::Markdown)
        .then(|| document_charset(body))
        .flatten()
    {
        (encoding, EncodingSource::Document, 0)
    } else if std::str::from_utf8(body).is_ok() {
        (UTF_8, EncodingSource::Utf8, 0)
    } else {
        (detect_encoding(body, host), EncodingSource::Detected, 0)
    };

    let content = &body[bom_len..];
    let (decoded, had_errors) = encoding.decode_without_bom_handling(content);
    // A wrong declaration on a valid UTF-8 body is common; trust the bytes.
    let (text, encoding, source) = if had_errors && encoding != UTF_8 {
        match std::str::from_utf8(content) {
            Ok(text) => (text.to_string(), UTF_8, EncodingSource::Utf8),
            Err(_) => (decoded.into_owned(), encoding, source),
        }
    } else {
        (decoded.into_owned(), encoding, source)
    };

    let kind = match class {
        MimeClass::Html => BodyKind::Html,
        MimeClass::Markdown => BodyKind::Markdown,
        MimeClass::Text => BodyKind::Text,
        MimeClass::Sniff if looks_like_html(&text) => BodyKind::Html,
        MimeClass::Sniff | MimeClass::Unsupported(_) => BodyKind::Text,
    };
    Ok(DecodedBody {
        kind,
        text,
        encoding,
        encoding_source: source,
    })
}

/// Best-effort text of an error page, for the excerpt in HTTP errors.
pub(super) fn decode_lossy(body: &[u8], content_type: &str, host: Option<&str>) -> (String, bool) {
    match decode(body, content_type, host) {
        Ok(decoded) => (decoded.text, decoded.kind == BodyKind::Html),
        Err(_) => (String::new(), false),
    }
}
