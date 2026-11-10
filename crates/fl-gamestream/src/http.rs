//! A deliberately tiny blocking HTTP/1.1 server for nvhttp: GET only, one request per
//! connection (`Connection: close`), thread per connection with a concurrency cap.
//! Moonlight's requests are short GETs with query strings; nothing more is needed.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const MAX_CONNECTIONS: usize = 32;
/// A pairing request carries the client certificate as hex PEM (a few KiB).
const MAX_HEADER: usize = 32 * 1024;
/// Per read/write, and also for a whole request head or TLS handshake, so a client
/// trickling bytes cannot hold a connection slot.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Request {
    pub path: String,
    pub query: HashMap<String, String>,
    pub peer: SocketAddr,
    pub local: SocketAddr,
    /// True on the TLS listener.
    pub https: bool,
    /// DER of the client certificate presented in the TLS handshake.
    pub client_cert: Option<Vec<u8>>,
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn xml(body: String) -> Response {
        Response { status: 200, content_type: "application/xml", body: body.into_bytes() }
    }
    pub fn status(status: u16) -> Response {
        Response { status, content_type: "text/plain", body: Vec::new() }
    }
}

pub type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;

/// Accept loop on its own thread. `stop` plus a self-connect ends it.
pub fn spawn_listener(
    listener: TcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    handler: Handler,
    stop: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let active = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name(if tls.is_some() { "fl-https".into() } else { "fl-http".into() })
        .spawn(move || {
            for conn in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = conn else {
                    // Out of descriptors or similar: do not spin on a failing accept.
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                };
                if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    tracing::warn!("nvhttp: too many connections, dropping one");
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let (tls, handler, active) = (tls.clone(), handler.clone(), active.clone());
                let spawned = std::thread::Builder::new().name("fl-http-conn".into()).spawn(move || {
                    if let Err(e) = serve_conn(stream, tls, &handler) {
                        tracing::debug!(error = %e, "nvhttp connection ended with an error");
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                });
                if spawned.is_err() {
                    tracing::warn!("nvhttp: could not spawn connection thread");
                }
            }
        })
}

fn serve_conn(
    stream: TcpStream,
    tls: Option<Arc<rustls::ServerConfig>>,
    handler: &Handler,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let _ = stream.set_nodelay(true);
    let peer = stream.peer_addr()?;
    let local = stream.local_addr()?;
    match tls {
        None => {
            let mut s = stream;
            let head = read_head(&mut s)?;
            let resp = dispatch(&head, peer, local, false, None, handler);
            write_response(&mut s, &resp)?;
            s.flush()
        }
        Some(cfg) => {
            let conn = rustls::ServerConnection::new(cfg).map_err(std::io::Error::other)?;
            let mut tls = rustls::StreamOwned::new(conn, stream);
            // Drive the handshake so the client certificate is known before dispatch.
            let deadline = Instant::now() + IO_TIMEOUT;
            while tls.conn.is_handshaking() {
                tls.conn.complete_io(&mut tls.sock)?;
                if Instant::now() > deadline {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
            }
            let cert = tls
                .conn
                .peer_certificates()
                .and_then(|c| c.first())
                .map(|c| c.as_ref().to_vec());
            let head = read_head(&mut tls)?;
            let resp = dispatch(&head, peer, local, true, cert, handler);
            write_response(&mut tls, &resp)?;
            tls.conn.send_close_notify();
            let _ = tls.conn.complete_io(&mut tls.sock);
            Ok(())
        }
    }
}

fn read_head(s: &mut impl Read) -> std::io::Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let deadline = Instant::now() + IO_TIMEOUT;
    loop {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        // The terminator may straddle two reads.
        let scan_from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if buf[scan_from..].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_HEADER {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        if Instant::now() > deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn dispatch(
    head: &str,
    peer: SocketAddr,
    local: SocketAddr,
    https: bool,
    client_cert: Option<Vec<u8>>,
    handler: &Handler,
) -> Response {
    let line = head.lines().next().unwrap_or_default();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or("/");
    if method != "GET" {
        return Response::status(405);
    }
    let (path, qs) = target.split_once('?').unwrap_or((target, ""));
    let query = parse_query(qs);
    let req = Request { path: path.to_string(), query, peer, local, https, client_cert };
    tracing::debug!(path = %req.path, https, %peer, "nvhttp request");
    handler(&req)
}

pub fn parse_query(qs: &str) -> HashMap<String, String> {
    qs.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                        continue;
                    }
                    _ => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn write_response(s: &mut impl Write, r: &Response) -> std::io::Result<()> {
    let reason = match r.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        r.status,
        r.content_type,
        r.body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(&r.body)?;
    s.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_decoding() {
        let q = parse_query("uniqueid=0123&devicename=My%20PC&x=a+b&bad=%zz&e=");
        assert_eq!(q["devicename"], "My PC");
        assert_eq!(q["x"], "a b");
        assert_eq!(q["bad"], "%zz");
        assert_eq!(q["e"], "");
    }
}
