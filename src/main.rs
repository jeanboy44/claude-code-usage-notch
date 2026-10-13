mod theme;
mod usage;

use std::cell::Cell;
use std::ptr::NonNull;
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAccessibilityButtonRole, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDidChangeScreenParametersNotification, NSBackingStoreType, NSBezierPath, NSColor,
    NSEvent, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSMainMenuWindowLevel,
    NSMenu, NSMenuItem, NSPanel, NSScreen, NSStringDrawing, NSTrackingArea, NSTrackingAreaOptions,
    NSView, NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
    NSWorkspaceDidWakeNotification,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSDate, NSDateFormatter, NSDictionary, NSLocale, NSNotification,
    NSNotificationCenter, NSPoint, NSRect, NSSize, NSString, NSTimeZone, NSTimer, NSURL,
};

use theme::{Color, Type, EXPANDED_W, FLARE, PAD, WING};
use usage::{snapshot, Limit, Snapshot};

const USAGE_PAGE: &str = "https://claude.ai/settings/usage";
const HOVER_IN_DELAY: f64 = 0.22;
const HOVER_OUT_DELAY: f64 = 0.3;

static WAKE: OnceLock<Sender<()>> = OnceLock::new();

#[derive(Default)]
struct Ivars {
    expanded: Cell<bool>,
    /// Pointer currently inside the panel (expansion waits for it to linger).
    hovering: Cell<bool>,
    hover_gen: Cell<u64>,
    blink: Cell<bool>,
    refreshing: Cell<bool>,
    seen_version: Cell<u64>,
    notch_h: Cell<f64>,
    /// Final panel height for the current state; content is hidden mid-animation.
    target_h: Cell<f64>,
}

// A panel that can never take keyboard focus or become main — the notch must not
// steal focus from whatever you're working in.
define_class!(
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "NotchiUsagePanel"]
    struct NotchPanel;

    impl NotchPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            false
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            false
        }
    }
);

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "NotchiUsageView"]
    #[ivars = Ivars]
    struct NotchView;

    impl NotchView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _e: &NSEvent) {
            self.hover_changed(true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _e: &NSEvent) {
            self.hover_changed(false);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _e: &NSEvent) {
            self.refresh();
        }

        // Covers right-click, ctrl-click and two-finger tap.
        #[unsafe(method_id(menuForEvent:))]
        fn menu_for_event(&self, _e: &NSEvent) -> Option<Retained<NSMenu>> {
            Some(self.menu())
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn ax_press(&self) -> bool {
            self.refresh();
            true
        }

        #[unsafe(method(refreshNow:))]
        fn refresh_now(&self, _sender: Option<&AnyObject>) {
            self.refresh();
        }

        #[unsafe(method(openUsagePage:))]
        fn open_usage_page(&self, _sender: Option<&AnyObject>) {
            if let Some(url) = NSURL::URLWithString(&NSString::from_str(USAGE_PAGE)) {
                NSWorkspace::sharedWorkspace().openURL(&url);
            }
        }
    }
);

impl NotchView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars::default());
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&this),
                None,
            )
        };
        this.addTrackingArea(&area);
        unsafe {
            let _: () = msg_send![&this, setAccessibilityElement: true];
            let _: () = msg_send![&this, setAccessibilityRole: NSAccessibilityButtonRole];
        }
        this.update_accessibility();
        this
    }

    fn menu(&self) -> Retained<NSMenu> {
        let mtm = self.mtm();
        let menu = NSMenu::new(mtm);
        let item = |title: &NSString, action, target: Option<&AnyObject>| unsafe {
            let it = NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), title, Some(action), ns_string!(""));
            it.setTarget(target);
            menu.addItem(&it);
        };
        item(ns_string!("Refresh Now"), sel!(refreshNow:), Some(self));
        item(ns_string!("Open Usage Page"), sel!(openUsagePage:), Some(self));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        item(ns_string!("Quit Notchi Usage"), sel!(terminate:), None);
        menu
    }

    fn refresh(&self) {
        self.ivars().refreshing.set(true);
        self.setNeedsDisplay(true);
        wake_worker();
    }

    /// Debounced hover: expand only if the pointer lingers, collapse only if it stays out.
    fn hover_changed(&self, inside: bool) {
        let iv = self.ivars();
        iv.hovering.set(inside);
        let gen = iv.hover_gen.get() + 1;
        iv.hover_gen.set(gen);
        let view = self.retain();
        after(if inside { HOVER_IN_DELAY } else { HOVER_OUT_DELAY }, move || {
            let iv = view.ivars();
            if iv.hover_gen.get() == gen && iv.hovering.get() == inside {
                view.set_expanded(inside);
            }
        });
    }

    fn set_expanded(&self, on: bool) {
        if self.ivars().expanded.replace(on) != on {
            self.relayout(!reduce_motion());
            if let Some(w) = self.window() {
                w.setHasShadow(on);
            }
        }
    }

    /// Recompute geometry and resize the panel to the current state.
    fn relayout(&self, animate: bool) {
        let Some(window) = self.window() else { return };
        let Some(g) = Geometry::current(self.mtm()) else { return };
        let iv = self.ivars();
        iv.notch_h.set(g.notch_h);
        let collapsed_w = g.notch_w + 2.0 * (WING + FLARE);
        let (w, h) = if iv.expanded.get() {
            let rows = snapshot().map_or(2, |s| rows(&s).len().max(1)) as f64;
            let content = theme::HEADER_H + theme::OVERLINE_H + rows * theme::ROW_H + theme::BOTTOM_PAD;
            (collapsed_w.max(EXPANDED_W), g.notch_h + 10.0 + content)
        } else {
            (collapsed_w, g.notch_h)
        };
        iv.target_h.set(h);
        let frame = NSRect::new(NSPoint::new(g.center_x - w / 2.0, g.top - h), NSSize::new(w, h));
        if frame != window.frame() {
            window.setFrame_display_animate(frame, true, animate);
            window.invalidateShadow();
        }
        self.setNeedsDisplay(true);
    }

    /// Fires every few seconds (randomised): blink and pick up new data.
    fn tick(&self) {
        let iv = self.ivars();
        let version = snapshot().map_or(0, |s| s.version);
        if iv.seen_version.replace(version) != version {
            iv.refreshing.set(false);
            self.update_accessibility();
            if iv.expanded.get() {
                self.relayout(false); // row count may have changed
            }
        }
        iv.blink.set(true);
        self.setNeedsDisplay(true);
        let view = self.retain();
        after(0.14, move || {
            view.ivars().blink.set(false);
            view.setNeedsDisplay(true);
        });
    }

    fn update_accessibility(&self) {
        let label = match snapshot() {
            None => "Claude usage: loading".to_string(),
            Some(s) => {
                let parts: Vec<String> = rows(&s)
                    .into_iter()
                    .map(|(name, l)| {
                        let reset = l.resets_at.map_or(String::new(), |at| format!(", resets in {}", countdown(at)));
                        format!("{name} {} left{reset}", pct(l.left()))
                    })
                    .collect();
                format!("Claude usage: {}", parts.join("; "))
            }
        };
        unsafe {
            let _: () = msg_send![self, setAccessibilityLabel: &*NSString::from_str(&label)];
        }
    }

    fn draw(&self) {
        let b = self.bounds();
        let (w, h) = (b.size.width, b.size.height);
        let iv = self.ivars();
        let notch_h = iv.notch_h.get().max(1.0);
        let expanded = iv.expanded.get();
        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor());
        let snap = snapshot();
        let stale = snap.as_ref().is_some_and(|s| s.error.is_some() || usage::now() - s.fetched_at > 600);
        let mood = Mood::of(snap.as_ref());

        // Body: black, square top, rounded bottom, flared top corners.
        let r = if expanded { theme::RADIUS_EXPANDED } else { (notch_h * 0.42).min(12.0) };
        let p = NSBezierPath::new();
        p.moveToPoint(pt(0.0, 0.0));
        p.curveToPoint_controlPoint1_controlPoint2(pt(FLARE, FLARE), pt(FLARE, 0.0), pt(FLARE, 0.0));
        p.lineToPoint(pt(FLARE, h - r));
        p.curveToPoint_controlPoint1_controlPoint2(pt(FLARE + r, h), pt(FLARE, h), pt(FLARE, h));
        p.lineToPoint(pt(w - FLARE - r, h));
        p.curveToPoint_controlPoint1_controlPoint2(pt(w - FLARE, h - r), pt(w - FLARE, h), pt(w - FLARE, h));
        p.lineToPoint(pt(w - FLARE, FLARE));
        p.curveToPoint_controlPoint1_controlPoint2(pt(w, 0.0), pt(w - FLARE, 0.0), pt(w - FLARE, 0.0));
        p.closePath();
        theme::SURFACE.ns().set();
        p.fill();

        // Mid-animation frames only show the shape, so text never slides or clips.
        if (h - iv.target_h.get()).abs() > 0.5 {
            return;
        }

        if !expanded {
            let s = ((notch_h - 12.0) * 24.0 / 15.0).clamp(18.0, 30.0);
            critter(FLARE + WING / 2.0 + 4.0, notch_h / 2.0, s, scale, mood, iv.blink.get());

            // Right ear: session (5h) and weekly (7d) left, stacked.
            let size = (notch_h / 2.9).clamp(9.5, theme::MICRO.size);
            let lh = size * 1.2;
            let y0 = (notch_h - 2.0 * lh) / 2.0;
            let (label_x, num_r) = (w - FLARE - WING + 3.0, w - FLARE - 8.0);
            let tag_ty = Type { size: size - 1.5, ..theme::MICRO_LABEL };
            let num_ty = Type { size, ..theme::MICRO };
            let limits = snap.as_ref().map_or([None, None], |s| [s.five_hour, s.seven_day]);
            for (i, (tag, l)) in ["5h", "7d"].into_iter().zip(limits).enumerate() {
                let y = y0 + i as f64 * lh;
                let (value, color) = match l {
                    // At 0% the useful number is when it comes back.
                    Some(l) if l.left() < 1.0 => (
                        l.resets_at.map_or("0%".to_string(), short_countdown),
                        theme::remaining(0.0),
                    ),
                    Some(l) => (pct(l.left()), theme::remaining(l.left())),
                    None => ("–".to_string(), theme::TEXT_TERTIARY),
                };
                let color = if stale { theme::TEXT_TERTIARY } else { color };
                text(tag, label_x, y + 1.2, &tag_ty, theme::TEXT_TERTIARY, Align::Left);
                text(&value, num_r, y, &num_ty, color, Align::Right);
            }
            return;
        }

        // Header: Clawd + mood line + status line.
        let top = notch_h + 10.0;
        critter(FLARE + PAD + 18.0, top + 18.0, 36.0, scale, mood, iv.blink.get());
        let x = FLARE + PAD + 48.0;
        text(mood.title(), x, top + 4.0, &theme::TITLE, theme::TEXT_PRIMARY, Align::Left);
        let status = match snap.as_ref() {
            _ if iv.refreshing.get() => "Refreshing…".to_string(),
            None => "Loading…".to_string(),
            Some(s) => match (s.error, s.fetched_at) {
                (Some(e), 0) => e.to_string(),
                (Some(e), t) => format!("{e} · data {}", ago(usage::now() - t)),
                (None, t) => format!("Updated {} · click to refresh", ago(usage::now() - t)),
            },
        };
        text(&status, x, top + 23.0, &theme::CAPTION, theme::TEXT_SECONDARY, Align::Left);

        // Limit rows.
        let Some(snap) = snap else { return };
        let (left, right) = (FLARE + PAD, w - FLARE - PAD);
        let bar_x = left + theme::COL_BAR_X;
        let bar_w = (right - theme::COL_BAR_END) - bar_x;
        let oy = top + theme::HEADER_H;
        text("REMAINING", bar_x, oy, &theme::OVERLINE, theme::TEXT_TERTIARY, Align::Left);
        text("RESETS IN", right, oy, &theme::OVERLINE, theme::TEXT_TERTIARY, Align::Right);
        for (i, (name, l)) in rows(&snap).into_iter().enumerate() {
            let y = oy + theme::OVERLINE_H + i as f64 * theme::ROW_H;
            let left_pct = l.left();
            let c = if stale { theme::TEXT_TERTIARY } else { theme::remaining(left_pct) };
            text(name, left, y, &theme::LABEL, theme::TEXT_SECONDARY, Align::Left);
            // The bar shows what's LEFT, so it shrinks as you use Claude — same as the number.
            let by = y + 4.0;
            pill(bar_x, by, bar_w, theme::BAR_H, theme::TRACK);
            if left_pct > 0.0 {
                pill(bar_x, by, (bar_w * left_pct / 100.0).max(theme::BAR_H), theme::BAR_H, c);
            }
            text(&pct(left_pct), right - theme::COL_PCT_R, y - 1.0, &theme::NUMERAL, c, Align::Right);
            let reset = match l.resets_at {
                Some(at) if at > usage::now() => reset_label(at),
                Some(_) => "just reset".to_string(),
                None => "not started".to_string(),
            };
            text(&reset, right, y, &theme::CAPTION, theme::TEXT_SECONDARY, Align::Right);
        }
    }
}

fn wake_worker() {
    if let Some(tx) = WAKE.get() {
        let _ = tx.send(());
    }
}

fn reduce_motion() -> bool {
    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

/// One-shot main-thread timer.
fn after(secs: f64, f: impl Fn() + 'static) {
    let block = RcBlock::new(move |_: NonNull<NSTimer>| f());
    let timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(secs, false, &block) };
    timer.setTolerance(secs * 0.2);
}

/// Cheap xorshift for blink jitter.
fn jitter(lo: f64, hi: f64) -> f64 {
    thread_local!(static SEED: Cell<u64> = Cell::new(usage::now() as u64 | 1));
    SEED.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        lo + (x % 10_000) as f64 / 10_000.0 * (hi - lo)
    })
}

fn schedule_tick(view: Retained<NotchView>) {
    let delay = if reduce_motion() { jitter(8.0, 12.0) } else { jitter(2.5, 6.0) };
    after(delay, move || {
        view.tick();
        schedule_tick(view.clone());
    });
}

fn rows(s: &Snapshot) -> Vec<(&'static str, Limit)> {
    [("Session", s.five_hour), ("Weekly", s.seven_day), ("Opus", s.seven_day_opus), ("Sonnet", s.seven_day_sonnet)]
        .into_iter()
        .filter_map(|(n, l)| Some((n, l?)))
        // Per-model rows that were never used are noise.
        .filter(|(n, l)| matches!(*n, "Session" | "Weekly") || l.used > 0.0 || l.resets_at.is_some())
        .collect()
}

/// Floor, so "100%" really means untouched and "0%" really means out.
fn pct(left: f64) -> String {
    if left > 0.0 && left < 1.0 {
        "<1%".to_string()
    } else {
        format!("{}%", left.floor() as i64)
    }
}

// ---------- geometry ----------

struct Geometry {
    center_x: f64,
    top: f64,
    notch_w: f64,
    notch_h: f64,
}

impl Geometry {
    fn current(mtm: MainThreadMarker) -> Option<Self> {
        let screens = NSScreen::screens(mtm);
        // Prefer the notched built-in display, else the menu-bar screen.
        let screen = screens.iter().find(|s| s.safeAreaInsets().top > 0.0).or_else(|| screens.firstObject())?;
        let f = screen.frame();
        let inset = screen.safeAreaInsets().top;
        let (notch_w, notch_h) = if inset > 0.0 {
            let l = screen.auxiliaryTopLeftArea();
            let r = screen.auxiliaryTopRightArea();
            (f.size.width - l.size.width - r.size.width, inset)
        } else {
            // No notch: a small pill hanging from the menu bar.
            let vf = screen.visibleFrame();
            (0.0, (f.origin.y + f.size.height - (vf.origin.y + vf.size.height)).clamp(24.0, 32.0))
        };
        Some(Geometry { center_x: f.origin.x + f.size.width / 2.0, top: f.origin.y + f.size.height, notch_w, notch_h })
    }
}

// ---------- Clawd ----------

#[derive(Clone, Copy, PartialEq)]
enum Mood {
    Sleepy,
    Happy,
    Busy,
    Careful,
    Dizzy,
}

impl Mood {
    /// Driven by whichever limit binds first, not just the session.
    fn of(s: Option<&Snapshot>) -> Self {
        let tightest = s.and_then(|s| rows(s).into_iter().map(|(_, l)| l.left()).reduce(f64::min));
        match tightest {
            None => Mood::Sleepy,
            Some(l) if l > 50.0 => Mood::Happy,
            Some(l) if l > 30.0 => Mood::Busy,
            Some(l) if l > 10.0 => Mood::Careful,
            Some(_) => Mood::Dizzy,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Mood::Sleepy => "Napping · no data yet",
            Mood::Happy => "Plenty of room",
            Mood::Busy => "Cruising along",
            Mood::Careful => "Pace yourself",
            Mood::Dizzy => "Almost out",
        }
    }
}

/// Clawd, drawn from the official Claude Code icon (24×24 viewBox, body spans y 5..20).
/// `s` is the sprite width; the sprite is 24×15 units, centred on (cx, cy).
/// Edges are snapped to device pixels so the pixel art stays crisp.
fn critter(cx: f64, cy: f64, s: f64, scale: f64, mood: Mood, blink: bool) {
    let u = s / 24.0;
    let (x0, y0) = (cx - 12.0 * u, cy - 7.5 * u);
    let snap = |v: f64| (v * scale).round() / scale;
    let rect = |x: f64, y: f64, w: f64, h: f64| {
        let (l, t) = (snap(x0 + x * u), snap(y0 + (y - 5.0) * u));
        let (r, b) = (snap(x0 + (x + w) * u), snap(y0 + (y - 5.0 + h) * u));
        NSBezierPath::fillRect(NSRect::new(pt(l, t), NSSize::new(r - l, b - t)));
    };

    theme::CLAWD.ns().set();
    rect(3.0, 5.0, 18.0, 12.08); // body
    rect(0.0, 10.95, 24.0, 3.1); // arms
    for lx in [4.487, 7.488, 15.0, 18.0] {
        rect(lx, 17.0, 1.51, 3.0); // legs
    }

    // Eyes are holes in the icon; paint them with the notch black.
    theme::CLAWD_EYE.ns().set();
    let (eye_top, eye_h) = match mood {
        Mood::Sleepy => (10.1, 0.85),
        Mood::Dizzy => (9.1, 0.9),
        _ if blink => (10.1, 0.85),
        _ => (8.1, 2.85),
    };
    for ex in [6.0, 16.51] {
        rect(ex, eye_top, 1.49, eye_h);
    }

    if matches!(mood, Mood::Careful | Mood::Dizzy) {
        // sweat drop
        theme::SWEAT.ns().set();
        rect(22.0, 5.5, 1.3, 1.3);
        rect(22.6, 4.6, 0.7, 0.9);
    }
    if mood == Mood::Sleepy {
        let ty = Type { size: (8.0 * u).max(7.0), ..theme::NUMERAL };
        text("z", x0 + 22.5 * u, y0 - 6.0 * u, &ty, theme::TEXT_SECONDARY, Align::Left);
    }
}

// ---------- drawing helpers ----------

fn pt(x: f64, y: f64) -> NSPoint {
    NSPoint::new(x, y)
}

fn pill(x: f64, y: f64, w: f64, h: f64, c: Color) {
    c.ns().set();
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(NSRect::new(pt(x, y), NSSize::new(w, h)), h / 2.0, h / 2.0)
        .fill();
}

enum Align {
    Left,
    Right,
}

fn text(s: &str, x: f64, y: f64, ty: &Type, color: Color, align: Align) {
    let font = if ty.mono {
        NSFont::monospacedDigitSystemFontOfSize_weight(ty.size, (ty.weight)())
    } else {
        NSFont::systemFontOfSize_weight(ty.size, (ty.weight)())
    };
    let color = color.ns();
    let ns = NSString::from_str(s);
    unsafe {
        let attrs = NSDictionary::<NSString, AnyObject>::from_slices(
            &[NSFontAttributeName, NSForegroundColorAttributeName],
            &[&*font as &AnyObject, &*color as &AnyObject],
        );
        let x = match align {
            Align::Left => x,
            Align::Right => x - ns.sizeWithAttributes(Some(&attrs)).width,
        };
        ns.drawAtPoint_withAttributes(pt(x, y), Some(&attrs));
    }
}

fn ago(secs: i64) -> String {
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

/// "1h 23m" / "2d 5h"
fn countdown(at: i64) -> String {
    let left = (at - usage::now()).max(0);
    let (d, h, m) = (left / 86400, left % 86400 / 3600, left % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

/// Fits the collapsed ear: "42m", "4h12", "2d5h".
fn short_countdown(at: i64) -> String {
    let left = (at - usage::now()).max(0);
    let (d, h, m) = (left / 86400, left % 86400 / 3600, left % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h{m:02}"),
        _ => format!("{d}d{h}h"),
    }
}

/// "1h 23m · 14:30" / "2d 5h · Fri 09:00"
fn reset_label(at: i64) -> String {
    thread_local! {
        static FMT: Retained<NSDateFormatter> = {
            let f = NSDateFormatter::new();
            f.setLocale(Some(&NSLocale::initWithLocaleIdentifier(NSLocale::alloc(), ns_string!("en_US_POSIX"))));
            f
        };
    }
    let same_day = at - usage::now() < 86400;
    let abs = FMT.with(|f| {
        f.setTimeZone(Some(&NSTimeZone::systemTimeZone())); // follow time-zone changes
        f.setDateFormat(Some(if same_day { ns_string!("HH:mm") } else { ns_string!("EEE HH:mm") }));
        f.stringFromDate(&NSDate::dateWithTimeIntervalSince1970(at as f64)).to_string()
    });
    format!("{} · {abs}", countdown(at))
}

// ---------- app ----------

fn main() {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let (tx, rx) = mpsc::channel();
    let _ = WAKE.set(tx);
    let cfg = usage::Config::load();
    std::thread::spawn(move || usage::run(cfg, rx));

    let panel: Retained<NotchPanel> = unsafe {
        msg_send![
            NotchPanel::alloc(mtm),
            initWithContentRect: NSRect::new(pt(0.0, 0.0), NSSize::new(1.0, 1.0)),
            styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            backing: NSBackingStoreType::Buffered,
            defer: false,
        ]
    };
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(false);
    panel.setHidesOnDeactivate(false);
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setLevel(NSMainMenuWindowLevel + 3);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );

    let view = NotchView::new(mtm);
    panel.setContentView(Some(&view));
    view.relayout(false);
    panel.orderFrontRegardless();

    // Displays added/removed/rearranged → re-hug the notch.
    let v = view.clone();
    let on_screens = RcBlock::new(move |_: NonNull<NSNotification>| v.relayout(false));
    let _screens_obs = unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSApplicationDidChangeScreenParametersNotification),
            None,
            None,
            &on_screens,
        )
    };
    // After sleep, refresh once the network is likely back.
    let on_wake = RcBlock::new(|_: NonNull<NSNotification>| after(3.0, wake_worker));
    let _wake_obs = unsafe {
        NSWorkspace::sharedWorkspace().notificationCenter().addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidWakeNotification),
            None,
            None,
            &on_wake,
        )
    };

    schedule_tick(view.clone());
    app.run();
}
