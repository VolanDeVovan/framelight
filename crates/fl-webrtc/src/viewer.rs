//! One browser: its peer connection and what the hub has to remember about it.

use crate::h264;
use anyhow::{Context, Result};
use fl_core::H264Profile;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::media::{MediaKind, Mid, Pt};
use str0m::{Candidate, Rtc, RtcConfig};

pub(crate) struct Viewer {
    pub id: u64,
    pub rtc: Rtc,
    pub peer: SocketAddr,
    /// Our candidate: the address this viewer's datagrams arrive on.
    pub local: SocketAddr,
    /// The best profile its offer listed.
    pub max_profile: H264Profile,
    pub joined: Instant,
    pub video: Option<Mid>,
    pub audio: Option<Mid>,
    /// Negotiated H.264 payload types (packetization-mode 1 preferred) by profile.
    video_pts: Vec<(H264Profile, Pt)>,
    pub audio_pt: Option<Pt>,
    /// ICE and DTLS are done.
    pub connected: bool,
    /// Has been sent a keyframe, so everything after it is decodable.
    pub started: bool,
    /// Went quiet: gets no video until it is heard from again.
    pub paused: bool,
    /// Latest loss fraction from its receiver reports (zero while little is sent).
    pub loss: f32,
    /// When its last datagram arrived.
    pub heard: Instant,
    /// Video bytes sent as of the previous report, and when that was.
    pub bytes_sent: u64,
    pub stats_at: Instant,
}

impl Viewer {
    /// Builds the peer connection for one browser and answers its offer. `peer` is the
    /// browser's HTTP address (for the log), `local` the address it reached us on with
    /// the media port: the one candidate we offer.
    pub fn from_offer(
        id: u64,
        sdp: &str,
        peer: SocketAddr,
        local: SocketAddr,
    ) -> Result<(Self, String)> {
        let offer =
            SdpOffer::from_sdp_string(sdp).map_err(|e| anyhow::anyhow!("bad SDP offer: {e}"))?;
        let max_profile = h264::offered_profiles(sdp)
            .into_iter()
            .max()
            .context("this browser does not offer H.264 video over WebRTC")?;

        let now = Instant::now();
        let mut rtc = RtcConfig::new()
            // The browser comes to us: we only answer connectivity checks.
            .set_ice_lite(true)
            .clear_codecs()
            .enable_h264(true)
            .enable_opus(true, false)
            .set_stats_interval(Some(Duration::from_secs(1)))
            .build(now);
        rtc.add_local_candidate(Candidate::host(local, "udp").context("local candidate")?);
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| anyhow::anyhow!("cannot answer the offer: {e}"))?;
        let viewer = Self {
            id,
            rtc,
            peer,
            local,
            max_profile,
            joined: now,
            video: None,
            audio: None,
            video_pts: Vec::new(),
            audio_pt: None,
            connected: false,
            started: false,
            paused: false,
            loss: 0.0,
            heard: now,
            bytes_sent: 0,
            stats_at: now,
        };
        Ok((viewer, answer.to_sdp_string()))
    }

    /// Remembers the payload types negotiated for a media line.
    pub fn learn_media(&mut self, mid: Mid, kind: MediaKind) {
        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };
        match kind {
            MediaKind::Video => {
                let mut found: Vec<(H264Profile, Pt, bool)> = Vec::new();
                for p in writer.payload_params() {
                    let spec = p.spec();
                    let profile = spec.format.profile_level_id.and_then(h264::profile_of);
                    let (Codec::H264, Some(profile)) = (spec.codec, profile) else {
                        continue;
                    };
                    let mode1 = spec.format.packetization_mode == Some(1);
                    match found.iter_mut().find(|f| f.0 == profile) {
                        Some(f) if mode1 && !f.2 => *f = (profile, p.pt(), mode1),
                        Some(_) => {}
                        None => found.push((profile, p.pt(), mode1)),
                    }
                }
                self.video_pts = found.into_iter().map(|f| (f.0, f.1)).collect();
                self.video = Some(mid);
            }
            MediaKind::Audio => {
                self.audio_pt = writer
                    .payload_params()
                    .find(|p| p.spec().codec == Codec::Opus)
                    .map(|p| p.pt());
                self.audio = Some(mid);
            }
        }
    }

    /// The payload type for a stream of `profile`.
    pub fn video_pt(&self, profile: Option<H264Profile>) -> Option<Pt> {
        pick_pt(&self.video_pts, profile)
    }
}

/// The payload type negotiated for exactly `profile`, else the one for the best profile
/// this viewer negotiated (a decoder for a higher profile also takes the lower ones).
fn pick_pt<T: Copy>(pts: &[(H264Profile, T)], profile: Option<H264Profile>) -> Option<T> {
    pts.iter()
        .find(|p| Some(p.0) == profile)
        .or_else(|| pts.iter().max_by_key(|p| p.0))
        .map(|p| p.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_type_follows_the_stream_profile() {
        use H264Profile::*;
        let chrome = [(ConstrainedBaseline, 102), (Main, 106), (High, 108)];
        assert_eq!(pick_pt(&chrome, Some(High)), Some(108));
        assert_eq!(pick_pt(&chrome, Some(ConstrainedBaseline)), Some(102));
        // A viewer that only listed High still gets a stream others forced down to Main.
        assert_eq!(pick_pt(&[(High, 108)], Some(Main)), Some(108));
        // A profile the SPS names but we do not know: the viewer's best.
        assert_eq!(pick_pt(&chrome, None), Some(108));
        assert_eq!(pick_pt::<u8>(&[], Some(High)), None);
    }
}
