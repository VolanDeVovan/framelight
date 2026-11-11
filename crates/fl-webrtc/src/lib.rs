//! Watch-only WebRTC streamer.
//!
//! A browser opens `http://<host>:<port>/`, the page posts an SDP offer and plays what
//! comes back: H.264 video and Opus audio from one [`VideoSource`] / [`AudioSource`]
//! pair, shared by every viewer. No accounts, no input: whoever can reach the port can
//! watch.
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
//! Threads (blocking, no async runtime), all owned by [`Server`]:
//!
//! | thread     | module | sleeps in |
//! |------------|--------|-----------|
//! | `fl-http`  | [`http`] accept loop (+ one short-lived thread per request) | `poll` |
//! | `fl-udp`   | [`net`] datagrams in | `poll` |
//! | `fl-hub`   | [`hub`] owns every peer connection | channel receive |
//! | `fl-video` | [`pump`] runs the video source | a condition variable |
//! | `fl-audio` | [`pump`] runs the audio source | a condition variable |
//!
//! With nobody watching all of them sleep without a timeout and the sources are
//! stopped.

mod adapt;
mod api;
mod clock;
mod h264;
mod http;
mod hub;
mod net;
mod pump;
mod viewer;

pub use adapt::{FLOOR_KBPS, MAX_KBPS};

use anyhow::{Context, Result};
use fl_core::{AudioSource, H264Profile, StreamConfig, VideoSource};
use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Free on the Steam Frame (Steam itself listens on 8080, 8081 and 8088).
pub const DEFAULT_PORT: u16 = 8090;

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

/// A running streamer. Dropping it disconnects every viewer, stops the sources and
/// closes the ports.
pub struct Server {
    status: Arc<Mutex<Status>>,
    tx: Sender<hub::Msg>,
    stop: Arc<net::StopSignal>,
    ctl: Arc<pump::Control>,
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
        // Nothing in here waits in a socket call: the threads sleep in poll().
        listener
            .set_nonblocking(true)
            .context("make the TCP listener non-blocking")?;
        socket
            .set_nonblocking(true)
            .context("make the UDP socket non-blocking")?;
        net::tune_media_socket(&socket);

        // Viewers decide the profile; this is only what an idle streamer reports.
        let stream = StreamConfig {
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            bitrate_kbps: cfg.bitrate_kbps,
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
        let (tx, rx) = channel();
        let ctl = Arc::new(pump::Control::new(stream.profile, stream.bitrate_kbps));
        let hub = hub::Hub::new(
            socket.try_clone().context("clone the UDP socket")?,
            rx,
            ctl.clone(),
            status.clone(),
            cfg.bitrate_kbps,
        );
        let api = api::Api::new(&cfg.title, cfg.port, tx.clone(), status.clone());
        let handler: http::Handler = Arc::new(move |req| api.route(&req));

        // From here on a failure drops `server`, which stops what was already started.
        let mut server = Server {
            status,
            tx: tx.clone(),
            stop: Arc::new(net::StopSignal::new().context("eventfd")?),
            ctl: ctl.clone(),
            threads: Vec::new(),
        };
        server.spawn("fl-hub", move || hub.run())?;
        {
            let (stop, tx) = (server.stop.clone(), tx.clone());
            server.spawn("fl-udp", move || net::udp_receiver(socket, stop, tx))?;
        }
        {
            let (ctl, tx) = (ctl.clone(), tx.clone());
            server.spawn("fl-video", move || pump::video(video, stream, &ctl, tx))?;
        }
        if let Some(audio) = audio {
            server.spawn("fl-audio", move || pump::audio(audio, &ctl, tx))?;
        }
        {
            let stop = server.stop.clone();
            server.spawn("fl-http", move || {
                http::accept_loop(listener, handler, &stop)
            })?;
        }
        tracing::info!(
            port = cfg.port,
            mode = %format!("{}x{}@{}", cfg.width, cfg.height, cfg.fps),
            bitrate_kbps = cfg.bitrate_kbps,
            "WebRTC streamer listening (page on TCP, media on UDP)"
        );
        Ok(server)
    }

    fn spawn(&mut self, name: &str, f: impl FnOnce() + Send + 'static) -> Result<()> {
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(f)
            .with_context(|| format!("spawn the {name} thread"))?;
        self.threads.push(thread);
        Ok(())
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    /// Disconnect every viewer, stop the sources, close the ports. The same as
    /// dropping the server; returns when every thread has ended.
    pub fn shutdown(self) {}
}

impl Drop for Server {
    fn drop(&mut self) {
        // The accept and receive threads, the pumps, and the hub, each by its own
        // wake-up; none of them depends on another one still running.
        self.stop.raise();
        self.ctl.shutdown();
        let _ = self.tx.send(hub::Msg::Shutdown);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// The IPv4 address this machine uses towards the LAN (the source address of its
/// default route). No packet is sent.
pub fn primary_ipv4() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// An address the streamer can be reached at, and the interface that has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalAddress {
    pub interface: String,
    pub ip: Ipv4Addr,
}

/// Every IPv4 address a browser elsewhere could reach this machine at: on interfaces
/// that are up and running, not loopback, not link-local. The streamer listens on all
/// of them. The default route's address comes first.
pub fn local_ipv4_addresses() -> Vec<LocalAddress> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates the list; every entry is read before freeifaddrs.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return found;
    }
    let wanted = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_uint;
    let mut entry = list;
    while !entry.is_null() {
        // SAFETY: a non-null entry of the list getifaddrs returned.
        let ifa = unsafe { &*entry };
        entry = ifa.ifa_next;
        if ifa.ifa_addr.is_null()
            || ifa.ifa_flags & wanted != wanted
            || ifa.ifa_flags & libc::IFF_LOOPBACK as libc::c_uint != 0
            // SAFETY: ifa_addr is non-null and starts with the family.
            || i32::from(unsafe { (*ifa.ifa_addr).sa_family }) != libc::AF_INET
        {
            continue;
        }
        // SAFETY: an AF_INET address is a sockaddr_in; the name is a C string.
        let (sin, name) = unsafe {
            (
                &*ifa.ifa_addr.cast::<libc::sockaddr_in>(),
                std::ffi::CStr::from_ptr(ifa.ifa_name),
            )
        };
        found.push(LocalAddress {
            interface: name.to_string_lossy().into_owned(),
            ip: Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)),
        });
    }
    // SAFETY: the list from getifaddrs above, no longer referenced.
    unsafe { libc::freeifaddrs(list) };
    reachable_first(found, primary_ipv4())
}

/// Drops what no other machine can use and puts the default route's address first.
fn reachable_first(mut found: Vec<LocalAddress>, primary: Option<Ipv4Addr>) -> Vec<LocalAddress> {
    found.retain(|a| !(a.ip.is_loopback() || a.ip.is_link_local() || a.ip.is_unspecified()));
    if let Some(i) = found.iter().position(|a| Some(a.ip) == primary) {
        let first = found.remove(i);
        found.insert(0, first);
    }
    found
}

#[cfg(test)]
mod address_tests {
    use super::*;

    fn at(interface: &str, ip: [u8; 4]) -> LocalAddress {
        LocalAddress {
            interface: interface.into(),
            ip: ip.into(),
        }
    }

    #[test]
    fn the_default_route_first_and_nothing_unreachable() {
        let found = vec![
            at("tailscale0", [100, 64, 0, 7]),
            at("lo", [127, 0, 0, 1]),
            at("wlan0", [192, 168, 0, 48]),
            at("usb0", [169, 254, 3, 1]),
        ];
        let wifi = Some(Ipv4Addr::new(192, 168, 0, 48));
        assert_eq!(
            reachable_first(found.clone(), wifi),
            [
                at("wlan0", [192, 168, 0, 48]),
                at("tailscale0", [100, 64, 0, 7])
            ]
        );
        // No default route: the interface order stays.
        assert_eq!(
            reachable_first(found, None),
            [
                at("tailscale0", [100, 64, 0, 7]),
                at("wlan0", [192, 168, 0, 48])
            ]
        );
    }

    #[test]
    fn this_machine_has_an_address() {
        // Whatever the test machine has, the call works and keeps its promises.
        for a in local_ipv4_addresses() {
            assert!(!a.ip.is_loopback() && !a.ip.is_link_local(), "{a:?}");
            assert!(!a.interface.is_empty());
        }
    }
}
