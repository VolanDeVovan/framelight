//! Watch-only WebRTC streamer.
//!
//! A browser opens `http://<host>:<port>/`, the page posts an SDP offer and plays what
//! comes back: H.264 video and Opus audio from one [`VideoSource`] / [`AudioSource`]
//! pair, shared by every viewer. No pairing, no accounts, no input: whoever can reach
//! the port can watch.
//!
//! | endpoint                | what |
//! |-------------------------|------|
//! | `GET /`                 | the viewer page |
//! | `POST /offer`           | SDP offer in, SDP answer out (`Viewer-Id` header) |
//! | `POST /bye?id=N`        | the page is closing |
//! | `GET /status`           | JSON: viewers, source state, bitrate |
//! | `POST /bitrate?kbps=N`  | bitrate ceiling of the shared encode |
//! | UDP, same port number   | ICE, DTLS, SRTP for all viewers |
//!
//! Threads (blocking, no async runtime): HTTP accept (+ one short-lived thread per
//! request), UDP receive, the hub that owns every peer connection, and the video and
//! audio pumps. With nobody watching all of them sleep in a blocking call and the
//! sources are stopped.

mod adapt;
mod h264;
mod http;
mod hub;

pub use adapt::{FLOOR_KBPS, MAX_KBPS};

use anyhow::{Context, Result};
use fl_core::{AudioSource, Codec, H264Profile, StreamConfig, VideoSource};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use str0m::change::SdpOffer;
use str0m::{Candidate, RtcConfig};

/// Free on the Steam Frame (Steam itself listens on 8080, 8081 and 8088).
pub const DEFAULT_PORT: u16 = 8090;

const PAGE: &str = include_str!("page.html");

#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// TCP port of the page and UDP port of the media.
    pub port: u16,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Initial bitrate ceiling; viewers can change it from the page.
    pub bitrate_kbps: u32,
    /// Shown as the page title.
    pub title: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 12_000,
            title: "Headset View".into(),
        }
    }
}

/// What the shared source is doing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SourceState {
    /// Nobody is watching; nothing is open.
    #[default]
    Idle,
    Starting,
    Running,
    /// Could not start or stopped with an error; retried while viewers stay.
    Failed(String),
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    /// Viewers with an established connection.
    pub viewers: usize,
    pub source: SourceState,
    pub bitrate_kbps: u32,
    pub ceiling_kbps: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `profile-level-id` of the stream as its SPS states it.
    pub profile_level_id: Option<u32>,
}

impl Status {
    fn json(&self) -> String {
        let (state, error) = match &self.source {
            SourceState::Idle => ("idle", ""),
            SourceState::Starting => ("starting", ""),
            SourceState::Running => ("running", ""),
            SourceState::Failed(e) => ("failed", e.as_str()),
        };
        format!(
            "{{\"viewers\":{},\"source\":\"{state}\",\"error\":\"{}\",\"bitrate_kbps\":{},\"ceiling_kbps\":{},\
             \"min_kbps\":{FLOOR_KBPS},\"max_kbps\":{MAX_KBPS},\"width\":{},\"height\":{},\"fps\":{},\"profile_level_id\":\"{}\"}}",
            self.viewers,
            json_escape(error),
            self.bitrate_kbps,
            self.ceiling_kbps,
            self.width,
            self.height,
            self.fps,
            self.profile_level_id
                .map(|p| format!("{p:06x}"))
                .unwrap_or_default(),
        )
    }
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

/// A running streamer. Dropping it does not stop it; call [`Server::shutdown`].
pub struct Server {
    port: u16,
    status: Arc<Mutex<Status>>,
    tx: Sender<hub::Msg>,
    stop: Arc<AtomicBool>,
    /// Ends the UDP thread's sleep.
    wake: std::os::fd::OwnedFd,
    threads: Vec<JoinHandle<()>>,
}

impl Server {
    pub fn start(
        cfg: ServerConfig,
        video: Box<dyn VideoSource>,
        audio: Option<Box<dyn AudioSource>>,
    ) -> Result<Server> {
        static CRYPTO: std::sync::Once = std::sync::Once::new();
        CRYPTO.call_once(|| str0m::crypto::from_feature_flags().install_process_default());

        let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        let listener = TcpListener::bind((any, cfg.port))
            .with_context(|| format!("bind TCP port {}", cfg.port))?;
        let socket = UdpSocket::bind((any, cfg.port))
            .with_context(|| format!("bind UDP port {}", cfg.port))?;
        hub::tune_socket(&socket);

        let stream = StreamConfig {
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            bitrate_kbps: cfg.bitrate_kbps,
            codec: Codec::H264,
            profile: H264Profile::High,
        };
        let status = Arc::new(Mutex::new(Status {
            bitrate_kbps: cfg.bitrate_kbps,
            ceiling_kbps: cfg.bitrate_kbps,
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            ..Status::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, mut threads, wake) =
            hub::spawn(socket, stream, video, audio, status.clone(), stop.clone())?;

        let page: Arc<str> = PAGE.replace("{{TITLE}}", &html_escape(&cfg.title)).into();
        let next_id = AtomicU64::new(1);
        let (port, tx2, status2) = (cfg.port, tx.clone(), status.clone());
        let handler: http::Handler =
            Arc::new(move |req| route(&req, &page, port, &next_id, &tx2, &status2));
        threads
            .push(http::spawn(listener, handler, stop.clone()).context("spawn the HTTP thread")?);
        tracing::info!(
            port = cfg.port,
            mode = %format!("{}x{}@{}", cfg.width, cfg.height, cfg.fps),
            bitrate_kbps = cfg.bitrate_kbps,
            "WebRTC streamer listening (page on TCP, media on UDP)"
        );
        Ok(Server {
            port: cfg.port,
            status,
            tx,
            stop,
            wake,
            threads,
        })
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    /// Disconnect every viewer, stop the sources, close the ports.
    pub fn shutdown(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.tx.send(hub::Msg::Shutdown);
        hub::wake(&self.wake);
        // Wake the blocking accept.
        let local = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port);
        let _ = TcpStream::connect_timeout(&local, Duration::from_millis(200));
        for t in self.threads {
            let _ = t.join();
        }
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn route(
    req: &http::Request,
    page: &Arc<str>,
    port: u16,
    next_id: &AtomicU64,
    tx: &Sender<hub::Msg>,
    status: &Mutex<Status>,
) -> http::Response {
    use http::Response;
    let status_json = || Response::json(status.lock().unwrap().json());
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            Response::new("text/html; charset=utf-8", page.as_bytes())
        }
        ("GET", "/status") => status_json(),
        ("POST", "/offer") => {
            let id = next_id.fetch_add(1, Ordering::SeqCst);
            match accept_offer(req, port, id) {
                Ok((viewer, answer)) => {
                    if tx.send(hub::Msg::Join(Box::new(viewer))).is_err() {
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
        ("POST", "/bye") => {
            if let Some(id) = req.query_value("id").and_then(|v| v.parse().ok()) {
                let _ = tx.send(hub::Msg::Leave(id));
            }
            Response::new("text/plain", "")
        }
        ("POST", "/bitrate") => match req.query_value("kbps").and_then(|v| v.parse::<u32>().ok()) {
            Some(kbps) => {
                let kbps = kbps.clamp(FLOOR_KBPS, MAX_KBPS);
                let _ = tx.send(hub::Msg::Ceiling(kbps));
                // The hub applies it a moment later; answer with what it will be.
                let mut s = status.lock().unwrap().clone();
                (s.ceiling_kbps, s.bitrate_kbps) = (kbps, kbps);
                Response::json(s.json())
            }
            None => Response::error(400, "expected ?kbps=N"),
        },
        (_, "/" | "/index.html" | "/status" | "/offer" | "/bye" | "/bitrate") => {
            Response::error(405, "method not allowed")
        }
        _ => Response::error(404, "not found"),
    }
}

/// Builds the peer connection for one browser and answers its offer.
fn accept_offer(req: &http::Request, port: u16, id: u64) -> Result<(hub::Viewer, String)> {
    let sdp = std::str::from_utf8(&req.body).context("the offer is not UTF-8")?;
    let offer =
        SdpOffer::from_sdp_string(sdp).map_err(|e| anyhow::anyhow!("bad SDP offer: {e}"))?;
    let profiles = h264::offered_profiles(sdp);
    let max_profile = profiles
        .iter()
        .copied()
        .max()
        .context("this browser does not offer H.264 video over WebRTC")?;

    let now = Instant::now();
    let mut rtc = RtcConfig::new()
        // The browser comes to us: we only answer connectivity checks.
        .set_ice_lite(true)
        .clear_codecs()
        .enable_h264(true)
        .enable_opus(true, false)
        .set_stats_interval(Some(Duration::from_secs(1)))
        .build(now);
    // The one address this browser has already reached us on.
    let local = SocketAddr::new(req.local.ip(), port);
    rtc.add_local_candidate(Candidate::host(local, "udp").context("local candidate")?);
    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| anyhow::anyhow!("cannot answer the offer: {e}"))?;
    Ok((
        hub::Viewer::new(id, rtc, req.peer, local, max_profile, now),
        answer.to_sdp_string(),
    ))
}

/// The IPv4 address this machine uses towards the LAN (the source address of its
/// default route), for showing a URL to type. No packet is sent.
pub fn primary_ipv4() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
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
        let j = s.json();
        assert!(j.contains("\"viewers\":2") && j.contains("\"source\":\"failed\""));
        assert!(
            j.contains(r#""error":"SteamVR \"is\" not\\running ""#),
            "{j}"
        );
        assert!(j.contains("\"profile_level_id\":\"64002a\""));
        assert_eq!(j.matches('{').count(), 1);
    }
}
