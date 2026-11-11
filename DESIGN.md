# framelight

Show what the Steam Frame headset shows to anyone on the same network: open a URL in
a browser and watch. The headset serves a small page and streams over WebRTC, so a TV
browser (LG webOS 10 = Chromium 120), a phone or a PC needs nothing installed and
nothing paired.

Watch-only: no input, no remote play, no accounts. Whoever can reach the port can
watch; that is the intended trust model for a home LAN.

(The first version was a GameStream host for Moonlight clients. It is in git history
under the tag `gamestream-final`; nothing of it is left in the tree.)

## Hard requirements

1. Near-zero headset load. Hardware encode, one small CPU copy per frame, and nothing
   running at all while streaming is switched off or nobody is watching.
2. One URL, any browser: `http://frame.local:8090` (avahi already announces the
   hostname) or the same by IP address. Plain HTTP: receive-only WebRTC does not need
   a secure origin (only camera/microphone capture does).
3. Switched on and off from inside the headset.
4. Written from scratch in Rust. Other projects are references only.

## Pipeline (on the headset)

```
SteamVR compositor
  └─ IVROverlayView::AcquireOverlayView("system.HeadsetView")  -> VkImage (RGBA)
       └─ one compute dispatch (nv12.comp: scale + BT.709 limited-range RGB->NV12)
          Not AB24: iris converts AB24 as full range while signalling limited in the VUI.
          The compositor image is only sampled, never transitioned.
            └─ one memcpy into the encoder's mmap'd input buffer
                 └─ iris V4L2 stateful encoder /dev/video-enc0 -> H.264 Annex-B
                      └─ hub: one RTP packetizer + SRTP per viewer (str0m) -> UDP
default sink monitor (dlopen'd libpulse) -> Opus, 20 ms packets -> the same hub
```

The compute pass writes a Vulkan host-visible buffer and one memcpy moves it into the
encoder's buffer (0.2 ms per frame at 1080p60). That copy is the only path.

Writing straight into the encoder's exported dmabuf was tried and removed: on the
Frame those GPU writes reach DRAM after the fence signals, so the encoder reads stale
32-byte lines (mismatches that shrink over 30 ms; PSNR dips to 36 dB against the copy
path). Release barriers to `QUEUE_FAMILY_FOREIGN_EXT` and `DMA_BUF_IOCTL_SYNC` did not
help. If it is ever worth 0.2 ms, the likely fix is the other direction: allocate
exportable memory in Vulkan and queue it to the encoder as `V4L2_MEMORY_DMABUF`. The
removed code is `git show fe01cef^:crates/fl-capture/src/lib.rs` (`import_dmabuf`).

Default mode is 1920x1080 at 60 fps. The Headset View is 2048x1280, so the picture is
scaled with its aspect kept (thin black bars left and right).

H.264 only, and one GOP per minute: viewers ask for a keyframe when they need one.
There is no drain and no encoder restart; a stream that ends closes the device.

## The streamer (`fl-webrtc`)

One TCP port and the UDP port with the same number (8090; Steam itself occupies 8080,
8081 and 8088 on the Frame).

| endpoint               | what |
|------------------------|------|
| `GET /`                | the viewer page (one embedded HTML file) |
| `POST /offer`          | SDP offer in, SDP answer out (`Viewer-Id` header) |
| `POST /bye?id=N`       | the page is closing (`sendBeacon`) |
| `GET /status`          | JSON: viewers, source state, bitrate, profile-level-id |
| `POST /bitrate?kbps=N` | bitrate ceiling of the shared encode |
| UDP 8090               | ICE, DTLS, SRTP for every viewer |

**Connection.** The page offers two receive-only transceivers and posts the offer; no
trickle, no STUN servers. The headset is ICE-lite and offers exactly one candidate: the
address the browser's HTTP request arrived on, which is by construction an address
that browser can reach. Browsers hide their own addresses behind mDNS names, which is
fine: they come to us, and their address is learned from the first check.

**One encode, many viewers.** The video and audio sources run once; the hub hands each
encoded frame to every viewer's packetizer (one shared copy of the frame, then
per-viewer SRTP, which is unavoidable). A viewer receives nothing until the first
keyframe after it connected. Keyframes are forced when a viewer connects and on
PLI/FIR, at most one per 300 ms; a connected viewer still without a picture asks again
every second. The encoder prepends SPS/PPS to every IDR, and the hub only lets a
viewer start on a frame that has both an IDR slice and an SPS.

**Codec and profile.** H.264 only (Chromium 120 has no HEVC over WebRTC), Opus for
audio. Browsers differ in what they offer: the headless Chromium used in the tests
offered Constrained Baseline and Main but not High, others add High or stop at
Constrained Baseline. The hub reads the `profile-level-id` values from each offer,
encodes in the weakest "best profile" among the current viewers (restarting the source
when that changes), and sends each viewer the payload type whose profile matches the
profile in the stream's own SPS. The level in the SPS is the real one (4.2 for
1080p60); the SDP keeps the browser's 3.1 and relies on `level-asymmetry-allowed`,
which is how WebRTC senders exceed 720p in general. Checked on the device: asking iris
for Main at level 4.2 gives an SPS of `4d002a`, sent on the browser's `4d001f` payload
type.

**Bitrate.** The page has a quality selector (3 to 30 Mbit/s) that sets a ceiling for
everyone, since there is one encode. Below the ceiling the rate follows the worst
viewer's packet loss from RTCP receiver reports: above 4 % loss it drops by as much as
is being lost (15 to 50 % per step, at most every 2 s), after 4 clean seconds it
climbs again, briskly while far below the rate that last lost packets and in 4 % steps
near it. Loss is ignored while the picture is nearly static (a trickle of packets says
nothing about the link). The iris encoder takes a new bitrate while streaming (V4L2
`video_bitrate` control: 12 -> 3 Mbit/s measured with `probe --bitrate-step`, no
keyframe, no restart); a source that cannot do that is simply restarted.

**Viewers that vanish.** A closing page says goodbye. A browser that is killed or
loses its network just goes quiet: after 1.5 s without any packet from it the hub
stops sending it video (megabits to nowhere crowd the link for the others), after 6 s
it is forgotten. If it speaks again in between, it resumes at the next keyframe. The
page reconnects by itself when its connection fails, never gets established, or the
picture stalls, and says so ("Cannot reach the headset" when `/status` does not answer
either).

**The page.** One picture and a bar of controls along the bottom: the quality presets,
sound, fullscreen, and the stream's numbers in small print. It is meant to work from a
couch as well as from a desk. Any key, pointer movement or touch shows the bar and puts
the focus in it; it leaves after 5 idle seconds, or on Back (key code 461 on an LG
remote), Escape or Backspace, which are left alone once it is gone. The key that brings
the bar back does nothing else.

| key               | what |
|-------------------|------|
| left, right       | move between the controls |
| OK / Enter, Space | press the focused one |
| up, down          | next / previous quality preset |
| M                 | sound on and off |
| F, double-click   | fullscreen |

The page starts muted, because browsers let a page make sound only after a gesture;
until then "Unmute" is the highlighted control and the one that has the focus. State
messages dim the frozen picture behind them. Sizes follow the viewport width (16 px
type on a phone, 25 px on a 1080p TV).

**Switching off.** Disabling (or stopping `serve`) closes every connection properly
(RTCP BYE, DTLS close_notify), so pages notice at once instead of after the browser's
ICE timeout, and then keep retrying until streaming is enabled again.

**When there is no picture.** With the headset in standby the compositor is paused and
hands out no view; the capture then writes black frames, so the stream and every
connection stay up and the picture returns by itself. If SteamVR is not running at all
the source fails to start; viewers stay connected, the page says what it is waiting
for, and the source is retried every 2 s.

**Idle.** Five seconds after the last viewer left, the encoder, Vulkan device, OpenVR
connection and audio capture are closed. What remains are threads asleep in blocking
calls (two in `poll`, a channel receive, two condition variables): no timers.

## The switch (`framelight app`)

Streaming is enabled and disabled from a window on the headset: one button, the state,
the viewer count and the addresses to type. The streamer listens on every interface,
so every address works; the window shows the default route's address and
`<hostname>.local` in large, and up to two more interfaces' addresses (Tailscale, for
instance) small with the interface's name (`getifaddrs`: up and running, not loopback
or link-local). The log lists all of them.

Why a plain 2D window and not a SteamVR dashboard overlay: SteamOS on the Frame runs
every flat application as a gamescope window shown as a screen in VR with the
controller as pointer (Telegram runs there that way), and Steam lists every desktop
entry in the user's application menu, so a `.desktop` file in the home directory is the
one launch path that needs no SteamVR application manifest, no Steam shortcut, no
autostart entry and no system change. An overlay would have to be registered with SteamVR and kept running to
be reachable.

The window is eframe/egui drawing with OpenGL (the `glow` backend, no wgpu) on X11,
which is all gamescope gives its clients; Wayland support is not compiled in. winit and
glutin `dlopen` what they need (libX11, libX11-xcb, libxcb, libXcursor, libXi,
libXrender, libxkbcommon, libxkbcommon-x11, and libGL or libEGL: GLX is tried first),
so the binary itself still links nothing but libc/libm. `WM_CLASS`, which the desktop
entry's `StartupWMClass` names, is the name of the binary: winit takes it from there.

`ui/view.rs` is the whole picture as a function of the window size, a theme and five
values (enabled, addresses, viewers, source state, error). The blocks sit at fixed rows
of a 90-row grid whose unit is a whole number of pixels (12 at 1080p, 8 at 720p), so
the button is in the same place in every state, and a line that would be too wide gets
a smaller size. The window asks for the size of the screen it opens on, and the
display's scale factor is ignored: the layout scales with the window by itself.

`ui/theme.rs` is the look as data: one font, text sizes, corner radius and ten colours.
There are two for now, picked by the `THEME` constant, and one of them will go:
`PIXEL` (Press Start 2P, an 8 px bitmap-style face whose sizes are kept to multiples
of 8 px so it stays sharp; square shapes, amber on near-black) and `STEAM` (Inter, dark
blue, light blue accent, rounded corners). Both fonts are OFL and compiled in from
`assets/fonts`, next to their licences; only the selected one ends up in the binary.

Nothing is drawn unless something happened: input, a signal, or, while streaming is
enabled, one frame a second for the viewer count. While it is disabled the process
does not wake up (under Xvfb with llvmpipe: no CPU time and no wake-up of the main
thread in 10 s). The streamer lives inside this process: closing the window ends streaming.
Input is ignored for the first 0.8 s so the click that launched the app cannot press
the button. Return, keypad Enter and space press it too, once per key press.

`SIGUSR1` presses the button, for a shell (`pkill -USR1 -x framelight`). `SIGINT`,
`SIGTERM` and `SIGUSR1` are blocked before any thread starts and read from a signalfd,
so a signal cannot fall between "checked" and "asleep". In the app a thread
(`fl-signals`) sleeps on that descriptor, hands each signal to the window over a
channel and asks egui for a frame; `SIGINT` and `SIGTERM` close the window.

Not checked on the headset yet: that its Xwayland offers the window a working GL
context (and which driver), what the GL context costs in memory (the old CPU-drawn
window was 12 MB in all), and that gamescope gives the window keyboard focus, which
winit needs before it reports keys.

`framelight serve` is the same streamer without a window, enabled until it is
stopped (asleep in `poll` on the signalfd).

## Command line

```
framelight app   [options]     the switch window
framelight serve [options]     headless
    --port N  --width W --height H --fps F  --bitrate-kbps B  --no-audio  --synthetic
framelight probe ...           capture + encode alone, to a file, with timings
framelight selftest ...        synthetic picture -> GPU NV12 -> encoder, checked
framelight version
```

`framelight help` has the details. Unknown options are errors. `--synthetic` replaces
SteamVR with a moving test picture (the encoder is still the real one). The process
runs at nice 10, below the game.

## Threads

Blocking threads, no async runtime. The streamer's all belong to `fl_webrtc::Server`,
which starts them in `start` and joins them when it is dropped:

| thread | does | asleep in |
|--------|------|-----------|
| `fl-http` | accepts; one short-lived thread per request | `poll` (listener + stop eventfd) |
| `fl-udp` | forwards datagrams to the hub | `poll` (socket + stop eventfd) |
| `fl-hub` | owns every peer connection; everything reaches it as a message on one channel | channel receive |
| `fl-video` | runs the video source while someone watches (capture tick + encoder poll) | a condition variable |
| `fl-audio` | runs the audio source while someone watches | a condition variable |

plus the libpulse capture thread while someone watches, and in `framelight app` the
window's event loop on the main thread, `fl-signals` (asleep in `poll` on the signalfd)
and whatever threads the GL driver starts. The hub tells the pumps what
it wants (run or not, profile, bitrate, keyframe due) through one mutex-protected
struct; the pumps answer with messages. Stopping raises the eventfd, tells the pumps
and sends the hub a message: no thread's exit depends on another thread.

Nothing is allocated per packet on the capture side; per frame there is one copy out
of the encoder buffer and one into the shared `Arc`.

## Measured on the headset (2026-11-10, 1920x1080@60, H.264 Main)

Taken before the cleanup of the same day, which did not touch the data path (capture,
encode, packetizing) but did change what sleeps where; not measured again since. The
window was the earlier CPU-drawn one: the first two rows (and the thread count and
memory) have to be measured again with the egui window.

| state | CPU of one core (of 8) |
|-------|------------------------|
| app open, streaming disabled | 0.00 % (1 thread, 12 MB RSS) |
| enabled, nobody watching | 0.00-0.07 % |
| 1 viewer, static picture (headset in standby, ~0.1 Mbit/s) | 3.1-3.8 % |
| 2 viewers, static picture | 3.4-3.7 % |
| 1 viewer, headset in use, ~11.4 Mbit/s | 5.1 % |
| each further viewer at 12 Mbit/s | about +2 % (synthetic source: 40.4 -> 42.8 %) |

Capture and encode alone are 2.1 % (`probe`); the rest is audio (0.5 %) and
packetizing, SRTP and `sendto` per viewer. Compositor GPU time (from SteamVR's frame
timings; the tool that read them was removed with the rest of the diagnostics): 2.51 ms
mean with nobody watching, 2.49-2.64 ms with one or two viewers, no dropped frames. Page open to
first decoded frame: 250-440 ms on a healthy link (ICE + DTLS 100-260 ms, the source
starts in ~50 ms and its first frame is a keyframe). Encode latency 9.5 ms mean.

Glass-to-glass was not measured end to end. From its parts: wait for the 60 Hz capture
tick (0-17 ms), GPU conversion 2 ms, encode 10 ms, network, browser jitter buffer
(4-24 ms reported by `getStats`), decode and the display's own lag: roughly 50-80 ms
plus the display.

The Frame's Wi-Fi is the limit, not its CPU. With the headset idle its 5 GHz uplink
(-77 dBm here) carried two viewers at 12 Mbit/s each with a handful of lost packets.
With the headset in use and streaming PC VR through its dongle, the same uplink lost
40-80 % of the packets of two 12 Mbit/s viewers until the rate adapted down to
3-4.5 Mbit/s, and connecting took 4-6 s instead of 0.3 s.

## Headset facts (probed 2026-11-10)

- SteamOS 0.4.5, kernel 6.18.0 aarch64, glibc 2.39, 8 cores, 16 GB.
- Session: gamescope (`--backend openvr`) with two Xwaylands (`:0` 1920x1080 for
  applications, `:1`), Steam in `-gamepadui -vrgamepadui`.
- `/dev/video-enc0` -> iris_encoder. CAPTURE: H264, HEVC. OUTPUT: Q08C, NV12, NV21,
  AB24 (RGBA), QC24. Controls: bitrate/peak/VBR, gop, b_frames (keep 0),
  force_key_frame, h264_profile (Baseline, Constrained Baseline, Main, High,
  Constrained High), h264_level, ltr_count<=2, intra_refresh_period(+type),
  prepend_sps_and_pps_to_idr, slice controls.
- `libopenvr_api.so`: `/opt/steamvr/bin/linuxarm64/` (dlopen it).
- avahi-daemon active (`frame.local`). User `steamos`, groups video, render, audio.
- Tools on device: v4l2-ctl, ffmpeg, gst-launch-1.0, glslc, pactl, pw-cat, gcc.

## Encoder quirks (from FrameCast "Findings", re-checked)

- Set the raw (OUTPUT) format first, then CAPTURE.
- Height padding: 720 -> 736, 1080 -> 1088; crop.
- Timestamps must be real frame times or rate control collapses.
- Never block on OUTPUT dequeue while frames are in flight.
- ffmpeg h264_v4l2m2m / gstreamer v4l2h264enc do not work on this driver.

## Build, deploy, start

Cross-compile on any Linux machine with rustup, zig, cargo-zigbuild, cmake and a C
compiler (cmake builds libopus; `rust-toolchain.toml` brings the aarch64 target;
`nix develop` provides all of it on Nix, and is the only thing the flake is for):

    cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.39 -p framelight

The result links only libc and libm: openvr_api, vulkan, libpulse and the window's X11
and GL libraries are dlopen'd;
libopus and the crypto (aws-lc-rs, the only C besides opus) are static.

Release builds come from CI instead (`.github/workflows/release.yml`, on a `v*` tag):
a native arm64 build on Ubuntu 24.04, whose glibc is the headset's 2.39.

`packaging/dist.sh BINARY OUTDIR` packs the binary, `packaging/framelight-app.sh` and
the icon into `framelight-aarch64-linux.tar.gz`. `packaging/install.sh` (attached to
every release) downloads that archive, or takes a local one as its argument, unpacks it
into `~/.local/share/framelight/` and writes `~/.local/share/applications/framelight.desktop`
with absolute paths; Steam lists it next to Firefox and Telegram as "framelight". Files
are copied to `.new` and moved over, so a running binary is never overwritten in place.
No sudo, no system changes, no `systemctl enable`. `install.sh --uninstall` removes both.

The app's log is `~/.local/share/framelight/app.log` (the previous run's is `app.log.1`).

The icon is `assets/framelight.svg`; `assets/framelight.png` is it rendered at 512 px
(`rsvg-convert -w 512 -h 512`).

## Workspace layout

- `crates/fl-core` — shared types: the two source traits, what flows through them, and
  the layout of the encoder's input buffers.
- `crates/fl-capture` — OpenVR Headset View -> NV12 in an encoder buffer (Vulkan
  compute); a synthetic picture provider for tests.
- `crates/fl-encode` — iris V4L2 stateful H.264 encoder, raw ioctls.
- `crates/fl-audio` — default sink monitor (dlopen'd libpulse) -> Opus; a tone source for tests.
- `crates/fl-webrtc` — the streamer. Depends only on the traits, so it runs on the
  desktop with other sources. Modules: `api` (the endpoints), `http`, `net` (UDP
  receive, stop signal), `hub`, `viewer`, `pump`, `adapt` (bitrate), `clock` (audio
  timeline), `h264` (SPS and SDP profile parsing), `page.html`.
- `crates/framelight` — the headset binary: `source` (capture + encode as a
  `VideoSource`), `ui` (the window: the eframe app, with `ui/view` for what it shows and
  `ui/theme` for how it looks), `signals`, `cli`, `probe`, `selftest`.
- `crates/fl-devhost` — desktop dev host: ffmpeg test pattern, monitor/tone audio.
- `tools/e2e` — headless-Chromium test of a running streamer.
- `packaging` — the launcher, the release archive script and the installer.
- `assets` — the icon, and `assets/fonts`: the window's fonts with their OFL texts.
- `tools/headset` — a CPU measuring helper.
- `tools/vicodec-vm` — the self-test and a probe against the kernel's vicodec in a VM.

## Interface between halves

`fl-core`: `VideoSource { start(cfg), next_frame() -> EncodedFrame, request_keyframe(),
set_bitrate(kbps), stop() }` and `AudioSource { start(), next_packet() -> AudioPacket,
stop() }`. Rules the streamer relies on:

- Nothing expensive is held between `stop` and the next `start`.
- `next_frame`/`next_packet` block, but never for more than a few seconds: the pumps
  can only stop a source between calls, so a stuck source returns an error instead and
  is started again.
- The first frame after `start` is a keyframe, and keyframes carry SPS/PPS.
- `EncodedFrame::captured` is the instant the picture was taken. It becomes the RTP
  timestamp and the sender-report clock, so it stays monotonic across source restarts.
  Audio has only a packet count paced by the sound card; the hub anchors it to arrival
  time and re-anchors when the two clocks drift 80 ms apart.
- `set_bitrate` may fail; the source is then restarted with the new rate.

## Tests

- `cargo test`, `cargo clippy --all-targets`: SDP/SPS parsing, payload type choice,
  bitrate control, clocks, HTTP parsing and routing, command-line parsing, and the
  window run headless with `egui_kittest` (no display, no GPU): in both themes and at
  1920x1080, 1280x720 and 800x600 every line is inside the window, none overlaps
  another or the button, the button is where it was in every state, a long address
  shrinks to fit, and a click, Return or space presses the button once.
- `crates/fl-webrtc/tests/loopback.rs` (part of `cargo test`, 12 s): the whole
  streamer over 127.0.0.1 with str0m playing the browser and fake sources. Offer and
  answer, ICE, DTLS, a first frame that is a keyframe with its SPS, two viewers on one
  encode, PLI, the bitrate endpoint, a goodbye, a viewer that vanishes, linger and
  idle, restart for the next viewer, shutdown with a viewer connected (which is told),
  the port bound again at once, and a source that fails to start. It needs no browser,
  encoder, GPU or sound.
- End to end on the desktop: `cargo run -p fl-devhost -- --audio tone`, then
  `node tools/e2e/browser-test.mjs http://<LAN address>:8090/ --tabs 2 --bitrate 3000
  --chromium /path/to/chromium` (any Chromium; `nix develop` puts one on PATH).
  It opens the page in headless Chromium (muted at the browser level, throwaway
  profile), and reports frames decoded, time to first frame, the negotiated codec line,
  audio energy and the server's status; it fails if a tab decoded nothing. Use the LAN
  address, not 127.0.0.1: browsers gather no loopback candidates.
- The same script against the headset (`http://frame.local:8090/`).
- `tools/vicodec-vm/run-vm.sh`: `framelight selftest` (the shader's output compared
  byte for byte with a CPU reference; one packet per frame) and a synthetic `probe`
  against vicodec with lavapipe, in a throwaway NixOS VM. Optional, and the one tool
  here that needs Nix.
- The window, by hand: `Xvfb` + `xdotool` (click, `windowfocus` then Return) + a
  screenshot; Mesa's llvmpipe does for a driver. On NixOS a cargo-built binary needs
  `LD_LIBRARY_PATH` to find the X11 and GL libraries it dlopens.

## Compared with SFRecorder (GPL-3)

Read in full; nothing was copied, so the project stays MIT.

- Capture: SFRecorder reads the panel scanout through KMS with a privileged helper and
  un-warps the lens itself. That needs a root-installed helper (ruled out here) and a
  second undistortion; the Headset View overlay is already the flat picture and needs
  no privileges. Its GPU stage is one compute dispatch into the encoder's dmabuf, the
  path that showed stale lines here (see Pipeline).
- Encoder: HEVC only, no keyframe-on-demand, bindgen at build time.
- Audio: `pa_simple` (blocking reads, link-time libpulse) into AAC. A stream must be
  stoppable at once and the binary must not link device libraries, hence the dlopen'd
  asynchronous API here.
- Taken as lessons, not code: run below the game for CPU (nice 10), and do
  not ask Vulkan for a low global queue priority (it starved on the Frame).

## Open ends

- Rate control is crude: loss from receiver reports, in steps at most every 2 s. str0m
  has a pacer and a TWCC-based estimator; they would react faster on a link as thin as
  the Frame's uplink during PC VR streaming, and a pacer would stop a keyframe leaving
  as one burst per viewer.
- `sendto` per packet; `sendmmsg`/UDP GSO would cut the per-viewer cost.
- IPv4 only.
- Zero-copy into the encoder (see Pipeline).
- Anyone on the network can also call `/bitrate` and `/bye?id=N` (ids are sequential).
  That is within the trust model (whoever can reach the port may watch), but it does
  mean one viewer can lower the quality for all or disconnect another.
- Not tried on a real TV, phone, Firefox or Safari: only headless Chromium 157 on Linux.

## References (read-only)

- FrameCast (MIT): framecap.cpp, hwenc.c, README Findings.
- SFRecorder (GPL-3): compared above; no code taken.
- str0m (MIT/Apache-2.0): the WebRTC implementation; `examples/chat.rs` is the model for
  the hub's run loop.
