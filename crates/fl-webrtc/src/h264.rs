//! The little of H.264 the streamer has to understand: where the NAL units of an
//! Annex-B access unit are, what the SPS says about profile and level, and how that
//! maps to the SDP `profile-level-id`.

use fl_core::H264Profile;

pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;

/// NAL units of an Annex-B buffer, without start codes (and without the zero bytes
/// that pad the end of a unit).
pub fn nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut pos = start_code(data, 0).map(|(_, payload)| payload);
    std::iter::from_fn(move || {
        let begin = pos?;
        let (end, next) = match start_code(data, begin) {
            Some((s, p)) => (s, Some(p)),
            None => (data.len(), None),
        };
        pos = next;
        let mut end = end;
        while end > begin && data[end - 1] == 0 {
            end -= 1;
        }
        Some(&data[begin..end])
    })
    .filter(|n| !n.is_empty())
}

/// The next `00 00 01` at or after `from`: (where it starts, where the NAL begins).
fn start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        // A start code ends in 0x01, so anything above that lets us skip ahead.
        match data[i + 2] {
            1 if data[i] == 0 && data[i + 1] == 0 => return Some((i, i + 3)),
            0 => i += 1,
            _ => i += 3,
        }
    }
    None
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal[0] & 0x1f
}

/// What a keyframe must look like for a browser to start decoding at it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyInfo {
    pub idr: bool,
    pub sps: Option<Sps>,
}

/// The first three bytes of a sequence parameter set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraints: u8,
    pub level_idc: u8,
}

impl Sps {
    pub fn profile(&self) -> Option<H264Profile> {
        H264Profile::from_idc(self.profile_idc)
    }

    /// As the SDP writes it: profile_idc, constraint flags, level_idc in hex.
    pub fn profile_level_id(&self) -> u32 {
        (self.profile_idc as u32) << 16 | (self.constraints as u32) << 8 | self.level_idc as u32
    }
}

/// Looks at the NAL units before (and including) the first IDR slice.
pub fn inspect(au: &[u8]) -> KeyInfo {
    let mut info = KeyInfo::default();
    for nal in nals(au) {
        match nal_type(nal) {
            NAL_SPS if nal.len() >= 4 => {
                info.sps = Some(Sps {
                    profile_idc: nal[1],
                    constraints: nal[2],
                    level_idc: nal[3],
                })
            }
            NAL_IDR => {
                info.idr = true;
                break;
            }
            1 => break,
            _ => {}
        }
    }
    info
}

/// The profile a `profile-level-id` stands for, as far as decoding goes: plain Baseline
/// counts as Constrained Baseline (the stream never uses Baseline-only tools).
pub fn profile_of(profile_level_id: u32) -> Option<H264Profile> {
    H264Profile::from_idc((profile_level_id >> 16) as u8)
}

/// Every `profile-level-id` in the video section of an SDP offer.
pub fn offered_profiles(sdp: &str) -> Vec<H264Profile> {
    let mut out = Vec::new();
    let mut in_video = false;
    for line in sdp.lines() {
        if let Some(m) = line.strip_prefix("m=") {
            in_video = m.starts_with("video");
        } else if in_video && line.starts_with("a=fmtp:") {
            let id = line
                .split([';', ' '])
                .find_map(|kv| kv.trim().strip_prefix("profile-level-id="))
                .and_then(|v| u32::from_str_radix(v.trim(), 16).ok());
            if let Some(p) = id.and_then(profile_of)
                && !out.contains(&p)
            {
                out.push(p);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn au(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for (i, n) in nals.iter().enumerate() {
            // Mix 3- and 4-byte start codes.
            v.extend_from_slice(if i % 2 == 0 {
                &[0, 0, 0, 1][..]
            } else {
                &[0, 0, 1][..]
            });
            v.extend_from_slice(n);
        }
        v
    }

    #[test]
    fn splits_annex_b() {
        let d = au(&[&[0x67, 100, 0, 42], &[0x68, 3], &[0x65, 9, 0, 0, 3, 1]]);
        let n: Vec<&[u8]> = nals(&d).collect();
        assert_eq!(
            n,
            vec![&[0x67, 100, 0, 42][..], &[0x68, 3], &[0x65, 9, 0, 0, 3, 1]]
        );
        assert_eq!(nals(&[]).count(), 0);
        assert_eq!(nals(&[0, 0, 1]).count(), 0);
        assert_eq!(nals(&[1, 2, 3, 4]).count(), 0);
    }

    #[test]
    fn keyframe_with_parameter_sets() {
        let k = inspect(&au(&[
            &[0x09, 0xf0],
            &[0x67, 100, 0, 42, 7],
            &[0x68, 3],
            &[0x65, 9],
        ]));
        assert!(k.idr);
        let sps = k.sps.unwrap();
        assert_eq!(sps.profile(), Some(H264Profile::High));
        assert_eq!(sps.profile_level_id(), 0x64002a);
        let p = inspect(&au(&[&[0x41, 1]]));
        assert_eq!(p, KeyInfo::default());
        // An IDR without parameter sets is not a place to start decoding.
        let bare = inspect(&au(&[&[0x65, 1]]));
        assert!(bare.idr && bare.sps.is_none());
    }

    #[test]
    fn profiles_in_an_offer() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=fmtp:111 minptime=10;useinbandfec=1\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 102 104 106 108 39\r\n\
                   a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r\n\
                   a=fmtp:104 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f\r\n\
                   a=fmtp:106 profile-level-id=4d001f;packetization-mode=1\r\n\
                   a=fmtp:108 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=64001f\r\n\
                   a=fmtp:39 level-idx=5;profile=0;tier=0\r\n";
        let mut p = offered_profiles(sdp);
        p.sort();
        assert_eq!(
            p,
            vec![
                H264Profile::ConstrainedBaseline,
                H264Profile::Main,
                H264Profile::High
            ]
        );
        assert!(
            offered_profiles("m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 VP8/90000\r\n")
                .is_empty()
        );
    }
}
