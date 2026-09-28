//! Resolve a topology `elf` string to firmware bytes.
//!
//! The JSON field stays a single string. The prefix selects exactly one source,
//! so a filesystem path and inline bytes cannot be combined:
//!
//! - `base64:<payload>` — standard Base64 (ASCII whitespace ignored)
//! - `http://` or `https://` — HTTP GET
//! - `file://` — local filesystem URL
//! - anything else — local filesystem path
//!
//! A string whose first scheme is not one of those is rejected. A relative path
//! such as `./base64:name` stays a path, because the prefix is only special at
//! the start of the string.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use url::Url;

/// Largest firmware image accepted from any source.
pub const MAX_FIRMWARE_BYTES: usize = 32 * 1024 * 1024;

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Failure while interpreting or reading an `elf` source string.
#[derive(Debug, thiserror::Error)]
pub enum ElfSourceError {
    /// The string is not a supported source.
    #[error("{0}")]
    Invalid(String),
    /// Reading a local path failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

enum Kind<'a> {
    Path(&'a str),
    File(&'a str),
    Http(&'a str),
    Base64(&'a str),
}

fn invalid(message: impl Into<String>) -> ElfSourceError {
    ElfSourceError::Invalid(message.into())
}

fn too_large(label: &str, limit: usize) -> ElfSourceError {
    invalid(format!("firmware from {label} exceeds {limit} bytes"))
}

/// Check the string without fetching a remote image or opening a local path.
pub(crate) fn check_spec(spec: &str) -> Result<(), ElfSourceError> {
    match classify(spec)? {
        Kind::Path(path) => {
            if path.is_empty() {
                return Err(invalid("elf path must not be empty"));
            }
            Ok(())
        }
        Kind::File(spec) => {
            let _ = file_url_path(spec)?;
            Ok(())
        }
        Kind::Http(spec) => {
            parse_http_url(spec)?;
            Ok(())
        }
        Kind::Base64(payload) => {
            let _ = decode_base64(payload, MAX_FIRMWARE_BYTES)?;
            Ok(())
        }
    }
}

/// Short label for logs. Passwords and Base64 payloads are omitted.
#[must_use]
pub(crate) fn describe(spec: &str) -> String {
    match classify(spec) {
        Ok(Kind::Path(path)) => path.to_string(),
        Ok(Kind::File(spec)) => file_url_path(spec)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "file".to_string()),
        Ok(Kind::Http(spec)) => redact_url(spec),
        Ok(Kind::Base64(_)) => "base64".to_string(),
        Err(_) => "elf".to_string(),
    }
}

/// Load firmware bytes for `spec`.
pub(crate) fn load(spec: &str) -> Result<Vec<u8>, ElfSourceError> {
    load_with_limit(spec, MAX_FIRMWARE_BYTES)
}

fn load_with_limit(spec: &str, limit: usize) -> Result<Vec<u8>, ElfSourceError> {
    match classify(spec)? {
        Kind::Path(path) => read_path(Path::new(path), limit),
        Kind::File(spec) => read_path(&file_url_path(spec)?, limit),
        Kind::Http(spec) => http_get(spec, limit),
        Kind::Base64(payload) => decode_base64(payload, limit),
    }
}

fn classify(spec: &str) -> Result<Kind<'_>, ElfSourceError> {
    if let Some(payload) = spec.strip_prefix("base64:") {
        return Ok(Kind::Base64(payload));
    }
    if let Some(scheme) = scheme_name(spec) {
        if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            return Ok(Kind::Http(spec));
        }
        if scheme.eq_ignore_ascii_case("file") {
            return Ok(Kind::File(spec));
        }
        return Err(invalid(format!(
            "unsupported elf URL scheme '{scheme}' (use a filesystem path, file://, http(s)://, or base64:)"
        )));
    }
    Ok(Kind::Path(spec))
}

/// Scheme token when `spec` is `scheme://...`.
fn scheme_name(spec: &str) -> Option<&str> {
    let (scheme, rest) = spec.split_once(':')?;
    if scheme.is_empty() || !rest.starts_with("//") {
        return None;
    }
    let scheme_ok = scheme
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.');
    scheme_ok.then_some(scheme)
}

fn read_path(path: &Path, limit: usize) -> Result<Vec<u8>, ElfSourceError> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(too_large(&path.display().to_string(), limit));
    }
    Ok(bytes)
}

fn decode_base64(payload: &str, limit: usize) -> Result<Vec<u8>, ElfSourceError> {
    let mut stripped = Vec::with_capacity(payload.len());
    for byte in payload.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if !is_base64_byte(byte) {
            return Err(invalid(
                "elf base64 payload contains a character outside standard base64",
            ));
        }
        stripped.push(byte);
    }
    if stripped.is_empty() {
        return Err(invalid("elf base64 payload must not be empty"));
    }
    if stripped.len() > max_encoded_len(limit) {
        return Err(too_large("base64", limit));
    }
    let bytes = STANDARD
        .decode(stripped)
        .map_err(|err| invalid(format!("invalid base64 elf: {err}")))?;
    if bytes.is_empty() {
        return Err(invalid("elf base64 payload decoded to empty firmware"));
    }
    if bytes.len() > limit {
        return Err(too_large("base64", limit));
    }
    Ok(bytes)
}

fn is_base64_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/' || byte == b'='
}

fn max_encoded_len(limit: usize) -> usize {
    limit.div_ceil(3) * 4
}

fn http_get(spec: &str, limit: usize) -> Result<Vec<u8>, ElfSourceError> {
    parse_http_url(spec)?;
    let mut response = ureq::get(spec)
        .config()
        .timeout_global(Some(FETCH_TIMEOUT))
        .build()
        .call()
        .map_err(|err| match err {
            ureq::Error::StatusCode(code) => invalid(format!(
                "http firmware fetch failed: HTTP {code} from {}",
                redact_url(spec)
            )),
            other => invalid(format!(
                "http firmware fetch failed: {}",
                public_detail(spec, other)
            )),
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(invalid(format!(
            "http firmware fetch failed: HTTP {status} from {}",
            redact_url(spec)
        )));
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(limit as u64)
        .read_to_vec()
        .map_err(|err| match err {
            ureq::Error::BodyExceedsLimit(_) => too_large(&redact_url(spec), limit),
            other => invalid(format!(
                "http firmware fetch failed: {}",
                public_detail(spec, other)
            )),
        })?;
    if bytes.len() > limit {
        return Err(too_large(&redact_url(spec), limit));
    }
    Ok(bytes)
}

fn parse_http_url(spec: &str) -> Result<Url, ElfSourceError> {
    let url = Url::parse(spec).map_err(|err| invalid(format!("invalid http elf URL: {err}")))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(invalid(format!(
            "unsupported elf URL scheme '{}'",
            url.scheme()
        )));
    }
    if url.host().is_none() {
        return Err(invalid("http elf URL is missing a host"));
    }
    Ok(url)
}

fn file_url_path(spec: &str) -> Result<PathBuf, ElfSourceError> {
    let url = Url::parse(spec).map_err(|err| invalid(format!("invalid file elf URL: {err}")))?;
    url.to_file_path()
        .map_err(|()| invalid("elf file URL is not a local path"))
}

fn redact_url(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return "url".to_string();
    };
    if url.password().is_some() {
        let _ = url.set_password(Some("***"));
    }
    url.to_string()
}

fn public_detail(spec: &str, detail: impl std::fmt::Display) -> String {
    let mut text = detail.to_string();
    if let Ok(url) = Url::parse(spec) {
        if let Some(password) = url.password() {
            if !password.is_empty() {
                text = text.replace(password, "***");
            }
        }
    }
    if text.contains(spec) {
        text = text.replace(spec, &redact_url(spec));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn loads_local_path_and_file_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guest.elf");
        std::fs::write(&path, b"\x7fELF-local").unwrap();
        assert_eq!(load(path.to_str().unwrap()).unwrap(), b"\x7fELF-local");

        let file_url = Url::from_file_path(&path).unwrap();
        assert_eq!(load(file_url.as_str()).unwrap(), b"\x7fELF-local");
    }

    #[test]
    fn colon_name_without_scheme_stays_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-base64:guest.elf");
        std::fs::write(&path, b"path-bytes").unwrap();
        assert_eq!(load(path.to_str().unwrap()).unwrap(), b"path-bytes");
    }

    #[test]
    fn decodes_base64_and_ignores_whitespace() {
        let spec = "base64:AA E=\n";
        assert_eq!(load(spec).unwrap(), b"\x00\x01");
        assert_eq!(describe(spec), "base64");
        assert!(check_spec("base64:").is_err());
        assert!(check_spec("base64:****").is_err());
    }

    #[test]
    fn enforces_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.elf");
        std::fs::write(&path, b"12345").unwrap();
        let err = load_with_limit(path.to_str().unwrap(), 4).unwrap_err();
        assert!(err.to_string().contains("exceeds"));

        let err = load_with_limit("base64:MTIzNDU=", 4).unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn rejects_unknown_scheme_and_empty_path() {
        let err = check_spec("sftp://example/a.elf").unwrap_err();
        assert!(err.to_string().contains("unsupported elf URL scheme"));
        assert!(check_spec("").is_err());
        assert!(check_spec("https://example.test/fw.elf").is_ok());
        assert!(check_spec("ftp://example.test/fw.elf").is_err());
    }

    #[test]
    fn describe_hides_password_and_base64() {
        let label = describe("http://alice:s3cret@127.0.0.1/fw.elf");
        assert!(!label.contains("s3cret"));
        assert!(label.contains("alice"));
        assert_eq!(describe("base64:YQ=="), "base64");
    }

    #[test]
    fn fetches_http() {
        let url = spawn_http(200, b"http-elf");
        assert_eq!(load(&url).unwrap(), b"http-elf");
        let missing = spawn_http(404, b"nope");
        let err = load(&missing).unwrap_err();
        assert!(err.to_string().contains("HTTP 404"));
    }

    #[test]
    fn http_body_limit() {
        let url = spawn_http(200, b"12345");
        let err = load_with_limit(&url, 4).unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    fn spawn_http(status: u16, body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            let header = format!(
                "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(header.as_bytes()).unwrap();
            sock.write_all(body).unwrap();
        });
        format!("http://127.0.0.1:{port}/fw.elf")
    }
}
