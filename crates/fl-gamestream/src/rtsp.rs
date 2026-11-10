//! RTSP on TCP 48010. moonlight-common-c (host 7.1.431, plain `rtsp://` session URL)
//! opens one TCP connection per request and reads the reply until the host closes it.
//! Sequence: OPTIONS, DESCRIBE, SETUP audio/video/control, ANNOUNCE (client SDP with
//! the negotiated stream), PLAY.

use crate::{
    AUDIO_PORT, CONTROL_PORT, Inner, Negotiated, VIDEO_PORT, parse_codec,
};
use anyhow::Result;
use fl_core::StreamConfig;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const SS_ENC_CONTROL_V2: u32 = 0x01;
const SS_ENC_AUDIO: u32 = 0x04;
/// What we offer: encrypted control (V2 nonces) and encrypted audio. Video encryption
/// is not offered (it would cost a GCM pass per packet on the headset).
const ENCRYPTION_SUPPORTED: u32 = SS_ENC_CONTROL_V2 | SS_ENC_AUDIO;
/// Legacy `x-nv-general.featureFlags` bit for audio encryption.
const NVFF_AUDIO_ENCRYPTION: u32 = 0x20;

/// Moonlight opens one connection per request, strictly in sequence.
const MAX_CONNECTIONS: usize = 8;
/// Head plus SDP of an ANNOUNCE is a few KiB.
const MAX_REQUEST: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub fn spawn(
    listener: TcpListener,
    inner: Arc<Inner>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let active = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new().name("fl-rtsp".into()).spawn(move || {
        for conn in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(s) = conn else {
                // Out of descriptors or similar: do not spin on a failing accept.
                std::thread::sleep(Duration::from_millis(100));
                continue;
            };
            // Only the client that launched may talk RTSP; anyone else is hung up on
            // before a byte is read.
            let peer = s.peer_addr().ok().map(|a| a.ip().to_canonical());
            let expected = inner.launch.lock().unwrap().as_ref().map(|l| l.peer_ip);
            if peer.is_none() || peer != expected {
                tracing::warn!(?peer, "RTSP connection without a matching launch — dropped");
                continue;
            }
            if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                tracing::warn!("RTSP: too many connections, dropping one");
                continue;
            }
            // A thread per connection keeps a slow request from blocking the next one
            // (PLAY waits for the previous stream's threads).
            active.fetch_add(1, Ordering::SeqCst);
            let (inner, active2) = (inner.clone(), active.clone());
            let spawned = std::thread::Builder::new().name("fl-rtsp-conn".into()).spawn(move || {
                if let Err(e) = handle(&inner, s) {
                    tracing::debug!(error = %e, "RTSP connection error");
                }
                active2.fetch_sub(1, Ordering::SeqCst);
            });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::SeqCst);
            }
        }
    })
}

struct Req {
    method: String,
    target: String,
    cseq: String,
    body: String,
}

fn read_request(s: &mut TcpStream) -> std::io::Result<Req> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut read_more = |buf: &mut Vec<u8>| -> std::io::Result<()> {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_REQUEST {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        if Instant::now() > deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        Ok(())
    };
    let header_end = loop {
        read_more(&mut buf)?;
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or_default().to_string();
    let target = first.next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > MAX_REQUEST {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    while buf.len() < header_end + len {
        read_more(&mut buf)?;
    }
    Ok(Req {
        method,
        target,
        cseq: headers.get("cseq").cloned().unwrap_or_else(|| "0".into()),
        body: String::from_utf8_lossy(&buf[header_end..header_end + len]).into_owned(),
    })
}

fn respond(s: &mut TcpStream, status: &str, cseq: &str, headers: &[(&str, String)], body: &str) -> std::io::Result<()> {
    let mut out = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    out.push_str("\r\n");
    out.push_str(body);
    s.write_all(out.as_bytes())?;
    s.flush()
}

fn handle(inner: &Arc<Inner>, mut s: TcpStream) -> Result<()> {
    s.set_read_timeout(Some(IO_TIMEOUT))?;
    s.set_write_timeout(Some(IO_TIMEOUT))?;
    let peer = s.peer_addr()?.ip().to_canonical();
    let local = s.local_addr()?.ip().to_canonical();
    let req = read_request(&mut s)?;
    tracing::debug!(method = %req.method, target = %req.target, %peer, "RTSP request");

    // Only the paired client that launched may drive RTSP.
    let launch = inner.launch.lock().unwrap().clone();
    let Some(launch) = launch.filter(|l| l.peer_ip == peer) else {
        tracing::warn!(%peer, method = %req.method, "RTSP request without a matching launch");
        respond(&mut s, "454 Session Not Found", &req.cseq, &[], "")?;
        return Ok(());
    };
    let session = ("Session", "DEADBEEFCAFE;timeout = 90".to_string());
    match req.method.as_str() {
        "OPTIONS" => respond(&mut s, "200 OK", &req.cseq, &[], "")?,
        "DESCRIBE" => {
            let sdp = describe_sdp(inner, local);
            respond(&mut s, "200 OK", &req.cseq, &[("Content-Type", "application/sdp".into())], &sdp)?
        }
        "SETUP" => {
            let ping = String::from_utf8_lossy(&launch.ping).into_owned();
            let kind = req.target.split("streamid=").nth(1).and_then(|r| r.split('/').next()).unwrap_or("");
            let extra = match kind {
                "audio" => Some((AUDIO_PORT, ("X-SS-Ping-Payload", ping))),
                "video" => Some((VIDEO_PORT, ("X-SS-Ping-Payload", ping))),
                "control" => Some((CONTROL_PORT, ("X-SS-Connect-Data", launch.connect_data.to_string()))),
                _ => None,
            };
            match extra {
                Some((port, x)) => respond(
                    &mut s,
                    "200 OK",
                    &req.cseq,
                    &[session, ("Transport", format!("server_port={port}")), x],
                    "",
                )?,
                None => respond(&mut s, "404 Not Found", &req.cseq, &[], "")?,
            }
        }
        "ANNOUNCE" => match parse_announce(inner, &launch, &req.body) {
            Ok(n) => {
                tracing::info!(
                    codec = ?n.stream.codec, w = n.stream.width, h = n.stream.height, fps = n.stream.fps,
                    kbps = n.stream.bitrate_kbps, packet_size = n.packet_size, control_v2 = n.control_v2,
                    audio_encrypt = n.audio_encrypt, audio_ms = n.audio_packet_ms,
                    audio_channels = n.audio_channels,
                    "RTSP ANNOUNCE negotiated"
                );
                if let Some(l) = inner.launch.lock().unwrap().as_mut().filter(|l| l.generation == launch.generation) {
                    l.negotiated = Some(n);
                }
                respond(&mut s, "200 OK", &req.cseq, &[session], "")?
            }
            Err(e) => {
                tracing::warn!(error = %e, "RTSP ANNOUNCE rejected");
                respond(&mut s, "400 Bad Request", &req.cseq, &[], "")?
            }
        },
        "PLAY" => match inner.start_stream() {
            Ok(()) => respond(&mut s, "200 OK", &req.cseq, &[session], "")?,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "RTSP PLAY failed");
                respond(&mut s, "500 Internal Server Error", &req.cseq, &[], "")?
            }
        },
        "TEARDOWN" => {
            inner.stop_stream("RTSP TEARDOWN", false);
            respond(&mut s, "200 OK", &req.cseq, &[session], "")?
        }
        _ => respond(&mut s, "501 Not Implemented", &req.cseq, &[], "")?,
    }
    Ok(())
}

fn describe_sdp(inner: &Inner, local: std::net::IpAddr) -> String {
    let mut sdp = format!(
        "v=0\r\no=- 0 0 IN {} {local}\r\ns=framelight\r\nt=0 0\r\n",
        if local.is_ipv4() { "IP4" } else { "IP6" }
    );
    sdp.push_str("a=x-ss-general.featureFlags:0\r\n");
    sdp.push_str(&format!("a=x-ss-general.encryptionSupported:{ENCRYPTION_SUPPORTED}\r\n"));
    sdp.push_str("a=x-ss-general.encryptionRequested:0\r\n");
    if inner.cfg.advertise_rfi {
        sdp.push_str("a=x-nv-video[0].refPicInvalidation:1\r\n");
    }
    // Opus layouts for clients set to 5.1 / 7.1: channel count, streams, coupled streams,
    // then the stream channel each speaker plays. One coupled stream carries the stereo
    // mix to front left/right; every other speaker plays a second, silent mono stream.
    // Without these lines a 5.1 client assumes GFE's four-stream layout (and cannot
    // decode our packets) and a 7.1 client gives up during the handshake.
    sdp.push_str("a=fmtp:97 surround-params=621012222\r\n");
    sdp.push_str("a=fmtp:97 surround-params=82101222222\r\n");
    if inner.cfg.hevc {
        // moonlight-common-c enables HEVC iff it finds the base64 VPS prefix.
        sdp.push_str("a=fmtp:96 sprop-parameter-sets=AAAAAU\r\n");
    }
    sdp.push_str(&format!("m=video {VIDEO_PORT}  \r\n"));
    sdp
}

fn sdp_attrs(body: &str) -> HashMap<String, String> {
    body.lines()
        .filter_map(|l| l.trim().strip_prefix("a="))
        .filter_map(|a| a.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

fn parse_announce(inner: &Inner, launch: &crate::Launch, body: &str) -> Result<Negotiated> {
    let a = sdp_attrs(body);
    let u = |k: &str| a.get(k).and_then(|v| v.parse::<u32>().ok());
    let codec = parse_codec(u("x-nv-vqos[0].bitStreamFormat")).ok_or_else(|| anyhow::anyhow!("unsupported codec (AV1?)"))?;
    let enabled = match codec {
        fl_core::Codec::H264 => inner.cfg.h264,
        fl_core::Codec::Hevc => inner.cfg.hevc,
    };
    anyhow::ensure!(enabled, "client asked for {codec:?}, which is disabled");
    if u("x-nv-video[0].dynamicRangeMode").unwrap_or(0) != 0 {
        tracing::warn!("client asked for HDR; streaming SDR");
    }
    let audio_channels = u("x-nv-audio.surround.numChannels").unwrap_or(2);
    anyhow::ensure!(matches!(audio_channels, 2 | 6 | 8), "unsupported audio channel count {audio_channels}");
    // Moonlight's own setting; the SDP's maximumBitrateKbps is already 80% of it.
    let configured = u("x-ml-video.configuredBitrateKbps")
        .or_else(|| u("x-nv-vqos[0].bw.maximumBitrateKbps"))
        .unwrap_or(20_000);
    // Leave room for FEC so the wire rate stays near what the user picked.
    let bitrate_kbps = (configured as u64 * 100 / (100 + inner.cfg.fec_percent as u64)) as u32;
    let ss_enc = u("x-ss-general.encryptionEnabled").unwrap_or(0) & ENCRYPTION_SUPPORTED;
    let legacy_flags = u("x-nv-general.featureFlags").unwrap_or(0);
    Ok(Negotiated {
        stream: StreamConfig {
            width: u("x-nv-video[0].clientViewportWd").unwrap_or(launch.width),
            height: u("x-nv-video[0].clientViewportHt").unwrap_or(launch.height),
            fps: u("x-nv-video[0].maxFPS").unwrap_or(launch.fps).max(1),
            bitrate_kbps,
            codec,
        },
        packet_size: u("x-nv-video[0].packetSize").unwrap_or(1024) as usize,
        min_fec: u("x-nv-vqos[0].fec.minRequiredFecPackets").unwrap_or(0),
        control_v2: ss_enc & SS_ENC_CONTROL_V2 != 0,
        audio_encrypt: ss_enc & SS_ENC_AUDIO != 0 || legacy_flags & NVFF_AUDIO_ENCRYPTION != 0,
        audio_packet_ms: u("x-nv-aqos.packetDuration").unwrap_or(5).clamp(5, 20),
        audio_channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdp_attributes_tolerate_trailing_spaces() {
        let a = sdp_attrs("v=0\r\na=x-nv-video[0].clientViewportWd:1920 \r\na=x-nv-video[0].packetSize:1392 \r\n");
        assert_eq!(a["x-nv-video[0].clientViewportWd"], "1920");
        assert_eq!(a["x-nv-video[0].packetSize"], "1392");
    }
}
