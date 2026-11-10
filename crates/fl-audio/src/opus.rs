//! Minimal wrapper over the statically linked libopus encoder.

use anyhow::{Result, bail};
use opusic_sys as sys;

pub struct Encoder {
    st: *mut sys::OpusEncoder,
    channels: usize,
}

// SAFETY: the encoder state is owned exclusively by this value and used from one thread
// at a time (`&mut self`).
unsafe impl Send for Encoder {}

impl Encoder {
    /// CELT-only (restricted low delay), constant bitrate.
    pub fn new(rate: u32, channels: u32, bitrate: u32, complexity: u32) -> Result<Self> {
        let mut err = 0;
        // SAFETY: plain constructor; `err` outlives the call.
        let st = unsafe {
            sys::opus_encoder_create(
                rate as i32,
                channels as i32,
                sys::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut err,
            )
        };
        if st.is_null() || err != sys::OPUS_OK {
            bail!("opus_encoder_create failed ({err})");
        }
        let enc = Self {
            st,
            channels: channels as usize,
        };
        enc.ctl(sys::OPUS_SET_BITRATE_REQUEST, bitrate as i32)?;
        enc.ctl(sys::OPUS_SET_VBR_REQUEST, 0)?;
        enc.ctl(sys::OPUS_SET_COMPLEXITY_REQUEST, complexity as i32)?;
        Ok(enc)
    }

    fn ctl(&self, request: i32, value: i32) -> Result<()> {
        // SAFETY: every request used here takes exactly one opus_int32 argument.
        let r = unsafe { sys::opus_encoder_ctl(self.st, request, value) };
        if r != sys::OPUS_OK {
            bail!("opus_encoder_ctl({request}, {value}) failed ({r})");
        }
        Ok(())
    }

    /// Encode one frame of interleaved samples into `out`; returns the packet length.
    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize> {
        let frame = (pcm.len() / self.channels) as i32;
        // SAFETY: `pcm` holds `frame * channels` samples and `out` is writable for its length.
        let n = unsafe {
            sys::opus_encode(
                self.st,
                pcm.as_ptr(),
                frame,
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        if n < 0 {
            bail!("opus_encode failed ({n})");
        }
        Ok(n as usize)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create, destroyed once.
        unsafe { sys::opus_encoder_destroy(self.st) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbr_celt_packets() {
        // TOC config 28..=31 = CELT fullband 2.5/5/10/20 ms; bit 2 = stereo.
        for (ms, config) in [(5u32, 29u8), (10, 30), (20, 31)] {
            let mut enc = Encoder::new(48_000, 2, 96_000, 5).unwrap();
            let frame = (48 * ms) as usize * 2;
            let mut out = [0u8; 1500];
            let mut lens = Vec::new();
            for i in 0..50 {
                let pcm: Vec<i16> = (0..frame)
                    .map(|n| ((((i * frame + n) / 2) as f32 * 0.115).sin() * 3000.0) as i16)
                    .collect();
                let n = enc.encode(&pcm, &mut out).unwrap();
                assert_eq!(out[0] >> 3, config, "{ms} ms");
                assert_eq!(out[0] & 0x04, 0x04, "stereo");
                lens.push(n);
            }
            assert!(
                lens.iter().all(|&n| n == (96_000 / 8 * ms / 1000) as usize),
                "{ms} ms: {lens:?}"
            );
        }
    }
}
