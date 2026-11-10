//! `VideoSource` for the headset: Headset View capture + iris encode.
//!
//! One thread, no spinning: `next_frame` captures on each frame tick into a
//! free encoder input, and in between sleeps in `poll()` on the encoder fd,
//! so a finished frame is returned the moment the encoder hands it over.
//!
//! Everything (encoder, Vulkan device, OpenVR connection) is opened in `start`
//! and dropped in `stop`: nothing is held while nobody is watching.
//!
//! While the headset is in standby the compositor is paused and hands out no view;
//! the capture then writes black frames, so the stream (and every viewer's
//! connection) stays up and the picture returns by itself.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fl_capture::openvr::OpenVrProvider;
use fl_capture::{CaptureInfo, HEADSET_VIEW, HeadsetCapture, SyntheticProvider, ViewProvider};
use fl_core::{EncodedFrame, StreamConfig, VideoSource};
use fl_encode::{DEFAULT_DEVICE, Encoder, EncoderConfig};
use tracing::{info, warn};

/// No encoded frame for this long while frames are queued means the encoder is stuck;
/// `next_frame` then fails and the caller starts the source again, reopening the device.
const ENCODER_STALL: Duration = Duration::from_secs(3);
/// How often SteamVR is asked whether it wants us gone.
const QUIT_POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Default)]
pub struct SourceOptions {
    /// Synthetic moving picture instead of SteamVR (no headset runtime needed).
    pub synthetic: bool,
    /// Encoder node and coded format, when not the Frame's (`vicodec` in the self-test).
    pub device: Option<String>,
    pub coded_format: Option<u32>,
}

/// Numbers about the frame `next_frame` returned last, for the probe.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    pub capture: CaptureInfo,
    pub encode_time: Duration,
    /// Time since the source started.
    pub pts_us: u64,
}

struct Running {
    cap: HeadsetCapture,
    enc: Encoder,
    period: Duration,
    start: Instant,
    next_tick: Instant,
    frames: u64,
    /// Capture numbers of the frames in the encoder, by timestamp.
    pending_capture: VecDeque<(u64, CaptureInfo)>,
    last_quit_poll: Instant,
    /// When the encoder last produced a frame (or the source started).
    last_output: Instant,
    dropped_ticks: u32,
}

pub struct HeadsetSource {
    opts: SourceOptions,
    run: Option<Running>,
    keyframe_requested: bool,
    pub last_stats: FrameStats,
}

impl HeadsetSource {
    pub fn new(opts: SourceOptions) -> Self {
        Self {
            opts,
            run: None,
            keyframe_requested: false,
            last_stats: FrameStats::default(),
        }
    }
}

impl VideoSource for HeadsetSource {
    fn start(&mut self, cfg: &StreamConfig) -> Result<()> {
        self.stop();
        if cfg.fps == 0 || cfg.width == 0 || cfg.height == 0 {
            bail!("bad stream config {cfg:?}");
        }
        // Fail before touching the encoder: this is retried while viewers wait.
        if !self.opts.synthetic && !fl_capture::openvr::steamvr_running() {
            bail!("SteamVR is not running on the headset");
        }
        let enc = Encoder::open(&EncoderConfig {
            device: self
                .opts
                .device
                .clone()
                .unwrap_or_else(|| DEFAULT_DEVICE.into()),
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            bitrate_kbps: cfg.bitrate_kbps,
            profile: cfg.profile,
            coded_format: self.opts.coded_format,
        })
        .context("opening the encoder")?;
        let provider: Box<dyn ViewProvider> = if self.opts.synthetic {
            Box::new(SyntheticProvider::new(cfg.width, cfg.height))
        } else {
            Box::new(OpenVrProvider::connect(HEADSET_VIEW)?)
        };
        let cap = HeadsetCapture::new(enc.layout(), provider)?;
        let now = Instant::now();
        info!(
            "headset source: {}x{} @{} {:?} {} kbit/s",
            cfg.width, cfg.height, cfg.fps, cfg.profile, cfg.bitrate_kbps
        );
        self.run = Some(Running {
            cap,
            enc,
            period: Duration::from_nanos(1_000_000_000 / cfg.fps as u64),
            start: now,
            next_tick: now,
            frames: 0,
            pending_capture: VecDeque::new(),
            last_quit_poll: now,
            last_output: now,
            dropped_ticks: 0,
        });
        self.keyframe_requested = false;
        Ok(())
    }

    fn next_frame(&mut self) -> Result<EncodedFrame> {
        let r = self.run.as_mut().context("source not started")?;
        loop {
            let now = Instant::now();
            // Wait for the encoder until the next tick is due (not at all if it is).
            let wait = r.next_tick.saturating_duration_since(now);
            if let Some(p) = r.enc.poll_packet(wait)? {
                return Ok(r.finish(&mut self.last_stats, p));
            }
            let now = Instant::now();
            if r.enc.in_flight() > 0 && now.duration_since(r.last_output) > ENCODER_STALL {
                bail!(
                    "encoder produced nothing for {:?} with {} frames queued",
                    ENCODER_STALL,
                    r.enc.in_flight()
                );
            }
            if now.duration_since(r.last_quit_poll) > QUIT_POLL {
                r.last_quit_poll = now;
                if r.cap.provider_mut().quit_requested() {
                    bail!("SteamVR is quitting");
                }
            }
            if now < r.next_tick {
                continue;
            }
            match r.enc.free_input() {
                Some(i) => {
                    let pts_us = r.next_tick.duration_since(r.start).as_micros() as u64;
                    let info = r.cap.capture(r.enc.input_map(i))?;
                    if std::mem::take(&mut self.keyframe_requested) {
                        r.enc.request_keyframe();
                    }
                    r.enc.queue_input(i, pts_us)?;
                    if r.pending_capture.len() >= 16 {
                        r.pending_capture.pop_front();
                    }
                    r.pending_capture.push_back((pts_us, info));
                }
                None => {
                    r.dropped_ticks += 1;
                    if r.dropped_ticks.is_power_of_two() {
                        warn!(
                            "encoder input queue full, dropped {} frame ticks so far",
                            r.dropped_ticks
                        );
                    }
                }
            }
            r.next_tick += r.period;
            if now > r.next_tick + r.period * 5 {
                // Far behind (suspend, stall): resync rather than burst.
                r.next_tick = now + r.period;
            }
        }
    }

    fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        let r = self.run.as_mut().context("source not started")?;
        r.enc.set_bitrate(kbps)
    }

    fn stop(&mut self) {
        // Dropping `Running` closes the capture (Vulkan device, OpenVR) and the encoder.
        if let Some(r) = self.run.take() {
            info!("headset source stopped after {} frames", r.frames);
        }
    }
}

impl Running {
    fn finish(&mut self, stats: &mut FrameStats, p: fl_encode::Packet) -> EncodedFrame {
        let mut capture = CaptureInfo::default();
        while let Some(&(ts, c)) = self.pending_capture.front() {
            if ts > p.pts_us {
                break;
            }
            self.pending_capture.pop_front();
            if ts == p.pts_us {
                capture = c;
                break;
            }
        }
        self.last_output = Instant::now();
        self.frames += 1;
        *stats = FrameStats {
            capture,
            encode_time: p.encode_time,
            pts_us: p.pts_us,
        };
        EncodedFrame {
            data: p.data,
            keyframe: p.keyframe,
            captured: self.start + Duration::from_micros(p.pts_us),
        }
    }
}
