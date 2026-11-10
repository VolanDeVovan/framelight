//! The hub: one thread that owns every viewer's peer connection.
//!
//! Everything reaches it as a [`Msg`] on one channel (datagrams from the UDP thread,
//! frames from the video pump, packets from the audio pump, requests from HTTP), so it
//! sleeps in a single blocking `recv` and nothing in here polls.

use crate::adapt::Bitrate;
use crate::clock::Clock;
use crate::h264::{self, Sps};
use crate::pump::Control;
use crate::viewer::Viewer;
use crate::{SourceState, Status};
use fl_core::{AudioPacket, EncodedFrame, H264Profile};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use str0m::media::{MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Event, IceConnectionState, Input, Output};

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
/// Audio timestamps follow the sound card's clock; re-anchor when it has drifted this
/// far from ours, so lip sync cannot wander off over a long session.
const AUDIO_MAX_SKEW_US: u64 = 80_000;
/// Messages handled in a row before the connections are driven again.
const BATCH: usize = 64;

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

/// What a viewer's peer connection reported, reduced to what the hub acts on.
enum Happened {
    Connected,
    Disconnected,
    Media(Mid, MediaKind),
    KeyframeRequest,
    /// From a receiver report: fraction lost, and bytes sent so far.
    Loss(Mid, f32, u64),
}

pub(crate) struct Hub {
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
    /// `socket` must be non-blocking: the hub never waits in a send.
    pub fn new(
        socket: UdpSocket,
        rx: Receiver<Msg>,
        ctl: Arc<Control>,
        status: Arc<Mutex<Status>>,
        ceiling_kbps: u32,
    ) -> Self {
        let now = Instant::now();
        Self {
            socket,
            rx,
            ctl,
            status,
            viewers: Vec::new(),
            epoch: now,
            last_video_us: 0,
            audio_clock: Clock::new(AUDIO_MAX_SKEW_US),
            rate: Bitrate::new(ceiling_kbps, now),
            stream_sps: None,
            source: SourceState::Idle,
            last_key_request: now,
            idle_at: None,
            unusable_keyframes: 0,
        }
    }

    pub fn run(mut self) {
        'outer: loop {
            let now = Instant::now();
            let deadline = self.drive(now);
            let first = if self.viewers.is_empty() && self.idle_at.is_none() {
                // Nobody watches and the sources are stopped: sleep until something
                // happens.
                match self.rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => break,
                }
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
            // Take what else is already queued before driving the connections again.
            let mut next = first;
            for _ in 0..BATCH {
                let Some(msg) = next.take() else { break };
                if !self.handle(msg) {
                    break 'outer;
                }
                next = self.rx.try_recv().ok();
            }
            if let Some(msg) = next
                && !self.handle(msg)
            {
                break;
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
        self.ctl.shutdown();
        self.source = SourceState::Idle;
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
        // One encode for everyone: the profile every viewer can decode.
        match self.viewers.iter().map(|v| v.max_profile).min() {
            None => {
                self.idle_at.get_or_insert(now + LINGER);
            }
            Some(profile) => {
                self.idle_at = None;
                self.ctl.update(|w| {
                    w.active = true;
                    w.profile = profile;
                });
            }
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
            if v.connected && !v.paused && now.duration_since(v.heard) > PAUSE_AFTER {
                tracing::info!(viewer = v.id, "viewer went quiet; video paused for it");
                // Whatever it last reported about loss says nothing any more.
                (v.paused, v.loss) = (true, 0.0);
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
            Msg::Udp(data, source) => self.datagram(&data, source, now),
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

    /// Hands a datagram to the connection it belongs to.
    fn datagram(&mut self, data: &[u8], source: SocketAddr, now: Instant) {
        let mut resumed = false;
        for v in &mut self.viewers {
            let Ok(receive) = Receive::new(Protocol::Udp, source, v.local, data) else {
                // Not STUN, DTLS or RTP at all.
                return;
            };
            let input = Input::Receive(now, receive);
            if !v.rtc.accepts(&input) {
                continue;
            }
            v.heard = now;
            if v.paused {
                tracing::info!(
                    viewer = v.id,
                    "viewer is back; video resumes at the next keyframe"
                );
                (v.paused, v.started) = (false, false);
                resumed = true;
            }
            if let Err(e) = v.rtc.handle_input(input) {
                tracing::info!(viewer = v.id, error = %e, "viewer connection failed");
                v.rtc.disconnect();
            }
            break;
        }
        if resumed {
            self.request_keyframe(now, false);
        }
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
        // Nothing can be sent before the first SPS says what the stream is.
        let Some(sps) = self.stream_sps else {
            return;
        };
        let profile: Option<H264Profile> = sps.profile();
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
