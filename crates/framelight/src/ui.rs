//! The switch: a window with one button, to be opened on the headset.
//!
//! SteamOS shows a non-Steam "game" that opens an X11 window as a flat screen inside
//! VR, with the controller as the pointer, so that is all this is: plain X11 over the
//! wire protocol (no toolkit, no library on the device), a CPU-drawn picture pushed
//! with PutImage when something changes, and a blocking wait in between. While
//! streaming is disabled the process holds one socket and sleeps in `poll`.
//! SIGUSR1 presses the button, for a shell without a pointer.

use crate::Streamer;
use anyhow::{Context, Result, bail};
use fl_webrtc::{Server, SourceState};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, ImageFormat, PropMode,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Input is ignored for this long after the window opens: the click or key press that
/// launched the app must not also press its button.
const INPUT_GRACE: std::time::Duration = std::time::Duration::from_millis(800);
/// How often the viewer count on screen is refreshed while streaming is enabled.
const REFRESH_MS: i32 = 1000;

const BACKGROUND: [u8; 3] = [0x10, 0x11, 0x14];
const INK: [u8; 3] = [0xf2, 0xf0, 0xea];
const DIM: [u8; 3] = [0x9a, 0x97, 0x8e];
const ACCENT: [u8; 3] = [0xff, 0xb4, 0x54];
const ON: [u8; 3] = [0x7b, 0xd8, 0x8f];
const WARN: [u8; 3] = [0xff, 0x7a, 0x6b];
const PANEL: [u8; 3] = [0x24, 0x26, 0x2c];

/// A 32-bit BGRX framebuffer, the layout X servers use for depth 24.
struct Canvas {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
    fn contains(&self, x: i32, y: i32) -> bool {
        let (x, y) = (x as i64, y as i64);
        x >= self.x as i64
            && x < (self.x + self.w) as i64
            && y >= self.y as i64
            && y < (self.y + self.h) as i64
    }
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            px: vec![0; w * h * 4],
        }
    }

    fn fill(&mut self, r: Rect, c: [u8; 3]) {
        for y in r.y.min(self.h)..(r.y + r.h).min(self.h) {
            let row = &mut self.px[y * self.w * 4..(y + 1) * self.w * 4];
            for x in r.x.min(self.w)..(r.x + r.w).min(self.w) {
                row[x * 4..x * 4 + 3].copy_from_slice(&[c[2], c[1], c[0]]);
            }
        }
    }

    /// 8x8 bitmap glyphs, each pixel `scale` wide. Characters outside ASCII are skipped.
    fn text(&mut self, x: usize, y: usize, scale: usize, c: [u8; 3], s: &str) {
        for (i, ch) in s.chars().filter(char::is_ascii).enumerate() {
            let glyph = font8x8::legacy::BASIC_LEGACY[ch as usize];
            for (gy, bits) in glyph.iter().enumerate() {
                for gx in 0..8 {
                    if bits >> gx & 1 == 1 {
                        self.fill(
                            Rect {
                                x: x + (i * 8 + gx) * scale,
                                y: y + gy * scale,
                                w: scale,
                                h: scale,
                            },
                            c,
                        );
                    }
                }
            }
        }
    }

    fn centered(&mut self, y: usize, scale: usize, c: [u8; 3], s: &str) {
        let w = text_width(s, scale);
        self.text(self.w.saturating_sub(w) / 2, y, scale, c, s);
    }
}

fn text_width(s: &str, scale: usize) -> usize {
    s.chars().filter(char::is_ascii).count() * 8 * scale
}

/// The largest scale (at most `want`) at which `s` fits into `width`.
fn fit(s: &str, want: usize, width: usize) -> usize {
    (1..=want)
        .rev()
        .find(|&k| text_width(s, k) <= width)
        .unwrap_or(1)
}

/// Everything the picture depends on; it is redrawn only when this changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct View {
    enabled: bool,
    urls: Vec<String>,
    viewers: usize,
    source: SourceState,
    error: Option<String>,
}

/// Draws the whole window and returns where the button is.
fn render(c: &mut Canvas, v: &View) -> Rect {
    let (w, h) = (c.w, c.h);
    c.fill(Rect { x: 0, y: 0, w, h }, BACKGROUND);
    // One unit is 1/90 of the height: 8 px at 720p, 12 px at 1080p.
    let u = (h / 90).max(2);
    let margin = 6 * u;
    let usable = w.saturating_sub(2 * margin);

    c.text(
        margin,
        5 * u,
        fit("framelight", u, usable),
        INK,
        "framelight",
    );
    let tagline = "what the headset shows, in any browser on this network";
    c.text(
        margin,
        15 * u,
        fit(tagline, (u / 3).max(1), usable),
        DIM,
        tagline,
    );

    let (state, state_color) = match (v.enabled, v.viewers) {
        (false, _) => ("Streaming is off".to_string(), DIM),
        (true, 0) => ("Streaming is on - nobody is watching".to_string(), ON),
        (true, 1) => ("Streaming is on - 1 viewer".to_string(), ON),
        (true, n) => (format!("Streaming is on - {n} viewers"), ON),
    };
    c.centered(
        25 * u,
        fit(&state, (u / 2).max(1), usable),
        state_color,
        &state,
    );

    let label = if v.enabled { "Disable" } else { "Enable" };
    let button = Rect {
        x: w / 2 - (w / 5).max(12 * u),
        y: 33 * u,
        w: 2 * (w / 5).max(12 * u),
        h: 14 * u,
    };
    c.fill(button, if v.enabled { PANEL } else { ACCENT });
    if v.enabled {
        // An outline: the filled accent is reserved for the action that starts something.
        let t = (u / 3).max(1);
        c.fill(Rect { h: t, ..button }, ACCENT);
        c.fill(
            Rect {
                y: button.y + button.h - t,
                h: t,
                ..button
            },
            ACCENT,
        );
        c.fill(Rect { w: t, ..button }, ACCENT);
        c.fill(
            Rect {
                x: button.x + button.w - t,
                w: t,
                ..button
            },
            ACCENT,
        );
    }
    let scale = fit(label, (u * 3 / 4).max(1), button.w);
    c.centered(
        button.y + (button.h - 8 * scale) / 2,
        scale,
        if v.enabled { ACCENT } else { BACKGROUND },
        label,
    );

    let mut y = 53 * u;
    if let Some(e) = &v.error {
        for line in wrap(e, usable / (8 * (u / 3).max(1))) {
            c.centered(y, (u / 3).max(1), WARN, &line);
            y += 4 * u;
        }
    } else if v.enabled {
        let hint = "Open this address in a browser:";
        c.centered(y, fit(hint, (u / 3).max(1), usable), DIM, hint);
        y += 6 * u;
        for (i, url) in v.urls.iter().enumerate() {
            if i > 0 {
                c.centered(y, (u / 3).max(1), DIM, "or");
                y += 5 * u;
            }
            let scale = fit(url, (u * 2 / 3).max(1), usable);
            c.centered(y, scale, INK, url);
            y += 8 * scale + 3 * u;
        }
        if let SourceState::Failed(e) = &v.source {
            for line in wrap(e, usable / (8 * (u / 3).max(1))) {
                c.centered(y, (u / 3).max(1), WARN, &line);
                y += 4 * u;
            }
        }
    } else {
        let hint = "Nothing runs until you enable it.";
        c.centered(y, fit(hint, (u / 3).max(1), usable), DIM, hint);
    }
    button
}

/// Greedy word wrap to `columns` characters, at most three lines.
fn wrap(s: &str, columns: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in s.split_whitespace() {
        match lines.last_mut() {
            Some(l) if l.len() + 1 + word.len() <= columns => {
                l.push(' ');
                l.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    lines.truncate(3);
    lines
}

struct Window {
    conn: RustConnection,
    id: u32,
    gc: u32,
    depth: u8,
    wm_protocols: u32,
    wm_delete: u32,
    /// Keycodes that act like a click on the button: Return, keypad Enter, space.
    activate_keys: Vec<u8>,
    canvas: Canvas,
}

impl Window {
    fn open() -> Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(None).context("cannot open the X display (is DISPLAY set?)")?;
        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let depth = screen.root_depth;
        let bpp = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .map(|f| f.bits_per_pixel);
        if bpp != Some(32) || !matches!(depth, 24 | 32) {
            bail!("unsupported X visual: depth {depth}, {bpp:?} bits per pixel");
        }
        let (w, h) = (screen.width_in_pixels, screen.height_in_pixels);
        let id = conn.generate_id()?;
        let gc = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            id,
            screen.root,
            0,
            0,
            w,
            h,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(screen.black_pixel)
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::BUTTON_PRESS
                        | EventMask::KEY_PRESS
                        | EventMask::STRUCTURE_NOTIFY,
                ),
        )?;
        conn.create_gc(gc, id, &CreateGCAux::new())?;
        let atom = |name: &str| -> Result<u32> {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        };
        let (wm_protocols, wm_delete) = (atom("WM_PROTOCOLS")?, atom("WM_DELETE_WINDOW")?);
        let utf8 = atom("UTF8_STRING")?;
        conn.change_property8(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"framelight",
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            id,
            atom("_NET_WM_NAME")?,
            utf8,
            b"framelight",
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"framelight\0Framelight\0",
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            id,
            wm_protocols,
            AtomEnum::ATOM,
            &[wm_delete],
        )?;

        let (first, last) = (setup.min_keycode, setup.max_keycode);
        let map = conn
            .get_keyboard_mapping(first, last - first + 1)?
            .reply()?;
        let per = map.keysyms_per_keycode as usize;
        // XK_Return, XK_KP_Enter, XK_space.
        let activate_keys = (0..map.keysyms.len() / per.max(1))
            .filter(|&i| matches!(map.keysyms[i * per], 0xff0d | 0xff8d | 0x20))
            .map(|i| first + i as u8)
            .collect();

        conn.map_window(id)?;
        conn.flush()?;
        Ok(Self {
            conn,
            id,
            gc,
            depth,
            wm_protocols,
            wm_delete,
            activate_keys,
            canvas: Canvas::new(w as usize, h as usize),
        })
    }

    /// Pushes the canvas to the window, in strips that fit a request.
    fn present(&self) -> Result<()> {
        let c = &self.canvas;
        let rows = ((self.conn.maximum_request_bytes() - 64) / (c.w * 4)).clamp(1, c.h);
        for (i, strip) in c.px.chunks(rows * c.w * 4).enumerate() {
            self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                self.id,
                self.gc,
                c.w as u16,
                (strip.len() / (c.w * 4)) as u16,
                0,
                (i * rows) as i16,
                0,
                self.depth,
                strip,
            )?;
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Sleeps until the X server has something to say, a signal arrives, or `timeout_ms`
    /// passes (-1: no limit).
    fn wait(&self, timeout_ms: i32) {
        let mut pfd = libc::pollfd {
            fd: self.conn.stream().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    }
}

pub fn run(streamer: &Streamer, stop: &AtomicBool, press: &AtomicBool) -> Result<()> {
    let mut win = Window::open()?;
    let mut server: Option<Server> = None;
    let mut error: Option<String> = None;
    let mut shown: Option<View> = None;
    let mut button = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };
    let mut dirty = true;
    let opened = std::time::Instant::now();
    tracing::info!("window open; streaming is off until it is enabled there");

    'outer: while !stop.load(Ordering::SeqCst) {
        let status = server.as_ref().map(Server::status);
        let view = View {
            enabled: server.is_some(),
            urls: if server.is_some() {
                streamer.urls()
            } else {
                Vec::new()
            },
            viewers: status.as_ref().map_or(0, |s| s.viewers),
            source: status.map(|s| s.source).unwrap_or_default(),
            error: error.clone(),
        };
        if dirty || shown.as_ref() != Some(&view) {
            button = render(&mut win.canvas, &view);
            win.present()?;
            shown = Some(view);
            dirty = false;
        }

        let mut toggle = press.swap(false, Ordering::SeqCst);
        if toggle {
            tracing::info!("button pressed by SIGUSR1");
        }
        let mut idle = true;
        while let Some(event) = win.conn.poll_for_event().context("X connection lost")? {
            idle = false;
            match event {
                Event::Expose(e) if e.count == 0 => dirty = true,
                Event::ConfigureNotify(e) => {
                    let (w, h) = (e.width as usize, e.height as usize);
                    if (w, h) != (win.canvas.w, win.canvas.h) && w > 0 && h > 0 {
                        win.canvas = Canvas::new(w, h);
                        dirty = true;
                    }
                }
                Event::ButtonPress(e)
                    if e.detail == 1 && button.contains(e.event_x as i32, e.event_y as i32) =>
                {
                    tracing::info!(x = e.event_x, y = e.event_y, "button clicked");
                    toggle = true
                }
                Event::KeyPress(e) if win.activate_keys.contains(&e.detail) => {
                    tracing::info!(keycode = e.detail, "button pressed from the keyboard");
                    toggle = true
                }
                Event::ClientMessage(e)
                    if e.type_ == win.wm_protocols && e.data.as_data32()[0] == win.wm_delete =>
                {
                    break 'outer;
                }
                Event::DestroyNotify(_) => break 'outer,
                _ => {}
            }
        }
        if toggle && opened.elapsed() < INPUT_GRACE {
            continue;
        }
        if toggle {
            error = None;
            match server.take() {
                Some(s) => {
                    tracing::info!("streaming disabled");
                    s.shutdown();
                }
                None => match streamer.start() {
                    Ok(s) => {
                        tracing::info!("streaming enabled: {}", streamer.urls().join(" or "));
                        server = Some(s);
                    }
                    Err(e) => {
                        tracing::warn!("cannot enable streaming: {e:#}");
                        error = Some(format!("Cannot start: {e:#}"));
                    }
                },
            }
            continue;
        }
        if idle && !dirty {
            win.wait(if server.is_some() { REFRESH_MS } else { -1 });
        }
    }
    if let Some(s) = server {
        s.shutdown();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(c: &Canvas, r: Rect, color: [u8; 3]) -> usize {
        let mut n = 0;
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                let p = &c.px[(y * c.w + x) * 4..];
                n += usize::from(p[..3] == [color[2], color[1], color[0]]);
            }
        }
        n
    }

    #[test]
    fn glyphs_are_drawn_scaled_and_clipped() {
        let mut c = Canvas::new(40, 20);
        c.text(0, 0, 2, INK, "I");
        let all = Rect {
            x: 0,
            y: 0,
            w: 40,
            h: 20,
        };
        let n = lit(&c, all, INK);
        assert!(
            n > 0 && n.is_multiple_of(4),
            "every glyph pixel is a 2x2 block: {n}"
        );
        // Drawing past the edges must not panic.
        c.text(30, 15, 3, INK, "framelight");
        c.fill(
            Rect {
                x: 35,
                y: 18,
                w: 100,
                h: 100,
            },
            ACCENT,
        );
        assert_eq!(text_width("frame.local", 3), 11 * 24);
        assert_eq!(fit("0123456789", 5, 250), 3);
    }

    #[test]
    fn layout_fits_the_headset_screens() {
        for (w, h) in [(1920, 1080), (1280, 720), (800, 600)] {
            let mut c = Canvas::new(w, h);
            let off = render(&mut c, &View::default());
            assert!(off.x + off.w <= w && off.y + off.h <= h, "{w}x{h}: {off:?}");
            assert!(
                lit(&c, off, ACCENT) > off.w * off.h / 2,
                "the Enable button is filled"
            );
            let on = View {
                enabled: true,
                urls: vec![
                    "http://frame.local:8090".into(),
                    "http://192.168.0.48:8090".into(),
                ],
                viewers: 2,
                source: SourceState::Failed(
                    "SteamVR is not running (no vrcompositor process)".into(),
                ),
                error: None,
            };
            let button = render(&mut c, &on);
            assert_eq!(button, off, "the button does not move when toggled");
            assert!(
                button.contains((w / 2) as i32, (button.y + 1) as i32) && !button.contains(0, 0)
            );
            // The addresses are below the button and inside the window.
            let below = Rect {
                x: 0,
                y: button.y + button.h,
                w,
                h: h - button.y - button.h,
            };
            assert!(lit(&c, below, INK) > 500, "{w}x{h}: URLs are drawn");
            assert!(
                lit(&c, below, WARN) > 100,
                "{w}x{h}: the source error is drawn"
            );
            assert_eq!(
                lit(
                    &c,
                    Rect {
                        x: 0,
                        y: h - 1,
                        w,
                        h: 1
                    },
                    INK
                ),
                0,
                "nothing runs off the bottom"
            );
        }
    }

    #[test]
    fn wraps_long_errors() {
        let lines = wrap(
            "Cannot start: bind TCP port 8090: Address already in use (os error 98)",
            30,
        );
        assert!(
            lines.len() <= 3 && lines.iter().all(|l| l.len() <= 30),
            "{lines:?}"
        );
        assert_eq!(lines[0], "Cannot start: bind TCP port");
    }
}
