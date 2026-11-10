//! `framelight selftest`: the capture and encoder without SteamVR.
//!
//! A synthetic picture goes through the GPU conversion into the encoder's input
//! buffers, where every byte is checked against a CPU reference, and through the
//! encoder, which must return one packet per frame with increasing timestamps.
//! `tools/vicodec-vm` runs it against the kernel's `vicodec` and lavapipe.

use crate::cli::Args;
use crate::probe::fourcc;
use anyhow::{Context, Result, bail};
use fl_capture::{HeadsetCapture, SyntheticProvider, expected_nv12};
use fl_core::{H264Profile, Nv12Layout};
use fl_encode::{DEFAULT_DEVICE, Encoder, EncoderConfig, Packet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::Duration;
use tracing::info;

const FPS: u32 = 30;
/// How long the encoder may take to hand back a frame before the test gives up.
const ENCODER_TIMEOUT: Duration = Duration::from_secs(2);

/// Compares the visible picture in an input buffer with the reference:
/// (bytes off by more than one, largest difference).
fn compare_nv12(got: &[u8], want: &[u8], l: &Nv12Layout) -> (usize, u8) {
    let (w, s) = (l.width as usize, l.stride as usize);
    let luma_rows = (0..l.height as usize).map(|y| y * s);
    let chroma_rows = (0..(l.height / 2) as usize).map(|y| l.uv_offset as usize + y * s);
    let (mut bad, mut max) = (0usize, 0u8);
    for row in luma_rows.chain(chroma_rows) {
        for (a, b) in got[row..row + w].iter().zip(&want[row..row + w]) {
            let d = a.abs_diff(*b);
            max = max.max(d);
            bad += usize::from(d > 1);
        }
    }
    (bad, max)
}

pub fn run(argv: &[String]) -> Result<()> {
    let args = Args::parse(
        argv,
        &["width", "height", "frames", "out", "device", "coded"],
        &[],
    )?;
    let width: u32 = args.get("width", 640)?;
    let height: u32 = args.get("height", 360)?;
    let frames: u32 = args.get("frames", 30)?;
    let out: String = args.get("out", "selftest.bin".to_string())?;
    let cfg = EncoderConfig {
        device: args.get("device", DEFAULT_DEVICE.to_string())?,
        width,
        height,
        fps: FPS,
        bitrate_kbps: 4000,
        profile: H264Profile::High,
        coded_format: args.opt("coded").map(fourcc).transpose()?,
    };

    let mut enc = Encoder::open(&cfg).context("opening the encoder")?;
    let layout = enc.layout();
    info!(
        "selftest: {width}x{height} on {}, input layout {layout:?}",
        cfg.device
    );
    let mut cap = HeadsetCapture::new(layout, Box::new(SyntheticProvider::new(width, height)))?;

    let mut file = BufWriter::new(File::create(&out).with_context(|| format!("create {out}"))?);
    let mut packets: Vec<Packet> = Vec::new();
    let mut take = |p: Packet, packets: &mut Vec<Packet>| -> Result<()> {
        file.write_all(&p.data)?;
        packets.push(p);
        Ok(())
    };
    let (mut bad_total, mut max_total) = (0usize, 0u8);
    for n in 0..frames {
        // A free input, without ever blocking on the OUTPUT queue.
        let idx = loop {
            if let Some(i) = enc.free_input() {
                break i;
            }
            match enc.poll_packet(ENCODER_TIMEOUT)? {
                Some(p) => take(p, &mut packets)?,
                None => bail!("no free input and no output for 2 s at frame {n}"),
            }
        };
        let capture = cap.capture(enc.input_map(idx))?;
        let want = expected_nv12(&SyntheticProvider::pattern(width, height, n), &layout);
        let (bad, max) = compare_nv12(enc.input_map(idx), &want, &layout);
        bad_total += bad;
        max_total = max_total.max(max);
        if n == 0 || bad > 0 {
            info!(
                "frame {n}: input buffer {idx}, {bad} bytes off by >1, max diff {max}, gpu {:.2} ms",
                capture.gpu_time.as_secs_f64() * 1e3
            );
        }
        if n == frames / 2 {
            enc.request_keyframe();
        }
        enc.queue_input(idx, n as u64 * 1_000_000 / FPS as u64)?;
        while let Some(p) = enc.poll_packet(Duration::ZERO)? {
            take(p, &mut packets)?;
        }
    }
    // The frames still in the encoder.
    while packets.len() < frames as usize {
        match enc.poll_packet(ENCODER_TIMEOUT)? {
            Some(p) => take(p, &mut packets)?,
            None => break,
        }
    }
    file.flush()?;

    let keyframes: Vec<usize> = (0..packets.len())
        .filter(|&i| packets[i].keyframe)
        .collect();
    let pts_increasing = packets.windows(2).all(|w| w[1].pts_us > w[0].pts_us);
    let encode_ms: f64 = packets
        .iter()
        .map(|p| p.encode_time.as_secs_f64() * 1e3)
        .sum::<f64>()
        / packets.len().max(1) as f64;
    info!(
        "selftest: {} packets for {frames} frames, {} bytes -> {out}; keyframes at {keyframes:?}; encode mean {encode_ms:.2} ms",
        packets.len(),
        packets.iter().map(|p| p.data.len()).sum::<usize>(),
    );
    info!(
        "selftest: input buffers vs CPU reference: {bad_total} bytes off by >1, max diff {max_total}"
    );

    let mut fails: Vec<String> = Vec::new();
    if packets.len() != frames as usize {
        fails.push(format!("{} packets for {frames} frames", packets.len()));
    }
    if !pts_increasing {
        fails.push("timestamps are not increasing".into());
    }
    if bad_total > 0 {
        fails.push(format!("{bad_total} input bytes differ from the reference"));
    }
    if fails.is_empty() {
        info!("selftest: PASS");
        return Ok(());
    }
    for f in &fails {
        tracing::error!("selftest: {f}");
    }
    bail!("selftest: FAIL")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_only_the_visible_picture() {
        // 4x2 picture in a buffer with stride 8 and one padding row.
        let l = Nv12Layout {
            width: 4,
            height: 2,
            stride: 8,
            rows: 3,
            uv_offset: 24,
        };
        let want = vec![100u8; l.bytes()];
        let mut got = want.clone();
        assert_eq!(compare_nv12(&got, &want, &l), (0, 0));
        got[5] = 0; // Right of the picture.
        got[17] = 0; // The padding row.
        got[1] = 101; // Off by one is rounding.
        assert_eq!(compare_nv12(&got, &want, &l), (0, 1));
        got[8] = 90; // Luma, second row.
        got[24 + 3] = 110; // Chroma.
        assert_eq!(compare_nv12(&got, &want, &l), (2, 10));
    }
}
