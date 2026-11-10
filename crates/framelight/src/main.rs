//! framelight: show what the Steam Frame shows, in any browser on the network.

mod cli;
mod probe;
mod selftest;
mod signals;
mod source;
mod ui;

use anyhow::Result;
use cli::Args;
use fl_webrtc::{Server, ServerConfig};
use signals::Signals;
use source::{HeadsetSource, SourceOptions};
use tracing::info;

const USAGE: &str = "\
framelight: stream the Steam Frame's Headset View to browsers on the network

  framelight app [options]
      The switch, as a window to open on the headset: Enable / Disable and the
      address to type into a browser. Streaming lives exactly as long as it is
      enabled there. `pkill -USR1 -x framelight` presses the button from a shell.
  framelight serve [options]
      The same streamer without a window: enabled until it is stopped.

      --port N           TCP port of the page, UDP port of the media (8090)
      --width W --height H --fps F
                         stream mode (1920 1080 60)
      --bitrate-kbps B   initial bitrate ceiling; the page can change it (12000)
      --no-audio         video only
      --synthetic        a moving test picture instead of SteamVR

Diagnostics:
  framelight probe [--seconds N] [--out FILE] [--profile high|main|baseline]
                   [--width W --height H --fps F] [--bitrate-kbps B]
                   [--bitrate-step KBPS] [--keyframe-every SECS] [--verbose]
                   [--synthetic] [--device /dev/videoN] [--coded FOURCC]
      Runs capture and encoder alone, writes the stream to a file and reports
      rates, latencies and CPU time. --bitrate-step switches to that bitrate
      halfway and reports the rate of each half.
  framelight selftest [--width W --height H --frames N] [--out FILE]
                      [--device /dev/videoN] [--coded FOURCC]
      Synthetic picture -> GPU NV12 -> encoder: checks the bytes in every input
      buffer against a CPU reference and that every frame comes back encoded.
";

/// Keeps the process below the game for CPU time.
const NICE: i32 = 10;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = match args.split_first() {
        Some((c, rest)) => (c.as_str(), rest),
        None => ("", &[][..]),
    };
    match command {
        "app" | "serve" => {
            // Before anything starts a thread: they all inherit the blocked signals.
            let signals = Signals::install()?;
            let streamer = Streamer::from_args(rest)?;
            lower_priority();
            if command == "app" {
                ui::run(&streamer, &signals)
            } else {
                serve(&streamer, &signals)
            }
        }
        "probe" => probe::run(rest),
        "selftest" => selftest::run(rest),
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(())
        }
        _ => {
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn lower_priority() {
    // SAFETY: plain syscall. Threads started from here on inherit the value.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, NICE) } != 0 {
        tracing::warn!("setpriority({NICE}): {}", std::io::Error::last_os_error());
    }
}

/// Everything `serve` and `app` need to start the streamer, any number of times.
pub struct Streamer {
    cfg: ServerConfig,
    synthetic: bool,
    audio: bool,
}

impl Streamer {
    fn from_args(argv: &[String]) -> Result<Self> {
        let args = Args::parse(
            argv,
            &["port", "width", "height", "fps", "bitrate-kbps"],
            &["no-audio", "synthetic"],
        )?;
        let d = ServerConfig::default();
        Ok(Self {
            cfg: ServerConfig {
                port: args.get("port", d.port)?,
                width: args.get("width", d.width)?,
                height: args.get("height", d.height)?,
                fps: args.get("fps", d.fps)?,
                bitrate_kbps: args.get("bitrate-kbps", d.bitrate_kbps)?,
                title: d.title,
            },
            synthetic: args.flag("synthetic"),
            audio: !args.flag("no-audio"),
        })
    }

    /// Binds the ports and starts the threads; the sources open with the first viewer.
    pub fn start(&self) -> Result<Server> {
        let video = HeadsetSource::new(SourceOptions {
            synthetic: self.synthetic,
            ..SourceOptions::default()
        });
        let audio: Option<Box<dyn fl_core::AudioSource>> = self
            .audio
            .then(|| Box::new(fl_audio::MonitorAudioSource::new()) as _);
        Server::start(self.cfg.clone(), Box::new(video), audio)
    }

    /// The addresses to type into a browser: by name (avahi announces `<hostname>.local`)
    /// and by number.
    pub fn urls(&self) -> Vec<String> {
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

fn serve(streamer: &Streamer, signals: &Signals) -> Result<()> {
    let server = streamer.start()?;
    info!("watch at {}", streamer.urls().join(" or "));
    signals.wait_for_stop();
    info!("shutting down");
    server.shutdown();
    Ok(())
}
