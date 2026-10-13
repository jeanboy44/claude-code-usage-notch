//! `--render-readme <dir>`: draws README images offline with the real drawing code —
//! PNG frames of the collapsed notch going through every weekly state (turned into a
//! GIF by `scripts/make_gifs.py`) plus one PNG of the expanded panel.
//! `--render-icon <png>`: the 1024 px app icon (turned into AppIcon.icns by
//! `scripts/make_icon.sh`).

use super::*;
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage};

/// A 14" MacBook Pro notch.
const NOTCH_W: f64 = 185.0;
const NOTCH_H: f64 = 32.0;
const CANVAS_W: f64 = 600.0;
const FPS: f64 = 20.0;

/// (weekly % left, seconds to hold) — one step per lost segment, through every band.
const SCRIPT: &[(f64, f64)] = &[
    (100.0, 1.6),
    (82.0, 1.5),
    (68.0, 1.5),
    (45.0, 2.2),
    (30.0, 2.2),
    (16.0, 2.6),
    (9.0, 2.4),
    (0.0, 2.8),
];

pub fn readme_frames(mtm: MainThreadMarker, dir: &str) {
    let _ = std::fs::create_dir_all(format!("{dir}/frames"));
    let now = usage::now();
    let limit = |used: f64, reset_in: i64| Some(Limit { used, resets_at: Some(now + reset_in) });
    let snap = |week_left: f64| Snapshot {
        five_hour: limit(28.0, 2 * 3600 + 13 * 60),
        seven_day: limit(100.0 - week_left, 3 * 86400 + 5 * 3600),
        fetched_at: now,
        ..Snapshot::default()
    };

    let view = NotchView::new(mtm, CAShapeLayer::new());
    view.ivars().notch_h.set(NOTCH_H);

    // Collapsed: one frame per 1/FPS through the script.
    let base = CACurrentMediaTime();
    let mut t = 0.0;
    let mut frame = 0;
    for &(week_left, hold) in SCRIPT {
        let end = t + hold;
        while t < end {
            CLOCK.with(|c| c.set(Some(base + t)));
            usage::set_snapshot(snap(week_left));
            let iv = view.ivars();
            // What the periodic tick would do: blink now and then, tremble bursts.
            iv.blink.set(t % 2.7 < 0.12);
            if t % 2.0 < 1.0 / FPS {
                iv.tremble_until.set(base + t + theme::TREMBLE_BURST);
            }
            let image = collapsed_frame(&view, week_left);
            save_png(&image, &format!("{dir}/frames/{frame:04}.png"));
            frame += 1;
            t += 1.0 / FPS;
        }
    }

    // Expanded panel with a believable session history.
    CLOCK.with(|c| c.set(None));
    let s = snap(59.0);
    let start = now - (5 * 3600 - (2 * 3600 + 13 * 60));
    let samples = (0..=((now - start) / 300))
        .map(|i| {
            let k = i as f64 * 300.0 / (now - start) as f64;
            let wiggle = ((i as f64) * 1.7).sin() * 1.5;
            usage::Sample { t: start + i * 300, session: (2.0 + 26.0 * k.powf(1.3) + wiggle).max(0.0) as f32 }
        })
        .collect();
    usage::set_history(samples);
    usage::set_snapshot(s.clone());
    let image = expanded_frame(&view, &s);
    save_png(&image, &format!("{dir}/expanded.png"));
    println!("rendered {frame} frames + expanded.png into {dir}");
}

/// Wallpaper + translucent menu bar + the hardware notch, on a `size` canvas.
fn backdrop(size: NSSize) {
    Color::hex(0xD9E3EF).ns().set();
    NSBezierPath::fillRect(NSRect::new(pt(0.0, 0.0), size));
    Color::hex(0xFFFFFF).alpha(0.55).ns().set();
    NSBezierPath::fillRect(NSRect::new(pt(0.0, size.height - NOTCH_H), NSSize::new(size.width, NOTCH_H)));
    let y = size.height - NOTCH_H + 8.0;
    let menu = Type { size: 13.0, weight: theme::LABEL.weight, mono: false };
    text_up("\u{F8FF}", 16.0, y, &menu, Color::hex(0x1D1D1F));
    text_up("9:41", size.width - 52.0, y, &menu, Color::hex(0x1D1D1F));
    // The camera cutout itself.
    theme::SURFACE.ns().set();
    notch_bezier(notch_rect(size), theme::RADIUS_COLLAPSED).fill();
}

fn notch_rect(size: NSSize) -> NSRect {
    NSRect::new(pt((size.width - NOTCH_W) / 2.0, size.height - NOTCH_H), NSSize::new(NOTCH_W, NOTCH_H))
}

fn collapsed_frame(view: &NotchView, week_left: f64) -> Retained<NSImage> {
    let size = NSSize::new(CANVAS_W, NOTCH_H + 14.0);
    let ears = Ears::new(usage::snapshot().as_ref(), NOTCH_H, false);
    let (lw, _) = ears.widths();
    let notch = notch_rect(size);
    let blob = NSRect::new(
        pt(notch.origin.x - FLARE - lw, notch.origin.y),
        NSSize::new(lw + NOTCH_W / 2.0 + 2.0 * FLARE, NOTCH_H),
    );
    let iv = view.ivars();
    iv.expanded.set(false);
    iv.shown_expanded.set(false);
    paint(size, |size| {
        backdrop(size);
        theme::SURFACE.ns().set();
        notch_bezier(blob, theme::RADIUS_COLLAPSED.min(NOTCH_H * theme::RADIUS_COLLAPSED_MAX_RATIO)).fill();
        draw_view(view, blob);
        let label = Type { size: 11.0, ..theme::CAPTION };
        let caption = format!("weekly {}% left", week_left.round());
        text_up(&caption, notch.origin.x + NOTCH_W + 14.0, 3.0, &label, Color::hex(0x5A6472));
    })
}

fn expanded_frame(view: &NotchView, snap: &Snapshot) -> Retained<NSImage> {
    let ears = Ears::new(Some(snap), NOTCH_H, false);
    let (lw, _) = ears.widths();
    let rows = rows(snap).len().max(1) as f64;
    let ew = (NOTCH_W + 2.0 * FLARE + 2.0 * lw).max(EXPANDED_W);
    let eh = expanded_height(NOTCH_H, rows);
    let size = NSSize::new(CANVAS_W.max(ew + 60.0), eh + 30.0);
    let panel = NSRect::new(pt((size.width - ew) / 2.0, size.height - eh), NSSize::new(ew, eh));
    let iv = view.ivars();
    iv.expanded.set(true);
    iv.shown_expanded.set(true);
    paint(size, |size| {
        backdrop(size);
        theme::SURFACE.ns().set();
        notch_bezier(panel, theme::RADIUS_EXPANDED).fill();
        draw_view(view, panel);
    })
}

/// Lays the view out at `rect`'s size and draws its content there.
fn draw_view(view: &NotchView, rect: NSRect) {
    view.setFrame(NSRect::new(pt(0.0, 0.0), rect.size));
    let bounds = view.bounds();
    let Some(rep) = view.bitmapImageRepForCachingDisplayInRect(bounds) else { return };
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    let image = NSImage::initWithSize(NSImage::alloc(), bounds.size);
    image.addRepresentation(&rep);
    image.drawInRect(rect);
}

/// Text in an unflipped (y-up) image context.
fn text_up(s: &str, x: f64, y: f64, ty: &Type, color: Color) {
    let ns = NSString::from_str(s);
    unsafe { ns.drawAtPoint_withAttributes(pt(x, y), Some(&attrs(ty, color))) };
}

#[allow(deprecated)]
fn paint(size: NSSize, f: impl FnOnce(NSSize)) -> Retained<NSImage> {
    let image = NSImage::initWithSize(NSImage::alloc(), size);
    image.lockFocus();
    f(size);
    image.unlockFocus();
    image
}

fn save_png(image: &NSImage, path: &str) {
    let Some(tiff) = image.TIFFRepresentation() else { return };
    let Some(rep) = NSBitmapImageRep::imageRepWithData(&tiff) else { return };
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) };
    if let Some(png) = png {
        png.writeToFile_atomically(&NSString::from_str(path), true);
    }
}

/// The app icon: a dark squircle with the notch at the top and Clawd standing on the
/// weekly bar — five day-segments lit green, two spent.
#[allow(deprecated)]
pub fn app_icon(path: &str) {
    let size = NSSize::new(1024.0, 1024.0);
    let image = NSImage::initWithSize(NSImage::alloc(), size);
    image.lockFocusFlipped(true);

    // macOS icon grid: 824 pt squircle inset 100 pt.
    let tile = NSRect::new(pt(100.0, 100.0), NSSize::new(824.0, 824.0));
    let squircle = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(tile, 185.0, 185.0);
    Color::hex(0x1C1C1E).ns().set();
    squircle.fill();
    NSGraphicsContext::saveGraphicsState_class();
    squircle.addClip();
    // Soft top light, like the rest of the macOS icon family.
    for i in 0..48 {
        let k = i as f64 / 48.0;
        Color::hex(0xFFFFFF).alpha(0.06 * (1.0 - k)).ns().set();
        NSBezierPath::fillRect(NSRect::new(pt(100.0, 100.0 + k * 520.0), NSSize::new(824.0, 520.0 / 48.0 + 0.5)));
    }
    // The notch, hanging from the top edge.
    theme::SURFACE.ns().set();
    collapsed_outline_at(pt(512.0 - 170.0, 100.0), 340.0, 92.0, 40.0).fill();
    NSGraphicsContext::restoreGraphicsState_class();

    // Clawd on the bar, centred below the notch.
    let sprite = 500.0;
    let clawd_h = sprite * 15.0 / 24.0;
    let (seg, gap, bar_h) = (82.0, 18.0, 34.0);
    let bar_w = 7.0 * seg + 6.0 * gap;
    let top = 372.0;
    let bar_y = top + clawd_h + 6.0;
    critter(512.0, top + clawd_h / 2.0, sprite, 1.0, Mood::Happy, Pose::default());
    for i in 0..7 {
        let x = 512.0 - bar_w / 2.0 + i as f64 * (seg + gap);
        let color = if i < 5 { theme::remaining(70.0) } else { Color::hex(0xFFFFFF).alpha(0.14) };
        pill(x, bar_y, seg, bar_h, color);
    }

    image.unlockFocus();
    save_png(&image, path);
    println!("rendered {path}");
}

/// Notch outline (square top with flares, rounded bottom) at `origin` in a flipped context.
fn collapsed_outline_at(origin: NSPoint, w: f64, h: f64, r: f64) -> Retained<NSBezierPath> {
    let p = collapsed_outline(w, h, r);
    let t = NSAffineTransform::transform();
    t.translateXBy_yBy(origin.x, origin.y);
    p.transformUsingAffineTransform(&t);
    p
}
