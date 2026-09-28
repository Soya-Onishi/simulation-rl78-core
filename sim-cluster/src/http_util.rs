//! Small blocking HTTP/1.1 helper for the control server and the web front-end.
//!
//! Connections are closed after one response. Bodies must fit in memory.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use serde::Serialize;

const MAX_BODY: usize = 64 * 1024 * 1024;

/// One inbound HTTP request.
#[derive(Debug)]
pub struct Incoming {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: Vec<u8>,
}

/// One outbound HTTP response.
pub struct Outgoing {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Outgoing {
    #[must_use]
    pub fn json(status: u16, value: &impl Serialize) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Self {
            status,
            content_type: "application/json".into(),
            body,
        }
    }

    #[must_use]
    pub fn text(status: u16, text: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8".into(),
            body: text.into().into_bytes(),
        }
    }

    #[must_use]
    pub fn html(html: &str) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8".into(),
            body: html.as_bytes().to_vec(),
        }
    }

    #[must_use]
    pub fn json_error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, &serde_json::json!({"error": message.into()}))
    }
}

/// HTTP client or server failure.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(String),
}

/// Response captured by [`exchange`].
#[derive(Debug)]
pub struct ClientResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl ClientResponse {
    #[must_use]
    pub fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Bind `addr` (`host:port`, port `0` allowed) and return the listener plus
/// the address actually bound.
pub fn bind(addr: &str) -> Result<(TcpListener, SocketAddr), HttpError> {
    let listener = TcpListener::bind(addr)?;
    let bound = listener.local_addr()?;
    Ok((listener, bound))
}

/// Serve requests until the process exits. Each connection is one request.
pub fn serve<F>(listener: TcpListener, handler: F)
where
    F: Fn(Incoming) -> Outgoing + Send + Sync + 'static,
{
    let handler = std::sync::Arc::new(handler);
    for conn in listener.incoming() {
        let Ok(stream) = conn else {
            continue;
        };
        let handler = std::sync::Arc::clone(&handler);
        std::thread::spawn(move || {
            if let Err(err) = handle_connection(stream, handler.as_ref()) {
                eprintln!("http: {err}");
            }
        });
    }
}

/// `method` against an `http://host:port/path` URL.
pub fn exchange(
    method: &str,
    url: &str,
    content_type: &str,
    body: &[u8],
) -> Result<ClientResponse, HttpError> {
    exchange_timeout(method, url, content_type, body, Duration::from_secs(30))
}

/// Like [`exchange`], with an explicit socket timeout.
pub fn exchange_timeout(
    method: &str,
    url: &str,
    content_type: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<ClientResponse, HttpError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| HttpError::Message(format!("only http:// URLs are supported, got {url}")))?;
    let (host_port, path) = match rest.split_once('/') {
        Some((host, path)) => (host, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    if host_port.is_empty() {
        return Err(HttpError::Message(format!("url {url} has no host")));
    }
    let (host, port) = if let Some((host, port)) = host_port.rsplit_once(':') {
        let port: u16 = port
            .parse()
            .map_err(|_| HttpError::Message(format!("bad port in {url}")))?;
        (host, port)
    } else {
        (host_port, 80)
    };
    let mut stream = TcpStream::connect((host, port))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut header = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !body.is_empty() {
        header.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    header.push_str("\r\n");
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    let (status, raw_body) = read_response(&mut stream)?;
    Ok(ClientResponse {
        status,
        body: raw_body,
    })
}

fn handle_connection<F>(mut stream: TcpStream, handler: &F) -> Result<(), HttpError>
where
    F: Fn(Incoming) -> Outgoing,
{
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let incoming = read_request(&mut stream)?;
    let outgoing = handler(incoming);
    let reason = reason_phrase(outgoing.status);
    let header = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        outgoing.status,
        outgoing.content_type,
        outgoing.body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(&outgoing.body)?;
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> Result<Incoming, HttpError> {
    let (head, rest) = read_head(stream)?;
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| HttpError::Message("empty request".into()))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| HttpError::Message("missing method".into()))?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| HttpError::Message("missing path".into()))?;
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.to_string(), String::new()),
    };
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    if content_length > MAX_BODY {
        return Err(HttpError::Message("request body too large".into()));
    }
    let mut body = rest;
    while body.len() < content_length {
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(content_length);
    Ok(Incoming {
        method,
        path,
        query,
        body,
    })
}

fn read_response(stream: &mut TcpStream) -> Result<(u16, Vec<u8>), HttpError> {
    let (head, rest) = read_head(stream)?;
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| HttpError::Message("empty HTTP response".into()))?;
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    let mut content_length: Option<usize> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().ok();
        }
    }
    let mut body = rest;
    if let Some(len) = content_length {
        while body.len() < len {
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n]);
        }
        body.truncate(len);
    } else {
        let mut buf = [0u8; 8192];
        loop {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if body.len() + n > MAX_BODY {
                return Err(HttpError::Message("response body too large".into()));
            }
            body.extend_from_slice(&buf[..n]);
        }
    }
    Ok((code, body))
}

fn read_head(stream: &mut TcpStream) -> Result<(String, Vec<u8>), HttpError> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let split_at = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(HttpError::Message(
                "connection closed before headers".into(),
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 1024 * 1024 {
            return Err(HttpError::Message("headers too large".into()));
        }
    };
    let head = String::from_utf8_lossy(&buf[..split_at]).into_owned();
    let rest = buf.split_off(split_at + 4);
    Ok((head, rest))
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_json_post() {
        let (listener, addr) = bind("127.0.0.1:0").unwrap();
        std::thread::spawn(move || {
            serve(listener, |req| {
                assert_eq!(req.method, "POST");
                assert_eq!(req.path, "/echo");
                Outgoing::json(201, &serde_json::json!({"ok": true, "n": req.body.len()}))
            });
        });
        let url = format!("http://{addr}/echo");
        let resp = exchange("POST", &url, "application/json", b"{}").unwrap();
        assert_eq!(resp.status, 201);
        assert!(resp.body_str().contains("\"ok\":true"));
    }
}
