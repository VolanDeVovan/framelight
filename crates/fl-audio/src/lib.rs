//! [`MonitorAudioSource`]: what the device plays (the default sink's monitor), as
//! 48 kHz stereo CBR Opus packets of [`PACKET_MS`] each.
//!
//! A capture thread (see [`pulse`]) records `@DEFAULT_MONITOR@` through a dlopen'd
//! libpulse (pipewire-pulse on SteamOS and the desktop) with one-packet fragments into a
//! small bounded ring. [`AudioSource::next_packet`] takes one packet of samples as soon
//! as they are there, so pacing follows the sound card's clock. When no data comes
//! (server down, sink suspended, capture stalled) it falls back to encoded silence at
//! real-time pace after [`STALL_SLACK`], so the viewers' audio clock never starves.

mod opus;
mod pulse;
mod tone;

pub use tone::ToneAudioSource;

use anyhow::{Context, Result};
use bytes::Bytes;
use fl_core::{AudioPacket, AudioSource};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// How long to wait for captured data past a packet's due time before sending silence.
/// Covers PipeWire delivering in bursts of one graph quantum (up to 1024 frames at 48 kHz).
pub const STALL_SLACK: Duration = Duration::from_millis(40);
/// Ring capacity: one large quantum plus slack; older samples are dropped beyond it.
const RING_MS: u32 = 60;
/// Behind by more than this (suspend, debugger): resync instead of bursting silence.
const RESYNC: Duration = Duration::from_millis(200);

/// Opus packet duration: what browsers expect by default (`ptime` 20).
pub const PACKET_MS: u32 = 20;

pub struct MonitorAudioSource {
    /// Opus bitrate (CBR), bits per second.
    pub bitrate: u32,
    /// Opus encoder complexity, 0..=10.
    pub complexity: u32,
    /// libpulse, loaded on the first start; `None` if it is not available.
    pulse: Option<Arc<pulse::Pulse>>,
    run: Option<Running>,
}

struct Running {
    shared: Arc<pulse::Shared>,
    thread: Option<JoinHandle<()>>,
    enc: opus::Encoder,
    /// Interleaved samples per packet.
    frame: usize,
    packet: Duration,
    pcm: Vec<i16>,
    out: Vec<u8>,
    /// Packet `k` is due at `base + k * packet`.
    base: Instant,
    emitted: u64,
    silent: bool,
}

impl Default for MonitorAudioSource {
    fn default() -> Self {
        Self::new()
    }
}

impl MonitorAudioSource {
    pub fn new() -> Self {
        Self { bitrate: 96_000, complexity: 5, pulse: None, run: None }
    }
}

impl AudioSource for MonitorAudioSource {
    fn start(&mut self) -> Result<()> {
        self.stop();
        if self.pulse.is_none() {
            match pulse::Pulse::load() {
                Ok(p) => self.pulse = Some(Arc::new(p)),
                // Keep the viewers' audio clock fed anyway: silence at real-time pace.
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "audio: cannot capture the monitor; sending silence")
                }
            }
        }
        let frame = (RATE * PACKET_MS / 1000) as usize * CHANNELS;
        let cap = (RATE * RING_MS / 1000) as usize * CHANNELS;
        let shared = Arc::new(pulse::Shared::new(cap, frame));
        let enc = opus::Encoder::new(RATE, CHANNELS as u32, self.bitrate, self.complexity)?;
        let frag_bytes = (frame * 2) as u32;
        let thread = match self.pulse.clone() {
            Some(pa) => {
                let shared = shared.clone();
                Some(
                    std::thread::Builder::new()
                        .name("fl-audio-cap".into())
                        .spawn(move || pulse::run(pa, shared, RATE, CHANNELS as u8, frag_bytes))
                        .context("spawn audio capture thread")?,
                )
            }
            None => None,
        };
        tracing::info!(packet_ms = PACKET_MS, bitrate = self.bitrate, "audio: monitor source started");
        self.run = Some(Running {
            shared,
            thread,
            enc,
            frame,
            packet: Duration::from_millis(PACKET_MS as u64),
            pcm: vec![0; frame],
            out: vec![0; 1500],
            base: Instant::now(),
            emitted: 0,
            silent: false,
        });
        Ok(())
    }

    fn next_packet(&mut self) -> Result<AudioPacket> {
        let r = self.run.as_mut().context("audio source not started")?;
        let due = r.base + r.packet * r.emitted as u32;
        let now = Instant::now();
        if now > due + RESYNC {
            r.base = now - r.packet * r.emitted as u32;
        }
        let due = r.base + r.packet * r.emitted as u32;
        // While already sending silence, keep its real-time pace; otherwise give the
        // capture some slack for quantum-sized bursts.
        let deadline = if r.silent { due } else { due + STALL_SLACK };
        let real = {
            let mut g = r.shared.ring.lock().unwrap();
            loop {
                if g.buf.len() >= r.frame {
                    for (d, s) in r.pcm.iter_mut().zip(g.buf.drain(..r.frame)) {
                        *d = s;
                    }
                    break true;
                }
                let now = Instant::now();
                if now >= deadline {
                    break false;
                }
                g = r.shared.cond.wait_timeout(g, deadline - now).unwrap().0;
            }
        };
        if real {
            if r.silent {
                tracing::debug!("audio: monitor data resumed");
            }
            r.silent = false;
            // Follow the capture clock: never let the silence deadline lag behind now.
            let now = Instant::now();
            if r.base + r.packet * ((r.emitted + 1) as u32) < now {
                r.base = now - r.packet * (r.emitted + 1) as u32;
            }
        } else {
            if !r.silent {
                tracing::debug!("audio: no monitor data; sending silence");
            }
            r.silent = true;
            r.pcm.fill(0);
        }
        let n = r.enc.encode(&r.pcm, &mut r.out)?;
        let pts_us = r.emitted * r.packet.as_micros() as u64;
        r.emitted += 1;
        Ok(AudioPacket { data: Bytes::copy_from_slice(&r.out[..n]), pts_us })
    }

    fn stop(&mut self) {
        if let Some(mut r) = self.run.take() {
            r.shared.stop.store(true, Ordering::SeqCst);
            r.shared.cond.notify_all();
            // The capture thread polls at most 50 ms (or sleeps 100 ms in backoff), then
            // disconnects the stream and frees the context and mainloop.
            if let Some(t) = r.thread.take() {
                let _ = t.join();
            }
            tracing::debug!(packets = r.emitted, "audio: monitor source stopped");
        }
    }
}

impl Drop for MonitorAudioSource {
    fn drop(&mut self) {
        self.stop();
    }
}
