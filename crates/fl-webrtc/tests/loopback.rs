//! The whole streamer over loopback, with str0m itself playing the browser: offer and
//! answer over HTTP, ICE, DTLS and SRTP over UDP, against fake sources. No browser, no
//! encoder, no sound.

use bytes::Bytes;
use fl_core::{AudioPacket, AudioSource, EncodedFrame, StreamConfig, VideoSource};
use fl_webrtc::{Server, ServerConfig};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use str0m::change::SdpAnswer;
use str0m::media::{Direction, KeyframeRequestKind, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};

/// What the test can see of the fake video source.
#[derive(Default)]
struct Seen {
    starts: AtomicUsize,
    stops: AtomicUsize,
    keyframe_requests: AtomicUsize,
    bitrate_kbps: AtomicU32,
}

struct FakeVideo {
    seen: Arc<Seen>,
    keyframe_due: bool,
}

impl VideoSource for FakeVideo {
    fn start(&mut self, cfg: &StreamConfig) -> anyhow::Result<()> {
        self.seen.starts.fetch_add(1, Ordering::SeqCst);
        self.seen
            .bitrate_kbps
            .store(cfg.bitrate_kbps, Ordering::SeqCst);
        self.keyframe_due = true;
        Ok(())
    }

    fn next_frame(&mut self) -> anyhow::Result<EncodedFrame> {
        std::thread::sleep(Duration::from_millis(10));
        let keyframe = std::mem::take(&mut self.keyframe_due);
        let mut data = Vec::new();
        if keyframe {
            // SPS (Constrained Baseline, level 3.1), PPS, IDR slice.
            data.extend_from_slice(&[0, 0, 0, 1, 0x67, 66, 0xc0, 31, 0xab, 0xcd]);
            data.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
            data.extend_from_slice(&[0, 0, 0, 1, 0x65]);
            data.extend(std::iter::repeat_n(0xab, 3000));
        } else {
            data.extend_from_slice(&[0, 0, 0, 1, 0x41]);
            data.extend(std::iter::repeat_n(0xcd, 300));
        }
        Ok(EncodedFrame {
            data: Bytes::from(data),
            keyframe,
            captured: Instant::now(),
        })
    }

    fn request_keyframe(&mut self) {
        self.seen.keyframe_requests.fetch_add(1, Ordering::SeqCst);
        self.keyframe_due = true;
    }

    fn set_bitrate(&mut self, kbps: u32) -> anyhow::Result<()> {
        self.seen.bitrate_kbps.store(kbps, Ordering::SeqCst);
        Ok(())
    }

    fn stop(&mut self) {
        self.seen.stops.fetch_add(1, Ordering::SeqCst);
    }
}

struct FakeAudio(u64);

impl AudioSource for FakeAudio {
    fn start(&mut self) -> anyhow::Result<()> {
        self.0 = 0;
        Ok(())
    }

    fn next_packet(&mut self) -> anyhow::Result<AudioPacket> {
        std::thread::sleep(Duration::from_millis(20));
        self.0 += 1;
        Ok(AudioPacket {
            // An Opus packet: CELT fullband 20 ms stereo, then payload.
            data: Bytes::from_static(&[0xfc, 1, 2, 3, 4, 5, 6, 7]),
            pts_us: (self.0 - 1) * 20_000,
        })
    }

    fn stop(&mut self) {}
}

/// One HTTP request; returns (status, head, body).
fn http(port: u16, method: &str, target: &str, body: &str) -> (u16, String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect to the streamer");
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    s.read_to_string(&mut response).unwrap();
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("a complete response");
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, head.to_owned(), body.to_owned())
}

fn status(port: u16) -> String {
    http(port, "GET", "/status", "").2
}

/// What a viewer received.
#[derive(Default)]
struct Received {
    connected: AtomicBool,
    /// The streamer closed the connection.
    closed: AtomicBool,
    video_frames: AtomicUsize,
    audio_packets: AtomicUsize,
    /// The first video frame, as it came out of the depacketizer.
    first_video: Mutex<Option<Vec<u8>>>,
}

struct Viewer {
    id: String,
    received: Arc<Received>,
    /// Send a PLI.
    want_keyframe: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Viewer {
    /// Posts an offer like the page does and plays the connection on a thread.
    fn join(port: u16) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let local = socket.local_addr().unwrap();
        let mut rtc = Rtc::new(Instant::now());
        rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
        let mut change = rtc.sdp_api();
        let video = change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        change.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        let (offer, pending) = change.apply().unwrap();
        let (code, head, answer) = http(port, "POST", "/offer", &offer.to_sdp_string());
        assert_eq!(code, 200, "{answer}");
        let id = head
            .lines()
            .find_map(|l| l.strip_prefix("Viewer-Id: "))
            .expect("a Viewer-Id header")
            .to_owned();
        let answer = SdpAnswer::from_sdp_string(&answer).expect("an SDP answer");
        rtc.sdp_api().accept_answer(pending, answer).unwrap();

        let received = Arc::new(Received::default());
        let want_keyframe = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (r, k, s) = (received.clone(), want_keyframe.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 2000];
            while !s.load(Ordering::SeqCst) && rtc.is_alive() {
                if k.swap(false, Ordering::SeqCst)
                    && let Some(mut w) = rtc.writer(video)
                {
                    w.request_keyframe(None, KeyframeRequestKind::Pli).unwrap();
                }
                let timeout = match rtc.poll_output().unwrap() {
                    Output::Transmit(t) => {
                        let _ = socket.send_to(&t.contents, t.destination);
                        continue;
                    }
                    Output::Event(Event::Connected) => {
                        r.connected.store(true, Ordering::SeqCst);
                        continue;
                    }
                    Output::Event(Event::MediaData(d)) => {
                        if d.mid == video {
                            r.first_video.lock().unwrap().get_or_insert(d.data.to_vec());
                            r.video_frames.fetch_add(1, Ordering::SeqCst);
                        } else {
                            r.audio_packets.fetch_add(1, Ordering::SeqCst);
                        }
                        continue;
                    }
                    Output::Event(Event::Closed) => {
                        r.closed.store(true, Ordering::SeqCst);
                        continue;
                    }
                    Output::Event(_) => continue,
                    Output::Timeout(t) => t,
                };
                let wait = timeout
                    .saturating_duration_since(Instant::now())
                    .clamp(Duration::from_millis(1), Duration::from_millis(20));
                socket.set_read_timeout(Some(wait)).unwrap();
                let input = match socket.recv_from(&mut buf) {
                    Ok((n, source)) => {
                        let Ok(receive) = Receive::new(Protocol::Udp, source, local, &buf[..n])
                        else {
                            continue;
                        };
                        Input::Receive(Instant::now(), receive)
                    }
                    Err(_) => Input::Timeout(Instant::now()),
                };
                rtc.handle_input(input).unwrap();
            }
        });
        Self {
            id,
            received,
            want_keyframe,
            stop,
            thread: Some(thread),
        }
    }

    /// Stops answering, like a browser that was killed.
    fn vanish(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().expect("the viewer thread");
        }
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.vanish();
    }
}

/// Waits until `cond` holds; panics with `what` after `limit`.
fn eventually(what: &str, limit: Duration, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn start(port: u16, seen: &Arc<Seen>) -> Server {
    let cfg = ServerConfig {
        port,
        ..ServerConfig::default()
    };
    let video = FakeVideo {
        seen: seen.clone(),
        keyframe_due: false,
    };
    Server::start(cfg, Box::new(video), Some(Box::new(FakeAudio(0)))).expect("start the streamer")
}

#[test]
fn viewers_share_one_stream_from_first_offer_to_shutdown() {
    let s = Duration::from_secs;
    let port = free_port();
    let seen = Arc::new(Seen::default());
    let server = start(port, &seen);

    // Enabled, nobody watching: nothing is open.
    assert!(status(port).contains("\"viewers\":0,\"source\":\"idle\""));
    assert_eq!(seen.starts.load(Ordering::SeqCst), 0);
    assert_eq!(http(port, "POST", "/offer", "not sdp").0, 400);
    assert!(http(port, "GET", "/", "").2.contains("<video"));

    // The first viewer starts the source and begins at a keyframe with its SPS.
    let mut first = Viewer::join(port);
    eventually("the first viewer's picture", s(10), || {
        first.received.video_frames.load(Ordering::SeqCst) >= 5
            && first.received.audio_packets.load(Ordering::SeqCst) >= 5
    });
    assert!(first.received.connected.load(Ordering::SeqCst));
    let starts_with_sps = |v: &Viewer| {
        let frame = v.received.first_video.lock().unwrap().clone().unwrap();
        frame.windows(5).next() == Some(&[0, 0, 0, 1, 0x67][..])
    };
    assert!(starts_with_sps(&first));
    assert_eq!(seen.starts.load(Ordering::SeqCst), 1);
    let st = status(port);
    assert!(st.contains("\"viewers\":1,\"source\":\"running\""), "{st}");
    assert!(st.contains("\"profile_level_id\":\"42c01f\""), "{st}");
    assert_eq!(server.status().viewers, 1);

    // A second viewer shares the encode and gets a keyframe of its own.
    let requests = seen.keyframe_requests.load(Ordering::SeqCst);
    let second = Viewer::join(port);
    eventually("the second viewer's picture", s(10), || {
        second.received.video_frames.load(Ordering::SeqCst) >= 5
    });
    assert!(starts_with_sps(&second));
    assert_eq!(seen.starts.load(Ordering::SeqCst), 1, "one encode for both");
    assert!(seen.keyframe_requests.load(Ordering::SeqCst) > requests);
    assert!(status(port).contains("\"viewers\":2"));

    // A PLI from a viewer reaches the source.
    std::thread::sleep(Duration::from_millis(400));
    let requests = seen.keyframe_requests.load(Ordering::SeqCst);
    second.want_keyframe.store(true, Ordering::SeqCst);
    eventually("the keyframe asked for by PLI", s(5), || {
        seen.keyframe_requests.load(Ordering::SeqCst) > requests
    });

    // The quality selector changes the running encode.
    let (code, _, body) = http(port, "POST", "/bitrate?kbps=3000", "");
    assert!(
        code == 200 && body.contains("\"ceiling_kbps\":3000"),
        "{body}"
    );
    eventually("the new bitrate at the source", s(5), || {
        seen.bitrate_kbps.load(Ordering::SeqCst) == 3000
    });
    assert_eq!(seen.starts.load(Ordering::SeqCst), 1, "without a restart");

    // One page closes, the other browser is killed: both are forgotten, and a few
    // seconds later the source is closed.
    http(port, "POST", &format!("/bye?id={}", second.id), "");
    eventually("the goodbye", s(5), || {
        status(port).contains("\"viewers\":1")
    });
    first.vanish();
    eventually("the silent viewer to be dropped", s(15), || {
        status(port).contains("\"viewers\":0")
    });
    assert_eq!(seen.stops.load(Ordering::SeqCst), 0, "the source lingers");
    eventually("the source to stop", s(10), || {
        seen.stops.load(Ordering::SeqCst) == 1 && status(port).contains("\"source\":\"idle\"")
    });

    // The next viewer starts it again.
    let third = Viewer::join(port);
    eventually("the third viewer's picture", s(10), || {
        third.received.video_frames.load(Ordering::SeqCst) >= 5
    });
    assert_eq!(seen.starts.load(Ordering::SeqCst), 2);

    // Disable, with a viewer connected: every thread ends and the ports are free for
    // the next Enable.
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    std::thread::spawn(move || {
        server.shutdown();
        d.store(true, Ordering::SeqCst);
    });
    eventually("shutdown", s(10), || done.load(Ordering::SeqCst));
    assert_eq!(
        seen.stops.load(Ordering::SeqCst),
        2,
        "shutdown stops the source"
    );
    eventually("the viewer to be told", s(2), || {
        third.received.closed.load(Ordering::SeqCst)
    });
    drop(third);
    let again = start(port, &seen);
    assert!(status(port).contains("\"viewers\":0"));
    drop(again);
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "the port is closed"
    );
}

#[test]
fn a_failing_source_is_reported_and_retried() {
    struct Broken(Arc<AtomicUsize>);
    impl VideoSource for Broken {
        fn start(&mut self, _: &StreamConfig) -> anyhow::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("SteamVR is not running on the headset")
        }
        fn next_frame(&mut self) -> anyhow::Result<EncodedFrame> {
            unreachable!("never started")
        }
        fn request_keyframe(&mut self) {}
        fn set_bitrate(&mut self, _: u32) -> anyhow::Result<()> {
            Ok(())
        }
        fn stop(&mut self) {}
    }
    let port = free_port();
    let attempts = Arc::new(AtomicUsize::new(0));
    let cfg = ServerConfig {
        port,
        ..ServerConfig::default()
    };
    let server = Server::start(cfg, Box::new(Broken(attempts.clone())), None).unwrap();
    let viewer = Viewer::join(port);
    // The viewer stays connected and the page can say what it is waiting for.
    eventually("the failure in /status", Duration::from_secs(10), || {
        let st = status(port);
        st.contains("\"viewers\":1")
            && st.contains("\"source\":\"failed\"")
            && st.contains("SteamVR is not running")
    });
    eventually("a retry", Duration::from_secs(10), || {
        attempts.load(Ordering::SeqCst) >= 2
    });
    assert!(viewer.received.connected.load(Ordering::SeqCst));
    drop(server);
}
