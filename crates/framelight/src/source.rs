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

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fl_capture::openvr::OpenVrProvider;
use fl_capture::{
    CaptureInfo, HEADSET_VIEW, HeadsetCapture, SyntheticProvider, TargetLayout, TargetMode,
    ViewProvider,
};
use fl_core::{EncodedFrame, StreamConfig, VideoSource};
use fl_encode::{Encoder, EncoderConfig};
use tracing::{info, warn};

/// No encoded frame for this long while frames are queued means the encoder is stuck;
/// `next_frame` then fails and the caller starts the source again, reopening the device.
const ENCODER_STALL: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Default)]
pub struct SourceOptions {
    /// Opt-in dmabuf (zero-copy) path, falling back to mmap on failure. Off by default:
    /// on the Frame the encoder read GPU writes to its exported buffers late (stale
    /// 32-byte lines), so one CPU copy (~2% of a core at 1080p60) is the safe default.
    pub zero_copy: bool,
    /// GOP in frames; 0 = 60 s worth.
    pub gop: u32,
    /// Cyclic intra refresh period in frames (0 = off).
    pub intra_refresh: u32,
    /// Colour bars instead of the Headset View.
    pub test_pattern: bool,
    /// Synthetic moving picture instead of SteamVR (no headset runtime needed).
    pub synthetic: bool,
    /// Encoder node and coded-format override (tests against `vicodec`).
    pub device: Option<String>,
    pub coded_format: Option<u32>,
}


/// Per-frame numbers for the probe.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    pub capture: CaptureInfo,
    pub encode_time: Duration,
}

struct Running {
    // Field order matters: capture (with its dmabuf imports) drops before the encoder.
    cap: HeadsetCapture,
    enc: Encoder,
    period: Duration,
    start: Instant,
    next_tick: Instant,
    frame_index: u32,
    pending_capture: std::collections::VecDeque<(u64, CaptureInfo)>,
    last_vr_poll: Instant,
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

    pub fn mode(&self) -> Option<TargetMode> {
        self.run.as_ref().map(|r| r.cap.mode())
    }

    pub fn capture(&self) -> Option<&HeadsetCapture> {
        self.run.as_ref().map(|r| &r.cap)
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
        let mut ec =
            EncoderConfig::new(cfg.width, cfg.height, cfg.fps, cfg.bitrate_kbps, cfg.codec);
        if self.opts.gop > 0 {
            ec.gop = self.opts.gop;
        }
        ec.profile = cfg.profile;
        ec.intra_refresh = self.opts.intra_refresh;
        if let Some(d) = &self.opts.device {
            ec.device = d.clone();
        }
        ec.coded_format = self.opts.coded_format;
        let enc = Encoder::open(&ec).context("opening the encoder")?;
        let l = enc.layout();
        let layout = TargetLayout {
            width: l.width,
            height: l.height,
            stride: l.stride,
            rows: l.padded_height,
            uv_offset: l.uv_offset,
        };

        let provider = || -> Result<Box<dyn ViewProvider>> {
            Ok(if self.opts.synthetic {
                Box::new(SyntheticProvider::new(cfg.width, cfg.height))
            } else {
                Box::new(OpenVrProvider::connect(HEADSET_VIEW)?)
            })
        };
        let cap = if self.opts.zero_copy {
            let fds: Vec<_> = (0..enc.num_inputs())
                .map(|i| enc.input_dmabuf(i).as_fd())
                .collect();
            match HeadsetCapture::new(layout, Some(&fds), provider()?) {
                Ok(c) => c,
                Err(e) => {
                    warn!("zero-copy capture failed ({e:#}); falling back to one CPU copy");
                    HeadsetCapture::new(layout, None, provider()?)?
                }
            }
        } else {
            HeadsetCapture::new(layout, None, provider()?)?
        };
        let mut cap = cap;
        cap.test_pattern = self.opts.test_pattern;
        let period = Duration::from_nanos(1_000_000_000 / cfg.fps as u64);
        let now = Instant::now();
        info!(
            "headset source: {}x{} @{} {:?} {:?} {} kbit/s, {:?}",
            cfg.width,
            cfg.height,
            cfg.fps,
            cfg.codec,
            cfg.profile,
            cfg.bitrate_kbps,
            cap.mode()
        );
        self.run = Some(Running {
            cap,
            enc,
            period,
            start: now,
            next_tick: now,
            frame_index: 0,
            pending_capture: Default::default(),
            last_vr_poll: now,
            last_output: now,
            dropped_ticks: 0,
        });
        self.keyframe_requested = false;
        Ok(())
    }

    fn next_frame(&mut self) -> Result<EncodedFrame> {
        let r = self.run.as_mut().context("source not started")?;
        loop {
            if let Some(p) = r.enc.poll_packet(Duration::ZERO)? {
                return Ok(finish(r, &mut self.last_stats, p));
            }
            let now = Instant::now();
            if r.enc.in_flight() > 0 && now.duration_since(r.last_output) > ENCODER_STALL {
                bail!(
                    "encoder produced nothing for {:?} with {} frames queued",
                    ENCODER_STALL,
                    r.enc.in_flight()
                );
            }
            if now.duration_since(r.last_vr_poll) > Duration::from_secs(1) {
                r.last_vr_poll = now;
                if r.cap.provider_mut().quit_requested() {
                    bail!("SteamVR is quitting");
                }
            }
            if now >= r.next_tick {
                if let Some(i) = r.enc.free_input() {
                    let pts_us = r.next_tick.duration_since(r.start).as_micros() as u64;
                    let info = if r.cap.mode() == TargetMode::DmaBuf {
                        r.cap.capture(i, None)?
                    } else {
                        let dst = r.enc.input_map(i);
                        // The mapping outlives this call; split the borrow by pointer.
                        let dst =
                            unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr(), dst.len()) };
                        r.cap.capture(i, Some(dst))?
                    };
                    if self.keyframe_requested {
                        self.keyframe_requested = false;
                        r.enc.request_keyframe();
                    }
                    r.enc.queue_input(i, pts_us)?;
                    if r.pending_capture.len() >= 16 {
                        r.pending_capture.pop_front();
                    }
                    r.pending_capture.push_back((pts_us, info));
                } else {
                    r.dropped_ticks += 1;
                    if r.dropped_ticks.is_power_of_two() {
                        warn!(
                            "encoder input queue full, dropped {} frame ticks so far",
                            r.dropped_ticks
                        );
                    }
                }
                r.next_tick += r.period;
                if now > r.next_tick + r.period * 5 {
                    // Far behind (suspend, stall): resync rather than burst.
                    r.next_tick = now + r.period;
                }
                continue;
            }
            let wait = r.next_tick - now;
            if let Some(p) = r.enc.poll_packet(wait)? {
                return Ok(finish(r, &mut self.last_stats, p));
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
        if let Some(r) = self.run.take() {
            info!("headset source stopped after {} frames", r.frame_index);
        }
    }
}

fn finish(r: &mut Running, stats: &mut FrameStats, p: fl_encode::Packet) -> EncodedFrame {
    let mut capture = CaptureInfo::default();
    while let Some(&(ts, c)) = r.pending_capture.front() {
        if ts > p.pts_us {
            break;
        }
        r.pending_capture.pop_front();
        if ts == p.pts_us {
            capture = c;
            break;
        }
    }
    r.last_output = Instant::now();
    *stats = FrameStats {
        capture,
        encode_time: p.encode_time,
    };
    let frame = EncodedFrame {
        data: p.data,
        keyframe: p.keyframe,
        captured: r.start + Duration::from_micros(p.pts_us),
        pts_us: p.pts_us,
        frame_index: r.frame_index,
    };
    r.frame_index = r.frame_index.wrapping_add(1);
    frame
}
