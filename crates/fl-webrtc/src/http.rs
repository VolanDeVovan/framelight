//! A deliberately tiny blocking HTTP/1.1 server: one request per connection
//! (`Connection: close`), a thread per connection with a concurrency cap. The viewer
//! page makes a handful of small requests; nothing more is needed.

use crate::net::StopSignal;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const MAX_CONNECTIONS: usize = 32;
const MAX_HEAD: usize = 16 * 1024;
/// An SDP offer from a browser is 5-10 KiB.
const MAX_BODY: usize = 256 * 1024;
/// Per read/write and for a whole request, so a client trickling bytes cannot hold a
/// connection slot.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: Vec<u8>,
    pub peer: SocketAddr,
    /// The address the client reached us on.
    pub local: SocketAddr,
}

impl Request {
    pub fn query_value(&self, key: &str) -> Option<&str> {
        self.query.split('&').find_map(|kv| {
            kv.split_once('=')
                .filter(|(k, _)| *k == key)
                .map(|(_, v)| v)
        })
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Response {
            status: 200,
            content_type,
            headers: Vec::new(),
            body: body.into(),
        }
    }
    pub fn json(body: String) -> Self {
        Self::new("application/json", body)
    }
    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Response {
            status,
            ..Self::new("text/plain; charset=utf-8", message.into())
        }
    }
    pub fn header(mut self, name: &'static str, value: String) -> Self {
        self.headers.push((name, value));
        self
    }
}

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// The accept loop; returns when `stop` is raised. `listener` must be non-blocking.
pub fn accept_loop(listener: TcpListener, handler: Handler, stop: &StopSignal) {
    let active = Arc::new(AtomicUsize::new(0));
    while stop.wait_readable(listener.as_raw_fd()) {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                // Out of descriptors or similar: do not spin on a failing accept.
                tracing::debug!(error = %e, "http: accept failed");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            tracing::warn!("http: too many connections, dropping one");
            continue;
        }
        active.fetch_add(1, Ordering::SeqCst);
        let (handler, active2) = (handler.clone(), active.clone());
        let spawned = std::thread::Builder::new()
            .name("fl-http-conn".into())
            .spawn(move || {
                if let Err(e) = serve(stream, &handler) {
                    tracing::debug!(error = %e, "http connection ended with an error");
                }
                active2.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!("http: could not spawn a connection thread");
        }
    }
}

fn serve(mut stream: TcpStream, handler: &Handler) -> std::io::Result<()> {
    // The listener is non-blocking; a connection is served with blocking calls.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let _ = stream.set_nodelay(true);
    let (peer, local) = (stream.peer_addr()?, stream.local_addr()?);
    let response = match read_request(&mut stream, peer, local) {
        Ok(request) => handler(request),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            Response::error(400, e.to_string())
        }
        Err(e) => return Err(e),
    };
    write_response(&mut stream, &response)
}

fn invalid(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_owned())
}

fn read_request(
    s: &mut impl Read,
    peer: SocketAddr,
    local: SocketAddr,
) -> std::io::Result<Request> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err(invalid("request head too large"));
        }
        if Instant::now() > deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        match s.read(&mut chunk)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head =
        std::str::from_utf8(&buf[..head_end]).map_err(|_| invalid("request head is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target)) = (first.next(), first.next()) else {
        return Err(invalid("bad request line"));
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value
                .trim()
                .parse()
                .map_err(|_| invalid("bad Content-Length"))?;
        }
    }
    if length > MAX_BODY {
        return Err(invalid("request body too large"));
    }
    let (method, path, query) = (method.to_owned(), path.to_owned(), query.to_owned());
    let mut body = buf.split_off(head_end);
    while body.len() < length {
        if Instant::now() > deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        match s.read(&mut chunk)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => body.extend_from_slice(&chunk[..n]),
        }
    }
    body.truncate(length);
    Ok(Request {
        method,
        path,
        query,
        body,
        peer,
        local,
    })
}

fn write_response(s: &mut impl Write, r: &Response) -> std::io::Result<()> {
    let reason = match r.status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        r.status,
        r.content_type,
        r.body.len()
    );
    for (name, value) in &r.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes())?;
    s.write_all(&r.body)?;
    s.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> SocketAddr {
        "127.0.0.1:1".parse().unwrap()
    }

    #[test]
    fn parses_a_post_with_a_body_split_across_reads() {
        struct Trickle(Vec<u8>, usize);
        impl Read for Trickle {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                let n = 7.min(self.0.len() - self.1).min(out.len());
                out[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
                self.1 += n;
                Ok(n)
            }
        }
        let raw = b"POST /bitrate?kbps=8000&x=1 HTTP/1.1\r\nHost: frame.local\r\ncontent-length: 5\r\n\r\nhelloEXTRA";
        let r = read_request(&mut Trickle(raw.to_vec(), 0), addr(), addr()).unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/bitrate"));
        assert_eq!(r.query_value("kbps"), Some("8000"));
        assert_eq!(r.query_value("x"), Some("1"));
        assert_eq!(r.query_value("k"), None);
        assert_eq!(r.body, b"hello");
    }

    #[test]
    fn rejects_garbage_and_oversized_requests() {
        let bad = |raw: &[u8]| {
            read_request(&mut &raw[..], addr(), addr())
                .err()
                .map(|e| e.kind())
        };
        assert_eq!(bad(b"\r\n\r\n"), Some(std::io::ErrorKind::InvalidData));
        assert_eq!(
            bad(b"GET / HTTP/1.1\r\nContent-Length: 99999999\r\n\r\n"),
            Some(std::io::ErrorKind::InvalidData)
        );
        assert_eq!(
            bad(b"GET / HTTP/1.1\r\n"),
            Some(std::io::ErrorKind::UnexpectedEof)
        );
        assert_eq!(
            bad(&[b'a'; MAX_HEAD + 8192]),
            Some(std::io::ErrorKind::InvalidData)
        );
    }

    #[test]
    fn writes_a_complete_response() {
        let mut out = Vec::new();
        write_response(
            &mut out,
            &Response::json("{}".into()).header("Viewer-Id", "7".into()),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 2\r\n") && text.contains("Viewer-Id: 7\r\n"));
        assert!(text.ends_with("\r\n\r\n{}"));
    }
}
