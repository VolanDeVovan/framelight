//! A [`VideoSource`] backed by an `ffmpeg` child process: `testsrc2` plus a wall clock,
//! stream time and frame counter, encoded with libx264 (ultrafast, zerolatency, no
//! B-frames, parameter sets on every IDR) to Annex-B on stdout. AUD NALs (inserted by a
//! bitstream filter) delimit access units. Keyframes: periodic, every second (GOP = fps),
//! which answers keyframe requests within a second; ffmpeg is restarted only if no
//! keyframe shows up at all. A bitrate change is a restart too.

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use fl_core::{Codec, EncodedFrame, H264Profile, StreamConfig, VideoSource};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct FfmpegSource {
    cfg: Option<StreamConfig>,
    child: Option<Child>,
    reader: Option<JoinHandle<()>>,
    rx: Option<Receiver<(Vec<u8>, bool)>>,
    epoch: Instant,
    last_restart: Instant,
    keyframe_pending: bool,
    last_key: Instant,
    frame_index: u32,
}

impl FfmpegSource {
    pub fn new() -> Self {
        FfmpegSource {
            cfg: None,
            child: None,
            reader: None,
            rx: None,
            epoch: Instant::now(),
            last_restart: Instant::now() - Duration::from_secs(10),
            keyframe_pending: false,
            last_key: Instant::now(),
            frame_index: 0,
        }
    }

    fn spawn(&mut self) -> Result<()> {
        let cfg = self.cfg.clone().context("not started")?;
        let (w, h, fps) = (cfg.width, cfg.height, cfg.fps.max(1));
        let kbps = cfg.bitrate_kbps.max(500);
        let gop = fps.to_string();
        let font = (h / 12).max(16);
        let small = (h / 24).max(12);
        if cfg.codec != Codec::H264 {
            bail!("the test pattern is H.264 only");
        }
        let profile = match cfg.profile {
            H264Profile::ConstrainedBaseline => "baseline",
            H264Profile::Main => "main",
            H264Profile::High => "high",
        };
        let vf = format!(
            "testsrc2=size={w}x{h}:rate={fps},\
             drawtext=text='%{{localtime\\:%X}}':fontsize={font}:fontcolor=white:box=1:boxcolor=black@0.6:boxborderw=12:x=(w-tw)/2:y=h/2-th,\
             drawtext=text='stream %{{pts\\:hms}}  frame %{{n}}  {w}x{h}@{fps} {profile} {kbps}k':fontsize={small}:fontcolor=yellow:box=1:boxcolor=black@0.6:boxborderw=8:x=(w-tw)/2:y=h/2+{font}"
        );
        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-re",
            "-f",
            "lavfi",
            "-i",
            &vf,
        ]);
        cmd.args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-profile:v",
            profile,
            "-x264-params",
            "repeat-headers=1",
            "-bsf:v",
            "h264_metadata=aud=insert",
            "-f",
            "h264",
        ]);
        cmd.args(["-pix_fmt", "yuv420p", "-g", &gop, "-bf", "0"])
            .args(["-b:v", &format!("{kbps}k"), "-maxrate", &format!("{kbps}k")])
            .args(["-bufsize", &format!("{}k", (kbps / fps).max(100) * 2)])
            .args(["-flush_packets", "1", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().context("spawn ffmpeg (is it on PATH?)")?;
        let stdout = child.stdout.take().context("ffmpeg stdout")?;
        let (tx, rx) = sync_channel(4);
        let reader = std::thread::Builder::new()
            .name("ffmpeg-reader".into())
            .spawn(move || read_access_units(stdout, tx))?;
        tracing::info!(w, h, fps, kbps, profile, "ffmpeg test pattern started");
        self.child = Some(child);
        self.reader = Some(reader);
        self.rx = Some(rx);
        self.last_restart = Instant::now();
        Ok(())
    }

    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.rx = None;
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

impl VideoSource for FfmpegSource {
    fn start(&mut self, cfg: &StreamConfig) -> Result<()> {
        self.kill();
        self.cfg = Some(cfg.clone());
        self.epoch = Instant::now();
        self.frame_index = 0;
        self.keyframe_pending = false;
        self.spawn()
    }

    fn next_frame(&mut self) -> Result<EncodedFrame> {
        // Restart-on-request is only a fallback for when periodic IDRs stall.
        if self.keyframe_pending
            && self.last_restart.elapsed() >= Duration::from_secs(3)
            && self.last_key.elapsed() >= Duration::from_secs(2)
        {
            self.keyframe_pending = false;
            tracing::info!("keyframe requested and none arrived: restarting the encoder");
            self.kill();
            self.spawn()?;
        }
        let rx = self
            .rx
            .as_ref()
            .ok_or_else(|| anyhow!("source not started"))?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok((data, keyframe)) => {
                if keyframe {
                    self.keyframe_pending = false;
                    self.last_key = Instant::now();
                }
                self.frame_index = self.frame_index.wrapping_add(1);
                Ok(EncodedFrame {
                    data: Bytes::from(data),
                    keyframe,
                    captured: Instant::now(),
                    pts_us: self.epoch.elapsed().as_micros() as u64,
                    frame_index: self.frame_index,
                })
            }
            Err(RecvTimeoutError::Timeout) => bail!("ffmpeg produced no frame for 5 s"),
            Err(RecvTimeoutError::Disconnected) => bail!("ffmpeg exited"),
        }
    }

    fn request_keyframe(&mut self) {
        if !self.keyframe_pending {
            tracing::info!("keyframe requested (next periodic IDR within 1 s)");
        }
        self.keyframe_pending = true;
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        bail!("ffmpeg takes its bitrate ({kbps} kbit/s asked) on the command line only")
    }

    fn stop(&mut self) {
        self.kill();
        tracing::info!("ffmpeg test pattern stopped");
    }
}

/// Split the Annex-B byte stream into access units at AUD NALs.
fn read_access_units(mut r: impl Read, tx: SyncSender<(Vec<u8>, bool)>) {
    let is_aud = |b: u8| b & 0x1f == 9;
    let is_irap = |b: u8| b & 0x1f == 5;
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut chunk = vec![0u8; 256 * 1024];
    // Start of the current AU in `buf`, and how far we have scanned for start codes.
    let mut scanned = 0usize;
    loop {
        let n = match r.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        // Find AUD start codes after position 0; each one closes the AU before it.
        let mut i = scanned.max(1);
        let mut cut_points = Vec::new();
        while i + 3 < buf.len() {
            if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 && is_aud(buf[i + 3]) {
                let start = if i > 0 && buf[i - 1] == 0 { i - 1 } else { i };
                if start > 0 {
                    cut_points.push(start);
                }
                i += 3;
            } else {
                i += 1;
            }
        }
        scanned = buf.len().saturating_sub(3);
        let mut consumed = 0;
        for cut in cut_points {
            let au = buf[consumed..cut].to_vec();
            consumed = cut;
            let key = has_nal(&au, is_irap);
            if tx.send((au, key)).is_err() {
                return;
            }
        }
        if consumed > 0 {
            buf.drain(..consumed);
            scanned = scanned.saturating_sub(consumed);
        }
    }
}

fn has_nal(au: &[u8], pred: impl Fn(u8) -> bool) -> bool {
    au.windows(4)
        .any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && pred(w[3]))
}
