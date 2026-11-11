//! What the switch shows and where: a pure function of the window size, the theme and
//! the [`View`]. No streamer in here, so all of it runs in tests without a display.

use super::theme::Theme;
use crate::Urls;
use eframe::egui::{
    self, Align, Button, Color32, CornerRadius, FontSelection, Key, Layout, Rect, RichText, Stroke,
    TextWrapMode, Ui, UiBuilder, Vec2, WidgetText, text::LayoutJob,
};
use fl_webrtc::SourceState;

/// Everything the picture depends on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub enabled: bool,
    pub urls: Urls,
    pub viewers: usize,
    pub source: SourceState,
    pub error: Option<String>,
}

impl View {
    /// The state in two lines: the short big one and the detail under it.
    fn state(&self) -> (&'static str, String) {
        match (self.enabled, self.viewers) {
            (false, _) => (
                "Streaming is off",
                "nothing runs until you enable it".into(),
            ),
            (true, 0) => ("Streaming is on", "nobody is watching".into()),
            (true, 1) => ("Streaming is on", "1 viewer is watching".into()),
            (true, n) => ("Streaming is on", format!("{n} viewers are watching")),
        }
    }
}

/// Where the blocks are, in pixels. It depends on the window alone, never on the
/// state, so the button stays where it is when it is pressed.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Places {
    /// 1/90 of the height (less in a narrow window): 12 px at 1080p, 8 px at 720p.
    /// Whole pixels, so every edge lands on one.
    unit: f32,
    /// The band across the top that holds title and tagline, and where their text goes.
    band: Rect,
    header: Rect,
    rule: Rect,
    state: Rect,
    button: Rect,
    details: Rect,
}

impl Places {
    const ROWS: f32 = 90.0;
    const COLUMNS: f32 = 120.0;

    fn new(window: Rect) -> Self {
        let unit = (window.height() / Self::ROWS)
            .min(window.width() / Self::COLUMNS)
            .floor()
            .max(2.0);
        // The rows are centred in what the rounding left over.
        let top = window.top()
            + ((window.height() - Self::ROWS * unit) / 2.0)
                .floor()
                .max(0.0);
        let margin = 10.0 * unit;
        let (left, right) = (window.left() + margin, window.right() - margin);
        let rows = |from: f32, to: f32| {
            Rect::from_min_max(
                egui::pos2(left, top + from * unit),
                egui::pos2(right, top + to * unit),
            )
        };
        let button = Vec2::new((56.0 * unit).min(right - left), 13.0 * unit);
        Self {
            unit,
            band: Rect::from_min_max(window.min, egui::pos2(window.right(), top + 22.0 * unit)),
            header: rows(7.0, 22.0),
            rule: rows(22.0, 22.0 + (unit / 4.0).round().max(1.0) / unit),
            state: rows(23.0, 37.0),
            button: Rect::from_center_size(
                egui::pos2(window.center().x.round(), top + 48.5 * unit),
                button,
            ),
            details: rows(59.0, 90.0),
        }
    }
}

/// Text in the theme's sizes, shrunk when it would not fit.
struct Type<'a> {
    theme: &'a Theme,
    unit: f32,
}

impl Type<'_> {
    /// `units` of the layout as a font size the theme's font is sharp at.
    fn size(&self, units: f32) -> f32 {
        self.snap(units * self.unit)
    }

    fn snap(&self, pixels: f32) -> f32 {
        let grid = self.theme.grid;
        ((pixels / grid).floor() * grid).max(grid)
    }

    fn text(&self, text: &str, size: f32, strong: bool, color: Color32) -> RichText {
        let text = RichText::new(text).size(size).color(color);
        match self.theme.strong {
            Some(weight) if strong => text.variation("wght", weight),
            _ => text,
        }
    }

    /// One line, as large as `units` and no wider than the space there is.
    fn line(&self, ui: &mut Ui, text: &str, units: f32, strong: bool, color: Color32) {
        let width = ui.available_width();
        let mut size = self.size(units);
        loop {
            let galley = WidgetText::from(self.text(text, size, strong, color)).into_galley(
                ui,
                Some(TextWrapMode::Extend),
                f32::INFINITY,
                FontSelection::Default,
            );
            let fitting = self.snap(size * width / galley.size().x.max(1.0));
            if galley.size().x <= width || fitting >= size {
                ui.label(galley);
                return;
            }
            size = fitting;
        }
    }

    /// A message of unknown length: wrapped, centred, at most three lines.
    fn message(&self, ui: &mut Ui, text: &str, color: Color32) {
        let font = egui::FontId::proportional(self.size(self.theme.size.small));
        let mut job = LayoutJob::simple(text.to_owned(), font, color, ui.available_width());
        job.wrap.max_rows = 3;
        job.halign = Align::Center;
        ui.label(job);
    }
}

/// A column of lines inside `rect`, `gap` (whole) pixels apart.
fn column(ui: &mut Ui, rect: Rect, layout: Layout, gap: f32, lines: impl FnOnce(&mut Ui)) {
    ui.scope_builder(UiBuilder::new().max_rect(rect).layout(layout), |ui| {
        ui.spacing_mut().item_spacing = Vec2::new(0.0, gap.round());
        lines(ui);
    });
}

/// How many of the other interfaces' addresses are shown.
const MORE_ADDRESSES: usize = 2;

/// Draws the switch into `ui` and says whether its button was pressed: clicked, or
/// Return, keypad Enter or space typed (held keys do not repeat it).
pub fn show(ui: &mut Ui, theme: &Theme, view: &View) -> bool {
    let at = Places::new(ui.max_rect());
    let unit = at.unit;
    let t = Type { theme, unit };
    let size = &theme.size;

    ui.painter().rect_filled(ui.max_rect(), 0, theme.background);
    ui.painter().rect_filled(at.band, 0, theme.header);
    ui.painter().rect_filled(at.rule, 0, theme.rule);

    column(
        ui,
        at.header,
        Layout::top_down(Align::Min),
        3.0 * unit,
        |ui| {
            t.line(ui, "framelight", size.title, true, theme.ink);
            t.line(
                ui,
                "what the headset shows, in any browser on this network",
                size.small,
                false,
                theme.dim,
            );
        },
    );

    let (state, detail) = view.state();
    // Built upwards from just above the button, so the distance to the button does
    // not depend on how tall the font's lines are.
    column(
        ui,
        at.state,
        Layout::bottom_up(Align::Center),
        2.5 * unit,
        |ui| {
            let color = if view.enabled { theme.on } else { theme.dim };
            t.line(ui, &detail, size.small, false, theme.dim);
            t.line(ui, state, size.state, true, color);
        },
    );

    let clicked = ui
        .scope(|ui| {
            // The filled accent is kept for the action that starts something; the
            // way back is an outline.
            let (fill, ink) = if view.enabled {
                (theme.panel, theme.accent)
            } else {
                (theme.accent, theme.on_accent)
            };
            let stroke = Stroke::new((unit / 3.0).round().max(1.0), theme.accent);
            let widgets = &mut ui.visuals_mut().widgets;
            for (visuals, lighter) in [
                (&mut widgets.inactive, 0.0),
                (&mut widgets.hovered, 0.12),
                (&mut widgets.active, 0.24),
            ] {
                visuals.weak_bg_fill = fill.lerp_to_gamma(theme.ink, lighter);
                visuals.bg_stroke = stroke;
                visuals.fg_stroke.color = ink;
                visuals.corner_radius = CornerRadius::same((theme.corner * unit) as u8);
                visuals.expansion = 0.0;
            }
            let label = if view.enabled { "Disable" } else { "Enable" };
            let label = t.text(label, t.size(size.button), true, ink);
            ui.put(at.button, Button::new(label).min_size(at.button.size()))
                .clicked()
        })
        .inner;

    column(
        ui,
        at.details,
        Layout::top_down(Align::Center),
        1.5 * unit,
        |ui| {
            if let Some(error) = &view.error {
                t.message(ui, error, theme.warn);
            } else if view.enabled {
                let hint = if view.urls.main.len() > 1 {
                    "Open one of these in a browser:"
                } else {
                    "Open this address in a browser:"
                };
                t.line(ui, hint, size.small, false, theme.dim);
                for url in &view.urls.main {
                    t.line(ui, url, size.address, false, theme.ink);
                }
                // Room for two; a headset has Wi-Fi and perhaps a VPN, and the log
                // lists every one.
                for url in view.urls.also.iter().take(MORE_ADDRESSES) {
                    t.line(ui, url, size.small, false, theme.dim);
                }
                if let SourceState::Failed(error) = &view.source {
                    t.message(ui, error, theme.warn);
                }
            }
        },
    );

    let typed = ui.input(|input| {
        input.events.iter().any(|event| {
            matches!(
                event,
                egui::Event::Key {
                    key: Key::Enter | Key::Space,
                    pressed: true,
                    repeat: false,
                    ..
                }
            )
        })
    });
    clicked || typed
}

#[cfg(test)]
mod tests {
    use super::super::theme::{PIXEL, STEAM};
    use super::*;
    use eframe::egui::accesskit::Role;
    use egui_kittest::Harness;
    use egui_kittest::kittest::{NodeT as _, Queryable as _};

    const WINDOWS: [Vec2; 3] = [
        Vec2::new(1920.0, 1080.0),
        Vec2::new(1280.0, 720.0),
        Vec2::new(800.0, 600.0),
    ];

    fn off() -> View {
        View::default()
    }

    /// The fullest picture there is: every address there is room for and a source
    /// that failed.
    fn on() -> View {
        View {
            enabled: true,
            urls: Urls {
                main: vec![
                    "http://192.168.0.48:8090".into(),
                    "http://steamos.local:8090".into(),
                ],
                also: vec![
                    "http://100.101.102.103:8090 (tailscale0)".into(),
                    "http://10.42.0.1:8090 (usb0)".into(),
                    "http://172.17.0.1:8090 (docker0)".into(),
                ],
            },
            viewers: 2,
            source: SourceState::Failed(
                "capture: SteamVR is not running (no vrcompositor process), and this message \
                 goes on for long enough to need all the lines an error may take up here"
                    .into(),
            ),
            error: None,
        }
    }

    fn failed() -> View {
        View {
            error: Some(
                "Cannot start: bind TCP port 8090: Address already in use (os error 98)".into(),
            ),
            ..View::default()
        }
    }

    /// The harness puts this much around the UI it is given.
    const HARNESS_MARGIN: f32 = 8.0;

    /// What `show` is given when the harness is asked for `window`.
    fn screen(window: Vec2) -> Rect {
        Rect::from_min_size(egui::Pos2::ZERO + Vec2::splat(HARNESS_MARGIN), window)
    }

    /// The switch in a headless window; the state is whether the button was pressed.
    /// A glyph the font lacks fails the test.
    fn harness(theme: &'static Theme, window: Vec2, view: View) -> Harness<'static, bool> {
        Harness::builder()
            .with_size(window + Vec2::splat(2.0 * HARNESS_MARGIN))
            .with_context_setup(|ctx| theme.install(ctx))
            .build_ui_state(move |ui, pressed| *pressed |= show(ui, theme, &view), false)
    }

    fn button(harness: &Harness<'_, bool>) -> Rect {
        harness.get_by_role(Role::Button).rect()
    }

    /// Every line of text, top to bottom.
    fn lines(harness: &Harness<'_, bool>) -> Vec<(String, Rect)> {
        let mut lines: Vec<_> = harness
            .get_all_by_role(Role::Label)
            .map(|node| {
                (
                    node.accesskit_node().value().unwrap_or_default(),
                    node.rect(),
                )
            })
            .collect();
        lines.sort_by(|a, b| a.1.top().total_cmp(&b.1.top()));
        lines
    }

    #[test]
    fn everything_fits_and_the_button_stays_put() {
        for theme in [&PIXEL, &STEAM] {
            for window in WINDOWS {
                let screen = screen(window);
                let place = button(&harness(theme, window, off()));
                assert!(screen.contains_rect(place), "{window}: {place}");
                assert_eq!(place.center().x, screen.center().x, "{window}");

                for view in [off(), on(), failed()] {
                    let harness = harness(theme, window, view.clone());
                    assert_eq!(button(&harness), place, "{window}: the button moved");
                    let lines = lines(&harness);
                    for pair in lines.windows(2) {
                        let ((above, a), (below, b)) = (&pair[0], &pair[1]);
                        assert!(
                            a.bottom() <= b.top(),
                            "{window}: {above:?} overlaps {below:?}"
                        );
                    }
                    for (text, rect) in &lines {
                        assert!(screen.contains_rect(*rect), "{window}: {text:?} at {rect}");
                        assert!(
                            rect.bottom() <= place.top() || rect.top() >= place.bottom(),
                            "{window}: {text:?} runs into the button"
                        );
                    }
                    // Title, tagline and the two state lines are above the button;
                    // addresses and errors are below it.
                    let below = lines.iter().filter(|(_, r)| r.top() > place.bottom());
                    let expected = match (&view.error, view.enabled) {
                        (Some(_), _) => 1,
                        (None, true) => 6,
                        (None, false) => 0,
                    };
                    assert_eq!(below.count(), expected, "{window}: {view:?}");
                    assert_eq!(lines.len(), 4 + expected, "{window}: {view:?}");
                }
            }
        }
    }

    #[test]
    fn layout_is_on_whole_pixels() {
        let unit = |w: f32, h: f32| {
            Places::new(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(w, h))).unit
        };
        assert_eq!(unit(1920.0, 1080.0), 12.0);
        assert_eq!(unit(1280.0, 720.0), 8.0);
        assert_eq!(unit(800.0, 600.0), 6.0);
        // A narrow window is limited by its width, a tiny one by the smallest unit.
        assert_eq!(unit(600.0, 1080.0), 5.0);
        assert_eq!(unit(100.0, 50.0), 2.0);

        for window in WINDOWS {
            let layout = Places::new(Rect::from_min_size(egui::Pos2::ZERO, window));
            for rect in [
                layout.header,
                layout.rule,
                layout.state,
                layout.button,
                layout.details,
            ] {
                for edge in [rect.left(), rect.right(), rect.top(), rect.bottom()] {
                    assert_eq!(edge.fract(), 0.0, "{window}: {rect}");
                }
            }
            assert!(layout.rule.height() >= 1.0);
        }
    }

    #[test]
    fn pixel_font_sizes_stay_on_its_grid() {
        for unit in [2.0, 5.0, 6.0, 8.0, 12.0, 13.0] {
            let t = Type {
                theme: &PIXEL,
                unit,
            };
            let s = &PIXEL.size;
            for units in [s.title, s.state, s.button, s.address, s.small] {
                let size = t.size(units);
                assert!(
                    size >= 8.0 && size % 8.0 == 0.0,
                    "{units} x {unit} -> {size}"
                );
                assert!(size <= (units * unit).max(8.0));
            }
        }
        assert_eq!(
            Type {
                theme: &STEAM,
                unit: 12.0
            }
            .size(2.6),
            31.0
        );
    }

    #[test]
    fn long_lines_shrink_to_the_window() {
        let view = View {
            urls: Urls {
                main: vec!["http://a-headset-with-a-very-long-hostname-indeed.local:8090".into()],
                also: Vec::new(),
            },
            ..on()
        };
        for theme in [&PIXEL, &STEAM] {
            let window = WINDOWS[2];
            let harness = harness(theme, window, view.clone());
            let (_, rect) = lines(&harness)
                .into_iter()
                .find(|(text, _)| text.contains("very-long"))
                .expect("the address is shown");
            assert!(screen(window).contains_rect(rect), "{rect}");
        }
    }

    #[test]
    fn the_button_is_pressed_by_click_return_and_space() {
        let window = WINDOWS[0];
        let pressed = |act: &dyn Fn(&Harness<'_, bool>)| {
            let mut harness = harness(&PIXEL, window, off());
            act(&harness);
            harness.run();
            *harness.state()
        };
        assert!(!pressed(&|_| {}));
        assert!(pressed(&|h| h.get_by_label("Enable").click()));
        assert!(pressed(&|h| h.key_press(Key::Enter)));
        assert!(pressed(&|h| h.key_press(Key::Space)));
        assert!(!pressed(&|h| h.key_press(Key::Escape)));
        // A key that is held down presses it once, not at the repeat rate.
        let mut harness = harness(&PIXEL, window, off());
        harness.key_down(Key::Enter);
        harness.run();
        assert!(std::mem::take(harness.state_mut()));
        harness.key_down(Key::Enter);
        harness.run();
        assert!(!*harness.state());
    }

    #[test]
    fn the_state_in_words() {
        let state = |enabled, viewers| {
            let view = View {
                enabled,
                viewers,
                ..View::default()
            };
            let (state, detail) = view.state();
            format!("{state}, {detail}")
        };
        assert_eq!(
            state(false, 0),
            "Streaming is off, nothing runs until you enable it"
        );
        assert_eq!(state(true, 0), "Streaming is on, nobody is watching");
        assert_eq!(state(true, 1), "Streaming is on, 1 viewer is watching");
        assert_eq!(state(true, 3), "Streaming is on, 3 viewers are watching");
        let mut harness = harness(&PIXEL, WINDOWS[0], on());
        harness.run();
        harness.get_by_label("Streaming is on");
        harness.get_by_label("2 viewers are watching");
        harness.get_by_label("Disable");
        harness.get_by_label("http://steamos.local:8090");
        harness.get_by_label("http://100.101.102.103:8090 (tailscale0)");
    }
}
