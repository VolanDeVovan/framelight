//! Video and audio sender threads, one of each per stream.
//!
//! Both learn the client's UDP endpoint from its pings (`X-SS-Ping-Payload` + BE seq,
//! or the legacy `PING`), only from the launch owner's IP, and keep re-learning it so a
//! NAT rebinding or Wi-Fi roam does not strand the stream.

use crate::fec::ReedSolomon;
use crate::h26x::FrameFixer;
use crate::video::Packetizer;
use crate::{Inner, StreamShared};
use fl_core::{AudioSource, VideoSource};
use socket2::{SockAddr, SockRef};
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const PING_TIMEOUT: Duration = Duration::from_secs(10);
const SOURCE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Large send buffer (a keyframe is queued in bursts) and DSCP AF41 like Sunshine;
/// both are best effort.
pub fn tune_socket(sock: &UdpSocket, sndbuf: usize) {
    let s = SockRef::from(sock);
    let _ = s.set_send_buffer_size(sndbuf);
    let _ = s.set_tos_v4(0x88);
}

fn lock_source<'a, T: ?Sized>(m: &'a Mutex<Box<T>>, shared: &StreamShared) -> Option<MutexGuard<'a, Box<T>>> {
    let deadline = Instant::now() + SOURCE_LOCK_TIMEOUT;
    loop {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(std::sync::TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        if Instant::now() > deadline || shared.stopped() {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Is this datagram a ping from our client?
fn is_ping(buf: &[u8], expect: &[u8; 16]) -> bool {
    buf.starts_with(expect) || buf == b"PING"
}

/// Block until the client pings this port; returns its address. Leaves the socket
/// non-blocking for the stream that follows.
fn wait_for_ping(sock: &UdpSocket, shared: &StreamShared, label: &str) -> Option<SocketAddr> {
    let deadline = Instant::now() + PING_TIMEOUT;
    // The socket outlives streams, and the previous one left it non-blocking.
    let _ = sock.set_nonblocking(false);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
    let mut buf = [0u8; 64];
    while !shared.stopped() && Instant::now() < deadline {
        if let Ok((n, from)) = sock.recv_from(&mut buf)
            && from.ip().to_canonical() == shared.launch.peer_ip
            && is_ping(&buf[..n], &shared.launch.ping)
        {
            tracing::info!(%from, "{label}: client endpoint learned");
            let _ = sock.set_nonblocking(true);
            return Some(from);
        }
    }
    None
}

/// Drain queued pings (the socket is non-blocking); follow the client if its port changed.
fn refresh_endpoint(sock: &UdpSocket, shared: &StreamShared, addr: &mut SocketAddr, label: &str) {
    let mut buf = [0u8; 64];
    while let Ok((n, from)) = sock.recv_from(&mut buf) {
        if from != *addr && from.ip().to_canonical() == shared.launch.peer_ip && is_ping(&buf[..n], &shared.launch.ping) {
            tracing::info!(old = %addr, new = %from, "{label}: client endpoint changed");
            *addr = from;
        }
    }
}

pub fn run_video(inner: &Arc<Inner>, shared: &Arc<StreamShared>) {
    let Ok(sock) = inner.video_sock.try_clone() else {
        shared.end("video socket unavailable", true);
        return;
    };
    let Some(mut src) = lock_source(&inner.video_src, shared) else {
        shared.end("video source busy (previous stream did not release it)", true);
        return;
    };
    let Some(mut addr) = wait_for_ping(&sock, shared, "video") else {
        if !shared.stopped() {
            shared.end("no video ping from the client", false);
        }
        return;
    };
    let cfg = shared.negotiated.stream.clone();
    if let Err(e) = src.start(&cfg) {
        tracing::error!(error = %format!("{e:#}"), "video source failed to start");
        shared.end("video source failed to start", true);
        return;
    }
    stream_video(&mut **src, &sock, shared, &mut addr, inner.cfg.fec_percent);
    src.stop();
    tracing::debug!("video: stopped");
}

fn stream_video(src: &mut dyn VideoSource, sock: &UdpSocket, shared: &StreamShared, addr: &mut SocketAddr, fec: u8) {
    use std::sync::atomic::Ordering;
    let n = &shared.negotiated;
    let mut packetizer = Packetizer::new(n.packet_size, fec, n.min_fec);
    let mut fixer = FrameFixer::new(n.stream.codec);
    let frame_interval = Duration::from_secs_f64(1.0 / n.stream.fps.max(1) as f64);
    let mut frame_index: u32 = 0;
    let mut first_pts: Option<u64> = None;
    let mut have_keyframe = false;
    let mut waited = 0u32;
    let mut send_errors = 0u32;
    let mut stats = (Instant::now(), 0u64, 0u64);
    while !shared.stopped() {
        if shared.keyframe.swap(false, Ordering::SeqCst) {
            src.request_keyframe();
        }
        let frame = match src.next_frame() {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "video source error");
                shared.end("video source error", true);
                break;
            }
        };
        if shared.stopped() {
            break;
        }
        refresh_endpoint(sock, shared, addr, "video");
        let Some(fixed) = fixer.fix(frame.data, frame.keyframe) else {
            continue;
        };
        if !have_keyframe {
            // The client discards everything before its first IDR anyway.
            if !fixed.decodable_start {
                // A keyframe without parameter sets cannot start the stream: ask again.
                // P-frames usually mean the requested keyframe is still in the encoder,
                // so those only repeat the request once a second.
                waited += 1;
                if fixed.keyframe || waited.is_multiple_of(n.stream.fps.max(1)) {
                    shared.keyframe.store(true, Ordering::SeqCst);
                }
                continue;
            }
            have_keyframe = true;
        }
        let base = *first_pts.get_or_insert(frame.pts_us);
        let rtp_ts = (frame.pts_us.saturating_sub(base) * 90 / 1000) as u32;
        frame_index = frame_index.wrapping_add(1);
        let shard_size = packetizer.shard_size();
        let Some(shards) = packetizer.packetize(&fixed.data, fixed.keyframe, frame_index, rtp_ts, 0) else {
            tracing::warn!(bytes = fixed.data.len(), "video: frame too large for the wire, dropped");
            shared.keyframe.store(true, Ordering::SeqCst);
            continue;
        };
        stats.1 += 1;
        stats.2 += fixed.data.len() as u64;
        if let Err(e) = send_shards(sock, *addr, shards, shard_size, frame_interval) {
            send_errors += 1;
            if send_errors == 1 {
                tracing::debug!(error = %e, "video: send failed");
            }
            if send_errors >= 3 * n.stream.fps {
                shared.end("video send keeps failing (client gone?)", false);
                break;
            }
        } else {
            send_errors = 0;
        }
        if stats.0.elapsed() >= Duration::from_secs(10) {
            let secs = stats.0.elapsed().as_secs_f64();
            tracing::info!(
                fps = format_args!("{:.1}", stats.1 as f64 / secs),
                kbps = (stats.2 as f64 * 8.0 / 1000.0 / secs) as u64,
                frame_index,
                "video: streaming"
            );
            stats = (Instant::now(), 0, 0);
        }
    }
}

/// Datagrams per `sendmmsg` call.
const BURST: usize = 32;

/// Send one frame's shards (`shards` holds them back to back). Large frames are spread
/// over up to ~40% of the frame interval in bursts so a keyframe does not hit the Wi-Fi
/// link as one wall of packets.
fn send_shards(
    sock: &UdpSocket,
    addr: SocketAddr,
    shards: &[u8],
    shard_size: usize,
    frame_interval: Duration,
) -> std::io::Result<()> {
    let addr = SockAddr::from(addr);
    let bursts = (shards.len() / shard_size).div_ceil(BURST);
    let gap = if bursts > 1 { frame_interval.mul_f64(0.4) / bursts as u32 } else { Duration::ZERO };
    let mut result = Ok(());
    for (i, burst) in shards.chunks(BURST * shard_size).enumerate() {
        if i > 0 {
            std::thread::sleep(gap);
        }
        if let Err(e) = send_burst(sock, &addr, burst, shard_size) {
            result = Err(e);
        }
    }
    result
}

/// One `sendmmsg` for up to [`BURST`] datagrams: a syscall per burst instead of one per
/// packet. The socket is non-blocking; if its buffer is full the rest of the burst is
/// retried once and then dropped (the client's FEC or an IDR request covers it).
fn send_burst(sock: &UdpSocket, addr: &SockAddr, burst: &[u8], shard_size: usize) -> std::io::Result<()> {
    let drop_pct = debug_drop_percent();
    let mut iov = [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }; BURST];
    let mut n = 0;
    for shard in burst.chunks_exact(shard_size).take(BURST) {
        if drop_pct > 0 && rand::random::<u32>() % 100 < drop_pct {
            continue;
        }
        iov[n] = libc::iovec { iov_base: shard.as_ptr() as *mut libc::c_void, iov_len: shard.len() };
        n += 1;
    }
    // SAFETY: mmsghdr is plain data for which all-zero is a valid empty value.
    let mut msgs: [libc::mmsghdr; BURST] = unsafe { std::mem::zeroed() };
    for (m, v) in msgs.iter_mut().zip(iov.iter_mut()).take(n) {
        m.msg_hdr.msg_name = addr.as_ptr() as *mut libc::c_void;
        m.msg_hdr.msg_namelen = addr.len();
        m.msg_hdr.msg_iov = v;
        m.msg_hdr.msg_iovlen = 1;
    }
    let (mut sent, mut retried) = (0, false);
    while sent < n {
        // SAFETY: `msgs[sent..n]` are initialised headers whose name and iovec pointers
        // refer to `addr`, `iov` and `burst`, all alive for the call; the kernel only
        // reads the payload.
        let r = unsafe { libc::sendmmsg(sock.as_raw_fd(), msgs[sent..].as_mut_ptr(), (n - sent) as libc::c_uint, 0) };
        if r > 0 {
            sent += r as usize;
            continue;
        }
        let e = std::io::Error::last_os_error();
        match e.kind() {
            std::io::ErrorKind::Interrupted => {}
            std::io::ErrorKind::WouldBlock if !retried => {
                retried = true;
                std::thread::sleep(Duration::from_micros(500));
            }
            std::io::ErrorKind::WouldBlock => return Ok(()),
            _ => return Err(e),
        }
    }
    Ok(())
}

/// Test hook: `FRAMELIGHT_DEBUG_DROP_PERCENT=N` drops N% of video datagrams at random,
/// to exercise the client's FEC recovery and the keyframe/RFI path.
fn debug_drop_percent() -> u32 {
    static P: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *P.get_or_init(|| {
        let p = std::env::var("FRAMELIGHT_DEBUG_DROP_PERCENT").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        if p > 0 {
            tracing::warn!(percent = p, "debug: dropping video packets on purpose");
        }
        p
    })
}

const AUDIO_DATA_SHARDS: usize = 4;
const AUDIO_FEC_SHARDS: usize = 2;
const RTP_PT_AUDIO: u8 = 97;
const RTP_PT_FEC: u8 = 127;

pub fn run_audio(inner: &Arc<Inner>, shared: &Arc<StreamShared>) {
    let Ok(sock) = inner.audio_sock.try_clone() else {
        return;
    };
    let Some(mut src) = lock_source(&inner.audio_src, shared) else {
        tracing::warn!("audio source busy; streaming without audio");
        return;
    };
    let Some(mut addr) = wait_for_ping(&sock, shared, "audio") else {
        return;
    };
    src.set_packet_duration(shared.negotiated.audio_packet_ms);
    if let Err(e) = src.start() {
        tracing::warn!(error = %format!("{e:#}"), "audio source failed to start; streaming without audio");
        return;
    }
    stream_audio(&mut **src, &sock, shared, &mut addr);
    src.stop();
    tracing::debug!("audio: stopped");
}

fn rtp_header(wire: &mut Vec<u8>, payload_type: u8, seq: u16, ts: u32) {
    wire.clear();
    wire.extend_from_slice(&[0x80, payload_type]);
    wire.extend_from_slice(&seq.to_be_bytes());
    wire.extend_from_slice(&ts.to_be_bytes());
    wire.extend_from_slice(&0u32.to_be_bytes());
}

fn stream_audio(src: &mut dyn AudioSource, sock: &UdpSocket, shared: &StreamShared, addr: &mut SocketAddr) {
    let n = &shared.negotiated;
    let key = shared.launch.key;
    let rikeyid = shared.launch.rikeyid;
    let rs = ReedSolomon::new(AUDIO_DATA_SHARDS, AUDIO_FEC_SHARDS).expect("audio FEC geometry");
    let mut seq: u16 = 0;
    // RTP timestamp in milliseconds, advanced by each packet's duration.
    let mut ts: u32 = 0;
    // The current FEC block: its payloads back to back, and the base (seq, ts).
    let mut block: Vec<u8> = Vec::new();
    let mut block_base = (0u16, 0u32);
    // Payload size of the block, or `None` once two payloads differed (no parity then:
    // Reed-Solomon needs equal shards, which CBR Opus gives).
    let mut block_shard: Option<usize> = None;
    let mut parity: Vec<u8> = Vec::new();
    let mut wire: Vec<u8> = Vec::with_capacity(1500);
    let mut opus: Vec<u8> = Vec::with_capacity(1500);
    let mut last_refresh = Instant::now();
    let mut warned_duration = false;
    let mut warned_layout = false;
    while !shared.stopped() {
        let pkt = match src.next_packet() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "audio source error; audio stops");
                break;
            }
        };
        if last_refresh.elapsed() > Duration::from_millis(500) {
            refresh_endpoint(sock, shared, addr, "audio");
            last_refresh = Instant::now();
        }
        if (seq as usize).is_multiple_of(AUDIO_DATA_SHARDS) {
            block.clear();
            block_base = (seq, ts);
            block_shard = None;
        }
        let at = block.len();
        // A surround client decodes a two-stream layout; stereo clients get the packet as is.
        opus.clear();
        if n.audio_channels == 2 || !crate::silence::append_as_surround(&pkt.data, &mut opus) {
            if n.audio_channels != 2 && !std::mem::replace(&mut warned_layout, true) {
                tracing::warn!("audio packets cannot be re-framed for the client's surround decoder; it will not play them");
            }
            opus.extend_from_slice(&pkt.data);
        }
        if n.audio_encrypt {
            let mut iv = [0u8; 16];
            iv[..4].copy_from_slice(&rikeyid.wrapping_add(seq as u32).to_be_bytes());
            block.extend_from_slice(&crate::crypto::cbc_encrypt(&key, &iv, &opus));
        } else {
            block.extend_from_slice(&opus);
        }
        let payload = &block[at..];
        if at == 0 {
            block_shard = Some(payload.len());
        } else if block_shard != Some(payload.len()) {
            block_shard = None;
        }
        rtp_header(&mut wire, RTP_PT_AUDIO, seq, ts);
        wire.extend_from_slice(payload);
        // A full socket buffer or a vanished client just loses the packet.
        let _ = sock.send_to(&wire, *addr);

        if let Some(len) = block_shard.filter(|&len| len > 0 && block.len() == AUDIO_DATA_SHARDS * len) {
            // Parity over the (encrypted) payloads.
            parity.resize(AUDIO_FEC_SHARDS * len, 0);
            rs.encode(&block, &mut parity, len);
            for (i, p) in parity.chunks_exact(len).enumerate() {
                rtp_header(&mut wire, RTP_PT_FEC, block_base.0.wrapping_add((AUDIO_DATA_SHARDS + i) as u16), block_base.1);
                // AUDIO_FEC_HEADER
                wire.push(i as u8);
                wire.push(RTP_PT_AUDIO);
                wire.extend_from_slice(&block_base.0.to_be_bytes());
                wire.extend_from_slice(&block_base.1.to_be_bytes());
                wire.extend_from_slice(&0u32.to_be_bytes());
                wire.extend_from_slice(p);
                let _ = sock.send_to(&wire, *addr);
            }
        }
        seq = seq.wrapping_add(1);
        let ms = crate::silence::opus_packet_ms(&pkt.data).unwrap_or(n.audio_packet_ms);
        if ms > n.audio_packet_ms && !warned_duration {
            warned_duration = true;
            tracing::warn!(
                packet_ms = ms,
                negotiated_ms = n.audio_packet_ms,
                "audio packets are longer than the client negotiated; it may fail to decode them"
            );
        }
        ts = ts.wrapping_add(ms);
    }
}
