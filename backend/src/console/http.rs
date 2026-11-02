use super::util::*;
use base64::Engine;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, Read, Write},
    net::TcpStream,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
use tungstenite::{
    protocol::{Role, WebSocket, WebSocketConfig},
    Message,
};
pub struct Request {
    pub method: String,
    pub target: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub version: String,
}
impl Request {
    pub fn read(stream: &TcpStream) -> io::Result<Self> {
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut r = BufReader::new(stream.try_clone()?);
        let mut lines = vec![];
        let mut total = 0;
        loop {
            let mut b = vec![];
            r.by_ref().take(16385).read_until(b'\n', &mut b)?;
            total += b.len();
            if total > 16384 || b.is_empty() || !b.ends_with(b"\r\n") {
                return Err(io::Error::other("invalid HTTP headers"));
            }
            if b == b"\r\n" {
                break;
            }
            lines.push(String::from_utf8(b[..b.len() - 2].to_vec()).map_err(io::Error::other)?);
        }
        let parts: Vec<_> = lines
            .first()
            .ok_or_else(|| io::Error::other("request line"))?
            .split_whitespace()
            .collect();
        if parts.len() != 3
            || !matches!(parts[0], "GET" | "POST" | "DELETE")
            || !parts[1].starts_with('/')
            || !matches!(parts[2], "HTTP/1.0" | "HTTP/1.1")
        {
            return Err(io::Error::other("request line"));
        }
        let (method, target, version) = (
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2].to_string(),
        );
        let mut headers = HashMap::new();
        for line in &lines[1..] {
            let (k, v) = line
                .split_once(':')
                .ok_or_else(|| io::Error::other("header"))?;
            let k = k.to_ascii_lowercase();
            if headers.insert(k, v.trim().to_string()).is_some() {
                return Err(io::Error::other("duplicate header"));
            }
        }
        if headers.contains_key("transfer-encoding") {
            return Err(io::Error::other("chunked requests unsupported"));
        }
        let length = headers
            .get("content-length")
            .map(|s| s.parse::<usize>().map_err(io::Error::other))
            .transpose()?
            .unwrap_or(0);
        if length > 220000 || (method != "POST" && length > 0) {
            return Err(io::Error::other("body size"));
        }
        let mut body = vec![0; length];
        r.read_exact(&mut body)?;
        let path = target.split('?').next().unwrap().to_string();
        Ok(Self {
            method,
            target,
            path,
            version,
            headers,
            body,
        })
    }
    pub fn json(&self) -> io::Result<Value> {
        let v = if self.body.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&self.body).map_err(io::Error::other)?
        };
        if !v.is_object() {
            return Err(io::Error::other("expected JSON object"));
        }
        Ok(v)
    }
    pub fn same_origin(&self, origin: &str) -> bool {
        self.headers
            .get("sec-fetch-site")
            .is_none_or(|s| s != "cross-site")
            && self.headers.get("origin").is_none_or(|s| s == origin)
    }
    pub fn query(&self, key: &str) -> Option<&str> {
        self.target.split_once('?')?.1.split('&').find_map(|s| {
            let (k, v) = s.split_once('=')?;
            (k == key).then_some(v)
        })
    }
}
pub fn reply(
    stream: &mut TcpStream,
    status: u16,
    body: &[u8],
    kind: &str,
    embedded: bool,
) -> io::Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        410 => "Gone",
        429 => "Too Many Requests",
        _ => "Service Unavailable",
    };
    write!(stream,"HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'self'; img-src 'self' blob:; media-src 'self' blob:; style-src 'self'; script-src 'self'; connect-src 'self'; frame-src 'self'; frame-ancestors {}\r\n\r\n",body.len(),if embedded{"'self'"}else{"'none'"})?;
    stream.write_all(body)
}
pub fn json_reply(s: &mut TcpStream, status: u16, v: &Value, embedded: bool) -> io::Result<()> {
    reply(
        s,
        status,
        &serde_json::to_vec(v)?,
        "application/json",
        embedded,
    )
}
pub struct Permit<'a>(&'a AtomicUsize);
impl<'a> Permit<'a> {
    pub fn take(counter: &'a AtomicUsize, limit: usize) -> Option<Self> {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |x| {
                (x < limit).then_some(x + 1)
            })
            .ok()
            .map(|_| Self(counter))
    }
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
pub fn websocket(
    mut stream: TcpStream,
    r: &Request,
    origin: &str,
    mut snapshot: impl FnMut() -> Value,
) -> io::Result<()> {
    let key = r
        .headers
        .get("sec-websocket-key")
        .map(String::as_str)
        .unwrap_or("");
    if r.version != "HTTP/1.1"
        || !r.same_origin(origin)
        || r.headers
            .get("upgrade")
            .is_none_or(|x| !x.eq_ignore_ascii_case("websocket"))
        || r.headers.get("connection").is_none_or(|x| {
            !x.split(',')
                .any(|x| x.trim().eq_ignore_ascii_case("upgrade"))
        })
        || r.headers.get("sec-websocket-version").map(String::as_str) != Some("13")
        || base64::engine::general_purpose::STANDARD
            .decode(key)
            .map(|b| b.len())
            .ok()
            != Some(16)
    {
        return json_reply(
            &mut stream,
            400,
            &json!({"error":"invalid_websocket_upgrade"}),
            false,
        );
    }
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write!(stream,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",tungstenite::handshake::derive_accept_key(key.as_bytes()))?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(256))
        .max_frame_size(Some(256));
    let mut ws = WebSocket::from_raw_socket(stream, Role::Server, Some(config));
    let mut sequence = 0u64;
    let mut next = Instant::now();
    while !stopped() {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            ws.get_mut().set_read_timeout(Some(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            ))?;
            if Instant::now() >= deadline {
                return Err(io::Error::other("ack deadline"));
            }
            match ws.read().map_err(io::Error::other)? {
                Message::Text(s) => {
                    let v: Value = serde_json::from_str(s.as_str()).map_err(io::Error::other)?;
                    if v["ack"].as_u64() != Some(sequence) {
                        return Err(io::Error::other("ack sequence"));
                    }
                    break;
                }
                Message::Close(_) => {
                    let _ = ws.flush();
                    return Ok(());
                }
                Message::Ping(_) | Message::Pong(_) => {
                    ws.flush().map_err(io::Error::other)?;
                }
                _ => return Err(io::Error::other("invalid acknowledgement")),
            }
        }
        let ack = now();
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
        let v = snapshot();
        sequence += 1;
        let message = json!({"type":"state","sequence":sequence,"before_sample_ms":number(&v["generated_monotonic_ns"]).saturating_sub(ack)as f64/1e6,"state":v});
        ws.send(Message::Text(message.to_string().into()))
            .map_err(io::Error::other)?;
        next = Instant::now() + Duration::from_nanos(1_000_000_000 / 60);
    }
    let _ = ws.close(None);
    Ok(())
}
pub fn proxy(stream: &mut TcpStream, r: &Request, address: &str, token: &str) -> io::Result<()> {
    let route = r.path.strip_prefix("/video").unwrap_or("");
    let public =
        r.method == "GET" && matches!(route, "/" | "/index.html" | "/app.js" | "/style.css");
    let parts: Vec<_> = route.trim_start_matches('/').split('/').collect();
    let session = parts.len() >= 3
        && parts[0] == "api"
        && parts[1] == "session"
        && parts[2].len() >= 16
        && parts[2].len() <= 64
        && safe_id(parts[2]);
    let allowed = public
        || (r.method == "POST" && matches!(route, "/api/session" | "/api/stop"))
        || session
            && ((r.method == "DELETE" && parts.len() == 3)
                || (parts.len() == 4
                    && ((r.method == "GET" && matches!(parts[3], "ice" | "stats"))
                        || (r.method == "POST" && matches!(parts[3], "ice" | "answer")))));
    if !allowed {
        return json_reply(
            stream,
            404,
            &json!({"error":"unknown_video_endpoint"}),
            true,
        );
    }
    let addr = format!("{address}:8091")
        .parse()
        .map_err(io::Error::other)?;
    let mut remote = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    remote.set_read_timeout(Some(Duration::from_secs(12)))?;
    remote.set_write_timeout(Some(Duration::from_secs(3)))?;
    write!(remote,"{} {} HTTP/1.1\r\nHost: {address}:8091\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",r.method,route,r.body.len())?;
    if !public {
        write!(remote, "Authorization: Bearer {token}\r\n")?;
    }
    remote.write_all(b"\r\n")?;
    remote.write_all(&r.body)?;
    let mut reader = BufReader::new(remote);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::other("upstream status"))?;
    let mut mime = "application/json".to_string();
    let mut total = 0;
    loop {
        line.clear();
        reader.by_ref().take(16385).read_line(&mut line)?;
        total += line.len();
        if total > 16384 || line.is_empty() {
            return Err(io::Error::other("upstream headers"));
        }
        if line == "\r\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-type") {
                mime = v.trim().to_string();
            }
        }
    }
    let mut b = vec![];
    reader.take(1048577).read_to_end(&mut b)?;
    if b.len() > 1048576 {
        return Err(io::Error::other("upstream response too large"));
    }
    reply(stream, status, &b, &mime, true)
}
