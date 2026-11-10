//! The pumps: one thread per source, running it only while somebody watches.
//!
//! They take their orders from the hub through [`Control`] (whether to run, in which
//! profile, at what bitrate, whether a keyframe is due) and send what the sources
//! produce back to it as messages.

use crate::SourceState;
use crate::hub::Msg;
use fl_core::{AudioSource, H264Profile, StreamConfig, VideoSource};
use std::sync::mpsc::Sender;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// How long a pump waits before it starts a failed source again.
const RESTART_DELAY: Duration = Duration::from_secs(2);

/// What the hub wants from the pumps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Want {
    /// Somebody is watching (or left a moment ago): the sources run.
    pub active: bool,
    pub shutdown: bool,
    pub profile: H264Profile,
    pub bitrate_kbps: u32,
    /// A keyframe is due; cleared by the video pump when it passes the request on.
    pub keyframe: bool,
}

pub(crate) struct Control {
    want: Mutex<Want>,
    changed: Condvar,
}

impl Control {
    pub fn new(profile: H264Profile, bitrate_kbps: u32) -> Self {
        Self {
            want: Mutex::new(Want {
                active: false,
                shutdown: false,
                profile,
                bitrate_kbps,
                keyframe: false,
            }),
            changed: Condvar::new(),
        }
    }

    pub fn update(&self, f: impl FnOnce(&mut Want)) {
        let mut w = self.want.lock().unwrap();
        let before = *w;
        f(&mut w);
        if *w != before {
            self.changed.notify_all();
        }
    }

    /// Ends both pumps.
    pub fn shutdown(&self) {
        self.update(|w| {
            w.shutdown = true;
            w.active = false;
        });
    }

    fn get(&self) -> Want {
        *self.want.lock().unwrap()
    }

    /// The current orders; a pending keyframe request is taken with them (so only the
    /// video pump calls this).
    fn take(&self) -> Want {
        let mut w = self.want.lock().unwrap();
        let now = *w;
        w.keyframe = false;
        now
    }

    /// Sleeps until the sources should run; `false` when asked to shut down.
    fn wait_active(&self) -> bool {
        let mut w = self.want.lock().unwrap();
        loop {
            if w.shutdown {
                return false;
            }
            if w.active {
                return true;
            }
            w = self.changed.wait(w).unwrap();
        }
    }

    /// Sleeps for `d`, or less if the sources are told to stop.
    fn pause(&self, d: Duration) {
        let w = self.want.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(w, d, |w| w.active && !w.shutdown)
            .unwrap();
    }
}

/// Why a run of the video source ended.
enum End {
    /// Nobody watches any more (or the streamer is shutting down).
    Stopped,
    /// The profile changed, or a bitrate the source cannot change while running.
    Reconfigure,
    Failed(anyhow::Error),
}

pub(crate) fn video(
    mut src: Box<dyn VideoSource>,
    base: StreamConfig,
    ctl: &Control,
    tx: Sender<Msg>,
) {
    let report = |s: SourceState| {
        let _ = tx.send(Msg::Source(s));
    };
    while ctl.wait_active() {
        // A source starts with a keyframe anyway, so a pending request is spent here.
        let want = ctl.take();
        let cfg = StreamConfig {
            profile: want.profile,
            bitrate_kbps: want.bitrate_kbps,
            ..base.clone()
        };
        report(SourceState::Starting);
        let end = match src.start(&cfg) {
            Ok(()) => {
                report(SourceState::Running);
                run_video(&mut *src, &cfg, ctl, &tx)
            }
            Err(e) => End::Failed(e),
        };
        src.stop();
        match end {
            End::Stopped => report(SourceState::Idle),
            End::Reconfigure => {}
            End::Failed(e) => {
                let e = format!("{e:#}");
                tracing::warn!(error = %e, "video source failed; retrying while viewers stay");
                report(SourceState::Failed(e));
                ctl.pause(RESTART_DELAY);
                if !ctl.get().active {
                    // Nobody is waiting for it any more: nothing is failing either.
                    report(SourceState::Idle);
                }
            }
        }
    }
}

fn run_video(
    src: &mut dyn VideoSource,
    cfg: &StreamConfig,
    ctl: &Control,
    tx: &Sender<Msg>,
) -> End {
    let mut bitrate = cfg.bitrate_kbps;
    loop {
        let want = ctl.take();
        if want.shutdown || !want.active {
            return End::Stopped;
        }
        if want.profile != cfg.profile {
            return End::Reconfigure;
        }
        if want.bitrate_kbps != bitrate {
            bitrate = want.bitrate_kbps;
            if let Err(e) = src.set_bitrate(bitrate) {
                // Not an error to show anyone: starting again applies it too.
                tracing::info!(error = %format!("{e:#}"), "bitrate change needs a restart of the source");
                return End::Reconfigure;
            }
        }
        if want.keyframe {
            src.request_keyframe();
        }
        match src.next_frame() {
            Ok(frame) => {
                if tx.send(Msg::Video(frame)).is_err() {
                    return End::Stopped;
                }
            }
            Err(e) => return End::Failed(e),
        }
    }
}

pub(crate) fn audio(mut src: Box<dyn AudioSource>, ctl: &Control, tx: Sender<Msg>) {
    while ctl.wait_active() {
        let mut result = src.start();
        while result.is_ok() {
            let want = ctl.get();
            if want.shutdown || !want.active {
                break;
            }
            result = src.next_packet().map(|p| {
                let _ = tx.send(Msg::Audio(p));
            });
        }
        src.stop();
        if let Err(e) = result {
            tracing::warn!(error = %format!("{e:#}"), "audio source failed; retrying while viewers stay");
            ctl.pause(RESTART_DELAY);
        }
    }
}
