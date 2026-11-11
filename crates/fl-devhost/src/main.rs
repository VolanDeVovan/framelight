//! Development host: runs the WebRTC streamer on this machine with a synthetic video
//! source (ffmpeg test pattern + running clock, libx264) and the default sink's monitor
//! as audio, so the whole viewer path can be tried without the headset.
//!
//! ```text
//! fl-devhost [--port N] [--width W --height H --fps F] [--bitrate-kbps B]
//!            [--audio monitor|tone|off]
//! ```
//! Then open `http://<this machine's LAN address>:8090/` in a browser. libpulse is
//! dlopen'd; on Nix, `nix develop` puts it on `LD_LIBRARY_PATH`.

mod ffmpeg;

use anyhow::{Result, bail};
use fl_webrtc::{Server, ServerConfig};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let mut cfg = ServerConfig {
        title: "framelight dev host".into(),
        ..ServerConfig::default()
    };
    let mut audio = "monitor".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |what: &str| {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{a} needs {what}"))
        };
        match a.as_str() {
            "--port" => cfg.port = value("a port")?.parse()?,
            "--width" => cfg.width = value("pixels")?.parse()?,
            "--height" => cfg.height = value("pixels")?.parse()?,
            "--fps" => cfg.fps = value("a frame rate")?.parse()?,
            "--bitrate-kbps" => cfg.bitrate_kbps = value("kbit/s")?.parse()?,
            "--audio" => audio = value("monitor|tone|off")?,
            "-h" | "--help" => {
                println!(
                    "fl-devhost [--port N] [--width W --height H --fps F] [--bitrate-kbps B] [--audio monitor|tone|off]"
                );
                return Ok(());
            }
            other => bail!("unknown argument {other}"),
        }
    }
    let audio: Option<Box<dyn fl_core::AudioSource>> = match audio.as_str() {
        "monitor" => Some(Box::new(fl_audio::MonitorAudioSource::new())),
        // 880 Hz left, 1760 Hz right: lets a test check the audio path without playing
        // or recording anything on this machine.
        "tone" => Some(Box::new(fl_audio::ToneAudioSource::new(880.0))),
        "off" => None,
        other => bail!("--audio {other}: expected monitor, tone or off"),
    };
    let port = cfg.port;
    let server = Server::start(cfg, Box::new(ffmpeg::FfmpegSource::new()), audio)?;
    match fl_webrtc::primary_ipv4() {
        Some(ip) => tracing::info!("open http://{ip}:{port}/ in a browser"),
        None => tracing::info!("open http://<this machine>:{port}/ in a browser"),
    }

    // SAFETY: installing a handler that only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    while !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }
    tracing::info!("shutting down");
    server.shutdown();
    Ok(())
}
