//! Annex-B access-unit fix-ups for what moonlight-common-c's depacketizer expects.
//!
//! The client recognises a keyframe by its first NAL (after an optional AUD and SEIs),
//! which must be an SPS (H.264) or VPS (HEVC), and it looks for the IRAP slice only
//! inside the frame's first packet. So every keyframe must carry its parameter sets up
//! front, with little else before the first slice. [`FrameFixer`] remembers the latest
//! parameter sets and prepends them to keyframes that arrive without, drops AUD NALs
//! and (on keyframes) oversized prefix SEIs, and swallows access units that hold
//! parameter sets only.
//!
//! Only the NALs before the first slice are looked at, and a frame that needs no change
//! is handed back as it came (no copy): the per-frame cost does not grow with bitrate.

use bytes::Bytes;
use fl_core::Codec;

pub fn nal_type(codec: Codec, first_byte: u8) -> u8 {
    match codec {
        Codec::H264 => first_byte & 0x1f,
        Codec::Hevc => (first_byte >> 1) & 0x3f,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// Parameter set; the index orders them as the decoder needs them
    /// (H.264: SPS, PPS; HEVC: VPS, SPS, PPS).
    Param(usize),
    Aud,
    PrefixSei,
    /// Picture data; `irap` for IDR/IRAP slices.
    Slice { irap: bool },
    Other,
}

fn kind(codec: Codec, t: u8) -> Kind {
    match (codec, t) {
        (Codec::H264, 7) => Kind::Param(0),
        (Codec::H264, 8) => Kind::Param(1),
        (Codec::H264, 9) => Kind::Aud,
        (Codec::H264, 6) => Kind::PrefixSei,
        (Codec::H264, 1..=5) => Kind::Slice { irap: t == 5 },
        (Codec::Hevc, 32..=34) => Kind::Param(t as usize - 32),
        (Codec::Hevc, 35) => Kind::Aud,
        (Codec::Hevc, 39) => Kind::PrefixSei,
        (Codec::Hevc, 0..=31) => Kind::Slice { irap: (16..=21).contains(&t) },
        _ => Kind::Other,
    }
}

/// Position of the next start code at or after `from`: `(start, payload)`, where
/// `start` includes the leading zero of a 4-byte code.
fn next_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        // A start code ends in 0x01, so anything above that lets us skip ahead.
        match data[i + 2] {
            1 if data[i] == 0 && data[i + 1] == 0 => {
                let start = if i > from && data[i - 1] == 0 { i - 1 } else { i };
                return Some((start, i + 3));
            }
            0 => i += 1,
            _ => i += 3,
        }
    }
    None
}

/// Bytes before the first slice that safely fit in the first packet (packet payload is
/// at least ~1000 bytes minus the 8-byte frame header).
const FIRST_PACKET_BUDGET: usize = 900;
const START_CODE: [u8; 4] = [0, 0, 0, 1];

pub struct FrameFixer {
    codec: Codec,
    /// Latest parameter sets by [`Kind::Param`] index, each with start codes.
    params: [Vec<u8>; 3],
}

pub struct Fixed {
    pub data: Bytes,
    /// The access unit holds an IRAP picture (or the source flagged it a keyframe).
    pub keyframe: bool,
    /// A keyframe that starts with its parameter sets: what the client needs to start
    /// (or restart) decoding.
    pub decodable_start: bool,
}

impl FrameFixer {
    pub fn new(codec: Codec) -> Self {
        FrameFixer { codec, params: Default::default() }
    }

    fn have_params(&self) -> bool {
        let needed = match self.codec {
            Codec::H264 => 2,
            Codec::Hevc => 3,
        };
        self.params[..needed].iter().all(|p| !p.is_empty())
    }

    /// `None`: nothing to send (no picture in this access unit).
    pub fn fix(&mut self, au: Bytes, source_keyframe: bool) -> Option<Fixed> {
        let codec = self.codec;
        // NALs before the first slice: (kind, payload start, payload end).
        let mut prefix: Vec<(Kind, usize, usize)> = Vec::new();
        let (first_start, mut payload) = next_start_code(&au, 0)?;
        let slice = loop {
            let k = kind(codec, nal_type(codec, *au.get(payload)?));
            if let Kind::Slice { irap } = k {
                let four = payload >= first_start + 4 && au[payload - 4] == 0;
                break Some((payload - 3 - usize::from(four), irap));
            }
            let Some((next, next_payload)) = next_start_code(&au, payload) else {
                prefix.push((k, payload, au.len()));
                break None;
            };
            let mut end = next;
            while end > payload && au[end - 1] == 0 {
                end -= 1;
            }
            prefix.push((k, payload, end));
            payload = next_payload;
        };

        let mut seen = [false; 3];
        for &(k, s, e) in &prefix {
            if let Kind::Param(i) = k {
                if !std::mem::replace(&mut seen[i], true) {
                    self.params[i].clear();
                }
                self.params[i].extend_from_slice(&START_CODE);
                self.params[i].extend_from_slice(&au[s..e]);
            }
        }
        // Parameter sets on their own (some encoders emit them as a separate buffer) are
        // kept for the next keyframe; the client must not see them as a frame.
        let (slice_start, irap) = slice?;
        let keyframe = irap || source_keyframe;
        let leading = prefix.iter().map(|p| p.0).find(|k| !matches!(k, Kind::Aud | Kind::PrefixSei));
        let starts_with_params = leading == Some(Kind::Param(0));
        let prepend = keyframe && !starts_with_params && self.have_params();
        let decodable_start = keyframe && (starts_with_params || prepend);

        // What stays in front of the first slice.
        let keep = |k: Kind, drop_sei: bool| match k {
            Kind::Aud => false,
            Kind::PrefixSei => !drop_sei,
            Kind::Param(_) => !prepend,
            _ => true,
        };
        let size = |drop_sei: bool| -> usize {
            let cached: usize = if prepend { self.params.iter().map(Vec::len).sum() } else { 0 };
            cached + prefix.iter().filter(|p| keep(p.0, drop_sei)).map(|&(_, s, e)| e - s + 4).sum::<usize>()
        };
        // Encoders such as x265 put a multi-KB info SEI before the first slice.
        let drop_sei = keyframe && size(false) > FIRST_PACKET_BUDGET;

        let untouched = !prepend && prefix.iter().all(|p| keep(p.0, drop_sei));
        let data = if untouched {
            au.slice(first_start..)
        } else {
            let mut out = Vec::with_capacity(size(drop_sei) + au.len() - slice_start);
            if prepend {
                self.params.iter().for_each(|p| out.extend_from_slice(p));
            }
            for &(k, s, e) in &prefix {
                if keep(k, drop_sei) {
                    out.extend_from_slice(&START_CODE);
                    out.extend_from_slice(&au[s..e]);
                }
            }
            out.extend_from_slice(&au[slice_start..]);
            Bytes::from(out)
        };
        Some(Fixed { data, keyframe, decodable_start })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn au(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(n);
        }
        v
    }

    fn fix(f: &mut FrameFixer, data: Vec<u8>, key: bool) -> Fixed {
        f.fix(Bytes::from(data), key).expect("a frame")
    }

    #[test]
    fn finds_three_and_four_byte_start_codes() {
        let d = [0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9];
        assert_eq!(next_start_code(&d, 0), Some((0, 3)));
        assert_eq!(next_start_code(&d, 3), Some((6, 10)));
        assert_eq!(next_start_code(&d, 10), Some((12, 15)));
        assert_eq!(next_start_code(&d, 15), None);
        assert_eq!(next_start_code(&[0, 0], 0), None);
    }

    #[test]
    fn h264_keyframe_without_sps_gets_cached_params_and_loses_aud() {
        let mut f = FrameFixer::new(Codec::H264);
        let first = fix(&mut f, au(&[&[0x09, 0xf0], &[0x67, 1], &[0x68, 2], &[0x65, 3]]), true);
        assert!(first.keyframe && first.decodable_start);
        assert_eq!(first.data, au(&[&[0x67, 1], &[0x68, 2], &[0x65, 3]]));
        let p = fix(&mut f, au(&[&[0x41, 4]]), false);
        assert!(!p.keyframe && !p.decodable_start);
        let bare_idr = fix(&mut f, au(&[&[0x09, 0xf0], &[0x65, 5]]), false);
        assert!(bare_idr.keyframe && bare_idr.decodable_start);
        assert_eq!(bare_idr.data, au(&[&[0x67, 1], &[0x68, 2], &[0x65, 5]]));
    }

    #[test]
    fn untouched_frames_are_not_copied() {
        let mut f = FrameFixer::new(Codec::H264);
        let src = Bytes::from(au(&[&[0x67, 1], &[0x68, 2], &[0x65, 3, 0, 0, 1, 0x65, 4]]));
        let out = f.fix(src.clone(), true).unwrap();
        assert_eq!(out.data.as_ptr(), src.as_ptr());
        let p = Bytes::from(au(&[&[0x41, 4, 0, 0, 0]]));
        assert_eq!(f.fix(p.clone(), false).unwrap().data.as_ptr(), p.as_ptr());
        // Leading bytes before the first start code are cut off without a copy.
        let padded = Bytes::from([&[0u8, 0][..], &au(&[&[0x41, 9]])].concat());
        let out = f.fix(padded.clone(), false).unwrap();
        assert_eq!(out.data, au(&[&[0x41, 9]]));
        assert_eq!(out.data.as_ptr(), padded[2..].as_ptr());
    }

    #[test]
    fn a_stray_pps_does_not_lose_the_sps() {
        let mut f = FrameFixer::new(Codec::H264);
        fix(&mut f, au(&[&[0x67, 1], &[0x68, 2], &[0x65, 3]]), true);
        // A P-frame with a PPS in front (Intel's Media Foundation encoder does this).
        let p = fix(&mut f, au(&[&[0x68, 7], &[0x41, 4]]), false);
        assert_eq!(p.data, au(&[&[0x68, 7], &[0x41, 4]]));
        let idr = fix(&mut f, au(&[&[0x65, 5]]), true);
        assert!(idr.decodable_start);
        assert_eq!(idr.data, au(&[&[0x67, 1], &[0x68, 7], &[0x65, 5]]));
    }

    #[test]
    fn parameter_sets_alone_are_held_back_for_the_next_keyframe() {
        let mut f = FrameFixer::new(Codec::Hevc);
        assert!(f.fix(Bytes::from(au(&[&[32 << 1, 1], &[33 << 1, 1], &[34 << 1, 1]])), true).is_none());
        let k = fix(&mut f, au(&[&[19 << 1, 1, 7]]), true);
        assert!(k.keyframe && k.decodable_start);
        assert_eq!(k.data, au(&[&[32 << 1, 1], &[33 << 1, 1], &[34 << 1, 1], &[19 << 1, 1, 7]]));
    }

    #[test]
    fn keyframe_without_any_parameter_sets_is_not_a_decodable_start() {
        let mut f = FrameFixer::new(Codec::Hevc);
        let k = fix(&mut f, au(&[&[19 << 1, 1, 7]]), true);
        assert!(k.keyframe && !k.decodable_start);
    }

    #[test]
    fn garbage_is_dropped_not_panicked_on() {
        let mut f = FrameFixer::new(Codec::H264);
        for d in [&[][..], &[1, 2, 3], &[0, 0, 1], &[0, 0, 0, 1], &[0, 0, 0, 0, 0]] {
            assert!(f.fix(Bytes::copy_from_slice(d), true).is_none());
        }
    }

    #[test]
    fn oversized_prefix_sei_is_dropped_on_keyframes() {
        let mut f = FrameFixer::new(Codec::Hevc);
        let mut sei = vec![39 << 1, 1];
        sei.extend(std::iter::repeat_n(0x55, 2400));
        let k = fix(&mut f, au(&[&[32 << 1, 1], &[33 << 1, 1], &[34 << 1, 1], &sei, &[21 << 1, 1, 9]]), false);
        assert_eq!(k.data, au(&[&[32 << 1, 1], &[33 << 1, 1], &[34 << 1, 1], &[21 << 1, 1, 9]]));
        // A small SEI stays.
        let k = fix(&mut f, au(&[&[32 << 1, 1], &[39 << 1, 1, 5], &[21 << 1, 1, 9]]), false);
        assert_eq!(k.data.len(), 4 * 3 + 2 + 3 + 3);
    }

    #[test]
    fn hevc_irap_detection_and_vps_first() {
        let mut f = FrameFixer::new(Codec::Hevc);
        // AUD(35) VPS(32) SPS(33) PPS(34) IDR_W_RADL(19)
        let k = fix(
            &mut f,
            au(&[&[35 << 1, 1, 0x50], &[32 << 1, 1], &[33 << 1, 1], &[34 << 1, 1], &[19 << 1, 1, 7]]),
            false,
        );
        assert!(k.keyframe && k.decodable_start);
        assert_eq!(k.data[4], 32 << 1);
        let p = fix(&mut f, au(&[&[1 << 1, 1, 8]]), false);
        assert!(!p.keyframe);
    }
}
