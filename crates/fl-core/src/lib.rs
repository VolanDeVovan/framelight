//! Contract between the capture/encode half and the WebRTC half.

use bytes::Bytes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    /// Diagnostics only (`framelight probe`): browsers do not take HEVC over WebRTC.
    Hevc,
}

/// H.264 profiles a browser may ask for, weakest first, so the profile every viewer can
/// decode is the minimum over the viewers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum H264Profile {
    ConstrainedBaseline,
    Main,
    High,
}

impl H264Profile {
    /// `profile_idc` as it appears in the SPS and in the SDP `profile-level-id`.
    pub fn idc(self) -> u8 {
        match self {
            H264Profile::ConstrainedBaseline => 66,
            H264Profile::Main => 77,
            H264Profile::High => 100,
        }
    }

    pub fn from_idc(idc: u8) -> Option<Self> {
        match idc {
            66 => Some(H264Profile::ConstrainedBaseline),
            77 => Some(H264Profile::Main),
            100 => Some(H264Profile::High),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    /// Ignored for HEVC.
    pub profile: H264Profile,
}

/// One encoded access unit, Annex-B.
#[derive(Clone, Debug)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub keyframe: bool,
    /// When the picture was taken; viewers' clocks are synchronised to this.
    pub captured: std::time::Instant,
    /// Time since the source started.
    pub pts_us: u64,
    pub frame_index: u32,
}

/// Produces encoded video; one encode is shared by every viewer.
///
/// A source holds nothing expensive between [`VideoSource::stop`] and the next
/// [`VideoSource::start`]: the host is idle whenever nobody is watching.
pub trait VideoSource: Send {
    /// Open capture and encoder for `cfg`. The first frame must be a keyframe, and every
    /// keyframe must carry its parameter sets (SPS/PPS).
    fn start(&mut self, cfg: &StreamConfig) -> anyhow::Result<()>;
    /// Blocks until the next frame is encoded. The caller can only stop the stream
    /// between calls, so this must not block for more than a few seconds: a source that
    /// has nothing to deliver returns an error instead, and the caller starts it again.
    fn next_frame(&mut self) -> anyhow::Result<EncodedFrame>;
    /// Make an upcoming frame a keyframe.
    fn request_keyframe(&mut self);
    /// Change the target bitrate of the running stream without a visible interruption.
    fn set_bitrate(&mut self, kbps: u32) -> anyhow::Result<()>;
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
    fn start(&mut self) -> anyhow::Result<()>;
    /// Blocks until the next packet is due (one packet duration, give or take).
    fn next_packet(&mut self) -> anyhow::Result<AudioPacket>;
    fn stop(&mut self);
}
