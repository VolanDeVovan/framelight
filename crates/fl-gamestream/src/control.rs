//! ENet control stream on UDP 47999, bound only while a stream runs.
//!
//! Every message is encrypted (host 7.1.431 → client `encryptedControlStream`):
//!
//! ```text
//! u16 LE 0x0001 | u16 LE length (= 4 + 16 + plaintext) | u32 LE seq | tag[16] | ciphertext
//! plaintext = u16 LE type | u16 LE payload length | payload
//! ```
//!
//! AES-128-GCM under the `/launch` `rikey`, no AAD. Nonce with `SS_ENC_CONTROL_V2`
//! (negotiated in ANNOUNCE): 12 bytes, seq LE in [0..4], [10..12] = "CC" from the client
//! and "HC" from the host. Without V2: 16 bytes, `iv[0] = seq as u8`, rest zero.
//!
//! Handled client messages (Gen7Enc table): 0x0302 IDR request, 0x0301 reference frame
//! invalidation (answered with a keyframe), 0x0200 periodic ping, 0x0307 start B,
//! 0x5502 FEC status. Input (0x0206) is ignored: this host is watch-only.

use crate::crypto::{gcm_open, gcm_seal};
use crate::{CONTROL_PORT, Inner, StreamShared};
use rusty_enet::{Event, Host, HostSettings, Packet, PacketReceived, PeerID, Socket, SocketOptions};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const TYPE_PING: u16 = 0x0200;
const TYPE_LOSS_STATS: u16 = 0x0201;
const TYPE_FRAME_STATS: u16 = 0x0204;
const TYPE_INPUT: u16 = 0x0206;
const TYPE_RFI: u16 = 0x0301;
const TYPE_IDR: u16 = 0x0302;
const TYPE_START_B: u16 = 0x0307;
const TYPE_FEC_STATUS: u16 = 0x5502;
const TYPE_TERMINATION: u16 = 0x0109;
/// `NVST_DISCONN_SERVER_TERMINATED_CLOSED`: graceful, intended end.
const TERMINATION_GRACEFUL: u32 = 0x8003_0023;

/// Moonlight connects right after PLAY; give up on a client that never shows.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// UDP socket for ENet that drops anything not from the launch owner's address and
/// blocks briefly in `receive`, which paces the service loop without busy-waiting.
struct OwnerSocket {
    sock: UdpSocket,
    owner: IpAddr,
}

impl Socket for OwnerSocket {
    type Address = SocketAddr;
    type Error = std::io::Error;

    fn init(&mut self, _opts: SocketOptions) -> std::io::Result<()> {
        self.sock.set_nonblocking(false)?;
        self.sock.set_read_timeout(Some(Duration::from_millis(5)))
    }

    fn send(&mut self, addr: SocketAddr, buf: &[u8]) -> std::io::Result<usize> {
        match self.sock.send_to(buf, addr) {
            Ok(n) => Ok(n),
            // A vanished client produces ICMP errors; ENet's timeout handles it.
            Err(e) if is_transient(&e) => Ok(buf.len()),
            Err(e) => Err(e),
        }
    }

    fn receive(&mut self, buf: &mut [u8; rusty_enet::MTU_MAX]) -> std::io::Result<Option<(SocketAddr, PacketReceived)>> {
        loop {
            match self.sock.recv_from(buf) {
                Ok((n, from)) => {
                    if from.ip().to_canonical() != self.owner {
                        continue;
                    }
                    return Ok(Some((from, PacketReceived::Complete(n))));
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    return Ok(None);
                }
                Err(e) if is_transient(&e) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }
}

fn is_transient(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::HostUnreachable
            | std::io::ErrorKind::NetworkUnreachable
            | std::io::ErrorKind::Interrupted
    )
}

fn bind() -> std::io::Result<UdpSocket> {
    // The previous stream's socket may still be closing; retry briefly.
    let mut last = None;
    for _ in 0..20 {
        match UdpSocket::bind((IpAddr::V4(Ipv4Addr::UNSPECIFIED), CONTROL_PORT)) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(last.unwrap())
}

struct Crypto {
    key: [u8; 16],
    v2: bool,
    host_seq: u32,
}

impl Crypto {
    fn nonce(&self, seq: u32, from_host: bool) -> Vec<u8> {
        if self.v2 {
            let mut iv = vec![0u8; 12];
            iv[0..4].copy_from_slice(&seq.to_le_bytes());
            iv[10] = if from_host { b'H' } else { b'C' };
            iv[11] = b'C';
            iv
        } else {
            let mut iv = vec![0u8; 16];
            iv[0] = seq as u8;
            iv
        }
    }

    /// Returns the plaintext `(type, payload)`.
    fn open(&self, d: &[u8]) -> Option<(u16, Vec<u8>)> {
        if d.len() < 8 + 16 + 4 || u16::from_le_bytes([d[0], d[1]]) != 0x0001 {
            return None;
        }
        let len = u16::from_le_bytes([d[2], d[3]]) as usize;
        if 4 + len > d.len() || len < 4 + 16 + 4 {
            return None;
        }
        let seq = u32::from_le_bytes(d[4..8].try_into().ok()?);
        let tag = &d[8..24];
        let ct = &d[24..4 + len];
        let pt = gcm_open(&self.key, &self.nonce(seq, false), ct, tag)?;
        let ty = u16::from_le_bytes([pt[0], pt[1]]);
        Some((ty, pt[4..].to_vec()))
    }

    fn seal(&mut self, ty: u16, payload: &[u8]) -> Vec<u8> {
        let mut pt = Vec::with_capacity(4 + payload.len());
        pt.extend_from_slice(&ty.to_le_bytes());
        pt.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        pt.extend_from_slice(payload);
        let seq = self.host_seq;
        self.host_seq = self.host_seq.wrapping_add(1);
        let (ct, tag) = gcm_seal(&self.key, &self.nonce(seq, true), &pt);
        let mut wire = Vec::with_capacity(8 + 16 + ct.len());
        wire.extend_from_slice(&0x0001u16.to_le_bytes());
        wire.extend_from_slice(&((4 + 16 + ct.len()) as u16).to_le_bytes());
        wire.extend_from_slice(&seq.to_le_bytes());
        wire.extend_from_slice(&tag);
        wire.extend_from_slice(&ct);
        wire
    }
}

pub fn run(_inner: &Arc<Inner>, shared: &Arc<StreamShared>) {
    let sock = match bind() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "control: cannot bind UDP 47999");
            shared.end("control port unavailable", false);
            return;
        }
    };
    let settings = HostSettings { peer_limit: 2, channel_limit: 0x30, ..Default::default() };
    let mut host = match Host::new(OwnerSocket { sock, owner: shared.launch.peer_ip }, settings) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = ?e, "control: ENet host init failed");
            shared.end("control init failed", false);
            return;
        }
    };
    let mut crypto = Crypto { key: shared.launch.key, v2: shared.negotiated.control_v2, host_seq: 0 };
    let mut peer: Option<PeerID> = None;
    let mut decrypt_failures = 0u32;
    tracing::debug!("control: listening on UDP 47999");

    loop {
        if shared.stopped() {
            if let Some(p) = peer {
                if shared.notify_client.load(Ordering::SeqCst) {
                    let wire = crypto.seal(TYPE_TERMINATION, &TERMINATION_GRACEFUL.to_be_bytes());
                    let _ = host.peer_mut(p).send(0, &Packet::reliable(wire.as_slice()));
                    tracing::info!("control: sent termination to client");
                }
                host.peer_mut(p).disconnect_later(0);
                // Let ENet flush the termination and the disconnect (~200 ms).
                let until = Instant::now() + Duration::from_millis(200);
                while Instant::now() < until {
                    if host.service().is_err() {
                        break;
                    }
                }
            }
            break;
        }
        loop {
            let ev = match host.service() {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "control: socket error");
                    break;
                }
            };
            match ev {
                Event::Connect { peer: p, data } => {
                    if data != shared.launch.connect_data {
                        tracing::warn!(data, "control: connect with wrong connect data — refused");
                        p.disconnect_now(0);
                        continue;
                    }
                    if peer.is_some_and(|o| o != p.id()) {
                        tracing::info!("control: client reconnected, replacing old peer");
                    }
                    // Notice a vanished client within ~10 s (Moonlight uses the same bound).
                    p.set_timeout(32, 5_000, 10_000);
                    peer = Some(p.id());
                    tracing::info!(client = ?p.address(), "control: client connected");
                }
                Event::Disconnect { peer: p, .. } => {
                    if peer == Some(p.id()) {
                        peer = None;
                        shared.end("control stream disconnected (client left or timed out)", false);
                    }
                }
                Event::Receive { peer: p, packet, .. } => {
                    if peer != Some(p.id()) {
                        continue;
                    }
                    match crypto.open(packet.data()) {
                        Some((ty, payload)) => {
                            decrypt_failures = 0;
                            on_message(shared, ty, &payload);
                        }
                        None => {
                            decrypt_failures += 1;
                            if decrypt_failures.is_power_of_two() {
                                tracing::warn!(len = packet.data().len(), decrypt_failures, "control: undecryptable message");
                            }
                        }
                    }
                }
            }
        }
        if peer.is_none() && shared.started.elapsed() > CONNECT_TIMEOUT && !shared.stopped() {
            shared.end("client never connected the control stream", false);
        }
    }
    tracing::debug!("control: stopped");
}

fn on_message(shared: &StreamShared, ty: u16, payload: &[u8]) {
    match ty {
        TYPE_IDR => {
            tracing::debug!("control: IDR request");
            shared.keyframe.store(true, Ordering::SeqCst);
        }
        TYPE_RFI => {
            let range = (payload.len() >= 16).then(|| {
                (
                    u32::from_le_bytes(payload[0..4].try_into().unwrap()),
                    u32::from_le_bytes(payload[8..12].try_into().unwrap()),
                )
            });
            tracing::debug!(?range, "control: reference frame invalidation → keyframe");
            shared.keyframe.store(true, Ordering::SeqCst);
        }
        TYPE_FEC_STATUS if payload.len() >= 21 => {
            let be16 = |o: usize| u16::from_be_bytes([payload[o], payload[o + 1]]);
            let (total, data_rx, parity_rx) = (be16(10), be16(14), be16(16));
            let frame = u32::from_be_bytes(payload[0..4].try_into().unwrap());
            if data_rx + parity_rx < total {
                tracing::debug!(frame, total, data_rx, parity_rx, "control: client lost a frame past FEC");
            } else {
                tracing::trace!(frame, total, data_rx, parity_rx, "control: client recovered via FEC");
            }
        }
        TYPE_PING | TYPE_LOSS_STATS | TYPE_FRAME_STATS | TYPE_START_B | TYPE_INPUT | TYPE_FEC_STATUS => {}
        other => tracing::trace!(ty = format_args!("{other:#06x}"), len = payload.len(), "control: unhandled message"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build what moonlight-common-c `sendMessageEnet` produces for one message.
    fn client_packet(key: &[u8; 16], v2: bool, seq: u32, ty: u16, payload: &[u8]) -> Vec<u8> {
        let mut pt = ty.to_le_bytes().to_vec();
        pt.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        pt.extend_from_slice(payload);
        let iv = Crypto { key: *key, v2, host_seq: 0 }.nonce(seq, false);
        let (ct, tag) = gcm_seal(key, &iv, &pt);
        let mut w = 0x0001u16.to_le_bytes().to_vec();
        w.extend_from_slice(&((4 + 16 + pt.len()) as u16).to_le_bytes());
        w.extend_from_slice(&seq.to_le_bytes());
        w.extend_from_slice(&tag);
        w.extend_from_slice(&ct);
        w
    }

    #[test]
    fn opens_client_messages_both_nonce_schemes() {
        let key = [0x42; 16];
        for v2 in [true, false] {
            let c = Crypto { key, v2, host_seq: 0 };
            let (ty, pl) = c.open(&client_packet(&key, v2, 7, TYPE_IDR, &[0, 0])).unwrap();
            assert_eq!((ty, pl.as_slice()), (TYPE_IDR, &[0u8, 0][..]));
        }
    }

    #[test]
    fn host_messages_use_the_host_nonce() {
        let key = [1; 16];
        let mut c = Crypto { key, v2: true, host_seq: 3 };
        let w = c.seal(TYPE_TERMINATION, &TERMINATION_GRACEFUL.to_be_bytes());
        let mut iv = vec![0u8; 12];
        iv[0] = 3;
        iv[10] = b'H';
        iv[11] = b'C';
        let pt = gcm_open(&key, &iv, &w[24..], &w[8..24]).unwrap();
        assert_eq!(&pt[0..2], &TYPE_TERMINATION.to_le_bytes());
        assert_eq!(&pt[4..8], &TERMINATION_GRACEFUL.to_be_bytes());
        assert_eq!(u16::from_le_bytes([w[2], w[3]]) as usize, w.len() - 4);
    }
}
