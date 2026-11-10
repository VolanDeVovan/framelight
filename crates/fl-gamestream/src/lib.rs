//! Watch-only GameStream (Moonlight-compatible) host.
//!
//! Planes and ports (all fixed; Moonlight derives them from the HTTP port):
//!
//! | port  | proto | what |
//! |-------|-------|------|
//! | 47989 | TCP   | nvhttp: serverinfo, pairing |
//! | 47984 | TCP   | nvhttps (mutual TLS): serverinfo, applist, appasset, launch/resume/cancel |
//! | 48010 | TCP   | RTSP: OPTIONS/DESCRIBE/SETUP/ANNOUNCE/PLAY, one request per connection |
//! | 47998 | UDP   | video RTP + FEC (client pings in, host streams out) |
//! | 47999 | UDP   | ENet control stream (only bound while a stream runs) |
//! | 48000 | UDP   | audio RTP + FEC |
//! | 5353  | UDP   | mDNS `_nvstream._tcp` |
//!
//! Lifecycle: `/launch` (or `/resume`) records a session; RTSP PLAY starts the stream:
//! control thread, video thread (calls [`VideoSource::start`]) and audio thread. The
//! stream ends on ENet disconnect/timeout, `/cancel`, a new launch, a source error or
//! host shutdown. The launch record outlives a dropped stream so the client can resume.

mod control;
mod crypto;
mod fec;
mod h26x;
mod http;
mod identity;
mod mdns;
mod media;
mod nvhttp;
mod pairing;
mod rtsp;
mod silence;
mod tls;
mod video;

pub use silence::SilenceAudioSource;

use anyhow::{Context, Result};
use fl_core::{AudioSource, Codec, StreamConfig, VideoSource};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const HTTP_PORT: u16 = 47989;
pub const HTTPS_PORT: u16 = 47984;
pub const RTSP_PORT: u16 = 48010;
pub const VIDEO_PORT: u16 = 47998;
pub const CONTROL_PORT: u16 = 47999;
pub const AUDIO_PORT: u16 = 48000;

/// Host version we claim. 7.1.431 selects: TCP RTSP, encrypted control stream with the
/// Gen7Enc packet table, multi-block FEC, the 8-byte frame header. The negative fourth
/// component marks us as "Sunshine" (X-SS-* extensions, SS ping payloads, FEC status).
pub(crate) const APP_VERSION: &str = "7.1.431.-1";
pub(crate) const GFE_VERSION: &str = "3.23.0.74";

pub(crate) const SCM_H264: u32 = 0x0000_0001;
pub(crate) const SCM_HEVC: u32 = 0x0000_0100;

/// The only app in the list.
pub(crate) const APP_ID: u32 = 1;

#[derive(Clone, Debug)]
pub struct HostConfig {
    /// Name shown in Moonlight and advertised over mDNS.
    pub name: String,
    /// Holds the host key/cert, unique id and paired clients.
    pub state_dir: PathBuf,
    /// Title of the single app.
    pub app_name: String,
    /// PNG for `/appasset` (box art). `None` serves a built-in placeholder.
    pub app_image_png: Option<Vec<u8>>,
    /// Fixed pairing PIN. `None`: pairing waits for [`GameStreamHost::submit_pin`].
    pub pin: Option<String>,
    pub mdns: bool,
    pub h264: bool,
    pub hevc: bool,
    /// Video FEC overhead in percent (Sunshine default 20).
    pub fec_percent: u8,
    /// Advertise reference-frame invalidation; RFI requests are answered with a keyframe.
    pub advertise_rfi: bool,
}

impl HostConfig {
    pub fn new(name: impl Into<String>, state_dir: impl Into<PathBuf>) -> Self {
        HostConfig {
            name: name.into(),
            state_dir: state_dir.into(),
            app_name: "Headset View".into(),
            app_image_png: None,
            pin: None,
            mdns: true,
            h264: true,
            hevc: true,
            fec_percent: 20,
            advertise_rfi: true,
        }
    }

    pub(crate) fn codec_mode_support(&self) -> u32 {
        (if self.h264 { SCM_H264 } else { 0 }) | (if self.hevc { SCM_HEVC } else { 0 })
    }
}

/// What the client launched (`/launch`, updated by `/resume`).
#[derive(Clone, Debug)]
pub(crate) struct Launch {
    pub generation: u64,
    pub appid: u32,
    pub key: [u8; 16],
    pub rikeyid: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Client certificate DER of the paired client that launched.
    pub owner: Vec<u8>,
    pub peer_ip: IpAddr,
    /// 16 ASCII chars echoed in the client's UDP pings (`X-SS-Ping-Payload`).
    pub ping: [u8; 16],
    /// ENet connect data the client must present (`X-SS-Connect-Data`).
    pub connect_data: u32,
    /// From RTSP ANNOUNCE.
    pub negotiated: Option<Negotiated>,
}

#[derive(Clone, Debug)]
pub(crate) struct Negotiated {
    pub stream: StreamConfig,
    pub packet_size: usize,
    pub min_fec: u32,
    pub control_v2: bool,
    pub audio_encrypt: bool,
    pub audio_packet_ms: u32,
    /// Channels the client's decoder is set up for: 2, or 6/8 when it asked for
    /// surround (it then gets the stereo mix in the front pair, see `silence`).
    pub audio_channels: u32,
}

/// State shared by one stream's threads.
pub(crate) struct StreamShared {
    pub launch: Launch,
    pub negotiated: Negotiated,
    pub stop: AtomicBool,
    /// On stop, tell the client the host ended the session (control TERMINATION).
    pub notify_client: AtomicBool,
    pub keyframe: AtomicBool,
    pub started: Instant,
}

impl StreamShared {
    pub fn end(&self, reason: &str, notify_client: bool) {
        if notify_client {
            self.notify_client.store(true, Ordering::SeqCst);
        }
        if !self.stop.swap(true, Ordering::SeqCst) {
            tracing::info!(reason, "stream ending");
        }
    }
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

pub(crate) struct Stream {
    pub shared: Arc<StreamShared>,
    pub threads: Vec<JoinHandle<()>>,
}

pub(crate) struct Inner {
    pub cfg: HostConfig,
    pub id: identity::Identity,
    pub paired: identity::PairedStore,
    pub pairing: pairing::Pairing,
    pub launch: Mutex<Option<Launch>>,
    pub stream: Mutex<Option<Stream>>,
    pub video_src: Arc<Mutex<Box<dyn VideoSource>>>,
    pub audio_src: Arc<Mutex<Box<dyn AudioSource>>>,
    pub video_sock: UdpSocket,
    pub audio_sock: UdpSocket,
    pub generation: AtomicU64,
    pub shutdown: AtomicBool,
}

impl Inner {
    /// Stop the running stream (if any) and wait for its threads.
    pub fn stop_stream(&self, reason: &str, notify_client: bool) {
        // The slot stays locked while the threads wind down, so stops and starts from
        // different connections (HTTP, RTSP, shutdown) happen one after another.
        let mut slot = self.stream.lock().unwrap();
        Self::stop_locked(&mut slot, reason, notify_client);
    }

    fn stop_locked(slot: &mut Option<Stream>, reason: &str, notify_client: bool) {
        if let Some(s) = slot.take() {
            s.shared.end(reason, notify_client);
            join_bounded(s.threads, Duration::from_secs(4));
        }
    }

    /// RTSP PLAY: start control + media threads for the current launch.
    pub fn start_stream(self: &Arc<Self>) -> Result<()> {
        let mut slot = self.stream.lock().unwrap();
        anyhow::ensure!(!self.shutdown.load(Ordering::SeqCst), "host is shutting down");
        let launch = self.launch.lock().unwrap().clone().context("no launched session")?;
        let negotiated = launch.negotiated.clone().context("PLAY before ANNOUNCE")?;
        // A previous stream (same or older launch) must release ports and sources first.
        Self::stop_locked(&mut slot, "new stream starting", false);
        let shared = Arc::new(StreamShared {
            launch,
            negotiated,
            stop: AtomicBool::new(false),
            notify_client: AtomicBool::new(false),
            keyframe: AtomicBool::new(true),
            started: Instant::now(),
        });
        type Body = fn(&Arc<Inner>, &Arc<StreamShared>);
        let bodies: [(&str, Body); 3] =
            [("fl-control", control::run), ("fl-video", media::run_video), ("fl-audio", media::run_audio)];
        let mut threads = Vec::with_capacity(bodies.len());
        for (name, body) in bodies {
            let (inner, sh) = (self.clone(), shared.clone());
            match std::thread::Builder::new().name(name.into()).spawn(move || body(&inner, &sh)) {
                Ok(t) => threads.push(t),
                Err(e) => {
                    shared.end("could not start a stream thread", false);
                    join_bounded(threads, Duration::from_secs(4));
                    return Err(e).context("spawn stream thread");
                }
            }
        }
        let s = &shared.negotiated.stream;
        tracing::info!(
            codec = ?s.codec, width = s.width, height = s.height, fps = s.fps,
            bitrate_kbps = s.bitrate_kbps, client = %shared.launch.peer_ip,
            "stream started"
        );
        *slot = Some(Stream { shared, threads });
        Ok(())
    }
}

/// Join threads, giving up (detaching) after `limit` so a wedged source cannot hang
/// the HTTP/RTSP handler that asked for the stop.
fn join_bounded(threads: Vec<JoinHandle<()>>, limit: Duration) {
    let deadline = Instant::now() + limit;
    for t in threads {
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if t.is_finished() {
            let _ = t.join();
        } else {
            tracing::warn!(thread = ?t.thread().name(), "stream thread did not stop in time; detaching");
        }
    }
}

/// A running host. Dropping it does not stop it; call [`GameStreamHost::shutdown`].
pub struct GameStreamHost {
    inner: Arc<Inner>,
    listeners: Vec<(SocketAddr, JoinHandle<()>)>,
    stop_listeners: Arc<AtomicBool>,
    _mdns: Option<mdns::Advert>,
}

impl GameStreamHost {
    pub fn start(
        cfg: HostConfig,
        video: Box<dyn VideoSource>,
        audio: Box<dyn AudioSource>,
    ) -> Result<GameStreamHost> {
        anyhow::ensure!(cfg.h264 || cfg.hevc, "at least one codec must be enabled");
        if cfg.pin.is_some() {
            // GameStream pairing proves knowledge of the PIN in a way that can be brute
            // forced offline after one failed attempt, which is harmless only when every
            // attempt has its own PIN.
            tracing::warn!("fixed pairing PIN: any device on this network can pair; prefer typing the PIN the client shows");
        }
        let id = identity::Identity::load_or_create(&cfg.state_dir)?;
        let paired = identity::PairedStore::load(&cfg.state_dir);
        let tls = tls::server_config(&id)?;
        let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        let video_sock = UdpSocket::bind((any, VIDEO_PORT)).context("bind video UDP 47998")?;
        let audio_sock = UdpSocket::bind((any, AUDIO_PORT)).context("bind audio UDP 48000")?;
        media::tune_socket(&video_sock, 4 << 20);
        let http = TcpListener::bind((any, HTTP_PORT)).context("bind HTTP 47989")?;
        let https = TcpListener::bind((any, HTTPS_PORT)).context("bind HTTPS 47984")?;
        let rtsp = TcpListener::bind((any, RTSP_PORT)).context("bind RTSP 48010")?;

        let inner = Arc::new(Inner {
            pairing: pairing::Pairing::new(cfg.pin.clone()),
            cfg,
            id,
            paired,
            launch: Mutex::new(None),
            stream: Mutex::new(None),
            video_src: Arc::new(Mutex::new(video)),
            audio_src: Arc::new(Mutex::new(audio)),
            video_sock,
            audio_sock,
            generation: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let mut listeners = Vec::new();
        for (l, tls) in [(http, None), (https, Some(tls))] {
            let addr = l.local_addr()?;
            let handler = nvhttp::handler(inner.clone());
            listeners.push((addr, http::spawn_listener(l, tls, handler, stop.clone())?));
        }
        let addr = rtsp.local_addr()?;
        listeners.push((addr, rtsp::spawn(rtsp, inner.clone(), stop.clone())?));

        let advert = if inner.cfg.mdns {
            match mdns::advertise(&inner.cfg.name, HTTP_PORT) {
                Ok(a) => Some(a),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "mDNS advertisement failed; clients must add the host by IP");
                    None
                }
            }
        } else {
            None
        };
        tracing::info!(
            name = %inner.cfg.name,
            uniqueid = %inner.id.uniqueid,
            paired_clients = inner.paired.count(),
            "GameStream host listening (HTTP 47989, HTTPS 47984, RTSP 48010, UDP 47998-48000)"
        );
        Ok(GameStreamHost { inner, listeners, stop_listeners: stop, _mdns: advert })
    }

    /// Deliver the PIN for a pairing that is waiting. Returns false if none is waiting.
    pub fn submit_pin(&self, pin: &str) -> bool {
        self.inner.pairing.pin.submit(pin)
    }

    pub fn pairing_waits_for_pin(&self) -> bool {
        self.inner.pairing.pin.is_waiting()
    }

    /// Whether a stream (control/video/audio threads) is currently running.
    pub fn streaming(&self) -> bool {
        self.inner.stream.lock().unwrap().as_ref().is_some_and(|s| !s.shared.stopped())
    }

    /// Stop any stream (telling the client), close listeners, withdraw mDNS.
    pub fn shutdown(self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.inner.stop_stream("host shutting down", true);
        self.stop_listeners.store(true, Ordering::SeqCst);
        for (addr, t) in self.listeners {
            // Wake the blocking accept.
            let _ = TcpStream::connect_timeout(
                &SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port()),
                Duration::from_millis(200),
            );
            let _ = t.join();
        }
    }
}

pub(crate) fn parse_codec(bitstream_format: Option<u32>) -> Option<Codec> {
    match bitstream_format.unwrap_or(0) {
        0 => Some(Codec::H264),
        1 => Some(Codec::Hevc),
        _ => None,
    }
}
