//! Capture of the default sink's monitor through a dlopen'd `libpulse.so.0`
//! (PulseAudio or pipewire-pulse), on a dedicated thread.
//!
//! Uses the asynchronous API with a private `pa_mainloop` driven by
//! `prepare(timeout)/poll/dispatch`, so the thread never blocks for long: it sleeps in
//! `poll` until the server sends data (once per graph quantum) or 50 ms pass, and checks
//! the stop flag in between. No callbacks: states are polled after each dispatch and
//! data is drained with `pa_stream_peek`/`pa_stream_drop`.

use anyhow::{Context, Result, bail};
use libloading::Library;
use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const LIB: &str = "libpulse.so.0";
/// Follows whatever the default sink is when the stream connects. Recording from a
/// monitor does not touch the default sink or any other stream's routing.
const DEVICE: &CStr = c"@DEFAULT_MONITOR@";

const PA_SAMPLE_S16LE: c_int = 3;
const PA_CONTEXT_NOAUTOSPAWN: c_int = 1;
const PA_STREAM_ADJUST_LATENCY: c_int = 0x2000;
const PA_CONTEXT_READY: c_int = 4;
const PA_CONTEXT_FAILED: c_int = 5;
const PA_CONTEXT_TERMINATED: c_int = 6;
const PA_STREAM_READY: c_int = 2;
const PA_STREAM_FAILED: c_int = 3;
const PA_STREAM_TERMINATED: c_int = 4;
const POLL_TIMEOUT_US: c_int = 50_000;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type P = *mut c_void;

/// The libpulse entry points we use, resolved once.
pub struct Pulse {
    _lib: Library,
    mainloop_new: unsafe extern "C" fn() -> P,
    mainloop_get_api: unsafe extern "C" fn(P) -> P,
    mainloop_prepare: unsafe extern "C" fn(P, c_int) -> c_int,
    mainloop_poll: unsafe extern "C" fn(P) -> c_int,
    mainloop_dispatch: unsafe extern "C" fn(P) -> c_int,
    mainloop_free: unsafe extern "C" fn(P),
    context_new: unsafe extern "C" fn(P, *const c_char) -> P,
    context_connect: unsafe extern "C" fn(P, *const c_char, c_int, *const c_void) -> c_int,
    context_get_state: unsafe extern "C" fn(P) -> c_int,
    context_errno: unsafe extern "C" fn(P) -> c_int,
    context_disconnect: unsafe extern "C" fn(P),
    context_unref: unsafe extern "C" fn(P),
    stream_new: unsafe extern "C" fn(P, *const c_char, *const SampleSpec, *const c_void) -> P,
    stream_connect_record:
        unsafe extern "C" fn(P, *const c_char, *const BufferAttr, c_int) -> c_int,
    stream_get_state: unsafe extern "C" fn(P) -> c_int,
    stream_peek: unsafe extern "C" fn(P, *mut *const c_void, *mut usize) -> c_int,
    stream_drop: unsafe extern "C" fn(P) -> c_int,
    stream_disconnect: unsafe extern "C" fn(P) -> c_int,
    stream_unref: unsafe extern "C" fn(P),
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
}

impl Pulse {
    pub fn load() -> Result<Self> {
        // SAFETY: libpulse's initialisers have no preconditions.
        let lib = unsafe { Library::new(LIB) }.with_context(|| format!("dlopen {LIB}"))?;
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: the field types match the libpulse prototypes of these symbols.
                *unsafe { lib.get(concat!($name, "\0").as_bytes()) }
                    .with_context(|| format!("dlsym {}", $name))?
            };
        }
        Ok(Self {
            mainloop_new: sym!("pa_mainloop_new"),
            mainloop_get_api: sym!("pa_mainloop_get_api"),
            mainloop_prepare: sym!("pa_mainloop_prepare"),
            mainloop_poll: sym!("pa_mainloop_poll"),
            mainloop_dispatch: sym!("pa_mainloop_dispatch"),
            mainloop_free: sym!("pa_mainloop_free"),
            context_new: sym!("pa_context_new"),
            context_connect: sym!("pa_context_connect"),
            context_get_state: sym!("pa_context_get_state"),
            context_errno: sym!("pa_context_errno"),
            context_disconnect: sym!("pa_context_disconnect"),
            context_unref: sym!("pa_context_unref"),
            stream_new: sym!("pa_stream_new"),
            stream_connect_record: sym!("pa_stream_connect_record"),
            stream_get_state: sym!("pa_stream_get_state"),
            stream_peek: sym!("pa_stream_peek"),
            stream_drop: sym!("pa_stream_drop"),
            stream_disconnect: sym!("pa_stream_disconnect"),
            stream_unref: sym!("pa_stream_unref"),
            strerror: sym!("pa_strerror"),
            _lib: lib,
        })
    }

    fn error(&self, ctx: P) -> String {
        // SAFETY: ctx is a live context; pa_strerror returns a static string.
        unsafe { CStr::from_ptr((self.strerror)((self.context_errno)(ctx))) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Captured interleaved S16 samples, bounded: the oldest are dropped when the consumer
/// falls behind, so latency never grows past `cap`.
pub struct Ring {
    pub buf: VecDeque<i16>,
}

pub struct Shared {
    pub ring: Mutex<Ring>,
    pub cond: Condvar,
    pub stop: AtomicBool,
    /// Ring capacity in samples.
    pub cap: usize,
    /// Wake the consumer once this many samples are buffered (one packet).
    pub notify_at: usize,
}

impl Shared {
    pub fn new(cap: usize, notify_at: usize) -> Self {
        Self {
            ring: Mutex::new(Ring {
                buf: VecDeque::with_capacity(cap + notify_at),
            }),
            cond: Condvar::new(),
            stop: AtomicBool::new(false),
            cap,
            notify_at,
        }
    }

    fn push(&self, bytes: Option<&[u8]>, len: usize) {
        let mut r = self.ring.lock().unwrap();
        match bytes {
            Some(b) => r
                .buf
                .extend(b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]))),
            // A hole in the stream: fill with silence.
            None => r.buf.extend(std::iter::repeat_n(0, len / 2)),
        }
        if r.buf.len() > self.cap {
            let excess = r.buf.len() - self.cap;
            r.buf.drain(..excess);
        }
        if r.buf.len() >= self.notify_at {
            self.cond.notify_one();
        }
    }
}

/// Capture thread body: (re)connect until stopped, backing off 1 s after a failure.
pub fn run(pa: Arc<Pulse>, shared: Arc<Shared>, rate: u32, channels: u8, frag_bytes: u32) {
    let mut failures = 0u32;
    while !shared.stop.load(Ordering::SeqCst) {
        match capture(&pa, &shared, rate, channels, frag_bytes) {
            Ok(()) => break,
            Err(e) => {
                failures += 1;
                if failures == 1 || failures.is_power_of_two() {
                    tracing::warn!(error = %format!("{e:#}"), failures, "audio monitor capture failed; retrying (sending silence)");
                }
                for _ in 0..10 {
                    if shared.stop.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

/// Owns the libpulse objects of one connection and frees them in order.
struct Conn<'a> {
    pa: &'a Pulse,
    ml: P,
    ctx: P,
    stream: P,
}

impl Drop for Conn<'_> {
    fn drop(&mut self) {
        let pa = self.pa;
        // SAFETY: each pointer is either null or a live object created on this thread.
        unsafe {
            if !self.stream.is_null() {
                (pa.stream_disconnect)(self.stream);
                (pa.stream_unref)(self.stream);
            }
            if !self.ctx.is_null() {
                (pa.context_disconnect)(self.ctx);
                (pa.context_unref)(self.ctx);
            }
            if !self.ml.is_null() {
                (pa.mainloop_free)(self.ml);
            }
        }
    }
}

fn capture(pa: &Pulse, shared: &Shared, rate: u32, channels: u8, frag_bytes: u32) -> Result<()> {
    let mut c = Conn {
        pa,
        ml: ptr::null_mut(),
        ctx: ptr::null_mut(),
        stream: ptr::null_mut(),
    };
    // SAFETY: straightforward libpulse object lifecycle on one thread; `Conn` frees.
    unsafe {
        c.ml = (pa.mainloop_new)();
        anyhow::ensure!(!c.ml.is_null(), "pa_mainloop_new failed");
        c.ctx = (pa.context_new)((pa.mainloop_get_api)(c.ml), c"framelight".as_ptr());
        anyhow::ensure!(!c.ctx.is_null(), "pa_context_new failed");
        if (pa.context_connect)(c.ctx, ptr::null(), PA_CONTEXT_NOAUTOSPAWN, ptr::null()) < 0 {
            bail!("connect to the sound server: {}", pa.error(c.ctx));
        }
        let mut logged_ready = false;
        loop {
            if shared.stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            if (pa.mainloop_prepare)(c.ml, POLL_TIMEOUT_US) < 0
                || (pa.mainloop_poll)(c.ml) < 0
                || (pa.mainloop_dispatch)(c.ml) < 0
            {
                bail!("pa_mainloop iteration failed");
            }
            match (pa.context_get_state)(c.ctx) {
                PA_CONTEXT_READY => {}
                PA_CONTEXT_FAILED | PA_CONTEXT_TERMINATED => {
                    bail!("sound server connection: {}", pa.error(c.ctx))
                }
                _ => continue,
            }
            if c.stream.is_null() {
                let ss = SampleSpec {
                    format: PA_SAMPLE_S16LE,
                    rate,
                    channels,
                };
                c.stream = (pa.stream_new)(c.ctx, c"framelight monitor".as_ptr(), &ss, ptr::null());
                anyhow::ensure!(!c.stream.is_null(), "pa_stream_new: {}", pa.error(c.ctx));
                // Small fragments (one packet). The thread drains every fragment as it
                // arrives, so the server-side buffer stays empty; latency is bounded by
                // our own ring.
                let attr = BufferAttr {
                    maxlength: u32::MAX,
                    tlength: u32::MAX,
                    prebuf: u32::MAX,
                    minreq: u32::MAX,
                    fragsize: frag_bytes,
                };
                if (pa.stream_connect_record)(
                    c.stream,
                    DEVICE.as_ptr(),
                    &attr,
                    PA_STREAM_ADJUST_LATENCY,
                ) < 0
                {
                    bail!("record {}: {}", DEVICE.to_string_lossy(), pa.error(c.ctx));
                }
                continue;
            }
            match (pa.stream_get_state)(c.stream) {
                PA_STREAM_READY => {}
                PA_STREAM_FAILED | PA_STREAM_TERMINATED => {
                    bail!(
                        "record stream on {}: {}",
                        DEVICE.to_string_lossy(),
                        pa.error(c.ctx)
                    )
                }
                _ => continue,
            }
            if !logged_ready {
                logged_ready = true;
                tracing::info!(device = %DEVICE.to_string_lossy(), frag_bytes, "audio: recording the default sink monitor");
            }
            loop {
                let mut data: *const c_void = ptr::null();
                let mut n = 0usize;
                if (pa.stream_peek)(c.stream, &mut data, &mut n) < 0 {
                    bail!("pa_stream_peek: {}", pa.error(c.ctx));
                }
                if n == 0 {
                    break;
                }
                let bytes =
                    (!data.is_null()).then(|| std::slice::from_raw_parts(data as *const u8, n));
                shared.push(bytes, n);
                (pa.stream_drop)(c.stream);
            }
        }
    }
}
