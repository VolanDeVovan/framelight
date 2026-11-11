//! How the switch looks: one small table per theme, all of it data. The layout and the
//! widgets are the same for every theme (see `view`).

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily};
use std::sync::Arc;

/// The theme in use. The other one is here to be compared with and then deleted.
pub const THEME: &Theme = &PIXEL;

pub struct Theme {
    /// The one typeface, compiled in (OFL, see `assets/fonts`).
    pub font: &'static [u8],
    /// A pixel font is sharp only at whole multiples of the grid it was drawn on, so
    /// text sizes are rounded down to a multiple of this many pixels. 1 for outlines.
    pub grid: f32,
    /// `wght` of the strong text (title, state, button) if the font is variable.
    pub strong: Option<f32>,
    /// Corner radius of the button, in layout units.
    pub corner: f32,
    /// Text heights, in layout units (1/90 of the window height).
    pub size: Sizes,
    pub background: Color32,
    /// The band behind the title, and the line under it.
    pub header: Color32,
    pub rule: Color32,
    /// Fill of the outlined button.
    pub panel: Color32,
    pub ink: Color32,
    pub dim: Color32,
    pub accent: Color32,
    /// Text on a surface filled with `accent`.
    pub on_accent: Color32,
    /// "Streaming is on".
    pub on: Color32,
    pub warn: Color32,
}

pub struct Sizes {
    pub title: f32,
    pub state: f32,
    pub button: f32,
    pub address: f32,
    /// Tagline, hints and errors.
    pub small: f32,
}

/// Pixel art: a bitmap-style face on an 8 px grid, flat square shapes, amber on near-black.
#[allow(dead_code)]
pub const PIXEL: Theme = Theme {
    font: include_bytes!("../../../../assets/fonts/PressStart2P-Regular.ttf"),
    grid: 8.0,
    strong: None,
    corner: 0.0,
    size: Sizes {
        title: 6.0,
        state: 4.0,
        button: 5.4,
        address: 4.0,
        small: 2.0,
    },
    background: Color32::from_rgb(0x10, 0x11, 0x14),
    header: Color32::from_rgb(0x10, 0x11, 0x14),
    rule: Color32::from_rgb(0x24, 0x26, 0x2c),
    panel: Color32::from_rgb(0x24, 0x26, 0x2c),
    ink: Color32::from_rgb(0xf2, 0xf0, 0xea),
    dim: Color32::from_rgb(0x9a, 0x97, 0x8e),
    accent: Color32::from_rgb(0xff, 0xb4, 0x54),
    on_accent: Color32::from_rgb(0x10, 0x11, 0x14),
    on: Color32::from_rgb(0x7b, 0xd8, 0x8f),
    warn: Color32::from_rgb(0xff, 0x7a, 0x6b),
};

/// Steam-like: dark blue surfaces, a light blue accent, rounded corners, Inter.
#[allow(dead_code)]
pub const STEAM: Theme = Theme {
    font: include_bytes!("../../../../assets/fonts/Inter-Variable.ttf"),
    grid: 1.0,
    strong: Some(650.0),
    corner: 1.0,
    size: Sizes {
        title: 6.0,
        state: 4.6,
        button: 4.6,
        address: 3.6,
        small: 2.3,
    },
    background: Color32::from_rgb(0x1b, 0x28, 0x38),
    header: Color32::from_rgb(0x17, 0x1a, 0x21),
    rule: Color32::TRANSPARENT,
    panel: Color32::from_rgb(0x21, 0x33, 0x47),
    ink: Color32::from_rgb(0xe6, 0xee, 0xf5),
    dim: Color32::from_rgb(0x8f, 0x98, 0xa0),
    accent: Color32::from_rgb(0x66, 0xc0, 0xf4),
    on_accent: Color32::from_rgb(0x12, 0x18, 0x21),
    on: Color32::from_rgb(0xa4, 0xd0, 0x07),
    warn: Color32::from_rgb(0xff, 0x7a, 0x6b),
};

impl Theme {
    /// Makes the theme's font the only one and sets what does not depend on the window.
    pub fn install(&self, ctx: &egui::Context) {
        let mut fonts = FontDefinitions::empty();
        fonts.font_data.insert(
            "framelight".to_owned(),
            Arc::new(FontData::from_static(self.font)),
        );
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.insert(family, vec!["framelight".to_owned()]);
        }
        ctx.set_fonts(fonts);
        // One look, whatever the desktop's light/dark preference says.
        ctx.set_theme(egui::Theme::Dark);
        ctx.all_styles_mut(|style| {
            style.interaction.selectable_labels = false;
            style.visuals.panel_fill = self.background;
        });
    }
}
