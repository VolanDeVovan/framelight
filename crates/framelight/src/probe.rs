//! `framelight probe`: run the headset source alone, write what it encodes to a file
//! and report rates, latencies and the CPU time it cost.

use crate::cli::Args;
use crate::source::{HeadsetSource, SourceOptions};
use anyhow::{Context, Result, bail};
use fl_core::{H264Profile, StreamConfig, VideoSource};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::Instant;
use tracing::info;

/// utime + stime of this process, in seconds.
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // The fields after the command name: state is [0], utime [11], stime [12].
    let rest = stat.rsplit_once(')').map_or("", |x| x.1);
    let ticks: f64 = rest
        .split_whitespace()
        .skip(11)
        .take(2)
        .filter_map(|v| v.parse::<f64>().ok())
        .sum();
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    ticks / hz.max(1.0)
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

/// The `p` quantile (0..=1) of `v`, which it sorts.
fn quantile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

pub fn fourcc(s: &str) -> Result<u32> {
    let Ok(b) = <[u8; 4]>::try_from(s.as_bytes()) else {
        bail!("fourcc must be 4 characters: {s}");
    };
    Ok(u32::from_le_bytes(b))
}

pub fn run(argv: &[String]) -> Result<()> {
    let args = Args::parse(
        argv,
        &[
            "seconds",
            "out",
            "profile",
            "width",
            "height",
            "fps",
            "bitrate-kbps",
            "bitrate-step",
            "keyframe-every",
            "device",
            "coded",
        ],
        &["synthetic", "verbose"],
    )?;
    let secs: f64 = args.get("seconds", 5.0)?;
    let cfg = StreamConfig {
        width: args.get("width", 1920)?,
        height: args.get("height", 1080)?,
        fps: args.get("fps", 60)?,
        bitrate_kbps: args.get("bitrate-kbps", 12_000)?,
        profile: match args.opt("profile").unwrap_or("high") {
            "high" => H264Profile::High,
            "main" => H264Profile::Main,
            "baseline" => H264Profile::ConstrainedBaseline,
            p => bail!("unknown profile {p}"),
        },
    };
    let bitrate_step: u32 = args.get("bitrate-step", 0)?;
    let keyframe_every: f64 = args.get("keyframe-every", 0.0)?;
    let out: String = args.get("out", "probe.h264".to_string())?;
    let verbose = args.flag("verbose");

    let mut file = BufWriter::new(File::create(&out).with_context(|| format!("create {out}"))?);
    let mut src = HeadsetSource::new(SourceOptions {
        synthetic: args.flag("synthetic"),
        device: args.opt("device").map(str::to_owned),
        coded_format: args.opt("coded").map(fourcc).transpose()?,
    });
    let t_start = Instant::now();
    src.start(&cfg)?;
    info!("start took {:.0} ms", t_start.elapsed().as_secs_f64() * 1e3);

    let cpu0 = cpu_seconds();
    let t0 = Instant::now();
    let mut last_keyframe_request = t0;
    let (mut frames, mut bytes, mut empty) = (0u64, 0u64, 0u64);
    // (when the bitrate was switched, bytes before, keyframes before).
    let mut stepped: Option<(f64, u64, usize)> = None;
    let (mut enc_ms, mut gpu_ms, mut copy_ms) = (Vec::new(), Vec::new(), Vec::new());
    let mut keyframe_sizes = Vec::new();
    while t0.elapsed().as_secs_f64() < secs {
        if keyframe_every > 0.0 && last_keyframe_request.elapsed().as_secs_f64() >= keyframe_every {
            last_keyframe_request = Instant::now();
            src.request_keyframe();
        }
        if bitrate_step > 0 && stepped.is_none() && t0.elapsed().as_secs_f64() >= secs / 2.0 {
            src.set_bitrate(bitrate_step)?;
            stepped = Some((t0.elapsed().as_secs_f64(), bytes, keyframe_sizes.len()));
        }
        let f = src.next_frame()?;
        let st = src.last_stats;
        file.write_all(&f.data)?;
        frames += 1;
        bytes += f.data.len() as u64;
        if f.keyframe {
            keyframe_sizes.push(f.data.len());
        }
        if !st.capture.had_image {
            empty += 1;
        }
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
        enc_ms.push(ms(st.encode_time));
        gpu_ms.push(ms(st.capture.gpu_time));
        copy_ms.push(ms(st.capture.copy_time));
        if verbose {
            info!(
                "frame {frames:5} pts {:9} us {:7} B{} gpu {:.2} ms copy {:.2} ms encode {:.2} ms",
                st.pts_us,
                f.data.len(),
                if f.keyframe { " KEY" } else { "    " },
                ms(st.capture.gpu_time),
                ms(st.capture.copy_time),
                ms(st.encode_time)
            );
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu0;
    src.stop();
    file.flush()?;

    let kbps = |bytes: u64, secs: f64| bytes as f64 * 8.0 / secs.max(1e-9) / 1000.0;
    info!(
        "summary: {frames} frames in {wall:.2}s ({:.1} fps), {} keyframes (sizes {keyframe_sizes:?}), {empty} without image",
        frames as f64 / wall,
        keyframe_sizes.len(),
    );
    info!(
        "summary: {:.0} kbit/s actual vs {} requested, {bytes} bytes -> {out}",
        kbps(bytes, wall),
        cfg.bitrate_kbps,
    );
    if let Some((at, bytes_before, keys_before)) = stepped {
        info!(
            "summary: bitrate step at {at:.1}s: {:.0} kbit/s before ({} asked), {:.0} kbit/s after ({bitrate_step} asked), {} keyframes after the step",
            kbps(bytes_before, at),
            cfg.bitrate_kbps,
            kbps(bytes - bytes_before, wall - at),
            keyframe_sizes.len() - keys_before
        );
    }
    info!(
        "summary: encode latency mean {:.2} p50 {:.2} p95 {:.2} max {:.2} ms",
        mean(&enc_ms),
        quantile(&mut enc_ms, 0.5),
        quantile(&mut enc_ms, 0.95),
        quantile(&mut enc_ms, 1.0)
    );
    info!(
        "summary: GPU conversion mean {:.2} p95 {:.2} ms; CPU copy mean {:.2} ms",
        mean(&gpu_ms),
        quantile(&mut gpu_ms, 0.95),
        mean(&copy_ms)
    );
    info!(
        "summary: process CPU {:.2}% of one core ({cpu:.3}s CPU over {wall:.2}s)",
        100.0 * cpu / wall,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_and_fourcc() {
        let mut v = [5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(quantile(&mut v, 0.5), 3.0);
        assert_eq!(quantile(&mut v, 1.0), 5.0);
        assert_eq!(quantile(&mut [], 0.5), 0.0);
        assert_eq!(mean(&[1.0, 2.0, 3.0]), 2.0);
        assert_eq!(fourcc("FWHT").unwrap(), u32::from_le_bytes(*b"FWHT"));
        assert!(fourcc("H26").is_err());
        assert!(cpu_seconds() >= 0.0);
    }
}
