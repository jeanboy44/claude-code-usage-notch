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
// Status bands: vivid green plus warm yellow and red, all kept slightly warm so they
// sit next to Claude clay without clashing.
const GREEN: Color = Color::hex(0x6FCB63);
const YELLOW: Color = Color::hex(0xF7C33C);
const RED: Color = Color::hex(0xF2573F);
const DUSTY_BLUE: Color = Color::hex(0x6A9BCC); // Anthropic accent blue

// ---- semantic roles ----
pub const SURFACE: Color = BLACK;
pub const TEXT_PRIMARY: Color = IVORY;
pub const TEXT_SECONDARY: Color = CLOUD;
pub const TEXT_TERTIARY: Color = CLOUD.alpha(0.75); // ≥ 4.5:1 on black
pub const TRACK: Color = IVORY.alpha(0.12);
pub const CLAWD: Color = CLAUDE_ORANGE;
pub const CLAWD_EYE: Color = BLACK;
/// Clawd with the power out: the clay body, desaturated.
pub const DEAD: Color = Color::hex(0x6B6963);
#[allow(dead_code)]
pub const ANTIGRAVITY: Color = Color::hex(0x4E88F5);
pub const ANTIGRAVITY_EYE: Color = BLACK;
pub const SWEAT: Color = DUSTY_BLUE;

/// Band floors (% left) shared by colours and Clawd's stance.
pub const BAND_GREEN: f64 = 50.0;
pub const BAND_YELLOW: f64 = 20.0;
/// Seconds Clawd shivers per tick in the yellow band.
pub const TREMBLE_BURST: f64 = 0.9;
/// Clawd walks to his new spot at this speed (pt/s); shorter moves just snap.
pub const WALK_SPEED: f64 = 10.0;
pub const WALK_MIN: f64 = 0.75;
/// 0%: let go (0.25 s), out of sight, then pop up head-only — whole sequence (s).
pub const GONE_ANIM: f64 = 1.25;

/// Status by **remaining** percentage: bar fill, % numerals and the collapsed readout.
pub fn remaining(left: f64) -> Color {
    match left {
        l if l >= BAND_GREEN => GREEN,   // comfortable
        l if l >= BAND_YELLOW => YELLOW, // getting low
        _ => RED,                 // nearly out
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
pub const MICRO: Type = Type { size: 11.0, weight: medium, mono: true };
/// Collapsed session % (base size + 1.5).
pub const MICRO_HERO: Type = Type { size: 12.5, weight: medium, mono: true };
/// Graph x-axis hour labels.
pub const AXIS: Type = Type { size: 9.0, weight: regular, mono: true };
pub const MICRO_LABEL: Type = Type { size: 9.5, weight: regular, mono: false };

// ---- spacing / shape ----
// collapsed ears are sized to their content
pub const EAR_OUTER: f64 = 9.0; // content inset from the outer edge
pub const EAR_INNER: f64 = 5.0; // breathing room next to the camera cutout
pub const RESET_GAP: f64 = 2.5; // session % ↔ its reset countdown
// weekly bar under Clawd: 7 day-segments, lit = weekly budget left
pub const WEEK_SEG_W: f64 = 7.0;
pub const WEEK_SEG_GAP: f64 = 1.5;
pub const WEEK_BAR_H: f64 = 3.0;
pub const SESSION_GAP: f64 = 8.0; // weekly bar ↔ session %
pub const fn week_bar_w() -> f64 {
    7.0 * WEEK_SEG_W + 6.0 * WEEK_SEG_GAP
}
/// How far the collapsed blob extends past the cutout's right edge.
pub const NOTCH_OVERHANG: f64 = 1.0;
pub const FLARE: f64 = 6.0; // concave top corners melting into the bezel
pub const EXPANDED_W: f64 = 400.0;
pub const PAD: f64 = 22.0;
pub const RADIUS_EXPANDED: f64 = 22.0;
/// Collapsed bottom radius, matched to the hardware notch's own bottom corner
/// (capped to a fraction of the notch height for unusually short menu bars).
pub const RADIUS_COLLAPSED: f64 = 14.0;
pub const RADIUS_COLLAPSED_MAX_RATIO: f64 = 0.45;
/// (stiffness, damping) at mass 1. Open ≈ ζ 0.78 (soft bounce), close ≈ ζ 1 (no overshoot).
pub const SPRING_OPEN: (f64, f64) = (300.0, 27.0);
pub const SPRING_CLOSE: (f64, f64) = (650.0, 51.0); // ≈ critical: snaps back in ~0.25 s
pub const HEADER_H: f64 = 46.0;
pub const OVERLINE_H: f64 = 18.0;
pub const ROW_H: f64 = 26.0;
pub const BOTTOM_PAD: f64 = 14.0;
// history graph under the rows
pub const GRAPH_GAP: f64 = 10.0;
pub const GRAPH_H: f64 = 56.0;
/// Hairline stroke for the session line.
pub const GRAPH_LINE: f64 = 1.0;
pub const GRAPH_GRID: Color = IVORY.alpha(0.07);
pub const GRAPH_PACE: Color = CLOUD.alpha(0.3);
pub const GRAPH_AXIS: Color = IVORY.alpha(0.3);
pub const GRAPH_NOW: Color = IVORY.alpha(0.35);
/// Room under the graph for hour labels.
pub const GRAPH_AXIS_H: f64 = 16.0;
pub const BAR_H: f64 = 7.0;
// row columns, measured from the content edges
pub const COL_BAR_X: f64 = 56.0; // from left
pub const COL_PCT_R: f64 = 150.0; // % right edge, from right
pub const COL_BAR_END: f64 = 196.0; // bar end, from right
