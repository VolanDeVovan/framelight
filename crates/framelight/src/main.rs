//! framelight: show what the Steam Frame shows, in any browser on the network.
//!
//!   framelight app [serve options]
//!       The switch, as a small window to open on the headset: Enable / Disable and
//!       the address to type into a browser. Streaming lives exactly as long as it is
//!       enabled in this window. `pkill -USR1 -x framelight` presses the button
//!       from a shell.
//!   framelight serve [--port N] [--width W --height H --fps F] [--bitrate-kbps B]
//!                    [--audio monitor|off] [--nice N]
//!                    [--test-pattern] [--synthetic] [--zero-copy] [--gop N]
//!                    [--intra-refresh N] [--device /dev/videoN]
//!       The same streamer without a window (over ssh, for tests): enabled until it
//!       is stopped. --audio monitor (default) streams what the device plays.
//!       --nice (default 10) keeps the process below the game for CPU time.
//! Diagnostics:
//!   framelight probe --seconds N --codec h264|hevc --profile high|main|baseline
//!                    --width W --height H --fps F --bitrate-kbps B --out FILE
//!                    [--bitrate-step KBPS] [--zero-copy] [--gop N] [--intra-refresh N]
//!                    [--keyframe-every SECS] [--quiet]
//!                    [--synthetic] [--device /dev/videoN] [--coded FOURCC]
//!       Encodes to a file and reports timings. --bitrate-step switches to that
//!       bitrate halfway and reports the rate of each half.
//!   framelight vrtimings --seconds N     (compositor timing baseline)
//!   framelight selftest [--device /dev/videoN] [--coded FWHT|H264|HEVC]
//!                       [--width W --height H --frames N --out FILE] [--zero-copy]
//!       Synthetic picture -> GPU NV12 -> encoder input, checks the bytes in every
//!       input buffer against a CPU reference, then runs the stateful encode flow:
//!       keyframe request, drain, restart, drain.

mod source;
mod ui;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fl_capture::openvr::{FrameTiming, OpenVr};
use fl_capture::{HeadsetCapture, SyntheticProvider, TargetLayout, TargetMode};
use fl_core::{Codec, H264Profile, StreamConfig, VideoSource};
use fl_encode::{Encoder, EncoderConfig};
use source::{HeadsetSource, SourceOptions};
use tracing::info;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("app") => app(&args[1..]),
        Some("serve") => serve(&args[1..]),
        Some("probe") => probe(&args[1..]),
        Some("vrtimings") => vrtimings(&args[1..]),
        Some("selftest") => selftest(&args[1..]),
        _ => {
            eprintln!(
                "usage: framelight app|serve|probe|vrtimings|selftest [options] (see source header)"
            );
            std::process::exit(2);
        }
    }
}

struct Args(BTreeMap<String, String>);

impl Args {
    fn parse(a: &[String], flags: &[&str]) -> Result<Self> {
        let mut m = BTreeMap::new();
        let mut i = 0;
        while i < a.len() {
            let k = a[i]
                .strip_prefix("--")
                .with_context(|| format!("unexpected argument {}", a[i]))?;
            if flags.contains(&k) {
                m.insert(k.to_owned(), "1".to_owned());
                i += 1;
            } else {
                let v = a
                    .get(i + 1)
                    .with_context(|| format!("--{k} needs a value"))?;
                m.insert(k.to_owned(), v.clone());
                i += 2;
            }
        }
        Ok(Self(m))
    }
    fn get<T: std::str::FromStr>(&self, k: &str, default: T) -> Result<T> {
        match self.0.get(k) {
            None => Ok(default),
            Some(v) => v
                .parse()
                .map_err(|_| anyhow::anyhow!("bad value for --{k}: {v}")),
        }
    }
    fn flag(&self, k: &str) -> bool {
        self.0.contains_key(k)
    }
}

/// utime + stime of this process, in seconds.
fn cpu_seconds() -> f64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let rest = s.rsplit_once(')').map(|x| x.1).unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    // Fields after the comm: state is [0], utime is field 14 overall -> [11].
    let ticks: f64 = f.get(11).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0)
        + f.get(12).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    ticks / hz.max(1.0)
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// Collects compositor frame timings without gaps or stale history: only
/// frames after the first sample count, keyed by frame index. (The
/// `m_flSystemTimeInSeconds` of SteamVR's 128-frame history was seen running
/// tens of seconds behind CLOCK_MONOTONIC on the Frame, so it is not used.)
#[derive(Default)]
struct TimingCollector {
    frames: BTreeMap<u32, FrameTiming>,
    floor: Option<u32>,
    started: Option<Instant>,
}

impl TimingCollector {
    fn sample(&mut self, batch: Vec<FrameTiming>) {
        let Some(floor) = self.floor else {
            if !batch.is_empty() {
                self.floor = Some(batch.iter().map(|t| t.frame_index).max().unwrap_or(0));
                self.started = Some(Instant::now());
            }
            return;
        };
        for t in batch {
            if t.frame_index > floor {
                self.frames.insert(t.frame_index, t);
            }
        }
    }
    fn summary(&self) -> String {
        if self.frames.is_empty() {
            return "no compositor timings available".into();
        }
        let mut gpu: Vec<f64> = self
            .frames
            .values()
            .map(|t| t.compositor_render_gpu_ms as f64)
            .collect();
        let mut total: Vec<f64> = self
            .frames
            .values()
            .map(|t| t.total_render_gpu_ms as f64)
            .collect();
        let cpu: Vec<f64> = self
            .frames
            .values()
            .map(|t| t.compositor_render_cpu_ms as f64)
            .collect();
        let dropped: u32 = self.frames.values().map(|t| t.num_dropped_frames).sum();
        let mis: u32 = self.frames.values().map(|t| t.num_mis_presented).sum();
        let span = self.started.map_or(0.0, |t| t.elapsed().as_secs_f64());
        let n = self.frames.len();
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        format!(
            "{n} compositor frames over {span:.1}s ({:.1} Hz): compositor GPU mean {:.2} p95 {:.2} ms; total GPU mean {:.2} p95 {:.2} ms; compositor CPU mean {:.2} ms; dropped {dropped}, mispresented {mis}",
            if span > 0.0 {
                (n - 1) as f64 / span
            } else {
                0.0
            },
            mean(&gpu),
            pct(&mut gpu, 0.95),
            mean(&total),
            pct(&mut total, 0.95),
            mean(&cpu),
        )
    }
}

fn vrtimings(a: &[String]) -> Result<()> {
    let args = Args::parse(a, &[])?;
    let secs: f64 = args.get("seconds", 10.0)?;
    let vr = OpenVr::connect()?;
    let mut tc = TimingCollector::default();
    let cpu0 = cpu_seconds();
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < secs {
        tc.sample(vr.frame_timings(200));
        std::thread::sleep(Duration::from_millis(500));
    }
    tc.sample(vr.frame_timings(200));
    info!("baseline (no capture): {}", tc.summary());
    info!(
        "vrtimings process CPU {:.2}%",
        100.0 * (cpu_seconds() - cpu0) / t0.elapsed().as_secs_f64()
    );
    Ok(())
}

fn probe(a: &[String]) -> Result<()> {
    let args = Args::parse(a, &["zero-copy", "quiet", "test-pattern", "synthetic"])?;
    let secs: f64 = args.get("seconds", 5.0)?;
    let codec = match args.get("codec", "h264".to_string())?.as_str() {
        "hevc" | "h265" => Codec::Hevc,
        "h264" | "avc" => Codec::H264,
        c => bail!("unknown codec {c}"),
    };
    let cfg = StreamConfig {
        width: args.get("width", 1920)?,
        height: args.get("height", 1080)?,
        fps: args.get("fps", 60)?,
        bitrate_kbps: args.get("bitrate-kbps", 20000)?,
        codec,
        profile: match args.get("profile", "high".to_string())?.as_str() {
            "high" => H264Profile::High,
            "main" => H264Profile::Main,
            "baseline" => H264Profile::ConstrainedBaseline,
            p => bail!("unknown profile {p}"),
        },
    };
    let bitrate_step: u32 = args.get("bitrate-step", 0)?;
    let out: String = args.get("out", "probe.h264".to_string())?;
    let kf_every: f64 = args.get("keyframe-every", 0.0)?;
    let quiet = args.flag("quiet");
    let opts = SourceOptions {
        zero_copy: args.flag("zero-copy"),
        gop: args.get("gop", 0)?,
        intra_refresh: args.get("intra-refresh", 0)?,
        test_pattern: args.flag("test-pattern"),
        synthetic: args.flag("synthetic"),
        device: args.0.get("device").cloned(),
        coded_format: args.0.get("coded").map(|c| fourcc(c)).transpose()?,
    };

    let mut file = BufWriter::new(File::create(&out).with_context(|| format!("create {out}"))?);
    let mut src = HeadsetSource::new(opts);
    let t_start = Instant::now();
    src.start(&cfg)?;
    info!(
        "start took {:.0} ms, mode {:?}",
        t_start.elapsed().as_secs_f64() * 1e3,
        src.mode()
    );

    let mut tc = TimingCollector::default();
    if let Some(c) = src.capture() {
        tc.sample(c.provider().frame_timings(200));
    }
    let cpu0 = cpu_seconds();
    let t0 = Instant::now();
    let mut last_sample = t0;
    let mut last_kf_req = t0;
    let (mut frames, mut keyframes, mut bytes, mut empty) = (0u64, 0u64, 0u64, 0u64);
    // (when the bitrate was switched, bytes before, keyframes before).
    let mut stepped: Option<(f64, u64, u64)> = None;
    let (mut enc_ms, mut gpu_ms, mut copy_ms) = (Vec::new(), Vec::new(), Vec::new());
    let mut kf_sizes = Vec::new();
    while t0.elapsed().as_secs_f64() < secs {
        if kf_every > 0.0 && last_kf_req.elapsed().as_secs_f64() >= kf_every {
            last_kf_req = Instant::now();
            src.request_keyframe();
        }
        if bitrate_step > 0 && stepped.is_none() && t0.elapsed().as_secs_f64() >= secs / 2.0 {
            src.set_bitrate(bitrate_step)?;
            stepped = Some((t0.elapsed().as_secs_f64(), bytes, keyframes));
        }
        let f = src.next_frame()?;
        let st = src.last_stats;
        file.write_all(&f.data)?;
        frames += 1;
        bytes += f.data.len() as u64;
        if f.keyframe {
            keyframes += 1;
            kf_sizes.push(f.data.len());
        }
        if !st.capture.had_image {
            empty += 1;
        }
        enc_ms.push(st.encode_time.as_secs_f64() * 1e3);
        gpu_ms.push(st.capture.gpu_time.as_secs_f64() * 1e3);
        copy_ms.push(st.capture.copy_time.as_secs_f64() * 1e3);
        if !quiet {
            info!(
                "frame {:5} pts {:9} us {:7} B{} capture gpu {:.2} ms copy {:.2} ms encode {:.2} ms",
                f.frame_index,
                f.pts_us,
                f.data.len(),
                if f.keyframe { " KEY" } else { "    " },
                st.capture.gpu_time.as_secs_f64() * 1e3,
                st.capture.copy_time.as_secs_f64() * 1e3,
                st.encode_time.as_secs_f64() * 1e3
            );
        }
        if last_sample.elapsed() > Duration::from_millis(500) {
            last_sample = Instant::now();
            if let Some(c) = src.capture() {
                tc.sample(c.provider().frame_timings(200));
            }
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu0;
    if let Some(c) = src.capture() {
        tc.sample(c.provider().frame_timings(200));
    }
    src.stop();
    file.flush()?;

    let mean = |v: &[f64]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    info!(
        "summary: {} frames in {:.2}s ({:.1} fps), {} keyframes (sizes {:?}), {} without image",
        frames,
        wall,
        frames as f64 / wall,
        keyframes,
        kf_sizes,
        empty
    );
    info!(
        "summary: {:.0} kbit/s actual vs {} requested, {} bytes -> {}",
        bytes as f64 * 8.0 / wall / 1000.0,
        cfg.bitrate_kbps,
        bytes,
        out
    );
    if let Some((at, bytes_before, keys_before)) = stepped {
        info!(
            "summary: bitrate step at {at:.1}s: {:.0} kbit/s before ({} asked), {:.0} kbit/s after ({bitrate_step} asked), {} keyframes after the step",
            bytes_before as f64 * 8.0 / at / 1000.0,
            cfg.bitrate_kbps,
            (bytes - bytes_before) as f64 * 8.0 / (wall - at) / 1000.0,
            keyframes - keys_before
        );
    }
    info!(
        "summary: encode latency mean {:.2} p50 {:.2} p95 {:.2} max {:.2} ms",
        mean(&enc_ms),
        pct(&mut enc_ms.clone(), 0.5),
        pct(&mut enc_ms.clone(), 0.95),
        pct(&mut enc_ms, 1.0)
    );
    info!(
        "summary: capture GPU wait mean {:.2} p95 {:.2} ms; CPU copy mean {:.2} ms",
        mean(&gpu_ms),
        pct(&mut gpu_ms.clone(), 0.95),
        mean(&copy_ms)
    );
    info!(
        "summary: process CPU {:.2}% of one core ({:.3}s CPU over {:.2}s)",
        100.0 * cpu / wall,
        cpu,
        wall
    );
    info!("summary: with probe running: {}", tc.summary());
    Ok(())
}

fn fourcc(s: &str) -> Result<u32> {
    let b = s.as_bytes();
    if b.len() != 4 {
        bail!("fourcc must be 4 characters: {s}");
    }
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_START: u64 = 0;
const DMA_BUF_SYNC_END: u64 = 4;
/// `_IOW('b', 0, struct dma_buf_sync)`.
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;

/// Brackets CPU reads of a dmabuf mapping so the exporter can flush/invalidate caches.
fn dmabuf_sync(fd: std::os::fd::RawFd, phase: u64) -> Result<()> {
    let flags: u64 = phase | DMA_BUF_SYNC_READ;
    loop {
        let r = unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC as _, &flags) };
        if r == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e).context("DMA_BUF_IOCTL_SYNC");
        }
    }
}

/// Read-only CPU mapping of a dmabuf, for the selftest's reference comparison.
struct DmabufView {
    ptr: *mut libc::c_void,
    len: usize,
}

impl DmabufView {
    fn map(fd: std::os::fd::RawFd, len: usize) -> Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("mmap dmabuf");
        }
        Ok(Self { ptr, len })
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for DmabufView {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

/// Compares an NV12 input buffer with the CPU reference; (bad bytes, max diff).
fn compare_nv12(
    got: &[u8],
    want: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    uv_offset: u32,
) -> (usize, u8) {
    let (mut bad, mut max) = (0usize, 0u8);
    let mut check = |o: usize| {
        let d = got[o].abs_diff(want[o]);
        max = max.max(d);
        if d > 1 {
            bad += 1;
        }
    };
    for y in 0..height as usize {
        for x in 0..width as usize {
            check(y * stride as usize + x);
        }
    }
    for y in 0..(height / 2) as usize {
        for x in 0..width as usize {
            check(uv_offset as usize + y * stride as usize + x);
        }
    }
    (bad, max)
}

fn selftest(a: &[String]) -> Result<()> {
    let args = Args::parse(a, &["zero-copy"])?;
    let width: u32 = args.get("width", 640)?;
    let height: u32 = args.get("height", 360)?;
    let frames: u32 = args.get("frames", 30)?;
    let fps: u32 = 30;
    let coded: String = args.get("coded", "H264".to_string())?;
    let out: String = args.get("out", "selftest.bin".to_string())?;
    let mut ec = EncoderConfig::new(
        width,
        height,
        fps,
        4000,
        if coded == "H264" {
            Codec::H264
        } else {
            Codec::Hevc
        },
    );
    if let Some(d) = args.0.get("device") {
        ec.device = d.clone();
    }
    if coded != "H264" && coded != "HEVC" {
        ec.coded_format = Some(fourcc(&coded)?);
    }
    let mut fails: Vec<String> = Vec::new();

    let mut enc = Encoder::open(&ec).context("opening the encoder")?;
    let l = enc.layout();
    let layout = TargetLayout {
        width: l.width,
        height: l.height,
        stride: l.stride,
        rows: l.padded_height,
        uv_offset: l.uv_offset,
    };
    info!(
        "selftest: {} {}x{} on {}, input layout {:?}",
        coded, width, height, ec.device, l
    );
    let synth = || Box::new(SyntheticProvider::new(width, height));
    let mut cap = if !args.flag("zero-copy") {
        HeadsetCapture::new(layout, None, synth())?
    } else {
        let fds: Vec<_> = (0..enc.num_inputs())
            .map(|i| enc.input_dmabuf(i).as_fd())
            .collect();
        match HeadsetCapture::new(layout, Some(&fds), synth()) {
            Ok(c) => c,
            Err(e) => {
                fails.push(format!("dmabuf import failed: {e:#}"));
                HeadsetCapture::new(layout, None, synth())?
            }
        }
    };
    info!("selftest: capture mode {:?}", cap.mode());

    let mut file = BufWriter::new(File::create(&out)?);
    let mut packets: Vec<fl_encode::Packet> = Vec::new();
    let (mut bad_total, mut max_total) = (0usize, 0u8);
    let half = frames / 2;
    let mut queued = 0u32;
    let take = |p: fl_encode::Packet,
                file: &mut BufWriter<File>,
                packets: &mut Vec<fl_encode::Packet>|
     -> Result<()> {
        file.write_all(&p.data)?;
        packets.push(p);
        Ok(())
    };
    for n in 0..frames {
        // A free input, without ever blocking on the OUTPUT queue.
        let idx = loop {
            if let Some(i) = enc.free_input() {
                break i;
            }
            match enc.poll_packet(Duration::from_secs(2))? {
                Some(p) => take(p, &mut file, &mut packets)?,
                None => bail!("no free input and no output for 2 s at frame {n}"),
            }
        };
        let info = if cap.mode() == TargetMode::DmaBuf {
            cap.capture(idx, None)?
        } else {
            let m = enc.input_map(idx);
            let m = unsafe { std::slice::from_raw_parts_mut(m.as_mut_ptr(), m.len()) };
            cap.capture(idx, Some(m))?
        };
        let want = fl_capture::expected_nv12(
            &SyntheticProvider::pattern(width, height, n),
            width,
            height,
            l.stride,
            l.uv_offset,
        );
        // The GPU wrote through the dmabuf; without a CPU-access bracket the mmap can
        // serve stale cache lines, which looks like corruption the encoder never sees.
        let dmabuf = (cap.mode() == TargetMode::DmaBuf)
            .then(|| std::os::fd::AsRawFd::as_raw_fd(enc.input_dmabuf(idx)));
        if let Some(fd) = dmabuf {
            dmabuf_sync(fd, DMA_BUF_SYNC_START)?;
        }
        // Read through a mapping of the dmabuf itself, which the sync bracket covers.
        let view = dmabuf
            .map(|fd| DmabufView::map(fd, l.size as usize))
            .transpose()?;
        let (bad, max) = compare_nv12(
            match &view {
                Some(v) => v.bytes(),
                None => &*enc.input_map(idx),
            },
            &want,
            width,
            height,
            l.stride,
            l.uv_offset,
        );
        if std::env::var_os("FL_SELFTEST_RECHECK").is_some() {
            std::thread::sleep(Duration::from_millis(30));
            let again = compare_nv12(
                match &view {
                    Some(v) => v.bytes(),
                    None => &*enc.input_map(idx),
                },
                &want,
                width,
                height,
                l.stride,
                l.uv_offset,
            );
            info!(
                "frame {n}: buffer {idx} first {bad}/{max}, after 30 ms {}/{}",
                again.0, again.1
            );
        }
        drop(view);
        if let Some(fd) = dmabuf {
            dmabuf_sync(fd, DMA_BUF_SYNC_END)?;
        }
        bad_total += bad;
        max_total = max_total.max(max);
        if n == 0 || bad > 0 {
            info!(
                "frame {n}: input buffer {idx}, {bad} bytes off by >1, max diff {max}, gpu {:.2} ms",
                info.gpu_time.as_secs_f64() * 1e3
            );
        }
        if n == half / 2 {
            enc.request_keyframe();
        }
        enc.queue_input(idx, n as u64 * 1_000_000 / fps as u64)?;
        queued += 1;
        while let Some(p) = enc.poll_packet(Duration::ZERO)? {
            take(p, &mut file, &mut packets)?;
        }
        if n + 1 == half {
            let before = packets.len();
            for p in enc.drain(Duration::from_secs(5))? {
                take(p, &mut file, &mut packets)?;
            }
            info!(
                "drain 1: {} packets after drain, {} total for {} queued",
                packets.len() - before,
                packets.len(),
                queued
            );
            if packets.len() as u32 != queued {
                fails.push(format!(
                    "drain 1: {} packets for {} frames",
                    packets.len(),
                    queued
                ));
            }
            enc.restart()?;
            info!("restarted");
        }
    }
    for p in enc.drain(Duration::from_secs(5))? {
        take(p, &mut file, &mut packets)?;
    }
    file.flush()?;
    let keyframes: Vec<usize> = packets
        .iter()
        .enumerate()
        .filter(|(_, p)| p.keyframe)
        .map(|(i, _)| i)
        .collect();
    let pts_ok = packets.windows(2).all(|w| w[1].pts_us > w[0].pts_us);
    let enc_ms: Vec<f64> = packets
        .iter()
        .map(|p| p.encode_time.as_secs_f64() * 1e3)
        .collect();
    info!(
        "selftest: {} packets for {} frames, {} bytes -> {out}; keyframes at {:?}; pts increasing {}; encode mean {:.2} ms",
        packets.len(),
        frames,
        packets.iter().map(|p| p.data.len()).sum::<usize>(),
        keyframes,
        pts_ok,
        enc_ms.iter().sum::<f64>() / enc_ms.len().max(1) as f64
    );
    info!(
        "selftest: input buffers vs CPU reference: {bad_total} bytes off by >1, max diff {max_total}"
    );
    if packets.len() as u32 != frames {
        fails.push(format!("{} packets for {frames} frames", packets.len()));
    }
    if !pts_ok {
        fails.push("pts not increasing".into());
    }
    if bad_total > 0 {
        fails.push(format!("{bad_total} input bytes differ from the reference"));
    }
    if fails.is_empty() {
        info!("selftest: PASS ({:?})", cap.mode());
        Ok(())
    } else {
        for f in &fails {
            tracing::error!("selftest: {f}");
        }
        bail!("selftest: FAIL")
    }
}

static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Set by SIGUSR1: press the app's button.
static TOGGLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_toggle(_: libc::c_int) {
    TOGGLE.store(true, std::sync::atomic::Ordering::SeqCst);
}

const SERVE_FLAGS: [&str; 3] = ["zero-copy", "test-pattern", "synthetic"];

/// Everything `serve` and `app` need to start the streamer, any number of times.
struct Streamer {
    cfg: fl_webrtc::ServerConfig,
    opts: SourceOptions,
    audio: bool,
}

impl Streamer {
    fn from_args(args: &Args) -> Result<Self> {
        let d = fl_webrtc::ServerConfig::default();
        let cfg = fl_webrtc::ServerConfig {
            port: args.get("port", d.port)?,
            width: args.get("width", d.width)?,
            height: args.get("height", d.height)?,
            fps: args.get("fps", d.fps)?,
            bitrate_kbps: args.get("bitrate-kbps", d.bitrate_kbps)?,
            title: d.title,
        };
        let opts = SourceOptions {
            zero_copy: args.flag("zero-copy"),
            gop: args.get("gop", 0)?,
            intra_refresh: args.get("intra-refresh", 0)?,
            test_pattern: args.flag("test-pattern"),
            synthetic: args.flag("synthetic"),
            device: args.0.get("device").cloned(),
            coded_format: None,
        };
        let audio = match args.get("audio", "monitor".to_string())?.as_str() {
            "monitor" => true,
            "off" => false,
            other => bail!("--audio {other}: expected monitor or off"),
        };
        let nice: i32 = args.get("nice", 10)?;
        // SAFETY: plain syscall. Threads started from here on inherit the value.
        if nice != 0 && unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) } != 0 {
            tracing::warn!("setpriority({nice}): {}", std::io::Error::last_os_error());
        }
        Ok(Self { cfg, opts, audio })
    }

    fn start(&self) -> Result<fl_webrtc::Server> {
        let audio: Option<Box<dyn fl_core::AudioSource>> = self
            .audio
            .then(|| Box::new(fl_audio::MonitorAudioSource::new()) as _);
        fl_webrtc::Server::start(
            self.cfg.clone(),
            Box::new(HeadsetSource::new(self.opts.clone())),
            audio,
        )
    }

    /// The addresses to type into a browser: by name (avahi announces `<hostname>.local`)
    /// and by number.
    fn urls(&self) -> Vec<String> {
        let port = self.cfg.port;
        let mut urls = Vec::new();
        if let Ok(name) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
            urls.push(format!("http://{}.local:{port}", name.trim()));
        }
        if let Some(ip) = fl_webrtc::primary_ipv4() {
            urls.push(format!("http://{ip}:{port}"));
        }
        urls
    }
}

fn install_signal_handlers() {
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

fn serve(a: &[String]) -> Result<()> {
    let streamer = Streamer::from_args(&Args::parse(a, &SERVE_FLAGS)?)?;
    let server = streamer.start()?;
    info!("watch at {}", streamer.urls().join(" or "));
    install_signal_handlers();
    while !STOP.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }
    info!("shutting down");
    server.shutdown();
    Ok(())
}

fn app(a: &[String]) -> Result<()> {
    let streamer = Streamer::from_args(&Args::parse(a, &SERVE_FLAGS)?)?;
    install_signal_handlers();
    // SAFETY: the handler only stores to an atomic.
    unsafe { libc::signal(libc::SIGUSR1, on_toggle as *const () as libc::sighandler_t) };
    ui::run(&streamer, &STOP, &TOGGLE)
}
