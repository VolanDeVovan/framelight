//! The hub: one thread that owns every viewer's peer connection.
//!
//! Everything reaches it as a [`Msg`] on one channel (datagrams from the UDP thread,
//! frames from the video pump, packets from the audio pump, requests from HTTP), so it
//! sleeps in a single blocking `recv` and nothing in here polls.
//!
//! The pumps run the sources only while somebody watches. They take their orders from
//! [`Want`]: which profile to encode, at what bitrate, and whether a keyframe is due.

use crate::adapt::Bitrate;
use crate::h264::{self, Sps};
use crate::{SourceState, Status};
use anyhow::{Context, Result};
use fl_core::{AudioPacket, AudioSource, EncodedFrame, H264Profile, StreamConfig, VideoSource};
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use str0m::format::Codec;
use str0m::media::{MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Event, IceConnectionState, Input, Output, Rtc};

/// Keep the sources open this long after the last viewer left: a page reload or a
/// reconnect then finds the stream running.
const LINGER: Duration = Duration::from_secs(5);
/// A viewer that has not completed ICE and DTLS by then is gone.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// A browser that is still there sends feedback several times a second. One that was
/// killed or lost its network just goes quiet: stop feeding it video at once (the
/// megabits would only crowd the link for everyone else), and forget it a little later.
const PAUSE_AFTER: Duration = Duration::from_millis(1500);
const SILENCE_TIMEOUT: Duration = Duration::from_secs(6);
/// Forced keyframes are large; viewers asking in a burst share one.
const KEYFRAME_MIN_GAP: Duration = Duration::from_millis(300);
/// A connected viewer still without a picture asks again after this long.
const KEYFRAME_RETRY: Duration = Duration::from_secs(1);
/// How long a pump waits before it starts a failed source again.
const RESTART_DELAY: Duration = Duration::from_secs(2);
/// Audio timestamps follow the sound card's clock; re-anchor when it has drifted this
/// far from ours, so lip sync cannot wander off over a long session.
const AUDIO_MAX_SKEW_US: u64 = 80_000;

pub(crate) enum Msg {
    Udp(Vec<u8>, SocketAddr),
    Video(EncodedFrame),
    Audio(AudioPacket),
    Source(SourceState),
    Join(Box<Viewer>),
    Leave(u64),
    Ceiling(u32),
    Shutdown,
}

/// What the hub wants from the pumps.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Want {
    active: bool,
    shutdown: bool,
    profile: H264Profile,
    bitrate_kbps: u32,
    keyframe: bool,
}

struct Control {
    want: Mutex<Want>,
    wake: Condvar,
}

impl Control {
    fn update(&self, f: impl FnOnce(&mut Want)) {
        let mut w = self.want.lock().unwrap();
        let before = *w;
        f(&mut w);
        if *w != before {
            self.wake.notify_all();
        }
    }

    /// Sleeps until `done` holds; `None` when asked to shut down.
    fn wait_until(&self, done: impl Fn(&Want) -> bool) -> Option<MutexGuard<'_, Want>> {
        let mut w = self.want.lock().unwrap();
        loop {
            if w.shutdown {
                return None;
            }
            if done(&w) {
                return Some(w);
            }
            w = self.wake.wait(w).unwrap();
        }
    }

    /// Sleeps for `d`, or less if the pump is told to stop.
    fn pause(&self, d: Duration) {
        let w = self.want.lock().unwrap();
        let _ = self
            .wake
            .wait_timeout_while(w, d, |w| w.active && !w.shutdown)
            .unwrap();
    }
}

pub(crate) fn tune_socket(socket: &UdpSocket) {
    // A keyframe leaves as a burst of a few hundred datagrams per viewer.
    let size: libc::c_int = 2 << 20;
    // SAFETY: plain setsockopt with a valid int.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &size as *const _ as *const libc::c_void,
            std::mem::size_of_val(&size) as libc::socklen_t,
        );
    }
}

pub(crate) fn spawn(
    socket: UdpSocket,
    stream: StreamConfig,
    video: Box<dyn VideoSource>,
    audio: Option<Box<dyn AudioSource>>,
    status: Arc<Mutex<Status>>,
    stop: Arc<AtomicBool>,
) -> Result<(Sender<Msg>, Vec<JoinHandle<()>>, OwnedFd)> {
    let (tx, rx) = channel();
    // The hub must never block in a send, and the receive thread sleeps in poll().
    socket
        .set_nonblocking(true)
        .context("make the UDP socket non-blocking")?;
    // SAFETY: plain eventfd(2); the result is checked and owned from here on.
    let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    anyhow::ensure!(wake >= 0, "eventfd: {}", std::io::Error::last_os_error());
    // SAFETY: a fresh descriptor nobody else owns.
    let wake = unsafe { OwnedFd::from_raw_fd(wake) };
    let wake_reader = wake.try_clone().context("clone the wake descriptor")?;
    let ctl = Arc::new(Control {
        want: Mutex::new(Want {
            active: false,
            shutdown: false,
            profile: stream.profile,
            bitrate_kbps: stream.bitrate_kbps,
            keyframe: false,
        }),
        wake: Condvar::new(),
    });
    let now = Instant::now();
    let hub = Hub {
        socket: socket.try_clone().context("clone the UDP socket")?,
        rx,
        ctl: ctl.clone(),
        status,
        viewers: Vec::new(),
        epoch: now,
        last_video_us: 0,
        audio_clock: Clock::new(AUDIO_MAX_SKEW_US),
        rate: Bitrate::new(stream.bitrate_kbps, now),
        stream_sps: None,
        source: SourceState::Idle,
        last_key_request: now,
        idle_at: None,
        unusable_keyframes: 0,
    };
    let mut threads = Vec::new();
    let named = |name: &str| std::thread::Builder::new().name(name.into());
    threads.push(named("fl-hub").spawn(move || hub.run())?);
    {
        let (tx, stop) = (tx.clone(), stop.clone());
        threads.push(named("fl-udp").spawn(move || udp_thread(socket, wake_reader, tx, stop))?);
    }
    {
        let (tx, ctl) = (tx.clone(), ctl.clone());
        threads.push(named("fl-video").spawn(move || video_pump(video, stream, &ctl, tx))?);
    }
    if let Some(audio) = audio {
        let tx = tx.clone();
        threads.push(named("fl-audio").spawn(move || audio_pump(audio, &ctl, tx))?);
    }
    Ok((tx, threads, wake))
}

/// Ends the UDP thread's sleep (after `stop` was set).
pub(crate) fn wake(fd: &OwnedFd) {
    let one = 1u64.to_ne_bytes();
    // SAFETY: writes 8 bytes from a valid buffer to an eventfd.
    unsafe {
        libc::write(
            fd.as_raw_fd(),
            one.as_ptr() as *const libc::c_void,
            one.len(),
        )
    };
}

fn udp_thread(socket: UdpSocket, wake: OwnedFd, tx: Sender<Msg>, stop: Arc<AtomicBool>) {
    let mut buf = vec![0u8; 2000];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two valid pollfds; no timeout, so this thread costs nothing while idle.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if ready < 0 {
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                tracing::warn!(error = %std::io::Error::last_os_error(), "UDP poll failed");
                std::thread::sleep(Duration::from_millis(100));
            }
            continue;
        }
        // Take everything that has arrived before sleeping again.
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, source)) => {
                    if tx.send(Msg::Udp(buf[..n].to_vec(), source)).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    // E.g. an ICMP error for a viewer that is gone; nothing to act on.
                    tracing::debug!(error = %e, "UDP receive failed");
                    break;
                }
            }
        }
    }
}

fn video_pump(mut src: Box<dyn VideoSource>, base: StreamConfig, ctl: &Control, tx: Sender<Msg>) {
    let report = |s: SourceState| tx.send(Msg::Source(s)).is_ok();
    loop {
        let Some(mut w) = ctl.wait_until(|w| w.active) else {
            return;
        };
        w.keyframe = false;
        let cfg = StreamConfig {
            profile: w.profile,
            bitrate_kbps: w.bitrate_kbps,
            ..base.clone()
        };
        drop(w);
        report(SourceState::Starting);
        let mut failure = src.start(&cfg).err();
        if failure.is_none() {
            report(SourceState::Running);
        }
        let mut bitrate = cfg.bitrate_kbps;
        while failure.is_none() {
            let want = {
                let mut w = ctl.want.lock().unwrap();
                let now = *w;
                w.keyframe = false;
                now
            };
            if want.shutdown || !want.active || want.profile != cfg.profile {
                break;
            }
            if want.bitrate_kbps != bitrate {
                bitrate = want.bitrate_kbps;
                if let Err(e) = src.set_bitrate(bitrate) {
                    // Not an error to show anyone: starting again applies it too.
                    tracing::info!(error = %format!("{e:#}"), "bitrate change needs a restart of the source");
                    break;
                }
            }
            if want.keyframe {
                src.request_keyframe();
            }
            match src.next_frame() {
                Ok(frame) => {
                    if tx.send(Msg::Video(frame)).is_err() {
                        src.stop();
                        return;
                    }
                }
                Err(e) => failure = Some(e),
            }
        }
        src.stop();
        match failure {
            Some(e) => {
                let e = format!("{e:#}");
                tracing::warn!(error = %e, "video source failed; retrying while viewers stay");
                report(SourceState::Failed(e));
                ctl.pause(RESTART_DELAY);
            }
            None => {
                report(SourceState::Idle);
            }
        }
    }
}

fn audio_pump(mut src: Box<dyn AudioSource>, ctl: &Control, tx: Sender<Msg>) {
    loop {
        if ctl.wait_until(|w| w.active).is_none() {
            return;
        }
        let mut result = src.start();
        while result.is_ok() {
            {
                let w = ctl.want.lock().unwrap();
                if w.shutdown || !w.active {
                    break;
                }
            }
            result = src.next_packet().map(|p| {
                let _ = tx.send(Msg::Audio(p));
            });
        }
        src.stop();
        if let Err(e) = result {
            tracing::warn!(error = %format!("{e:#}"), "audio source failed; retrying while viewers stay");
            ctl.pause(RESTART_DELAY);
        }
    }
}

/// Maps the audio source's timestamps (a packet count, restarting from zero with the
/// source and paced by the sound card) onto the media timeline, which only moves
/// forward: anchored to the arrival of a packet, and anchored again when the two
/// clocks have drifted more than `max_skew_us` apart.
struct Clock {
    offset_us: i64,
    last_pts: Option<u64>,
    last_out: u64,
    max_skew_us: u64,
}

impl Clock {
    fn new(max_skew_us: u64) -> Self {
        Self {
            offset_us: 0,
            last_pts: None,
            last_out: 0,
            max_skew_us,
        }
    }

    fn map(&mut self, pts_us: u64, now_us: u64) -> u64 {
        let mapped = pts_us as i64 + self.offset_us;
        let restarted = self.last_pts.is_none_or(|last| pts_us < last);
        let skewed = mapped.abs_diff(now_us as i64) > self.max_skew_us;
        if restarted || skewed {
            self.offset_us = now_us as i64 - pts_us as i64;
        }
        self.last_pts = Some(pts_us);
        let out = ((pts_us as i64 + self.offset_us).max(0) as u64).max(self.last_out + 1);
        self.last_out = out;
        out
    }
}

pub(crate) struct Viewer {
    id: u64,
    rtc: Rtc,
    peer: SocketAddr,
    /// Our candidate: the address this viewer's datagrams arrive on.
    local: SocketAddr,
    /// The best profile its offer listed.
    max_profile: H264Profile,
    joined: Instant,
    video: Option<Mid>,
    audio: Option<Mid>,
    /// Negotiated H.264 payload types (packetization-mode 1 preferred) by profile.
    video_pts: Vec<(H264Profile, Pt)>,
    audio_pt: Option<Pt>,
    connected: bool,
    /// Has been sent a keyframe, so everything after it is decodable.
    started: bool,
    /// Went quiet: gets no video until it is heard from again.
    paused: bool,
    /// Latest loss fraction from its receiver reports (zero while little is sent).
    loss: f32,
    /// When its last datagram arrived.
    heard: Instant,
    /// Video bytes sent as of the previous report, and when that was.
    bytes_sent: u64,
    stats_at: Instant,
}

impl Viewer {
    pub(crate) fn new(
        id: u64,
        rtc: Rtc,
        peer: SocketAddr,
        local: SocketAddr,
        max_profile: H264Profile,
        now: Instant,
    ) -> Self {
        Self {
            id,
            rtc,
            peer,
            local,
            max_profile,
            joined: now,
            video: None,
            audio: None,
            video_pts: Vec::new(),
            audio_pt: None,
            connected: false,
            started: false,
            paused: false,
            loss: 0.0,
            heard: now,
            bytes_sent: 0,
            stats_at: now,
        }
    }

    fn learn_media(&mut self, mid: Mid, kind: MediaKind) {
        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };
        match kind {
            MediaKind::Video => {
                let mut found: Vec<(H264Profile, Pt, bool)> = Vec::new();
                for p in writer.payload_params() {
                    let spec = p.spec();
                    let profile = spec.format.profile_level_id.and_then(h264::profile_of);
                    let (Codec::H264, Some(profile)) = (spec.codec, profile) else {
                        continue;
                    };
                    let mode1 = spec.format.packetization_mode == Some(1);
                    match found.iter_mut().find(|f| f.0 == profile) {
                        Some(f) if mode1 && !f.2 => *f = (profile, p.pt(), mode1),
                        Some(_) => {}
                        None => found.push((profile, p.pt(), mode1)),
                    }
                }
                self.video_pts = found.into_iter().map(|f| (f.0, f.1)).collect();
                self.video = Some(mid);
            }
            MediaKind::Audio => {
                self.audio_pt = writer
                    .payload_params()
                    .find(|p| p.spec().codec == Codec::Opus)
                    .map(|p| p.pt());
                self.audio = Some(mid);
            }
        }
    }

    /// The payload type for a stream of `profile`: the exact one, else the best this
    /// viewer negotiated (a decoder for a higher profile also takes the lower ones).
    fn video_pt(&self, profile: H264Profile) -> Option<Pt> {
        pick_pt(&self.video_pts, profile)
    }
}

fn pick_pt<T: Copy>(pts: &[(H264Profile, T)], profile: H264Profile) -> Option<T> {
    pts.iter()
        .find(|p| p.0 == profile)
        .or_else(|| pts.iter().max_by_key(|p| p.0))
        .map(|p| p.1)
}

/// What a viewer's peer connection reported, reduced to what the hub acts on.
enum Happened {
    Connected,
    Disconnected,
    Media(Mid, MediaKind),
    KeyframeRequest,
    /// From a receiver report: fraction lost, and bytes sent so far.
    Loss(Mid, f32, u64),
}

struct Hub {
    socket: UdpSocket,
    rx: Receiver<Msg>,
    ctl: Arc<Control>,
    status: Arc<Mutex<Status>>,
    viewers: Vec<Viewer>,
    /// Zero of the media timeline.
    epoch: Instant,
    /// Media time of the last video frame, to keep it strictly increasing.
    last_video_us: u64,
    audio_clock: Clock,
    rate: Bitrate,
    /// From the latest keyframe: what is actually on the wire.
    stream_sps: Option<Sps>,
    source: SourceState,
    last_key_request: Instant,
    /// When to stop the sources, once the last viewer has left.
    idle_at: Option<Instant>,
    unusable_keyframes: u32,
}

impl Hub {
    fn run(mut self) {
        loop {
            let now = Instant::now();
            let deadline = self.drive(now);
            let first = if self.viewers.is_empty() && self.idle_at.is_none() {
                // Nobody watches and the sources are stopped: sleep until something happens.
                self.rx.recv().ok()
            } else {
                match self
                    .rx
                    .recv_timeout(deadline.saturating_duration_since(now))
                {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            };
            let Some(first) = first else {
                if self.viewers.is_empty() && self.idle_at.is_none() {
                    break;
                }
                self.tick(Instant::now());
                continue;
            };
            // Take what else is already queued before driving the connections again.
            let mut next = Some(first);
            let mut budget = 64;
            while let Some(msg) = next.take() {
                if !self.handle(msg) {
                    self.close();
                    return;
                }
                budget -= 1;
                if budget > 0 {
                    next = self.rx.try_recv().ok();
                }
            }
            self.tick(Instant::now());
        }
        self.close();
    }

    fn close(&mut self) {
        for v in &mut self.viewers {
            v.rtc.disconnect();
        }
        self.viewers.clear();
        self.ctl.update(|w| {
            w.shutdown = true;
            w.active = false;
        });
        self.publish();
    }

    /// Sends what the connections have to send and collects what happened to them.
    /// Returns when the hub must wake up next at the latest.
    fn drive(&mut self, now: Instant) -> Instant {
        let mut deadline = now + Duration::from_secs(1);
        let mut happened: Vec<(u64, Happened)> = Vec::new();
        for v in &mut self.viewers {
            while v.rtc.is_alive() {
                match v.rtc.poll_output() {
                    Ok(Output::Transmit(t)) => {
                        // A full socket buffer or a vanished route drops the packet, as
                        // the network would; NACK repairs what matters.
                        let _ = self.socket.send_to(&t.contents, t.destination);
                    }
                    Ok(Output::Timeout(t)) => {
                        deadline = deadline.min(t);
                        break;
                    }
                    Ok(Output::Event(e)) => {
                        let what = match e {
                            Event::Connected => Happened::Connected,
                            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                                Happened::Disconnected
                            }
                            Event::MediaAdded(m) => Happened::Media(m.mid, m.kind),
                            Event::KeyframeRequest(_) => Happened::KeyframeRequest,
                            Event::MediaEgressStats(s) => match s.loss {
                                Some(loss) => Happened::Loss(s.mid, loss, s.bytes),
                                None => continue,
                            },
                            _ => continue,
                        };
                        happened.push((v.id, what));
                    }
                    Err(e) => {
                        tracing::info!(viewer = v.id, error = %e, "viewer connection failed");
                        v.rtc.disconnect();
                    }
                }
            }
        }
        let mut membership_changed = false;
        for (id, what) in happened {
            let Some(v) = self.viewers.iter_mut().find(|v| v.id == id) else {
                continue;
            };
            match what {
                Happened::Connected => {
                    v.connected = true;
                    v.heard = now;
                    tracing::info!(
                        viewer = id, peer = %v.peer, profile = ?v.max_profile,
                        connect_ms = now.duration_since(v.joined).as_millis() as u64,
                        "viewer connected"
                    );
                    membership_changed = true;
                    self.request_keyframe(now, true);
                }
                Happened::Disconnected => v.rtc.disconnect(),
                Happened::Media(mid, kind) => v.learn_media(mid, kind),
                Happened::KeyframeRequest => self.request_keyframe(now, false),
                Happened::Loss(mid, loss, bytes) => {
                    if v.video == Some(mid) {
                        // A still picture is a trickle of packets, and one lost packet
                        // of twenty says nothing about what the link can carry.
                        let kbps = (bytes.saturating_sub(v.bytes_sent) * 8 / 1000) as f32
                            / now.duration_since(v.stats_at).as_secs_f32().max(0.1);
                        (v.bytes_sent, v.stats_at) = (bytes, now);
                        v.loss = if kbps * 3.0 >= self.rate.current() as f32 {
                            loss
                        } else {
                            0.0
                        };
                        self.adapt(now);
                    }
                }
            }
        }
        let before = self.viewers.len();
        self.viewers.retain(|v| {
            let late = !v.connected && now.duration_since(v.joined) > CONNECT_TIMEOUT;
            let silent = v.connected && now.duration_since(v.heard) > SILENCE_TIMEOUT;
            if late {
                tracing::info!(viewer = v.id, peer = %v.peer, "viewer never connected; dropped");
            } else if silent {
                tracing::info!(viewer = v.id, peer = %v.peer, "viewer went silent; dropped");
            } else if !v.rtc.is_alive() {
                tracing::info!(viewer = v.id, peer = %v.peer, "viewer left");
            }
            v.rtc.is_alive() && !late && !silent
        });
        if self.viewers.len() != before || membership_changed {
            self.membership(now);
        }
        if let Some(idle_at) = self.idle_at {
            if now >= idle_at {
                self.idle_at = None;
                self.ctl.update(|w| w.active = false);
                // The next audience starts from the ceiling again.
                let kbps = self.rate.set_ceiling(self.rate.ceiling(), now);
                self.set_bitrate(kbps);
            } else {
                deadline = deadline.min(idle_at);
            }
        }
        deadline
    }

    /// The set of viewers changed: decide whether the sources run and in which profile.
    fn membership(&mut self, now: Instant) {
        if self.viewers.is_empty() {
            self.idle_at.get_or_insert(now + LINGER);
        } else {
            self.idle_at = None;
            // One encode for everyone: the profile every viewer can decode.
            let profile = self
                .viewers
                .iter()
                .map(|v| v.max_profile)
                .min()
                .unwrap_or(H264Profile::High);
            self.ctl.update(|w| {
                w.active = true;
                w.profile = profile;
            });
        }
        self.publish();
    }

    fn request_keyframe(&mut self, now: Instant, force: bool) {
        if force || now.duration_since(self.last_key_request) >= KEYFRAME_MIN_GAP {
            self.last_key_request = now;
            self.ctl.update(|w| w.keyframe = true);
        }
    }

    fn adapt(&mut self, now: Instant) {
        let worst = self
            .viewers
            .iter()
            .filter(|v| v.connected && !v.paused)
            .map(|v| v.loss)
            .fold(0.0, f32::max);
        if let Some(kbps) = self.rate.on_loss(worst, now) {
            tracing::info!(kbps, worst_loss = %format!("{:.1}%", worst * 100.0), "bitrate adapted");
            self.set_bitrate(kbps);
        }
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.ctl.update(|w| w.bitrate_kbps = kbps);
        self.publish();
    }

    fn publish(&self) {
        let mut s = self.status.lock().unwrap();
        s.viewers = self.viewers.iter().filter(|v| v.connected).count();
        s.source = self.source.clone();
        s.bitrate_kbps = self.rate.current();
        s.ceiling_kbps = self.rate.ceiling();
        s.profile_level_id = self.stream_sps.map(|sps| sps.profile_level_id());
    }

    /// Moves time forward in every connection.
    fn tick(&mut self, now: Instant) {
        for v in &mut self.viewers {
            if v.rtc.is_alive() && v.rtc.handle_input(Input::Timeout(now)).is_err() {
                v.rtc.disconnect();
            }
        }
        for v in &mut self.viewers {
            if v.connected && !v.paused && now.duration_since(v.heard) > PAUSE_AFTER {
                tracing::info!(viewer = v.id, "viewer went quiet; video paused for it");
                v.paused = true;
            }
        }
        let waiting = self
            .viewers
            .iter()
            .any(|v| v.connected && !v.started && !v.paused);
        if waiting && now.duration_since(self.last_key_request) >= KEYFRAME_RETRY {
            self.request_keyframe(now, true);
        }
    }

    /// `false` ends the hub.
    fn handle(&mut self, msg: Msg) -> bool {
        let now = Instant::now();
        match msg {
            Msg::Udp(data, source) => {
                for v in &mut self.viewers {
                    let Ok(receive) = Receive::new(Protocol::Udp, source, v.local, &data) else {
                        break;
                    };
                    let input = Input::Receive(now, receive);
                    if v.rtc.accepts(&input) {
                        v.heard = now;
                        if v.paused {
                            tracing::info!(
                                viewer = v.id,
                                "viewer is back; video resumes at the next keyframe"
                            );
                            (v.paused, v.started) = (false, false);
                        }
                        if let Err(e) = v.rtc.handle_input(input) {
                            tracing::info!(viewer = v.id, error = %e, "viewer connection failed");
                            v.rtc.disconnect();
                        }
                        break;
                    }
                }
            }
            Msg::Video(frame) => self.video(frame, now),
            Msg::Audio(packet) => self.audio(packet, now),
            Msg::Source(state) => {
                if state == SourceState::Running {
                    tracing::info!("video source running");
                }
                self.source = state;
                self.publish();
            }
            Msg::Join(viewer) => {
                tracing::info!(viewer = viewer.id, peer = %viewer.peer, "viewer joining");
                self.viewers.push(*viewer);
                self.membership(now);
            }
            Msg::Leave(id) => {
                if let Some(v) = self.viewers.iter_mut().find(|v| v.id == id) {
                    v.rtc.disconnect();
                }
            }
            Msg::Ceiling(kbps) => {
                let kbps = self.rate.set_ceiling(kbps, now);
                tracing::info!(kbps, "bitrate ceiling set from the page");
                self.set_bitrate(kbps);
            }
            Msg::Shutdown => return false,
        }
        true
    }

    fn video(&mut self, frame: EncodedFrame, now: Instant) {
        // Only keyframes are parsed, and only up to their first slice.
        let key = if frame.keyframe {
            h264::inspect(&frame.data)
        } else {
            h264::KeyInfo::default()
        };
        let startable = key.idr && key.sps.is_some();
        if let Some(sps) = key.sps
            && self.stream_sps != Some(sps)
        {
            tracing::info!(
                profile_level_id = %format!("{:06x}", sps.profile_level_id()),
                "stream parameters (from the SPS)"
            );
            self.stream_sps = Some(sps);
            self.publish();
        }
        if frame.keyframe && !startable {
            self.unusable_keyframes += 1;
            if self.unusable_keyframes.is_power_of_two() {
                tracing::warn!(
                    count = self.unusable_keyframes,
                    "keyframe without an IDR slice or parameter sets: new viewers cannot start on it"
                );
            }
        }
        let Some(profile) = self.stream_sps.and_then(|s| s.profile()) else {
            return;
        };
        // Media time is capture time, so the viewers' clocks (RTCP sender reports) tie
        // each frame to the moment it was taken, across restarts of the source.
        let captured = frame
            .captured
            .saturating_duration_since(self.epoch)
            .as_micros() as u64;
        let t = captured.max(self.last_video_us + 1);
        self.last_video_us = t;
        let wallclock = self.epoch + Duration::from_micros(t);
        // One copy of the frame, shared by every viewer's packetizer.
        let data: Arc<[u8]> = Arc::from(&frame.data[..]);
        for v in &mut self.viewers {
            if !v.connected || v.paused || (!v.started && !startable) {
                continue;
            }
            let (Some(mid), Some(pt)) = (v.video, v.video_pt(profile)) else {
                continue;
            };
            let Some(writer) = v.rtc.writer(mid) else {
                continue;
            };
            match writer.write(pt, wallclock, MediaTime::from_micros(t), data.clone()) {
                Ok(()) => {
                    if !v.started {
                        v.started = true;
                        tracing::info!(
                            viewer = v.id,
                            since_join_ms = now.duration_since(v.joined).as_millis() as u64,
                            "first keyframe sent"
                        );
                    }
                }
                Err(e) => tracing::debug!(viewer = v.id, error = %e, "video write failed"),
            }
        }
    }

    fn audio(&mut self, packet: AudioPacket, now: Instant) {
        let t = self.audio_clock.map(
            packet.pts_us,
            now.duration_since(self.epoch).as_micros() as u64,
        );
        let wallclock = self.epoch + Duration::from_micros(t);
        let data: Arc<[u8]> = Arc::from(&packet.data[..]);
        for v in &mut self.viewers {
            let (true, Some(mid), Some(pt)) = (v.connected, v.audio, v.audio_pt) else {
                continue;
            };
            let Some(writer) = v.rtc.writer(mid) else {
                continue;
            };
            if let Err(e) = writer.write(pt, wallclock, MediaTime::from_micros(t), data.clone()) {
                tracing::debug!(viewer = v.id, error = %e, "audio write failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_survives_source_restarts_and_never_goes_back() {
        let mut c = Clock::new(80_000);
        assert_eq!(c.map(0, 1_000_000), 1_000_000);
        assert_eq!(
            c.map(20_000, 1_030_000),
            1_020_000,
            "keeps the source's spacing, not arrival jitter"
        );
        assert_eq!(c.map(40_000, 1_041_000), 1_040_000);
        // The source restarted: its timestamps begin again.
        assert_eq!(c.map(0, 5_000_000), 5_000_000);
        assert_eq!(c.map(20_000, 5_030_000), 5_020_000);
        // Equal timestamps still move forward.
        assert_eq!(c.map(20_000, 5_030_000), 5_020_001);
    }

    #[test]
    fn audio_clock_reanchors_when_it_drifts() {
        let mut c = Clock::new(80_000);
        assert_eq!(c.map(0, 1_000_000), 1_000_000);
        assert_eq!(c.map(20_000, 1_025_000), 1_020_000);
        // The sound card's clock fell 100 ms behind ours.
        assert_eq!(c.map(40_000, 1_140_000), 1_140_000);
        assert_eq!(c.map(60_000, 1_161_000), 1_160_000);
    }

    #[test]
    fn payload_type_follows_the_stream_profile() {
        use H264Profile::*;
        let chrome = [(ConstrainedBaseline, 102), (Main, 106), (High, 108)];
        assert_eq!(pick_pt(&chrome, High), Some(108));
        assert_eq!(pick_pt(&chrome, ConstrainedBaseline), Some(102));
        // A viewer that only listed High still gets a stream others forced down to Main.
        assert_eq!(pick_pt(&[(High, 108)], Main), Some(108));
        assert_eq!(pick_pt::<u8>(&[], High), None);
    }
}
