//! GameStream video packetization (what moonlight-common-c `RtpVideoQueue.c` and
//! `VideoDepacketizer.c` expect from a 7.1.431 "Sunshine" host).
//!
//! Each UDP datagram ("shard") is `16 + packet_size` bytes, always full size:
//!
//! ```text
//!  0..12  RTP: 0x90 (v2 + extension bit), pt 0, seq BE16, ts BE32 (90 kHz), ssrc 0
//! 12..16  RTP extension word, zero
//! 16..32  NV_VIDEO_PACKET, little-endian:
//!           streamPacketIndex u32 (data-packet counter << 8)
//!           frameIndex u32 (starts at 1, +1 per frame)
//!           flags u8 (0x1 pic data, 0x2 EOF, 0x4 SOF — per FEC block)
//!           extraFlags u8 (0), multiFecFlags u8 (0x10)
//!           multiFecBlocks u8 (block << 4 | lastBlock << 6)
//!           fecInfo u32 (dataShards << 22 | shardIndex << 12 | fecPercent << 4)
//! 32..    payload, zero padded. The frame's payload stream is an 8-byte frame header
//!         `01 | latency u16 LE | type (1 = P, 2 = IDR) | lastPayloadLen u32 LE`
//!         followed by the Annex-B access unit.
//! ```
//!
//! A frame is split into at most four FEC blocks. Parity shards are Reed-Solomon over
//! the whole shards (headers included), then get their own RTP and NV header fields
//! written over the parity bytes; the client rewrites exactly those fields in any data
//! shard it recovers.

use crate::fec::{MAX_SHARDS, ReedSolomon};
use std::collections::HashMap;

pub const RTP_HEADER: usize = 16;
pub const NV_HEADER: usize = 16;
const FRAME_HEADER: usize = 8;
const MAX_BLOCKS: usize = 4;
/// `fecInfo` carries the data-shard count in ten bits.
const MAX_DATA_PER_BLOCK: usize = 1023;

const FLAG_PIC: u8 = 0x1;
const FLAG_EOF: u8 = 0x2;
const FLAG_SOF: u8 = 0x4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Block {
    start: usize,
    data: usize,
    parity: usize,
    percent: usize,
}

/// `(parity, percent)` for a block of `data` shards. The client derives parity as
/// `ceil(data * percent / 100)`, so the percentage is what goes on the wire.
fn fec_params(data: usize, percent: usize, min_parity: usize) -> (usize, usize) {
    let max_parity = MAX_SHARDS.saturating_sub(data);
    if max_parity == 0 {
        return (0, 0);
    }
    let min_parity = min_parity.min(max_parity);
    let min_percent = if min_parity == 0 { 0 } else { (min_parity - 1) * 100 / data + 1 };
    let max_percent = (max_parity * 100 / data).min(255);
    let p = percent.max(min_percent).min(max_percent);
    ((data * p).div_ceil(100), p)
}

fn plan(n: usize, percent: usize, min_parity: usize) -> Option<Vec<Block>> {
    if n == 0 || n > MAX_BLOCKS * MAX_DATA_PER_BLOCK {
        return None;
    }
    let per_block = (MAX_SHARDS * 100 / (100 + percent)).max(1);
    let blocks = n.div_ceil(per_block).clamp(1, MAX_BLOCKS);
    let (base, extra) = (n / blocks, n % blocks);
    let mut out = Vec::with_capacity(blocks);
    let mut start = 0;
    for b in 0..blocks {
        let data = base + usize::from(b < extra);
        let (parity, percent) = fec_params(data, percent, min_parity);
        out.push(Block { start, data, parity, percent });
        start += data;
    }
    Some(out)
}

pub struct Packetizer {
    packet_size: usize,
    fec_percent: usize,
    min_parity: usize,
    seq: u16,
    stream_packet_index: u32,
    encoders: HashMap<(usize, usize), ReedSolomon>,
    /// The current frame's shards back to back, reused from frame to frame.
    buf: Vec<u8>,
}

impl Packetizer {
    /// `packet_size` is the client's `x-nv-video[0].packetSize`: NV header + payload.
    pub fn new(packet_size: usize, fec_percent: u8, min_parity: u32) -> Self {
        Packetizer {
            packet_size: packet_size.clamp(NV_HEADER + 64, 1500),
            fec_percent: fec_percent as usize,
            min_parity: min_parity as usize,
            seq: 0,
            stream_packet_index: 0,
            encoders: HashMap::new(),
            buf: Vec::new(),
        }
    }

    /// Size of every datagram.
    pub fn shard_size(&self) -> usize {
        RTP_HEADER + self.packet_size
    }

    /// Split one access unit into wire shards (data and parity, in send order), returned
    /// back to back, each [`Packetizer::shard_size`] bytes. `None` when the frame is too
    /// large for the wire format.
    pub fn packetize(
        &mut self,
        au: &[u8],
        keyframe: bool,
        frame_index: u32,
        rtp_ts: u32,
        latency_100us: u16,
    ) -> Option<&[u8]> {
        let payload_per = self.packet_size - NV_HEADER;
        let total = FRAME_HEADER + au.len();
        let n = total.div_ceil(payload_per);
        let blocks = plan(n, self.fec_percent, self.min_parity)?;
        let last_len = match total % payload_per {
            0 => payload_per,
            r => r,
        };
        let mut header = [0u8; FRAME_HEADER];
        header[0] = 0x01;
        header[1..3].copy_from_slice(&latency_100us.to_le_bytes());
        header[3] = if keyframe { 2 } else { 1 };
        header[4..8].copy_from_slice(&(last_len as u32).to_le_bytes());

        let shard_size = self.shard_size();
        let last_block = (blocks.len() - 1) as u8;
        let shards = n + blocks.iter().map(|b| b.parity).sum::<usize>();
        // Zeroed: the tail of the last data shard is padding the client relies on.
        self.buf.clear();
        self.buf.resize(shards * shard_size, 0);
        let mut rest = self.buf.as_mut_slice();
        for (bi, b) in blocks.iter().enumerate() {
            let multi = ((bi as u8) << 4) | (last_block << 6);
            let (block, tail) = rest.split_at_mut((b.data + b.parity) * shard_size);
            rest = tail;
            let (data, parity) = block.split_at_mut(b.data * shard_size);
            for (k, s) in data.chunks_exact_mut(shard_size).enumerate() {
                write_rtp(s, self.seq, rtp_ts);
                self.seq = self.seq.wrapping_add(1);
                let mut flags = FLAG_PIC;
                if k == 0 {
                    flags |= FLAG_SOF;
                }
                if k == b.data - 1 {
                    flags |= FLAG_EOF;
                }
                let nv = &mut s[RTP_HEADER..RTP_HEADER + NV_HEADER];
                nv[0..4].copy_from_slice(&(self.stream_packet_index << 8).to_le_bytes());
                self.stream_packet_index = self.stream_packet_index.wrapping_add(1);
                nv[4..8].copy_from_slice(&frame_index.to_le_bytes());
                nv[8] = flags;
                nv[10] = 0x10;
                nv[11] = multi;
                nv[12..16].copy_from_slice(&fec_info(k, b).to_le_bytes());
                // This shard's slice of [frame header ++ AU].
                copy_stream(&mut s[RTP_HEADER + NV_HEADER..], &header, au, (b.start + k) * payload_per);
            }
            if b.parity > 0 {
                let rs = self
                    .encoders
                    .entry((b.data, b.parity))
                    .or_insert_with(|| ReedSolomon::new(b.data, b.parity).expect("valid geometry"));
                rs.encode(data, parity, shard_size);
                for (j, s) in parity.chunks_exact_mut(shard_size).enumerate() {
                    write_rtp(s, self.seq, rtp_ts);
                    self.seq = self.seq.wrapping_add(1);
                    let nv = &mut s[RTP_HEADER..RTP_HEADER + NV_HEADER];
                    nv[4..8].copy_from_slice(&frame_index.to_le_bytes());
                    nv[10] = 0x10;
                    nv[11] = multi;
                    nv[12..16].copy_from_slice(&fec_info(b.data + j, b).to_le_bytes());
                }
            }
        }
        Some(&self.buf)
    }
}

fn fec_info(index: usize, b: &Block) -> u32 {
    ((b.data as u32) << 22) | ((index as u32) << 12) | ((b.percent as u32) << 4)
}

fn write_rtp(s: &mut [u8], seq: u16, ts: u32) {
    s[0] = 0x90;
    s[1] = 0;
    s[2..4].copy_from_slice(&seq.to_be_bytes());
    s[4..8].copy_from_slice(&ts.to_be_bytes());
    s[8..12].fill(0);
}

/// Copy `[header ++ au][off .. off + dst.len()]` into `dst`; the remainder stays zero.
fn copy_stream(dst: &mut [u8], header: &[u8], au: &[u8], off: usize) {
    let mut w = 0;
    let mut pos = off;
    if pos < header.len() {
        let n = (header.len() - pos).min(dst.len());
        dst[..n].copy_from_slice(&header[pos..pos + n]);
        w += n;
        pos += n;
    }
    if w < dst.len() {
        let a = pos - header.len();
        if a < au.len() {
            let n = (au.len() - a).min(dst.len() - w);
            dst[w..w + n].copy_from_slice(&au[a..a + n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fec::tests::decode;

    fn packetize(p: &mut Packetizer, au: &[u8], key: bool, frame: u32, ts: u32) -> Option<Vec<Vec<u8>>> {
        let size = p.shard_size();
        Some(p.packetize(au, key, frame, ts, 0)?.chunks_exact(size).map(<[u8]>::to_vec).collect())
    }

    fn nv(s: &[u8]) -> (u32, u32, u8, u8, u32) {
        let h = &s[RTP_HEADER..RTP_HEADER + NV_HEADER];
        (
            u32::from_le_bytes(h[0..4].try_into().unwrap()),
            u32::from_le_bytes(h[4..8].try_into().unwrap()),
            h[8],
            h[11],
            u32::from_le_bytes(h[12..16].try_into().unwrap()),
        )
    }

    #[test]
    fn fec_params_respect_wire_rules() {
        for data in 1..=300 {
            let (p, pct) = fec_params(data, 20, 2);
            assert_eq!(p, (data * pct).div_ceil(100), "client-derived parity for {data}");
            assert!(data + p <= MAX_SHARDS || p == 0, "{data}+{p}");
            if data <= 212 {
                assert!(p >= 2, "min parity for {data}");
            }
            assert!(pct <= 255);
        }
    }

    #[test]
    fn single_block_frame_layout_and_payload() {
        let au: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let mut p = Packetizer::new(1024, 20, 2);
        let shards = packetize(&mut p, &au, true, 1, 9000).unwrap();
        let payload_per = 1024 - NV_HEADER;
        let n = (au.len() + 8).div_ceil(payload_per);
        let (data, parity) = (n, fec_params(n, 20, 2).0);
        assert_eq!(shards.len(), data + parity);
        for (i, s) in shards.iter().enumerate() {
            assert_eq!(s.len(), 16 + 1024);
            assert_eq!(s[0], 0x90);
            assert_eq!(u16::from_be_bytes([s[2], s[3]]), i as u16);
            let (_, frame, _, multi, info) = nv(s);
            assert_eq!(frame, 1);
            assert_eq!(multi, 0);
            assert_eq!((info >> 22) as usize, data);
            assert_eq!(((info >> 12) & 0x3ff) as usize, i);
        }
        let (spi0, _, f0, _, _) = nv(&shards[0]);
        assert_eq!(spi0, 0);
        assert_eq!(f0, FLAG_PIC | FLAG_SOF);
        assert_eq!(nv(&shards[data - 1]).2, FLAG_PIC | FLAG_EOF);
        // The buffer is reused: a second, smaller frame must not see the first one's bytes.
        let small = packetize(&mut p, &[0xab; 10], false, 2, 9100).unwrap();
        assert_eq!(small[0][32 + 8..32 + 18], [0xab; 10]);
        assert!(small[0][32 + 18..].iter().all(|&b| b == 0));
        // Reassemble the payload stream.
        let mut stream = Vec::new();
        for s in &shards[..data] {
            stream.extend_from_slice(&s[32..]);
        }
        assert_eq!(&stream[..4], &[1, 0, 0, 2]);
        assert_eq!(&stream[8..8 + au.len()], &au[..]);
    }

    #[test]
    fn lost_data_shards_are_recoverable_like_the_client_does() {
        let au: Vec<u8> = (0..20000u32).map(|i| (i * 7 % 253) as u8).collect();
        let mut p = Packetizer::new(1392, 20, 2);
        let shards = packetize(&mut p, &au, false, 5, 1234).unwrap();
        let (_, _, _, _, info) = nv(&shards[0]);
        let data = (info >> 22) as usize;
        let pct = ((info >> 4) & 0xff) as usize;
        let parity = (data * pct).div_ceil(100);
        assert_eq!(shards.len(), data + parity);
        let rs = ReedSolomon::new(data, parity).unwrap();
        let mut held: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
        held[0] = None;
        held[data / 2] = None;
        decode(&rs, &mut held, shards[0].len()).unwrap();
        for i in [0, data / 2] {
            let rec = held[i].as_ref().unwrap();
            // Fields the client does not rewrite must come back intact.
            assert_eq!(rec[16..20], shards[i][16..20], "streamPacketIndex");
            assert_eq!(rec[24], shards[i][24], "flags");
            assert_eq!(rec[32..], shards[i][32..], "payload");
        }
    }

    #[test]
    fn big_frames_use_multiple_blocks() {
        let au = vec![0x55u8; 600 * 1376];
        let mut p = Packetizer::new(1392, 20, 2);
        let shards = packetize(&mut p, &au, true, 2, 0).unwrap();
        let blocks: std::collections::BTreeSet<u8> = shards.iter().map(|s| (nv(s).3 >> 4) & 3).collect();
        assert!(blocks.len() >= 3);
        assert!(shards.iter().all(|s| nv(s).3 >> 6 == (blocks.len() - 1) as u8));
        // Data packet indices are contiguous across blocks.
        let mut spis: Vec<u32> = shards
            .iter()
            .filter(|s| nv(s).2 & FLAG_PIC != 0 && (((nv(s).4 >> 12) & 0x3ff) < (nv(s).4 >> 22)))
            .map(|s| nv(s).0 >> 8)
            .collect();
        spis.dedup();
        assert!(spis.windows(2).all(|w| w[1] == w[0] + 1));
    }

    #[test]
    fn oversized_frame_is_refused() {
        let mut p = Packetizer::new(1392, 20, 2);
        assert!(p.packetize(&vec![0u8; 4 * 1023 * 1376 + 1], true, 1, 0, 0).is_none());
    }
}
