# framelight

Stream what the Steam Frame headset shows to TVs, phones and PCs, with near-zero
cost on the headset. The headset acts as a **Moonlight-compatible (GameStream) host**,
so stock Moonlight clients work everywhere: moonlight-tv on the LG webOS TV,
moonlight-qt on Linux, Moonlight on iOS/Android/Steam Machine.

Watch-only: no input injection. One "app" in the app list: `Headset View`.

## Hard requirements

1. Near-zero headset load. No CPU pixel copies, no software encode, nothing runs
   (beyond mDNS + listeners) while no client is connected.
2. Clients discover the headset by mDNS (`_nvstream._tcp`).
3. Written from scratch in Rust. Other projects are references only.

## Pipeline (on the headset)

```
SteamVR compositor
  └─ IVROverlayView::AcquireOverlayView("system.HeadsetView")  -> VkImage (RGBA)
       └─ one compute dispatch (nv12.comp: scale + BT.709 limited-range RGB->NV12)
          writing into a dmabuf exported from the encoder's OUTPUT queue (VIDIOC_EXPBUF).
          Not AB24: iris converts AB24 as full range while signalling limited in the VUI.
          The compositor image is only sampled, never transitioned (blitting it froze after frame 1).
            └─ iris V4L2 stateful encoder /dev/video-enc0 -> HEVC (or H.264) Annex-B
                 └─ GameStream video packetizer + Reed-Solomon FEC -> UDP
PipeWire/Pulse monitor -> Opus -> GameStream audio
```

Default path today: the compute pass writes a Vulkan host-visible buffer and one memcpy
moves it into the mmap'd encoder buffer (0.2 ms, ~2.6% of a core at 1080p60, measured
2026-11-10). Writing straight into the encoder's exported dmabuf (`--zero-copy`) is
off by default: on the Frame those GPU writes reach DRAM after the fence signals, so
the encoder and an uncached CPU mapping read stale 32-byte lines (selftest mismatches
that shrink over 30 ms; PSNR dips to 36 dB against the copy path). Release barriers to
`QUEUE_FAMILY_FOREIGN_EXT` and `DMA_BUF_IOCTL_SYNC` did not help. The likely fix is the
other direction: allocate exportable memory in Vulkan and queue it to the encoder as
`V4L2_MEMORY_DMABUF`.

## Headset facts (probed 2026-11-10)

- SteamOS 0.4.5, kernel 6.18.0 aarch64, glibc 2.39, 8 cores, 16 GB.
- `/dev/video-enc0` -> iris_encoder. CAPTURE: H264, HEVC. OUTPUT: Q08C, NV12, NV21,
  AB24 (RGBA), QC24. Controls: bitrate/peak/VBR, gop, b_frames (keep 0),
  force_key_frame, ltr_count<=2, use_ltr_frames, frame_ltr_index,
  intra_refresh_period(+type), prepend_sps_and_pps_to_idr, slice controls.
- `libopenvr_api.so`: `/opt/steamvr/bin/linuxarm64/` (dlopen it).
- avahi-daemon active. User `steamos`, groups video, render, audio.
- Tools on device: v4l2-ctl, ffmpeg, gst-launch-1.0, glslc, pactl, pw-cat, gcc.

## Encoder quirks (from FrameCast "Findings", verify)

- Set the raw (OUTPUT) format first, then CAPTURE.
- Height padding: 720 -> 736, 1080 -> 1088; crop.
- Timestamps must be real frame times or rate control collapses.
- Never block on OUTPUT dequeue while frames are in flight.
- ffmpeg h264_v4l2m2m / gstreamer v4l2h264enc do not work on this driver.

## Build

Cross-compile on the desktop:
`nix shell nixpkgs#cargo-zigbuild nixpkgs#zig -c cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.39`
No link-time dependency on device libraries: openvr_api, vulkan, libpulse are dlopen'd.
Deploy: `scp` into `/home/steamos/framelight/` (never overwrite a running binary in
place, copy to `.new` and `mv`). No sudo, no system changes, no `systemctl enable`.

## Workspace layout

- `crates/fl-core` — the two traits below; nothing else.
- `crates/fl-capture` — OpenVR Headset View -> NV12 in an encoder buffer (Vulkan compute).
- `crates/fl-encode` — iris V4L2 stateful encoder, raw ioctls.
- `crates/fl-audio` — default sink monitor (dlopen'd libpulse) -> CBR Opus; a tone source for tests.
- `crates/fl-gamestream` — GameStream host: mDNS, HTTP/HTTPS serverinfo, pairing,
  applist, launch/resume/cancel, RTSP, video RTP+FEC, ENet control, audio.
  Depends only on the traits, so it runs on the desktop with a synthetic source.
- `crates/framelight` — the headset binary (`serve`, plus `probe`/`selftest`/`vrtimings`).
- `crates/fl-devhost` — desktop dev host: ffmpeg test pattern, monitor/silence/tone audio.

## Interface between halves

`fl-core`: `VideoSource { start(cfg), next_frame() -> EncodedFrame, request_keyframe(), stop() }`
and `AudioSource { set_packet_duration(ms), start(), next_packet() -> AudioPacket, stop() }`.
Rules the host relies on:

- Nothing expensive is held between `stop` and the next `start` (encoder, Vulkan device
  and the OpenVR connection live only while a client watches).
- `next_frame`/`next_packet` block, but never for more than a few seconds: the stream
  can only be stopped between calls, so a stuck source returns an error instead.
- Audio packets of one stream have equal sizes (CBR), which the audio FEC needs.

## Threads

Blocking threads, no async runtime. Idle: three accept loops and the mDNS daemon.
Per stream: control (ENet), video (capture tick + encoder poll + packetize + `sendmmsg`),
audio (Opus + send) and the libpulse capture thread. One stream at a time; starts and
stops are serialised on the stream slot. Nothing is allocated per packet (one reused buffer per frame), and the only per-byte
CPU work besides copying the encoded frame is the Reed-Solomon parity (NEON on the
headset).

## Protocol notes (things a client depends on)

- Keyframes must start with VPS/SPS(/PPS); the IRAP slice must begin in the first packet
  (`h26x.rs` fixes up what the encoder emits).
- `serverinfo` lists display modes: moonlight-tv refuses unlisted modes when its
  "unsupported settings" switch is off.
- Clients set to 5.1/7.1 build a multistream Opus decoder. DESCRIBE offers a two-stream
  layout (stereo pair + silent mono) and packets are re-framed for it; stereo clients
  get plain packets.
- RFI is advertised but answered with a keyframe; advertising it makes Moonlight report
  loss a frame earlier.
- mDNS is IPv4 only, like the listeners.
- A fixed `--pin` is a convenience for a headset without a keyboard, not a secret: the
  GameStream ceremony lets an attacker test PIN guesses offline after one attempt.

## Compared with SFRecorder (GPL-3)

Read in full; nothing was copied, so the project stays MIT.

- Capture: SFRecorder reads the panel scanout through KMS with a privileged helper and
  un-warps the lens itself. That needs a root-installed helper (ruled out here) and a
  second undistortion; the Headset View overlay is already the flat picture and needs
  no privileges. Its GPU stage has the same shape as ours (one compute dispatch into the
  encoder's dmabuf).
- Encoder: HEVC only, no keyframe-on-demand, no drain/restart, bindgen at build time.
  Ours covers H.264 too and the stateful flow Moonlight needs.
- Audio: `pa_simple` (blocking reads, link-time libpulse) into AAC. A stream must be
  stoppable at once and the binary must not link device libraries, hence the dlopen'd
  asynchronous API here.
- Taken as lessons, not code: run below the game for CPU (`--nice`, default 10), and do
  not ask Vulkan for a low global queue priority (it starved on the Frame).

## References (read-only, in the session scratchpad `refs/`)

- FrameCast (MIT): framecap.cpp, hwenc.c, README Findings.
- moonshine (BSD-2): GameStream protocol in Rust.
- SFRecorder (GPL-3): compared above; no code taken.
- moonlight-common-c (GPL-3): the client's protocol implementation, read to see what it expects.
