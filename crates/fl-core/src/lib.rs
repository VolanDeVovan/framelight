//! Contract between the capture/encode half and the GameStream half.

use bytes::Bytes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Clone, Debug)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
}

/// One encoded access unit, Annex-B.
#[derive(Clone, Debug)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub keyframe: bool,
    pub pts_us: u64,
    pub frame_index: u32,
}

/// Produces encoded video for one stream at a time.
///
/// A source holds nothing expensive between [`VideoSource::stop`] and the next
/// [`VideoSource::start`]: the host is idle whenever no client is watching.
pub trait VideoSource: Send {
    /// Open capture and encoder for `cfg`. The first frame must be a keyframe.
    fn start(&mut self, cfg: &StreamConfig) -> anyhow::Result<()>;
    /// Blocks until the next frame is encoded. The caller can only stop the stream
    /// between calls, so this must not block for more than a few seconds: a source that
    /// has nothing to deliver returns an error instead, which ends the stream.
    fn next_frame(&mut self) -> anyhow::Result<EncodedFrame>;
    /// Make an upcoming frame a keyframe that carries its parameter sets.
    fn request_keyframe(&mut self);
    /// Release everything `start` opened. Also called after an error.
    fn stop(&mut self);
}

/// One Opus packet (48 kHz, stereo).
#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub data: Bytes,
    pub pts_us: u64,
}

pub trait AudioSource: Send {
    /// Called before each [`AudioSource::start`] with the Opus packet duration the
    /// client negotiated (`x-nv-aqos.packetDuration`, normally 5 ms). Sources that
    /// produce a fixed duration may ignore it.
    fn set_packet_duration(&mut self, ms: u32) {
        let _ = ms;
    }
    fn start(&mut self) -> anyhow::Result<()>;
    /// Blocks until the next packet is due (one packet duration, give or take). All
    /// packets of a stream should have the same size: the audio FEC needs that.
    fn next_packet(&mut self) -> anyhow::Result<AudioPacket>;
    fn stop(&mut self);
}
