//! A deliberately small HTTP/1.1 server and client on the standard library,
//! used by `zboard web` and `zboard serve`. One thread per connection,
//! `Connection: close`, bounded headers and bodies. No TLS: listen on
//! localhost or a VPN address, whose tunnel already encrypts traffic.
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    /// Lower-cased header names.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub peer: SocketAddr,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub fn json(&self) -> Result<Value> {
        serde_json::from_slice(&self.body).context("request body must be JSON")
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub headers: Vec<(&'static str, String)>,
}

impl Response {
    pub fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_default(),
            headers: Vec::new(),
        }
    }

    pub fn error(status: u16, message: impl std::fmt::Display) -> Self {
        Self::json(status, &serde_json::json!({"ok": false, "error": message.to_string()}))
    }

    pub fn html(body: &'static str) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
            headers: vec![
                (
                    "Content-Security-Policy",
                    "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'".into(),
                ),
                ("X-Frame-Options", "DENY".into()),
                ("Referrer-Policy", "no-referrer".into()),
            ],
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        _ => "Error",
    }
}

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// Accept connections forever, one thread each.
pub fn serve(listener: TcpListener, max_body: usize, handler: Handler) -> Result<()> {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let handler = Arc::clone(&handler);
        std::thread::spawn(move || {
            let _ = handle(stream, max_body, &handler);
        });
    }
    Ok(())
}

fn handle(stream: TcpStream, max_body: usize, handler: &Handler) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    stream.set_write_timeout(Some(Duration::from_secs(20)))?;
    let peer = stream.peer_addr()?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let response = match read_request(&mut reader, peer, max_body) {
        Ok(request) => handler(request),
        Err(RequestError::TooLarge) => Response::error(413, "request too large"),
        Err(RequestError::Bad(error)) => Response::error(400, format!("{error:#}")),
    };
    write_response(stream, &response)
}

enum RequestError {
    TooLarge,
    Bad(anyhow::Error),
}

impl From<anyhow::Error> for RequestError {
    fn from(error: anyhow::Error) -> Self {
        Self::Bad(error)
    }
}

impl From<std::io::Error> for RequestError {
    fn from(error: std::io::Error) -> Self {
        Self::Bad(error.into())
    }
}

fn read_line(reader: &mut impl BufRead, limit: usize) -> Result<String, RequestError> {
    let mut line = Vec::new();
    reader.take(limit as u64 + 1).read_until(b'\n', &mut line)?;
    if line.len() > limit {
        return Err(RequestError::TooLarge);
    }
    Ok(String::from_utf8(line)
        .map_err(|_| anyhow::anyhow!("header is not UTF-8"))?
        .trim_end_matches(['\r', '\n'])
        .to_owned())
}

fn read_request(reader: &mut impl BufRead, peer: SocketAddr, max_body: usize) -> Result<Request, RequestError> {
    let line = read_line(reader, 8192)?;
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().context("missing request target")?.to_owned();
    if !parts.next().is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err(anyhow::anyhow!("unsupported protocol").into());
    }
    let mut headers = BTreeMap::new();
    for _ in 0..100 {
        let line = read_line(reader, 8192)?;
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').context("malformed header")?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    if headers.contains_key("transfer-encoding") {
        return Err(anyhow::anyhow!("chunked bodies are not supported").into());
    }
    let length: usize = headers
        .get("content-length")
        .map(|v| v.parse())
        .transpose()
        .map_err(|_| anyhow::anyhow!("invalid content-length"))?
        .unwrap_or(0);
    if length > max_body {
        return Err(RequestError::TooLarge);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    Ok(Request {
        method,
        path: percent_decode(path),
        query: parse_query(query),
        headers,
        body,
        peer,
    })
}

fn write_response(mut stream: TcpStream, response: &Response) -> Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n",
        response.status,
        reason(response.status),
        response.content_type,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()?;
    Ok(())
}

pub fn parse_query(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        index += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Minimal client for `http://host:port` URLs: one request per connection.
pub fn post_json(url: &str, path: &str, token: Option<&str>, body: &Value) -> Result<(u16, Value)> {
    let address = url
        .strip_prefix("http://")
        .with_context(|| format!("remote URL must start with http:// (got {url}); use a VPN or SSH tunnel for encryption"))?
        .trim_end_matches('/');
    if address.contains('/') {
        bail!("remote URL must be http://host:port without a path");
    }
    let socket = address
        .to_socket_addrs()
        .with_context(|| format!("resolve {address}"))?
        .next()
        .context("remote address did not resolve")?;
    let mut stream = TcpStream::connect_timeout(&socket, Duration::from_secs(10))
        .with_context(|| format!("connect to {address}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let payload = serde_json::to_vec(body)?;
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        payload.len()
    );
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&payload)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("malformed HTTP response")?;
    let head = String::from_utf8_lossy(&response[..split]);
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .context("malformed HTTP status")?;
    let value = serde_json::from_slice(&response[split + 4..]).context("remote reply was not JSON")?;
    Ok((status, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings_decode() {
        let query = parse_query("after=01ABC&text=hello+there%21&empty");
        assert_eq!(query["after"], "01ABC");
        assert_eq!(query["text"], "hello there!");
        assert_eq!(query["empty"], "");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn server_round_trip_and_body_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            serve(
                listener,
                64,
                Arc::new(|request: Request| {
                    let value = request.json().unwrap_or(Value::Null);
                    Response::json(200, &serde_json::json!({"path": request.path, "echo": value,
                        "auth": request.header("authorization")}))
                }),
            )
        });
        let url = format!("http://{address}");
        let (status, value) =
            post_json(&url, "/v1/echo", Some("secret"), &serde_json::json!({"a": 1})).unwrap();
        assert_eq!(status, 200);
        assert_eq!(value["echo"]["a"], 1);
        assert_eq!(value["auth"], "Bearer secret");
        let big = serde_json::json!({"text": "x".repeat(200)});
        assert_eq!(post_json(&url, "/", None, &big).unwrap().0, 413);
        assert!(post_json("https://example.com", "/", None, &big).is_err());
    }
}
