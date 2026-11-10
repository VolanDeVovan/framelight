//! iris V4L2 stateful encoder (`/dev/video-enc0`) on the Steam Frame.
//!
//! NV12 goes in on the OUTPUT queue, H.264 Annex-B comes out on the CAPTURE queue.
//!
//! The driver also takes RGBA (`AB24`), but its own RGB->YUV conversion is
//! BT.709 *full* range while the VUI it writes says *limited* range (and it
//! ignores the quantization we ask for), so AB24 input decodes with crushed
//! blacks and clipped whites. With NV12 the GPU does a correct limited-range
//! conversion, and it is 2.7x less memory traffic for the encoder. The input
//! buffers are MMAP buffers owned by the driver; [`Encoder::input_map`] is where
//! the capture copies each frame.
//!
//! Driver quirks this code works around (see FrameCast's notes, all
//! re-checked on the device):
//! - the OUTPUT (raw) format must be set before the CAPTURE format;
//! - the raw height is padded to 16 lines (720 -> 736, 1080 -> 1088), so the
//!   input layout has a padded height and a crop selection is set;
//! - buffer timestamps must be real frame times or rate control collapses;
//! - nothing may block on the OUTPUT queue while frames are in flight, so all
//!   dequeues here are non-blocking and waiting happens in `poll()`.
//!
//! There is no drain or restart: a stream that ends closes the device.

pub mod sys;

use std::collections::VecDeque;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use fl_core::{H264Profile, Nv12Layout};
use sys::*;
use tracing::{debug, info, warn};

pub const DEFAULT_DEVICE: &str = "/dev/video-enc0";

/// Seconds between scheduled IDRs. Long: viewers ask for a keyframe when they need one.
const GOP_SECONDS: u32 = 60;
/// Input (raw) and output (coded) buffers. A few of each: frames are queued one per
/// tick and come back within a frame or two.
const NUM_INPUTS: u32 = 3;
const NUM_OUTPUTS: u32 = 4;

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub profile: H264Profile,
    /// Coded fourcc instead of H.264 (`FWHT` drives the kernel's `vicodec` in the
    /// self-test). When set, the H.264 controls are skipped.
    pub coded_format: Option<u32>,
}

/// One encoded access unit.
#[derive(Clone, Debug)]
pub struct Packet {
    pub data: Bytes,
    pub keyframe: bool,
    pub pts_us: u64,
    /// QBUF of the input to DQBUF of this output.
    pub encode_time: Duration,
}

/// `V4L2_MPEG_VIDEO_H264_LEVEL_*` menu index of the lowest level that allows the mode
/// (Table A-1: macroblocks per second and frame size), from 3.1 up.
fn h264_level_menu(width: u32, height: u32, fps: u32) -> i32 {
    let mbs = width.div_ceil(16) * height.div_ceil(16);
    let rate = mbs * fps;
    // (menu index, MaxMBPS, MaxFS): 3.1, 3.2, 4, 4.2, 5, 5.1, 5.2
    const LEVELS: [(i32, u32, u32); 7] = [
        (9, 108_000, 3_600),
        (10, 216_000, 5_120),
        (11, 245_760, 8_192),
        (13, 522_240, 8_704),
        (14, 589_824, 22_080),
        (15, 983_040, 36_864),
        (16, 2_073_600, 36_864),
    ];
    LEVELS
        .iter()
        .find(|&&(_, mbps, fs)| rate <= mbps && mbs <= fs)
        .map_or(16, |l| l.0)
}

/// A driver buffer mapped into this process.
struct Mapping {
    ptr: *mut u8,
    len: usize,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: ptr/len come from a successful mmap.
        unsafe { libc::munmap(self.ptr as *mut _, self.len) };
    }
}

struct InputBuf {
    map: Mapping,
    queued: bool,
}

pub struct Encoder {
    fd: OwnedFd,
    layout: Nv12Layout,
    /// Bytes the driver expects as `bytesused` of an input.
    input_size: u32,
    inputs: Vec<InputBuf>,
    outputs: Vec<Mapping>,
    /// (pts_us, time queued) per input in flight, in queue order.
    in_flight: VecDeque<(u64, Instant)>,
    keyframe_pending: bool,
}

// SAFETY: the raw pointers are private mmaps of this encoder's buffers.
unsafe impl Send for Encoder {}

fn set_ctrl(fd: RawFd, id: u32, value: i32, name: &str) -> bool {
    let mut c = v4l2_control { id, value };
    match ioctl(fd, VIDIOC_S_CTRL, &mut c) {
        Ok(()) => {
            debug!("ctrl {name}={value}");
            true
        }
        Err(e) => {
            warn!("ctrl {name}={value}: {e}");
            false
        }
    }
}

fn bitrate_bps(kbps: u32) -> i32 {
    (kbps as i64 * 1000).clamp(1, i32::MAX as i64) as i32
}

fn set_bitrate_ctrls(fd: RawFd, kbps: u32) -> bool {
    let bps = bitrate_bps(kbps);
    set_ctrl(fd, CID_MPEG_VIDEO_BITRATE, bps, "bitrate")
        & set_ctrl(
            fd,
            CID_MPEG_VIDEO_BITRATE_PEAK,
            bps.saturating_add(bps / 2),
            "peak_bitrate",
        )
}

/// A zeroed multi-planar buffer descriptor with one plane.
fn buffer(type_: u32, plane: &mut v4l2_plane) -> v4l2_buffer {
    let mut b: v4l2_buffer = zeroed();
    b.type_ = type_;
    b.memory = MEMORY_MMAP;
    b.planes = plane;
    b.length = 1;
    b
}

fn map_buffer(fd: RawFd, type_: u32, index: u32) -> Result<Mapping> {
    let mut plane: v4l2_plane = zeroed();
    let mut b = buffer(type_, &mut plane);
    b.index = index;
    ioctl(fd, VIDIOC_QUERYBUF, &mut b).context("VIDIOC_QUERYBUF")?;
    let len = plane.length as usize;
    // SAFETY: mmap of a driver buffer at the offset QUERYBUF reported; the union holds
    // `mem_offset` for MMAP buffers.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            plane.m.mem_offset as libc::off_t,
        )
    };
    if ptr == libc::MAP_FAILED {
        bail!("mmap encoder buffer: {}", io::Error::last_os_error());
    }
    Ok(Mapping {
        ptr: ptr as *mut u8,
        len,
    })
}

fn set_format(fd: RawFd, type_: u32, pixelformat: u32, cfg: &EncoderConfig) -> io::Result<()> {
    let mut f: v4l2_format = zeroed();
    f.type_ = type_;
    f.fmt.pix_mp = v4l2_pix_format_mplane {
        width: cfg.width,
        height: cfg.height,
        pixelformat,
        num_planes: 1,
        colorspace: COLORSPACE_REC709,
        ycbcr_enc: YCBCR_ENC_709,
        quantization: QUANTIZATION_LIM_RANGE,
        xfer_func: XFER_FUNC_709,
        ..Default::default()
    };
    ioctl(fd, VIDIOC_S_FMT, &mut f)
}

fn get_format(fd: RawFd, type_: u32) -> io::Result<v4l2_pix_format_mplane> {
    let mut f: v4l2_format = zeroed();
    f.type_ = type_;
    ioctl(fd, VIDIOC_G_FMT, &mut f)?;
    // SAFETY: the driver filled the multi-planar member for an MPLANE type.
    Ok(unsafe { f.fmt.pix_mp })
}

fn request_buffers(fd: RawFd, type_: u32, count: u32) -> io::Result<u32> {
    let mut rb = v4l2_requestbuffers {
        count,
        type_,
        memory: MEMORY_MMAP,
        ..Default::default()
    };
    ioctl(fd, VIDIOC_REQBUFS, &mut rb)?;
    Ok(rb.count)
}

impl Encoder {
    pub fn open(cfg: &EncoderConfig) -> Result<Self> {
        let path = CString::new(cfg.device.as_str())?;
        // SAFETY: plain open(2).
        let raw = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            bail!("open {}: {}", cfg.device, io::Error::last_os_error());
        }
        // SAFETY: raw is a fresh fd we own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let f = fd.as_raw_fd();

        // Raw input first: the coded size follows from it.
        let coded = cfg.coded_format.unwrap_or(PIX_FMT_H264);
        set_format(f, BUF_TYPE_VIDEO_OUTPUT_MPLANE, PIX_FMT_NV12, cfg)
            .context("S_FMT output (raw)")?;
        set_format(f, BUF_TYPE_VIDEO_CAPTURE_MPLANE, coded, cfg).context("S_FMT capture")?;

        // Encode only the visible area, not the padding rows.
        let mut sel = v4l2_selection {
            type_: BUF_TYPE_VIDEO_OUTPUT,
            target: SEL_TGT_CROP,
            r: v4l2_rect {
                left: 0,
                top: 0,
                width: cfg.width,
                height: cfg.height,
            },
            ..Default::default()
        };
        if let Err(e) = ioctl(f, VIDIOC_S_SELECTION, &mut sel) {
            warn!("S_SELECTION crop: {e}");
        }

        let mut parm: v4l2_streamparm = zeroed();
        parm.type_ = BUF_TYPE_VIDEO_OUTPUT_MPLANE;
        // v4l2_outputparm.timeperframe = 1 / fps.
        parm.parm[2] = 1;
        parm.parm[3] = cfg.fps;
        if let Err(e) = ioctl(f, VIDIOC_S_PARM, &mut parm) {
            warn!("S_PARM: {e}");
        }

        let op = get_format(f, BUF_TYPE_VIDEO_OUTPUT_MPLANE).context("G_FMT output")?;
        let cp = get_format(f, BUF_TYPE_VIDEO_CAPTURE_MPLANE).context("G_FMT capture")?;
        for (got, want) in [(op.pixelformat, PIX_FMT_NV12), (cp.pixelformat, coded)] {
            if got != want {
                bail!(
                    "encoder took {} instead of {}",
                    fourcc_str(got),
                    fourcc_str(want)
                );
            }
        }
        let stride = op.plane_fmt[0].bytesperline;
        let layout = Nv12Layout {
            width: cfg.width,
            height: cfg.height,
            stride,
            rows: op.height,
            uv_offset: stride * op.height,
        };
        let input_size = op.plane_fmt[0].sizeimage;
        if layout.stride < cfg.width || layout.rows < cfg.height {
            bail!("encoder input layout too small: {layout:?}");
        }
        info!(
            "encoder: input {}x{} NV12 stride {} rows {} size {input_size}; coded {} {}x{} bufsize {}",
            cfg.width,
            cfg.height,
            layout.stride,
            layout.rows,
            fourcc_str(cp.pixelformat),
            cp.width,
            cp.height,
            cp.plane_fmt[0].sizeimage
        );

        if cfg.coded_format.is_none() {
            Self::set_codec_controls(f, cfg);
        }

        let count = request_buffers(f, BUF_TYPE_VIDEO_OUTPUT_MPLANE, NUM_INPUTS)
            .context("REQBUFS output")?;
        let mut inputs = Vec::new();
        for index in 0..count {
            let map = map_buffer(f, BUF_TYPE_VIDEO_OUTPUT_MPLANE, index)?;
            if map.len < layout.bytes() {
                bail!(
                    "encoder input buffer is {} bytes, the layout needs {}",
                    map.len,
                    layout.bytes()
                );
            }
            inputs.push(InputBuf { map, queued: false });
        }
        let count = request_buffers(f, BUF_TYPE_VIDEO_CAPTURE_MPLANE, NUM_OUTPUTS)
            .context("REQBUFS capture")?;
        let mut outputs = Vec::new();
        for index in 0..count {
            outputs.push(map_buffer(f, BUF_TYPE_VIDEO_CAPTURE_MPLANE, index)?);
        }
        if inputs.is_empty() || outputs.is_empty() {
            bail!("the encoder gave no buffers");
        }
        debug!(
            "encoder: {} input buffers of {} bytes, {} output buffers of {} bytes",
            inputs.len(),
            inputs[0].map.len,
            outputs.len(),
            outputs[0].len
        );

        let mut enc = Self {
            fd,
            layout,
            input_size,
            inputs,
            outputs,
            in_flight: VecDeque::new(),
            keyframe_pending: false,
        };
        for i in 0..enc.outputs.len() {
            enc.queue_output(i as u32)?;
        }
        let mut t = BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        ioctl(f, VIDIOC_STREAMON, &mut t).context("STREAMON capture")?;
        let mut t = BUF_TYPE_VIDEO_OUTPUT_MPLANE;
        ioctl(f, VIDIOC_STREAMON, &mut t).context("STREAMON output")?;
        Ok(enc)
    }

    fn set_codec_controls(f: RawFd, cfg: &EncoderConfig) {
        set_ctrl(f, CID_MPEG_VIDEO_B_FRAMES, 0, "b_frames");
        set_ctrl(
            f,
            CID_MPEG_VIDEO_BITRATE_MODE,
            BITRATE_MODE_VBR,
            "bitrate_mode",
        );
        set_bitrate_ctrls(f, cfg.bitrate_kbps);
        set_ctrl(f, CID_MPEG_VIDEO_FRAME_RC_ENABLE, 1, "frame_rc");
        let gop = cfg
            .fps
            .saturating_mul(GOP_SECONDS)
            .clamp(1, i32::MAX as u32);
        set_ctrl(f, CID_MPEG_VIDEO_GOP_SIZE, gop as i32, "gop");
        let profile = match cfg.profile {
            H264Profile::ConstrainedBaseline => H264_PROFILE_CONSTRAINED_BASELINE,
            H264Profile::Main => H264_PROFILE_MAIN,
            H264Profile::High => H264_PROFILE_HIGH,
        };
        set_ctrl(f, CID_MPEG_VIDEO_H264_PROFILE, profile, "h264_profile");
        set_ctrl(
            f,
            CID_MPEG_VIDEO_H264_LEVEL,
            h264_level_menu(cfg.width, cfg.height, cfg.fps),
            "h264_level",
        );
        set_ctrl(
            f,
            CID_MPEG_VIDEO_HEADER_MODE,
            HEADER_MODE_JOINED_WITH_1ST_FRAME,
            "header_mode",
        );
        // A viewer can only start on a keyframe that carries its parameter sets.
        set_ctrl(
            f,
            CID_MPEG_VIDEO_PREPEND_SPSPPS_TO_IDR,
            1,
            "prepend_sps_pps_to_idr",
        );
    }

    /// Changes the target bitrate of the running encoder (takes effect within a few
    /// frames on iris; no restart, no keyframe).
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        if !set_bitrate_ctrls(self.fd.as_raw_fd(), kbps) {
            bail!("the encoder refused a bitrate change to {kbps} kbit/s");
        }
        Ok(())
    }

    pub fn layout(&self) -> Nv12Layout {
        self.layout
    }

    /// CPU mapping of input buffer `index`, to be filled before
    /// [`Encoder::queue_input`]. Only write to a buffer that
    /// [`Encoder::free_input`] returned.
    pub fn input_map(&mut self, index: usize) -> &mut [u8] {
        let m = &self.inputs[index].map;
        // SAFETY: the mapping lives as long as self, and `&mut self` makes this the
        // only slice into it.
        unsafe { std::slice::from_raw_parts_mut(m.ptr, m.len) }
    }

    /// Frames queued whose output has not come back yet.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Make the next queued frame a keyframe.
    pub fn request_keyframe(&mut self) {
        self.keyframe_pending = true;
    }

    /// Reclaims inputs the encoder has consumed (never blocks) and returns
    /// one that is free to write.
    pub fn free_input(&mut self) -> Option<usize> {
        self.reclaim_inputs();
        self.inputs.iter().position(|b| !b.queued)
    }

    fn reclaim_inputs(&mut self) {
        loop {
            let mut plane: v4l2_plane = zeroed();
            let mut b = buffer(BUF_TYPE_VIDEO_OUTPUT_MPLANE, &mut plane);
            // EAGAIN when there is nothing to take back; anything else shows up as a
            // stalled encoder, which the caller handles.
            if ioctl(self.fd.as_raw_fd(), VIDIOC_DQBUF, &mut b).is_err() {
                break;
            }
            if let Some(i) = self.inputs.get_mut(b.index as usize) {
                i.queued = false;
            }
        }
    }

    /// Hands a filled input buffer to the encoder. `pts_us` must be the real
    /// frame time (rate control derives the frame rate from it).
    pub fn queue_input(&mut self, index: usize, pts_us: u64) -> Result<()> {
        if self.inputs[index].queued {
            bail!("input buffer {index} is already queued");
        }
        if self.keyframe_pending {
            self.keyframe_pending = false;
            set_ctrl(
                self.fd.as_raw_fd(),
                CID_MPEG_VIDEO_FORCE_KEY_FRAME,
                1,
                "force_key_frame",
            );
        }
        let len = self.inputs[index].map.len as u32;
        let mut plane: v4l2_plane = zeroed();
        plane.bytesused = self.input_size.min(len);
        plane.length = len;
        let mut b = buffer(BUF_TYPE_VIDEO_OUTPUT_MPLANE, &mut plane);
        b.index = index as u32;
        b.timestamp.tv_sec = (pts_us / 1_000_000) as libc::time_t;
        b.timestamp.tv_usec = (pts_us % 1_000_000) as libc::suseconds_t;
        ioctl(self.fd.as_raw_fd(), VIDIOC_QBUF, &mut b).context("QBUF output")?;
        self.inputs[index].queued = true;
        // Entries leave when their timestamp comes back on an output buffer; a driver
        // that does not copy timestamps must not make this grow for ever.
        if self.in_flight.len() >= 4 * self.inputs.len() {
            self.in_flight.pop_front();
        }
        self.in_flight.push_back((pts_us, Instant::now()));
        Ok(())
    }

    fn queue_output(&mut self, index: u32) -> Result<()> {
        let mut plane: v4l2_plane = zeroed();
        plane.length = self.outputs[index as usize].len as u32;
        let mut b = buffer(BUF_TYPE_VIDEO_CAPTURE_MPLANE, &mut plane);
        b.index = index;
        ioctl(self.fd.as_raw_fd(), VIDIOC_QBUF, &mut b).context("QBUF capture")
    }

    /// Waits up to `timeout` for an encoded frame. `None` on timeout.
    pub fn poll_packet(&mut self, timeout: Duration) -> Result<Option<Packet>> {
        if let Some(p) = self.try_dequeue()? {
            return Ok(Some(p));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let mut pfd = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
            // SAFETY: one valid pollfd.
            let r = unsafe { libc::poll(&mut pfd, 1, ms) };
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(e).context("poll encoder");
            }
            if pfd.revents & libc::POLLERR != 0 {
                bail!("encoder device error (POLLERR)");
            }
            if let Some(p) = self.try_dequeue()? {
                return Ok(Some(p));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }

    fn try_dequeue(&mut self) -> Result<Option<Packet>> {
        loop {
            let mut plane: v4l2_plane = zeroed();
            let mut b = buffer(BUF_TYPE_VIDEO_CAPTURE_MPLANE, &mut plane);
            if let Err(e) = ioctl(self.fd.as_raw_fd(), VIDIOC_DQBUF, &mut b) {
                return match e.raw_os_error() {
                    Some(libc::EAGAIN) => Ok(None),
                    _ => Err(e).context("DQBUF capture"),
                };
            }
            let (index, flags) = (b.index, b.flags);
            let pts_us = b.timestamp.tv_sec as u64 * 1_000_000 + b.timestamp.tv_usec as u64;
            let m = self
                .outputs
                .get(index as usize)
                .context("the encoder returned a buffer it was never given")?;
            let end = (plane.bytesused as usize).min(m.len);
            let start = (plane.data_offset as usize).min(end);
            // SAFETY: inside the mapping; the buffer is dequeued, so the driver does
            // not write to it until it is queued again below.
            let data = Bytes::copy_from_slice(unsafe {
                std::slice::from_raw_parts(m.ptr.add(start), end - start)
            });
            self.queue_output(index)?;
            // Opportunistically free inputs too.
            self.reclaim_inputs();
            if flags & BUF_FLAG_ERROR != 0 {
                warn!("encoder flagged an output buffer as corrupt (pts {pts_us})");
            }
            if data.is_empty() {
                continue;
            }
            // Match to the input with this timestamp (the driver copies it).
            let mut queued_at = None;
            while let Some(&(ts, at)) = self.in_flight.front() {
                if ts > pts_us {
                    break;
                }
                self.in_flight.pop_front();
                if ts == pts_us {
                    queued_at = Some(at);
                    break;
                }
            }
            return Ok(Some(Packet {
                data,
                keyframe: flags & BUF_FLAG_KEYFRAME != 0,
                pts_us,
                encode_time: queued_at.map_or(Duration::ZERO, |t| t.elapsed()),
            }));
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let f = self.fd.as_raw_fd();
        for mut t in [BUF_TYPE_VIDEO_OUTPUT_MPLANE, BUF_TYPE_VIDEO_CAPTURE_MPLANE] {
            let _ = ioctl(f, VIDIOC_STREAMOFF, &mut t);
        }
        // The mappings and the device close after this (fields drop in order).
    }
}

#[cfg(test)]
mod tests {
    use super::h264_level_menu;

    #[test]
    fn level_follows_the_mode() {
        assert_eq!(h264_level_menu(1280, 720, 30), 9); // 3.1
        assert_eq!(h264_level_menu(1280, 720, 60), 10); // 3.2
        assert_eq!(h264_level_menu(1920, 1080, 30), 11); // 4
        assert_eq!(h264_level_menu(1920, 1080, 60), 13); // 4.2
        assert_eq!(h264_level_menu(2560, 1440, 60), 15); // 5.1
        assert_eq!(h264_level_menu(3840, 2160, 60), 16); // 5.2
    }
}
