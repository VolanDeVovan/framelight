//! Development GameStream host: runs `fl-gamestream` on this machine with a synthetic
//! video source (ffmpeg test pattern + running clock, encoded to Annex-B H.264/HEVC at
//! the negotiated size, fps and bitrate) and the default sink's monitor as audio
//! (`--audio silence` for empty Opus frames instead).
//!
//! ```text
//! fl-devhost [--name NAME] [--state DIR] [--pin NNNN] [--no-mdns] [--no-hevc] [--no-h264]
//!            [--audio monitor|silence|tone]
//! ```
//! Without `--pin`, type the PIN Moonlight shows into this process's stdin.

mod ffmpeg;

use anyhow::Result;
use fl_gamestream::{GameStreamHost, HostConfig, SilenceAudioSource};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,fl_gamestream=debug".into()),
        )
        .init();

    let mut name = format!("framelight-dev ({})", hostname());
    let mut state = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".local/state/framelight-devhost");
    let mut pin = None;
    let mut mdns = true;
    let (mut h264, mut hevc) = (true, true);
    let mut audio = "monitor".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = args.next().expect("--name NAME"),
            "--state" => state = args.next().expect("--state DIR").into(),
            "--pin" => pin = Some(args.next().expect("--pin NNNN")),
            "--no-mdns" => mdns = false,
            "--no-hevc" => hevc = false,
            "--no-h264" => h264 = false,
            "--audio" => audio = args.next().expect("--audio monitor|silence|tone"),
            "-h" | "--help" => {
                println!(
                    "fl-devhost [--name NAME] [--state DIR] [--pin NNNN] [--no-mdns] [--no-hevc] [--no-h264] [--audio monitor|silence|tone]"
                );
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other}"),
        }
    }

    let mut cfg = HostConfig::new(name, state);
    cfg.pin = pin;
    cfg.mdns = mdns;
    cfg.h264 = h264;
    cfg.hevc = hevc;
    let audio: Box<dyn fl_core::AudioSource> = match audio.as_str() {
        "monitor" => Box::new(fl_audio::MonitorAudioSource::new()),
        "silence" => Box::new(SilenceAudioSource::new()),
        // 880 Hz left, 1760 Hz right: lets a test check the client's decoded audio
        // without playing or recording anything on this machine.
        "tone" => Box::new(fl_audio::ToneAudioSource::new(880.0)),
        other => anyhow::bail!("--audio {other}: expected monitor, silence or tone"),
    };
    let host = GameStreamHost::start(cfg, Box::new(ffmpeg::FfmpegSource::new()), audio)?;

    // SAFETY: installing a handler that only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
            let _ = tx.send(line.trim().to_string());
            line.clear();
        }
    });
    let mut announced = false;
    while !STOP.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(pin) if !pin.is_empty() => {
                if host.submit_pin(&pin) {
                    tracing::info!("PIN submitted");
                } else {
                    tracing::warn!("no pairing is waiting for a PIN");
                }
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            // stdin closed (e.g. </dev/null): keep polling the stop flag without spinning.
            Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(Duration::from_millis(200)),
        }
        let waiting = host.pairing_waits_for_pin();
        if waiting && !announced {
            tracing::info!("a client is pairing: type the PIN it shows and press Enter");
        }
        announced = waiting;
    }
    tracing::info!("shutting down");
    host.shutdown();
    Ok(())
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "host".into())
}
