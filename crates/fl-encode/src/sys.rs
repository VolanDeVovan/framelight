//! The slice of the V4L2 UAPI the encoder needs, written out by hand for
//! 64-bit Linux (aarch64 and x86_64 share these layouts). Sizes are checked at
//! compile time against the values `<linux/videodev2.h>` gives.

#![allow(non_camel_case_types)]

use std::io;
use std::os::fd::RawFd;

pub const BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
pub const BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
pub const BUF_TYPE_VIDEO_OUTPUT_MPLANE: u32 = 10;
pub const MEMORY_MMAP: u32 = 1;
pub const SEL_TGT_CROP: u32 = 0;

pub const BUF_FLAG_KEYFRAME: u32 = 0x0000_0008;
pub const BUF_FLAG_ERROR: u32 = 0x0000_0040;

pub const PIX_FMT_NV12: u32 = fourcc(b"NV12");
pub const PIX_FMT_H264: u32 = fourcc(b"H264");

pub const COLORSPACE_REC709: u32 = 3;
pub const XFER_FUNC_709: u8 = 1;
pub const YCBCR_ENC_709: u8 = 2;
pub const QUANTIZATION_LIM_RANGE: u8 = 2;

pub const CID_MPEG_VIDEO_B_FRAMES: u32 = 0x0099_09ca;
pub const CID_MPEG_VIDEO_GOP_SIZE: u32 = 0x0099_09cb;
pub const CID_MPEG_VIDEO_BITRATE_MODE: u32 = 0x0099_09ce;
pub const CID_MPEG_VIDEO_BITRATE: u32 = 0x0099_09cf;
pub const CID_MPEG_VIDEO_BITRATE_PEAK: u32 = 0x0099_09d0;
pub const CID_MPEG_VIDEO_FRAME_RC_ENABLE: u32 = 0x0099_09d7;
pub const CID_MPEG_VIDEO_HEADER_MODE: u32 = 0x0099_09d8;
pub const CID_MPEG_VIDEO_FORCE_KEY_FRAME: u32 = 0x0099_09e5;
pub const CID_MPEG_VIDEO_H264_LEVEL: u32 = 0x0099_0a67;
pub const CID_MPEG_VIDEO_H264_PROFILE: u32 = 0x0099_0a6b;
pub const CID_MPEG_VIDEO_PREPEND_SPSPPS_TO_IDR: u32 = 0x0099_0b84;

pub const BITRATE_MODE_VBR: i32 = 0;
pub const HEADER_MODE_JOINED_WITH_1ST_FRAME: i32 = 1;
pub const H264_PROFILE_CONSTRAINED_BASELINE: i32 = 1;
pub const H264_PROFILE_MAIN: i32 = 2;
pub const H264_PROFILE_HIGH: i32 = 4;

pub const VIDIOC_S_FMT: u64 = 0xc0d0_5605;
pub const VIDIOC_G_FMT: u64 = 0xc0d0_5604;
pub const VIDIOC_REQBUFS: u64 = 0xc014_5608;
pub const VIDIOC_QUERYBUF: u64 = 0xc058_5609;
pub const VIDIOC_QBUF: u64 = 0xc058_560f;
pub const VIDIOC_DQBUF: u64 = 0xc058_5611;
pub const VIDIOC_STREAMON: u64 = 0x4004_5612;
pub const VIDIOC_STREAMOFF: u64 = 0x4004_5613;
pub const VIDIOC_S_CTRL: u64 = 0xc008_561c;
pub const VIDIOC_S_SELECTION: u64 = 0xc040_565f;
pub const VIDIOC_S_PARM: u64 = 0xc0cc_5616;

pub const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}

pub fn fourcc_str(v: u32) -> String {
    v.to_le_bytes().iter().map(|&b| b as char).collect()
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct v4l2_plane_pix_format {
    pub sizeimage: u32,
    pub bytesperline: u32,
    pub reserved: [u16; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct v4l2_pix_format_mplane {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub colorspace: u32,
    pub plane_fmt: [v4l2_plane_pix_format; 8],
    pub num_planes: u8,
    pub flags: u8,
    pub ycbcr_enc: u8,
    pub quantization: u8,
    pub xfer_func: u8,
    pub reserved: [u8; 7],
}

#[repr(C)]
pub union v4l2_format_union {
    pub pix_mp: v4l2_pix_format_mplane,
    pub raw: [u8; 200],
    // The kernel union holds pointers (v4l2_window), hence 8-byte alignment.
    _align: [u64; 25],
}

#[repr(C)]
pub struct v4l2_format {
    pub type_: u32,
    pub fmt: v4l2_format_union,
}

#[repr(C)]
#[derive(Default)]
pub struct v4l2_requestbuffers {
    pub count: u32,
    pub type_: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub flags: u8,
    pub reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union v4l2_plane_m {
    pub mem_offset: u32,
    pub userptr: libc::c_ulong,
    pub fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_plane {
    pub bytesused: u32,
    pub length: u32,
    pub m: v4l2_plane_m,
    pub data_offset: u32,
    pub reserved: [u32; 11],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_timecode {
    pub type_: u32,
    pub flags: u32,
    pub frames: u8,
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    pub userbits: [u8; 4],
}

#[repr(C)]
pub struct v4l2_buffer {
    pub index: u32,
    pub type_: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub timestamp: libc::timeval,
    pub timecode: v4l2_timecode,
    pub sequence: u32,
    pub memory: u32,
    pub planes: *mut v4l2_plane,
    pub length: u32,
    pub reserved2: u32,
    pub request_fd: i32,
}

#[repr(C)]
#[derive(Default)]
pub struct v4l2_control {
    pub id: u32,
    pub value: i32,
}

#[repr(C)]
#[derive(Default)]
pub struct v4l2_rect {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct v4l2_selection {
    pub type_: u32,
    pub target: u32,
    pub flags: u32,
    pub r: v4l2_rect,
    pub reserved: [u32; 9],
}

#[repr(C)]
pub struct v4l2_streamparm {
    pub type_: u32,
    /// v4l2_outputparm: capability, outputmode, timeperframe {num, den}, ...
    pub parm: [u32; 50],
}

const _: () = {
    assert!(size_of::<v4l2_format>() == 208);
    assert!(size_of::<v4l2_pix_format_mplane>() == 192);
    assert!(size_of::<v4l2_buffer>() == 88);
    assert!(size_of::<v4l2_plane>() == 64);
    assert!(size_of::<v4l2_requestbuffers>() == 20);
    assert!(size_of::<v4l2_selection>() == 64);
    assert!(size_of::<v4l2_streamparm>() == 204);
    assert!(std::mem::offset_of!(v4l2_buffer, planes) == 64);
};

pub fn zeroed<T>() -> T {
    // SAFETY: only used for the plain-old-data UAPI structs above.
    unsafe { std::mem::zeroed() }
}

/// ioctl with EINTR retry.
pub fn ioctl<T>(fd: RawFd, req: u64, arg: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: `arg` is the struct the request expects (caller's contract).
        let r = unsafe { libc::ioctl(fd, req as _, arg as *mut T) };
        if r >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
}
