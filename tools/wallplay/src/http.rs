//! A minimal HTTP/1.1 server for a handful of clients on a local network.
//!
//! Every connection gets its own thread, so a handler may block, as the
//! displays' long polls do. Connections are kept alive between requests, which
//! keeps clock synchronization round trips free of connection setup. Request
//! heads and bodies are limited in size; chunked uploads and TLS are not
//! supported.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Largest request head.
pub const MAX_HEAD: usize = 64 * 1024;
/// Largest request body, enough for a full-resolution phone photo.
pub const MAX_BODY: usize = 64 * 1024 * 1024;
/// Most connections served at once.
const MAX_CONNECTIONS: usize = 256;
/// How long an idle connection, or a slow request, may take.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// One parsed request.
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub method: String,
    /// The path, without the query.
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub peer: Option<SocketAddr>,
}

impl Request {
    /// The last query parameter named `name`, percent-decoded.
    #[must_use]
    pub fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// A header by case-insensitive name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The body parsed as JSON.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body).context("invalid JSON body")
    }
}

/// A complete response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Response {
    #[must_use]
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: content_type.to_owned(),
            body: body.into(),
        }
    }

    #[must_use]
    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "text/html; charset=utf-8", body)
    }

    /// `value` as JSON with status 200.
    #[must_use]
    pub fn json(value: &impl Serialize) -> Self {
        match serde_json::to_vec_pretty(value) {
            Ok(body) => Self::new(200, "application/json", body),
            Err(error) => Self::error(500, &error.to_string()),
        }
    }

    /// `{"error": message}` with `status`.
    #[must_use]
    pub fn error(status: u16, message: &str) -> Self {
        let body = serde_json::json!({ "error": message }).to_string();
        Self::new(status, "application/json", body)
    }

    #[must_use]
    pub fn no_content() -> Self {
        Self::new(204, "text/plain", Vec::new())
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        422 => "Unprocessable Content",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// Decode `%XX` escapes and `+` as a space, as in query strings.
#[must_use]
pub fn percent_decode(text: &str) -> String {
    let mut bytes = Vec::with_capacity(text.len());
    let mut input = text.bytes();
    while let Some(byte) = input.next() {
        match byte {
            b'+' => bytes.push(b' '),
            b'%' => {
                let digits = [input.next(), input.next()];
                let value = digits
                    .iter()
                    .map(|digit| digit.and_then(|digit| char::from(digit).to_digit(16)))
                    .try_fold(0, |value, digit| Some(value * 16 + digit?));
                if let Some(value) = value.and_then(|value| u8::try_from(value).ok()) {
                    bytes.push(value);
                } else {
                    bytes.push(b'%');
                    bytes.extend(digits.into_iter().flatten());
                }
            }
            byte => bytes.push(byte),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Why a request could not be read.
#[derive(Debug)]
enum ReadError {
    /// The connection closed or failed; nothing can be answered.
    Closed,
    /// A malformed or oversized request, answered with this response.
    Rejected(Response),
}

impl From<io::Error> for ReadError {
    fn from(_error: io::Error) -> Self {
        Self::Closed
    }
}

/// A request head, owned, with the body length it announces.
#[derive(Debug)]
struct Head {
    request: Request,
    length: usize,
    close: bool,
}

fn parse_head(head: &httparse::Request<'_, '_>) -> Result<Head, ReadError> {
    let reject = |status, message: &str| ReadError::Rejected(Response::error(status, message));
    let target = head.path.unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let headers: Vec<(String, String)> = head
        .headers
        .iter()
        .map(|header| {
            (
                header.name.to_owned(),
                String::from_utf8_lossy(header.value).trim().to_owned(),
            )
        })
        .collect();
    let find = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.to_ascii_lowercase())
    };
    if find("transfer-encoding").is_some() {
        return Err(reject(411, "chunked request bodies are not supported"));
    }
    let length = match find("content-length") {
        Some(length) => length
            .parse()
            .map_err(|_err| reject(400, "invalid Content-Length"))?,
        None => 0,
    };
    if length > MAX_BODY {
        return Err(reject(413, "request body too large"));
    }
    let close = head.version == Some(0) || find("connection").is_some_and(|value| value == "close");
    let request = Request {
        method: head.method.unwrap_or("GET").to_owned(),
        path: path.to_owned(),
        query: query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (percent_decode(key), percent_decode(value))
            })
            .collect(),
        headers,
        body: Vec::new(),
        peer: None,
    };
    Ok(Head {
        request,
        length,
        close,
    })
}

/// Read the next request from `stream`, keeping bytes past it in `buffer`.
/// Returns `None` when the client closed an idle connection.
fn read_request(stream: &mut impl Read, buffer: &mut Vec<u8>) -> Result<Option<Head>, ReadError> {
    let mut chunk = [0; 16 * 1024];
    loop {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut parsed = httparse::Request::new(&mut headers);
        match parsed.parse(buffer) {
            Ok(httparse::Status::Complete(size)) => {
                let mut head = parse_head(&parsed)?;
                buffer.drain(..size);
                while buffer.len() < head.length {
                    let read = stream.read(&mut chunk)?;
                    if read == 0 {
                        return Err(ReadError::Closed);
                    }
                    buffer.extend(chunk.get(..read).unwrap_or_default());
                }
                head.request.body = buffer.drain(..head.length).collect();
                return Ok(Some(head));
            }
            Ok(httparse::Status::Partial) if buffer.len() > MAX_HEAD => {
                return Err(ReadError::Rejected(Response::error(
                    431,
                    "request head too large",
                )));
            }
            Ok(httparse::Status::Partial) => {
                let read = stream.read(&mut chunk)?;
                if read == 0 {
                    return if buffer.is_empty() {
                        Ok(None)
                    } else {
                        Err(ReadError::Closed)
                    };
                }
                buffer.extend(chunk.get(..read).unwrap_or_default());
            }
            Err(error) => {
                return Err(ReadError::Rejected(Response::error(
                    400,
                    &error.to_string(),
                )));
            }
        }
    }
}

fn write_response(stream: &mut impl Write, response: &Response, close: bool) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: {}\r\n\r\n",
        response.status,
        reason(response.status),
        response.content_type,
        response.body.len(),
        if close { "close" } else { "keep-alive" },
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

/// Something that answers requests.
pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

fn serve_connection(mut stream: TcpStream, handler: &Handler) {
    let peer = stream.peer_addr().ok();
    let configured = stream
        .set_read_timeout(Some(IDLE_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(IDLE_TIMEOUT)))
        .and_then(|()| stream.set_nodelay(true));
    if configured.is_err() {
        return;
    }
    let mut buffer = Vec::new();
    loop {
        match read_request(&mut stream, &mut buffer) {
            Ok(Some(Head {
                mut request, close, ..
            })) => {
                request.peer = peer;
                let response = handler(request);
                if write_response(&mut stream, &response, close).is_err() || close {
                    return;
                }
            }
            Ok(None) | Err(ReadError::Closed) => return,
            Err(ReadError::Rejected(response)) => {
                // The rest of the stream cannot be parsed reliably.
                if write_response(&mut stream, &response, true).is_err() {
                    return;
                }
                return;
            }
        }
    }
}

/// Serve connections from `listener` until it fails, one thread each.
pub fn serve(listener: &TcpListener, handler: &Handler) -> Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::AcqRel);
            let busy = Response::error(503, "too many connections");
            if write_response(&mut stream, &busy, true).is_err() {
                continue;
            }
            continue;
        }
        let (handler, finished) = (Arc::clone(handler), Arc::clone(&active));
        let started = std::thread::Builder::new()
            .name("wallplay-http".to_owned())
            .spawn(move || {
                serve_connection(stream, &handler);
                finished.fetch_sub(1, Ordering::AcqRel);
            });
        // Only this connection is lost, closed with the failed thread's
        // closure; the server keeps accepting.
        if let Err(error) = started {
            active.fetch_sub(1, Ordering::AcqRel);
            eprintln!("wallplay: starting a connection thread: {error}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(input: &[u8]) -> Vec<Result<Option<Head>, ReadError>> {
        let mut stream = input;
        let mut buffer = Vec::new();
        let mut results = Vec::new();
        loop {
            let result = read_request(&mut stream, &mut buffer);
            let done = !matches!(result, Ok(Some(_)));
            results.push(result);
            if done {
                return results;
            }
        }
    }

    #[test]
    fn pipelined_requests_parse_with_queries_and_bodies() {
        let input = b"GET /api/displays/a/poll?since=3&w=1512&name=lobby%20left+x HTTP/1.1\r\nHost: x\r\n\r\n\
                      POST /api/photo HTTP/1.1\r\nContent-Type: image/jpeg\r\ncontent-length: 5\r\nConnection: close\r\n\r\nhello";
        let results = read_all(input);
        let [Ok(Some(first)), Ok(Some(second)), Ok(None)] = results.as_slice() else {
            panic!("unexpected results: {results:?}")
        };
        assert_eq!(first.request.method, "GET");
        assert_eq!(first.request.path, "/api/displays/a/poll");
        assert_eq!(first.request.query("since"), Some("3"));
        assert_eq!(first.request.query("name"), Some("lobby left x"));
        assert!(first.request.body.is_empty() && !first.close);
        assert_eq!(second.request.header("CONTENT-TYPE"), Some("image/jpeg"));
        assert_eq!(second.request.body, b"hello");
        assert!(second.close);
    }

    #[test]
    fn bad_requests_are_rejected_with_a_status() {
        let status = |input: &[u8]| match read_all(input).pop() {
            Some(Err(ReadError::Rejected(response))) => Some(response.status),
            _ => None,
        };
        assert_eq!(
            status(b"GET / HTTP/1.1\r\nContent-Length: x\r\n\r\n"),
            Some(400)
        );
        assert_eq!(
            status(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"),
            Some(411)
        );
        let huge = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert_eq!(status(huge.as_bytes()), Some(413));
        let long = format!("GET /{} HTTP/1.1\r\n", "a".repeat(MAX_HEAD + 1));
        assert_eq!(status(long.as_bytes()), Some(431));
        assert_eq!(status(b"\x01\x02 / HTTP/1.1\r\n\r\n"), Some(400));
        assert!(matches!(
            read_all(b"POST / HTTP/1.1\r\nContent-Length: 9\r\n\r\nshort").pop(),
            Some(Err(ReadError::Closed))
        ));
    }

    #[test]
    fn percent_decoding_keeps_invalid_escapes() {
        assert_eq!(percent_decode("a%2Fb+c"), "a/b c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        assert_eq!(percent_decode("%E2%9C%93"), "\u{2713}");
    }

    #[test]
    fn responses_carry_length_and_connection() {
        let mut output = Vec::new();
        write_response(
            &mut output,
            &Response::json(&serde_json::json!({"a": 1})),
            false,
        )
        .expect("write");
        let text = String::from_utf8(output).expect("utf-8");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("Content-Length: 12\r\n") && text.contains("keep-alive"));
        assert!(text.ends_with("{\n  \"a\": 1\n}"));
    }
}
