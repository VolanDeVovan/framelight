//! [`ToneAudioSource`]: a steady sine through the same Opus encoder as the monitor
//! source, paced by the clock. For end-to-end tests of the audio path that must not
//! touch (or depend on) the machine's sound devices.

use crate::{CHANNELS, PACKET, PACKET_SAMPLES, RATE, new_encoder, opus};
use anyhow::{Context, Result};
use bytes::Bytes;
use fl_core::{AudioPacket, AudioSource};
use std::time::Instant;

pub struct ToneAudioSource {
    /// Left channel frequency; the right channel plays an octave above.
    hz: f32,
    run: Option<Running>,
}

struct Running {
    enc: opus::Encoder,
    pcm: Vec<i16>,
    out: Vec<u8>,
    start: Instant,
    emitted: u64,
}

impl ToneAudioSource {
    pub fn new(hz: f32) -> Self {
        Self { hz, run: None }
    }
}

impl AudioSource for ToneAudioSource {
    fn start(&mut self) -> Result<()> {
        self.run = Some(Running {
            enc: new_encoder()?,
            pcm: vec![0; PACKET_SAMPLES],
            out: vec![0; 1500],
            start: Instant::now(),
            emitted: 0,
        });
        Ok(())
    }

    fn next_packet(&mut self) -> Result<AudioPacket> {
        let (hz, packet) = (self.hz, PACKET);
        let r = self.run.as_mut().context("audio source not started")?;
        let due = r.start + packet * r.emitted as u32;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let per_packet = (r.pcm.len() / CHANNELS) as u64;
        for (i, s) in r.pcm.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            let t = (r.emitted * per_packet + i as u64) as f32 / RATE as f32;
            let w = std::f32::consts::TAU * hz * t;
            s[0] = (w.sin() * 8000.0) as i16;
            s[1] = ((2.0 * w).sin() * 8000.0) as i16;
        }
        let n = r.enc.encode(&r.pcm, &mut r.out)?;
        let pts_us = r.emitted * packet.as_micros() as u64;
        r.emitted += 1;
        Ok(AudioPacket {
            data: Bytes::copy_from_slice(&r.out[..n]),
            pts_us,
        })
    }

    fn stop(&mut self) {
        self.run = None;
    }
}
