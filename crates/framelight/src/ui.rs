//! The switch: a window with one button, to be opened on the headset.
//!
//! SteamOS opens an app from its menu (a desktop entry) as an X11 window shown flat
//! inside VR, with the controller as the pointer. The window is eframe/egui on OpenGL;
//! it is repainted when something happens (input, a signal, once a second while
//! streaming is enabled for the viewer count) and the process sleeps in between.
//! SIGUSR1 presses the button, for a shell without a pointer.

mod theme;
mod view;

use crate::signals::{Signal, Signals};
use crate::{Streamer, Urls};
use anyhow::{Result, anyhow};
use eframe::egui;
use fl_webrtc::Server;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};
use theme::THEME;
use view::View;

/// Input is ignored for this long after the window opens: the click or key press that
/// launched the app must not also press its button.
const INPUT_GRACE: Duration = Duration::from_millis(800);
/// How often the viewer count on screen is refreshed while streaming is enabled.
const REFRESH: Duration = Duration::from_secs(1);

pub fn run(streamer: &Streamer, signals: Signals) -> Result<()> {
    let options = eframe::NativeOptions {
        // WM_CLASS, which the desktop entry's StartupWMClass names, is not set here:
        // on X11 winit takes it from the name of the binary.
        viewport: egui::ViewportBuilder::default()
            .with_title("Framelight")
            .with_inner_size([1280.0, 720.0]),
        // The palette is flat colours; keep them exact.
        dithering: false,
        ..Default::default()
    };
    eframe::run_native(
        "framelight",
        options,
        Box::new(|cc| Ok(Box::new(Switch::new(cc, streamer, signals)))),
    )
    .map_err(|e| anyhow!("the window: {e}"))
}

struct Switch<'a> {
    streamer: &'a Streamer,
    /// Streaming is enabled exactly while this is here.
    server: Option<Server>,
    /// Why the last attempt to enable it failed.
    error: Option<String>,
    signals: Receiver<Signal>,
    opened: Instant,
    /// Whether the window has been given the size of the screen yet.
    sized: bool,
    view: View,
}

impl<'a> Switch<'a> {
    fn new(cc: &eframe::CreationContext<'_>, streamer: &'a Streamer, signals: Signals) -> Self {
        THEME.install(&cc.egui_ctx);
        // Signals wake the window like input does: a thread asleep on the signalfd
        // hands them over and asks for a frame.
        let (tx, rx) = channel();
        let ctx = cc.egui_ctx.clone();
        std::thread::Builder::new()
            .name("fl-signals".into())
            .spawn(move || {
                while tx.send(signals.wait()).is_ok() {
                    ctx.request_repaint();
                }
            })
            .expect("spawn the signal thread");
        tracing::info!("window open; streaming is off until it is enabled there");
        Self {
            streamer,
            server: None,
            error: None,
            signals: rx,
            opened: Instant::now(),
            sized: false,
            view: View::default(),
        }
    }

    fn toggle(&mut self) {
        self.error = None;
        match self.server.take() {
            Some(server) => {
                tracing::info!("streaming disabled");
                server.shutdown();
            }
            None => match self.streamer.start() {
                Ok(server) => {
                    tracing::info!("streaming enabled: {}", self.streamer.urls());
                    self.server = Some(server);
                }
                Err(e) => {
                    tracing::warn!("cannot enable streaming: {e:#}");
                    self.error = Some(format!("Cannot start: {e:#}"));
                }
            },
        }
    }

    fn view(&self) -> View {
        let status = self.server.as_ref().map(Server::status);
        View {
            enabled: self.server.is_some(),
            urls: match self.server {
                Some(_) => self.streamer.urls(),
                None => Urls::default(),
            },
            viewers: status.as_ref().map_or(0, |s| s.viewers),
            source: status.map(|s| s.source).unwrap_or_default(),
            error: self.error.clone(),
        }
    }
}

impl eframe::App for Switch<'_> {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The layout is in pixels and scales with the window's height by itself.
        ctx.set_pixels_per_point(1.0);
        // The window fills the screen it opens on, as an app on the headset does.
        if !self.sized
            && let Some(screen) = ctx.input(|input| input.viewport().monitor_size)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(screen));
            self.sized = true;
        }
        while let Ok(signal) = self.signals.try_recv() {
            match signal {
                Signal::Stop => {
                    tracing::info!("stopped by a signal");
                    // eframe acts on a close request in the pass after the one that
                    // asked for it. With the window hidden (the headset not worn)
                    // nothing else asks for that pass, so ask here.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    ctx.request_repaint();
                }
                Signal::Toggle => {
                    tracing::info!("button pressed by SIGUSR1");
                    self.toggle();
                }
            }
        }
        if self.server.is_some() {
            ctx.request_repaint_after(REFRESH);
        }
        self.view = self.view();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let pressed = view::show(ui, THEME, &self.view);
        if pressed && self.opened.elapsed() >= INPUT_GRACE {
            tracing::info!("button pressed");
            self.toggle();
            // What was just drawn is the state before the press.
            ui.ctx().request_repaint();
        }
    }

    /// Closing the window ends streaming.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(server) = self.server.take() {
            tracing::info!("window closed; streaming disabled");
            server.shutdown();
        }
    }
}
