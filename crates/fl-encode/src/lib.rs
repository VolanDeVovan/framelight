//! iris V4L2 stateful encoder (`/dev/video-enc0`) on the Steam Frame.
//!
//! NV12 goes in on the OUTPUT queue, H.264 or HEVC Annex-B comes out on the
//! CAPTURE queue.
//!
//! The driver also takes RGBA (`AB24`), but its own RGB->YUV conversion is
//! BT.709 *full* range while the VUI it writes says *limited* range (and it
//! ignores the quantization we ask for), so AB24 input decodes with crushed
//! blacks and clipped whites. With NV12 the GPU does a correct limited-range
//! conversion, and it is 2.7x less memory traffic for the encoder. The input
//! buffers are MMAP buffers owned by the driver; [`Encoder::input_dmabuf`]
//! exports them (VIDIOC_EXPBUF) so the GPU can write frames straight into
//! them, and [`Encoder::input_map`] gives the CPU mapping for the one-copy
//! fallback.
//!
//! Driver quirks this code works around (see FrameCast's notes, all
//! re-checked on the device):
//! - the OUTPUT (raw) format must be set before the CAPTURE format;
//! - the raw height is padded to 16 lines (720 -> 736, 1080 -> 1088), so the
//!   input layout has a padded height and a crop selection is set;
//! - buffer timestamps must be real frame times or rate control collapses;
//! - nothing may block on the OUTPUT queue while frames are in flight, so all
//!   dequeues here are non-blocking and waiting happens in `poll()`.

pub mod sys;

use std::collections::VecDeque;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use fl_core::Codec;
use sys::*;
use tracing::{debug, info, warn};

pub const DEFAULT_DEVICE: &str = "/dev/video-enc0";

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    /// Frames between scheduled IDRs. Large: clients ask for IDRs on loss.
    pub gop: u32,
    /// Cyclic intra refresh period in frames (0 = off).
    pub intra_refresh: u32,
    pub num_inputs: u32,
    pub num_outputs: u32,
    /// Coded fourcc override (e.g. `FWHT` to drive the kernel's `vicodec`
    /// in tests). When set, the H.264/HEVC-specific controls are skipped.
    pub coded_format: Option<u32>,
}

impl EncoderConfig {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32, codec: Codec) -> Self {
        Self {
            device: DEFAULT_DEVICE.into(),
            width,
            height,
            fps,
            bitrate_kbps,
            codec,
            gop: fps.saturating_mul(60).max(1),
            intra_refresh: 0,
            num_inputs: 3,
            num_outputs: 4,
            coded_format: None,
        }
    }
}

/// Where a frame must be written into an input buffer: single-plane NV12,
/// luma rows followed by interleaved CbCr rows at `uv_offset`.
#[derive(Clone, Copy, Debug)]
pub struct InputLayout {
    pub width: u32,
    pub height: u32,
    /// Bytes per row of both planes.
    pub stride: u32,
    /// Byte offset of the chroma plane.
    pub uv_offset: u32,
    /// Rows in the buffer (height rounded up by the driver).
    pub padded_height: u32,
    /// Bytes the driver expects as `bytesused`.
    pub size: u32,
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
    dmabuf: OwnedFd,
    queued: bool,
}

pub struct Encoder {
    fd: OwnedFd,
    layout: InputLayout,
    inputs: Vec<InputBuf>,
    outputs: Vec<Mapping>,
    /// (pts_us, time queued) per input in flight, in queue order.
    in_flight: VecDeque<(u64, Instant)>,
    keyframe_pending: bool,
    /// The encoder returned its LAST buffer (after `drain`).
    drained: bool,
    streaming: bool,
}

// The raw pointers are private mmaps of this encoder's buffers.
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

fn map_buffer(fd: RawFd, type_: u32, index: u32) -> Result<Mapping> {
    let mut plane: v4l2_plane = zeroed();
    let mut b: v4l2_buffer = zeroed();
    b.index = index;
    b.type_ = type_;
    b.memory = MEMORY_MMAP;
    b.planes = &mut plane;
    b.length = 1;
    ioctl(fd, VIDIOC_QUERYBUF, &mut b).context("VIDIOC_QUERYBUF")?;
    let len = plane.length as usize;
    // SAFETY: mmap of a driver buffer at the offset QUERYBUF reported.
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

        let quant = QUANTIZATION_LIM_RANGE;
        let raw = PIX_FMT_NV12;
        // Raw input first: the coded size follows from it.
        let mut of: v4l2_format = zeroed();
        of.type_ = BUF_TYPE_VIDEO_OUTPUT_MPLANE;
        unsafe {
            let p = &mut of.fmt.pix_mp;
            p.width = cfg.width;
            p.height = cfg.height;
            p.pixelformat = raw;
            p.num_planes = 1;
            p.colorspace = COLORSPACE_REC709;
            p.ycbcr_enc = YCBCR_ENC_709;
            p.quantization = quant;
            p.xfer_func = XFER_FUNC_709;
        }
        ioctl(f, VIDIOC_S_FMT, &mut of).context("S_FMT output (raw)")?;

        let coded = match (cfg.coded_format, cfg.codec) {
            (Some(f), _) => f,
            (None, Codec::H264) => PIX_FMT_H264,
            (None, Codec::Hevc) => PIX_FMT_HEVC,
        };
        let mut cf: v4l2_format = zeroed();
        cf.type_ = BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        unsafe {
            let p = &mut cf.fmt.pix_mp;
            p.width = cfg.width;
            p.height = cfg.height;
            p.pixelformat = coded;
            p.num_planes = 1;
            p.colorspace = COLORSPACE_REC709;
            p.ycbcr_enc = YCBCR_ENC_709;
            p.quantization = quant;
            p.xfer_func = XFER_FUNC_709;
        }
        ioctl(f, VIDIOC_S_FMT, &mut cf).context("S_FMT capture")?;

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
        parm.parm[2] = 1;
        parm.parm[3] = cfg.fps;
        if let Err(e) = ioctl(f, VIDIOC_S_PARM, &mut parm) {
            warn!("S_PARM: {e}");
        }

        of.type_ = BUF_TYPE_VIDEO_OUTPUT_MPLANE;
        ioctl(f, VIDIOC_G_FMT, &mut of).context("G_FMT output")?;
        cf.type_ = BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        ioctl(f, VIDIOC_G_FMT, &mut cf).context("G_FMT capture")?;
        let (op, cp) = unsafe { (of.fmt.pix_mp, cf.fmt.pix_mp) };
        if op.pixelformat != raw {
            bail!(
                "encoder took {} instead of {}",
                fourcc_str(op.pixelformat),
                fourcc_str(raw)
            );
        }
        if cp.pixelformat != coded {
            bail!(
                "encoder took {} instead of {}",
                fourcc_str(cp.pixelformat),
                fourcc_str(coded)
            );
        }
        let stride = op.plane_fmt[0].bytesperline;
        let layout = InputLayout {
            uv_offset: stride * op.height,
            width: cfg.width,
            height: cfg.height,
            stride,
            padded_height: op.height,
            size: op.plane_fmt[0].sizeimage,
        };
        if layout.stride < cfg.width || layout.padded_height < cfg.height {
            bail!("encoder input layout too small: {layout:?}");
        }
        info!(
            "encoder: input {}x{} {} stride {} rows {} size {} (cs {} enc {} q {}); coded {} {}x{} bufsize {}",
            op.width,
            cfg.height,
            fourcc_str(raw),
            layout.stride,
            layout.padded_height,
            layout.size,
            op.colorspace,
            op.ycbcr_enc,
            op.quantization,
            fourcc_str(cp.pixelformat),
            cp.width,
            cp.height,
            cp.plane_fmt[0].sizeimage
        );

        if cfg.coded_format.is_none() {
            Self::set_codec_controls(f, cfg);
        }

        let mut rb = v4l2_requestbuffers {
            count: cfg.num_inputs,
            type_: BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            memory: MEMORY_MMAP,
            ..Default::default()
        };
        ioctl(f, VIDIOC_REQBUFS, &mut rb).context("REQBUFS output")?;
        let mut inputs = Vec::new();
        for index in 0..rb.count {
            let map = map_buffer(f, BUF_TYPE_VIDEO_OUTPUT_MPLANE, index)?;
            let mut e = v4l2_exportbuffer {
                type_: BUF_TYPE_VIDEO_OUTPUT_MPLANE,
                index,
                plane: 0,
                flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
                ..Default::default()
            };
            ioctl(f, VIDIOC_EXPBUF, &mut e).context("VIDIOC_EXPBUF")?;
            // SAFETY: EXPBUF returned a new fd.
            let dmabuf = unsafe { OwnedFd::from_raw_fd(e.fd) };
            inputs.push(InputBuf {
                map,
                dmabuf,
                queued: false,
            });
        }

        let mut rb = v4l2_requestbuffers {
            count: cfg.num_outputs,
            type_: BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            memory: MEMORY_MMAP,
            ..Default::default()
        };
        ioctl(f, VIDIOC_REQBUFS, &mut rb).context("REQBUFS capture")?;
        let mut outputs = Vec::new();
        for index in 0..rb.count {
            outputs.push(map_buffer(f, BUF_TYPE_VIDEO_CAPTURE_MPLANE, index)?);
        }
        info!(
            "encoder: {} input buffers of {} bytes, {} output buffers of {} bytes",
            inputs.len(),
            inputs.first().map_or(0, |b| b.map.len),
            outputs.len(),
            outputs.first().map_or(0, |b| b.len)
        );

        let mut enc = Self {
            fd,
            layout,
            inputs,
            outputs,
            in_flight: VecDeque::new(),
            keyframe_pending: false,
            drained: false,
            streaming: false,
        };
        for i in 0..enc.outputs.len() {
            enc.queue_output(i as u32)?;
        }
        let mut t = BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        ioctl(f, VIDIOC_STREAMON, &mut t).context("STREAMON capture")?;
        let mut t = BUF_TYPE_VIDEO_OUTPUT_MPLANE;
        ioctl(f, VIDIOC_STREAMON, &mut t).context("STREAMON output")?;
        enc.streaming = true;
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
        let bps = (cfg.bitrate_kbps as i64 * 1000).clamp(1, i32::MAX as i64) as i32;
        set_ctrl(f, CID_MPEG_VIDEO_BITRATE, bps, "bitrate");
        set_ctrl(
            f,
            CID_MPEG_VIDEO_BITRATE_PEAK,
            bps.saturating_add(bps / 2),
            "peak_bitrate",
        );
        set_ctrl(f, CID_MPEG_VIDEO_FRAME_RC_ENABLE, 1, "frame_rc");
        set_ctrl(
            f,
            CID_MPEG_VIDEO_GOP_SIZE,
            cfg.gop.min(i32::MAX as u32) as i32,
            "gop",
        );
        match cfg.codec {
            Codec::H264 => set_ctrl(
                f,
                CID_MPEG_VIDEO_H264_PROFILE,
                H264_PROFILE_HIGH,
                "h264_profile",
            ),
            Codec::Hevc => set_ctrl(
                f,
                CID_MPEG_VIDEO_HEVC_PROFILE,
                HEVC_PROFILE_MAIN,
                "hevc_profile",
            ),
        };
        set_ctrl(
            f,
            CID_MPEG_VIDEO_HEADER_MODE,
            HEADER_MODE_JOINED_WITH_1ST_FRAME,
            "header_mode",
        );
        set_ctrl(
            f,
            CID_MPEG_VIDEO_PREPEND_SPSPPS_TO_IDR,
            1,
            "prepend_sps_pps_to_idr",
        );
        if cfg.intra_refresh > 0 {
            set_ctrl(
                f,
                CID_MPEG_VIDEO_INTRA_REFRESH_PERIOD_TYPE,
                INTRA_REFRESH_CYCLIC,
                "intra_refresh_type",
            );
            set_ctrl(
                f,
                CID_MPEG_VIDEO_INTRA_REFRESH_PERIOD,
                cfg.intra_refresh as i32,
                "intra_refresh_period",
            );
        }
    }

    pub fn layout(&self) -> InputLayout {
        self.layout
    }

    pub fn num_inputs(&self) -> usize {
        self.inputs.len()
    }

    /// The dmabuf behind input buffer `index` (borrowed; dup it to keep it).
    pub fn input_dmabuf(&self, index: usize) -> &OwnedFd {
        &self.inputs[index].dmabuf
    }

    /// CPU mapping of input buffer `index`, for the copy fallback. Only
    /// write to a buffer that [`Encoder::free_input`] returned.
    pub fn input_map(&mut self, index: usize) -> &mut [u8] {
        let m = &self.inputs[index].map;
        // SAFETY: the mapping lives as long as self; the buffer is not queued.
        unsafe { std::slice::from_raw_parts_mut(m.ptr, m.len) }
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

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
            let mut b: v4l2_buffer = zeroed();
            b.type_ = BUF_TYPE_VIDEO_OUTPUT_MPLANE;
            b.memory = MEMORY_MMAP;
            b.planes = &mut plane;
            b.length = 1;
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
        let mut plane: v4l2_plane = zeroed();
        plane.bytesused = self.layout.size.min(self.inputs[index].map.len as u32);
        plane.length = self.inputs[index].map.len as u32;
        let mut b: v4l2_buffer = zeroed();
        b.index = index as u32;
        b.type_ = BUF_TYPE_VIDEO_OUTPUT_MPLANE;
        b.memory = MEMORY_MMAP;
        b.planes = &mut plane;
        b.length = 1;
        b.timestamp.tv_sec = (pts_us / 1_000_000) as libc::time_t;
        b.timestamp.tv_usec = (pts_us % 1_000_000) as libc::suseconds_t;
        ioctl(self.fd.as_raw_fd(), VIDIOC_QBUF, &mut b).context("QBUF output")?;
        self.inputs[index].queued = true;
        // Entries leave when their timestamp comes back on an output buffer; a driver
        // that does not copy timestamps must not make this grow for ever.
        if self.in_flight.len() >= 4 * self.inputs.len().max(1) {
            self.in_flight.pop_front();
        }
        self.in_flight.push_back((pts_us, Instant::now()));
        Ok(())
    }

    /// Finishes every frame in flight (V4L2_ENC_CMD_STOP) and returns them.
    /// The encoder then produces nothing until [`Encoder::restart`].
    pub fn drain(&mut self, timeout: Duration) -> Result<Vec<Packet>> {
        let mut cmd = v4l2_encoder_cmd {
            cmd: ENC_CMD_STOP,
            ..Default::default()
        };
        ioctl(self.fd.as_raw_fd(), VIDIOC_ENCODER_CMD, &mut cmd).context("ENCODER_CMD STOP")?;
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        while !self.drained {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!(
                    "encoder did not finish draining ({} frames still in flight)",
                    self.in_flight.len()
                );
            }
            if let Some(p) = self.poll_packet(left)? {
                out.push(p);
            }
        }
        self.reclaim_inputs();
        self.in_flight.clear();
        Ok(out)
    }

    /// Resumes encoding after [`Encoder::drain`]: V4L2_ENC_CMD_START, and
    /// also a STREAMOFF/STREAMON cycle of the CAPTURE queue, the other resume
    /// path the spec allows. START alone is not enough everywhere: vicodec
    /// (kernel 7.3) clears its stopped state before it checks it, so the
    /// LAST-buffer flag on CAPTURE is never reset and every DQBUF says EPIPE.
    pub fn restart(&mut self) -> Result<()> {
        let f = self.fd.as_raw_fd();
        let mut cmd = v4l2_encoder_cmd {
            cmd: ENC_CMD_START,
            ..Default::default()
        };
        ioctl(f, VIDIOC_ENCODER_CMD, &mut cmd).context("ENCODER_CMD START")?;
        let mut t = BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        ioctl(f, VIDIOC_STREAMOFF, &mut t).context("STREAMOFF capture")?;
        for i in 0..self.outputs.len() {
            self.queue_output(i as u32)?;
        }
        let mut t = BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        ioctl(f, VIDIOC_STREAMON, &mut t).context("STREAMON capture")?;
        self.drained = false;
        Ok(())
    }

    pub fn is_drained(&self) -> bool {
        self.drained
    }

    fn queue_output(&mut self, index: u32) -> Result<()> {
        let mut plane: v4l2_plane = zeroed();
        plane.length = self.outputs[index as usize].len as u32;
        let mut b: v4l2_buffer = zeroed();
        b.index = index;
        b.type_ = BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        b.memory = MEMORY_MMAP;
        b.planes = &mut plane;
        b.length = 1;
        ioctl(self.fd.as_raw_fd(), VIDIOC_QBUF, &mut b).context("QBUF capture")
    }

    /// Waits up to `timeout` for an encoded frame. `None` on timeout.
    pub fn poll_packet(&mut self, timeout: Duration) -> Result<Option<Packet>> {
        if self.drained {
            return Ok(None);
        }
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
            if self.drained || Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }

    fn try_dequeue(&mut self) -> Result<Option<Packet>> {
        let f = self.fd.as_raw_fd();
        loop {
            let mut plane: v4l2_plane = zeroed();
            let mut b: v4l2_buffer = zeroed();
            b.type_ = BUF_TYPE_VIDEO_CAPTURE_MPLANE;
            b.memory = MEMORY_MMAP;
            b.planes = &mut plane;
            b.length = 1;
            if let Err(e) = ioctl(f, VIDIOC_DQBUF, &mut b) {
                match e.raw_os_error() {
                    Some(libc::EAGAIN) => return Ok(None),
                    // After the LAST buffer, until the encoder is restarted.
                    Some(libc::EPIPE) => {
                        self.drained = true;
                        return Ok(None);
                    }
                    _ => return Err(e).context("DQBUF capture"),
                }
            }
            let index = b.index;
            let pts_us = b.timestamp.tv_sec as u64 * 1_000_000 + b.timestamp.tv_usec as u64;
            let start = plane.data_offset as usize;
            let end = (plane.bytesused as usize).min(self.outputs[index as usize].len);
            let data = if end > start {
                let m = &self.outputs[index as usize];
                // SAFETY: inside the mapping; the buffer is dequeued.
                Bytes::copy_from_slice(unsafe {
                    std::slice::from_raw_parts(m.ptr.add(start), end - start)
                })
            } else {
                Bytes::new()
            };
            let flags = b.flags;
            self.queue_output(index)?;
            // Opportunistically free inputs too.
            self.reclaim_inputs();
            if flags & BUF_FLAG_ERROR != 0 {
                warn!("encoder flagged an output buffer as corrupt (pts {pts_us})");
            }
            if flags & BUF_FLAG_LAST != 0 {
                self.drained = true;
            }
            if data.is_empty() {
                if self.drained {
                    return Ok(None);
                }
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
            let encode_time = queued_at.map_or(Duration::ZERO, |t| t.elapsed());
            return Ok(Some(Packet {
                data,
                keyframe: flags & BUF_FLAG_KEYFRAME != 0,
                pts_us,
                encode_time,
            }));
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if self.streaming {
            let f = self.fd.as_raw_fd();
            let mut t = BUF_TYPE_VIDEO_OUTPUT_MPLANE;
            let _ = ioctl(f, VIDIOC_STREAMOFF, &mut t);
            let mut t = BUF_TYPE_VIDEO_CAPTURE_MPLANE;
            let _ = ioctl(f, VIDIOC_STREAMOFF, &mut t);
        }
        // Mappings and fds drop after this (munmap, close); exported dmabufs
        // keep the memory alive until every importer lets go.
    }
}
