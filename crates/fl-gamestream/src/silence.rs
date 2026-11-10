//! An [`AudioSource`] that produces silence without an encoder, and the Opus packet
//! helpers the audio sender needs.

use bytes::Bytes;
use fl_core::{AudioPacket, AudioSource};
use std::time::{Duration, Instant};

const PACKET: Duration = Duration::from_millis(5);
/// TOC: config 29 (CELT fullband, 5 ms), stereo, one frame.
const TOC_CELT_FB_5MS_STEREO: u8 = (29 << 3) | 0x04;
/// A CELT frame that starts with these bytes has its silence flag set: it decodes to
/// digital silence whatever the frame size and channel count.
const CELT_SILENCE: [u8; 2] = [0xff, 0xfe];

/// One silent 5 ms stereo Opus packet every 5 ms: keeps the client's audio pipeline fed.
#[derive(Default)]
pub struct SilenceAudioSource {
    start: Option<Instant>,
    sent: u64,
}

impl SilenceAudioSource {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AudioSource for SilenceAudioSource {
    fn start(&mut self) -> anyhow::Result<()> {
        self.start = Some(Instant::now());
        self.sent = 0;
        Ok(())
    }

    fn next_packet(&mut self) -> anyhow::Result<AudioPacket> {
        const SILENCE: [u8; 3] = [TOC_CELT_FB_5MS_STEREO, CELT_SILENCE[0], CELT_SILENCE[1]];
        let start = *self.start.get_or_insert_with(Instant::now);
        let due = start + PACKET * self.sent as u32;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        } else if now - due > Duration::from_millis(200) {
            // Fell far behind (suspend, debugger): resync instead of bursting.
            self.start = Some(now);
            self.sent = 0;
        }
        let pts_us = self.sent * PACKET.as_micros() as u64;
        self.sent += 1;
        Ok(AudioPacket { data: Bytes::from_static(&SILENCE), pts_us })
    }

    fn stop(&mut self) {
        self.start = None;
    }
}

/// Turn a stereo Opus packet into the two-stream packet a surround client decodes
/// (see the `surround-params` in RTSP DESCRIBE): the stereo stream in self-delimited
/// framing (RFC 6716 appendix B), then a silent mono stream of the same duration.
/// `false` (nothing appended) for packets other than one frame per packet (code 0),
/// which is all the sources here produce.
pub(crate) fn append_as_surround(stereo: &[u8], out: &mut Vec<u8>) -> bool {
    let Some((&toc, frame)) = stereo.split_first() else {
        return false;
    };
    if toc & 0x03 != 0 {
        return false;
    }
    out.push(toc);
    match frame.len() {
        n @ 0..=251 => out.push(n as u8),
        n => {
            let first = 252 + (n & 3);
            out.extend_from_slice(&[first as u8, ((n - first) / 4) as u8]);
        }
    }
    out.extend_from_slice(frame);
    // Same configuration (hence duration), mono.
    out.push(toc & !0x04);
    out.extend_from_slice(&CELT_SILENCE);
    true
}

/// Duration of an Opus packet in milliseconds from its TOC (RFC 6716 §3.1), rounded up.
pub(crate) fn opus_packet_ms(data: &[u8]) -> Option<u32> {
    let toc = *data.first()?;
    let config = toc >> 3;
    // Frame size in units of 0.5 ms.
    let half_ms: u32 = match config {
        0..=11 => [20, 40, 80, 120][(config % 4) as usize],
        12..=15 => [20, 40][(config % 2) as usize],
        _ => [5, 10, 20, 40][(config % 4) as usize],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*data.get(1)? & 0x3f) as u32,
    };
    Some((half_ms * frames).div_ceil(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surround_packets_are_self_delimited_stereo_plus_silent_mono() {
        let mut out = Vec::new();
        assert!(append_as_surround(&[TOC_CELT_FB_5MS_STEREO, 1, 2, 3], &mut out));
        assert_eq!(out, [TOC_CELT_FB_5MS_STEREO, 3, 1, 2, 3, 29 << 3, 0xff, 0xfe]);
        // Two-byte length: 300 = 252 + 4 * 12.
        let mut big = vec![TOC_CELT_FB_5MS_STEREO];
        big.extend(std::iter::repeat_n(7, 300));
        out.clear();
        assert!(append_as_surround(&big, &mut out));
        assert_eq!(out[..3], [TOC_CELT_FB_5MS_STEREO, 252, 12]);
        assert_eq!(out.len(), 3 + 300 + 3);
        // Two frames per packet (code 1) cannot be re-framed this simply.
        out.clear();
        assert!(!append_as_surround(&[TOC_CELT_FB_5MS_STEREO | 1, 1, 2], &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn toc_durations() {
        assert_eq!(opus_packet_ms(&[TOC_CELT_FB_5MS_STEREO]), Some(5));
        assert_eq!(opus_packet_ms(&[(31 << 3) | 0x04]), Some(20));
        assert_eq!(opus_packet_ms(&[(30 << 3) | 1, 0]), Some(20)); // two 10 ms frames
        assert_eq!(opus_packet_ms(&[(28 << 3) | 3, 3]), Some(8)); // three 2.5 ms frames, rounded up
    }
}
