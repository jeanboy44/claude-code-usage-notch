//! Design tokens. Claude/Anthropic palette on a notch-black surface — see DESIGN.md.
//! Every colour, type size and spacing value the view uses lives here.

use objc2::rc::Retained;
use objc2_app_kit::{NSColor, NSFontWeight, NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold};

#[derive(Clone, Copy)]
pub struct Color(u32, f64);

impl Color {
    pub const fn hex(rgb: u32) -> Self {
        Color(rgb, 1.0)
    }
    pub const fn alpha(self, a: f64) -> Self {
        Color(self.0, a)
    }
    pub fn ns(self) -> Retained<NSColor> {
        let c = |shift: u32| ((self.0 >> shift) & 0xff) as f64 / 255.0;
        NSColor::colorWithSRGBRed_green_blue_alpha(c(16), c(8), c(0), self.1)
    }
}

// ---- palette (Anthropic brand) ----
const BLACK: Color = Color::hex(0x000000); // must stay pure black to blend with the notch
const IVORY: Color = Color::hex(0xFAF9F5);
const CLOUD: Color = Color::hex(0xB0AEA5);
const CLAUDE_ORANGE: Color = Color::hex(0xD97757); // Clawd
const SKY: Color = Color::hex(0x6A9BCC);
const AMBER: Color = Color::hex(0xE3A857);
const BRICK: Color = Color::hex(0xD4524A);

// ---- semantic roles ----
pub const SURFACE: Color = BLACK;
pub const TEXT_PRIMARY: Color = IVORY;
pub const TEXT_SECONDARY: Color = CLOUD;
pub const TEXT_TERTIARY: Color = CLOUD.alpha(0.6);
pub const TRACK: Color = IVORY.alpha(0.12);
pub const CLAWD: Color = CLAUDE_ORANGE;
pub const CLAWD_EYE: Color = BLACK;
pub const SWEAT: Color = SKY;

/// Status by **remaining** percentage: bar fill, % numerals and the collapsed label.
pub fn remaining(left: f64) -> Color {
    match left {
        l if l > 30.0 => CLAUDE_ORANGE, // plenty
        l if l > 10.0 => AMBER,         // getting low
        _ => BRICK,                     // nearly out
    }
}

// ---- type scale (SF Pro, light weights, tabular digits for numbers) ----
pub struct Type {
    pub size: f64,
    pub weight: fn() -> NSFontWeight,
    /// Monospaced digits so numbers don't jitter as they change.
    pub mono: bool,
}
fn semibold() -> NSFontWeight {
    unsafe { NSFontWeightSemibold }
}
fn medium() -> NSFontWeight {
    unsafe { NSFontWeightMedium }
}
fn regular() -> NSFontWeight {
    unsafe { NSFontWeightRegular }
}
pub const TITLE: Type = Type { size: 14.0, weight: semibold, mono: false };
pub const LABEL: Type = Type { size: 11.0, weight: medium, mono: false };
pub const NUMERAL: Type = Type { size: 12.0, weight: medium, mono: true };
pub const CAPTION: Type = Type { size: 10.5, weight: regular, mono: true };
pub const OVERLINE: Type = Type { size: 9.5, weight: medium, mono: false };
/// Collapsed notch readout; size is scaled to the notch height at draw time.
pub const MICRO: Type = Type { size: 10.5, weight: medium, mono: true };
pub const MICRO_LABEL: Type = Type { size: 9.0, weight: regular, mono: false };

// ---- spacing / shape ----
pub const WING: f64 = 50.0; // side "ear" width next to the notch
pub const FLARE: f64 = 6.0; // concave top corners melting into the bezel
pub const EXPANDED_W: f64 = 400.0;
pub const PAD: f64 = 22.0;
pub const RADIUS_EXPANDED: f64 = 20.0;
pub const HEADER_H: f64 = 46.0;
pub const OVERLINE_H: f64 = 18.0;
pub const ROW_H: f64 = 26.0;
pub const BOTTOM_PAD: f64 = 14.0;
pub const BAR_H: f64 = 7.0;
// row columns, measured from the content edges
pub const COL_BAR_X: f64 = 48.0; // from left
pub const COL_PCT_R: f64 = 150.0; // % right edge, from right
pub const COL_BAR_END: f64 = 196.0; // bar end, from right
