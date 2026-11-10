//! The HTTP endpoints: the page, the offer/answer exchange, status and bitrate.

use crate::adapt::{FLOOR_KBPS, MAX_KBPS};
use crate::http::{Request, Response};
use crate::hub::Msg;
use crate::viewer::Viewer;
use crate::{SourceState, Status};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

const PAGE: &str = include_str!("page.html");

pub(crate) struct Api {
    page: String,
    /// UDP port of the media: the candidate offered to every browser.
    port: u16,
    next_id: AtomicU64,
    tx: Sender<Msg>,
    status: Arc<Mutex<Status>>,
}

impl Api {
    pub fn new(title: &str, port: u16, tx: Sender<Msg>, status: Arc<Mutex<Status>>) -> Self {
        Self {
            page: PAGE.replace("{{TITLE}}", &html_escape(title)),
            port,
            next_id: AtomicU64::new(1),
            tx,
            status,
        }
    }

    fn send(&self, msg: Msg) -> bool {
        self.tx.send(msg).is_ok()
    }

    pub fn route(&self, req: &Request) -> Response {
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") | ("GET", "/index.html") => {
                Response::new("text/html; charset=utf-8", self.page.as_bytes())
            }
            ("GET", "/status") => Response::json(status_json(&self.status.lock().unwrap())),
            ("POST", "/offer") => self.offer(req),
            ("POST", "/bye") => {
                if let Some(id) = req.query_value("id").and_then(|v| v.parse().ok()) {
                    self.send(Msg::Leave(id));
                }
                Response::new("text/plain", "")
            }
            ("POST", "/bitrate") => {
                match req.query_value("kbps").and_then(|v| v.parse::<u32>().ok()) {
                    Some(kbps) => {
                        let kbps = kbps.clamp(FLOOR_KBPS, MAX_KBPS);
                        self.send(Msg::Ceiling(kbps));
                        // The hub applies it a moment later; answer with what it will be.
                        let mut s = self.status.lock().unwrap().clone();
                        (s.ceiling_kbps, s.bitrate_kbps) = (kbps, kbps);
                        Response::json(status_json(&s))
                    }
                    None => Response::error(400, "expected ?kbps=N"),
                }
            }
            (_, "/" | "/index.html" | "/status" | "/offer" | "/bye" | "/bitrate") => {
                Response::error(405, "method not allowed")
            }
            _ => Response::error(404, "not found"),
        }
    }

    fn offer(&self, req: &Request) -> Response {
        let Ok(sdp) = std::str::from_utf8(&req.body) else {
            return Response::error(400, "the offer is not UTF-8");
        };
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        // The one address this browser has already reached us on.
        let local = SocketAddr::new(req.local.ip(), self.port);
        match Viewer::from_offer(id, sdp, req.peer, local) {
            Ok((viewer, answer)) => {
                if !self.send(Msg::Join(Box::new(viewer))) {
                    return Response::error(503, "the streamer is shutting down");
                }
                Response::new("application/sdp", answer).header("Viewer-Id", id.to_string())
            }
            Err(e) => {
                tracing::warn!(peer = %req.peer, error = %format!("{e:#}"), "offer rejected");
                Response::error(400, format!("{e:#}"))
            }
        }
    }
}

fn status_json(s: &Status) -> String {
    let (state, error) = match &s.source {
        SourceState::Idle => ("idle", ""),
        SourceState::Starting => ("starting", ""),
        SourceState::Running => ("running", ""),
        SourceState::Failed(e) => ("failed", e.as_str()),
    };
    format!(
        "{{\"viewers\":{},\"source\":\"{state}\",\"error\":\"{}\",\"bitrate_kbps\":{},\"ceiling_kbps\":{},\
         \"min_kbps\":{FLOOR_KBPS},\"max_kbps\":{MAX_KBPS},\"width\":{},\"height\":{},\"fps\":{},\"profile_level_id\":\"{}\"}}",
        s.viewers,
        json_escape(error),
        s.bitrate_kbps,
        s.ceiling_kbps,
        s.width,
        s.height,
        s.fps,
        s.profile_level_id
            .map(|p| format!("{p:06x}"))
            .unwrap_or_default(),
    )
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_json_is_valid_and_escaped() {
        let s = Status {
            viewers: 2,
            source: SourceState::Failed("SteamVR \"is\" not\\running\n".into()),
            bitrate_kbps: 9600,
            ceiling_kbps: 12000,
            width: 1920,
            height: 1080,
            fps: 60,
            profile_level_id: Some(0x64002a),
        };
        let j = status_json(&s);
        assert!(j.contains("\"viewers\":2") && j.contains("\"source\":\"failed\""));
        assert!(
            j.contains(r#""error":"SteamVR \"is\" not\\running ""#),
            "{j}"
        );
        assert!(j.contains("\"profile_level_id\":\"64002a\""));
        assert_eq!(j.matches('{').count(), 1);
    }

    fn request(method: &str, path: &str, query: &str) -> Request {
        let addr: SocketAddr = "192.168.0.48:8090".parse().unwrap();
        Request {
            method: method.into(),
            path: path.into(),
            query: query.into(),
            body: Vec::new(),
            peer: addr,
            local: addr,
        }
    }

    #[test]
    fn routes_and_clamps() {
        let (tx, rx) = std::sync::mpsc::channel();
        let api = Api::new("a <b>", 8090, tx, Arc::default());
        let page = api.route(&request("GET", "/", ""));
        assert_eq!(page.status, 200);
        assert!(
            String::from_utf8(page.body)
                .unwrap()
                .contains("<title>a &lt;b&gt;</title>")
        );
        assert_eq!(api.route(&request("GET", "/nope", "")).status, 404);
        assert_eq!(api.route(&request("GET", "/offer", "")).status, 405);
        assert_eq!(
            api.route(&request("POST", "/bitrate", "kbps=x")).status,
            400
        );
        // An offer that is not SDP is refused without reaching the hub.
        assert_eq!(api.route(&request("POST", "/offer", "")).status, 400);
        assert!(rx.try_recv().is_err());

        let r = api.route(&request("POST", "/bitrate", "kbps=1"));
        assert!(
            String::from_utf8(r.body)
                .unwrap()
                .contains(&format!("\"ceiling_kbps\":{FLOOR_KBPS}"))
        );
        assert!(matches!(rx.try_recv(), Ok(Msg::Ceiling(FLOOR_KBPS))));
        api.route(&request("POST", "/bye", "id=7"));
        assert!(matches!(rx.try_recv(), Ok(Msg::Leave(7))));
    }
}
