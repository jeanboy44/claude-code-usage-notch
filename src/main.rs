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
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBezierPath, NSColor,
    NSEvent, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSMainMenuWindowLevel, NSMenu, NSMenuItem, NSPanel, NSScreen,
    NSStringDrawing, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSDate, NSDateFormatter, NSDictionary, NSLocale, NSPoint, NSRect, NSSize, NSString, NSTimer,
};

use theme::{Color, Type, EXPANDED_W, FLARE, PAD, WING};
use usage::{Limit, Snapshot, STATE};

static WAKE: OnceLock<Sender<()>> = OnceLock::new();

#[derive(Default)]
struct Ivars {
    expanded: Cell<bool>,
    blink: Cell<bool>,
    seen_version: Cell<u64>,
    notch_h: Cell<f64>,
}

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
            self.set_expanded(true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _e: &NSEvent) {
            self.set_expanded(false);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _e: &NSEvent) {
            wake_worker();
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, e: &NSEvent) {
            let mtm = self.mtm();
            let menu = NSMenu::new(mtm);
            unsafe {
                let refresh = NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    ns_string!("Refresh Now"),
                    Some(sel!(refreshNow:)),
                    ns_string!(""),
                );
                refresh.setTarget(Some(self));
                menu.addItem(&refresh);
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                menu.addItem(&NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    ns_string!("Quit Notchi Usage"),
                    Some(sel!(terminate:)),
                    ns_string!("q"),
                ));
            }
            NSMenu::popUpContextMenu_withEvent_forView(&menu, e, self);
        }

        #[unsafe(method(refreshNow:))]
        fn refresh_now(&self, _sender: Option<&AnyObject>) {
            wake_worker();
        }
    }
);

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
        this
    }

    fn set_expanded(&self, on: bool) {
        if self.ivars().expanded.replace(on) != on {
            self.relayout(true);
        }
    }

    /// Recompute geometry (handles display changes) and resize the panel.
    fn relayout(&self, animate: bool) {
        let Some(window) = self.window() else { return };
        let Some(g) = Geometry::current(self.mtm()) else { return };
        self.ivars().notch_h.set(g.notch_h);
        let collapsed_w = g.notch_w + 2.0 * (WING + FLARE);
        let (w, h) = if self.ivars().expanded.get() {
            let rows = snapshot().map_or(2, |s| rows(&s).len().max(1)) as f64;
            let content = theme::HEADER_H + theme::OVERLINE_H + rows * theme::ROW_H + theme::BOTTOM_PAD;
            (collapsed_w.max(EXPANDED_W), g.notch_h + 10.0 + content)
        } else {
            (collapsed_w, g.notch_h)
        };
        let frame = NSRect::new(NSPoint::new(g.center_x - w / 2.0, g.top - h), NSSize::new(w, h));
        if frame != window.frame() {
            window.setFrame_display_animate(frame, true, animate);
        }
        self.setNeedsDisplay(true);
    }

    /// Called every few seconds: blink, pick up new data, keep countdown fresh.
    fn tick(&self) {
        let iv = self.ivars();
        let version = snapshot().map_or(0, |s| s.version);
        if iv.seen_version.replace(version) != version && iv.expanded.get() {
            self.relayout(false); // row count may have changed
        }
        if !iv.expanded.get() {
            self.relayout(false); // cheap no-op unless the screen changed
        }
        iv.blink.set(true);
        self.setNeedsDisplay(true);
        let view = self.retain();
        let unblink = RcBlock::new(move |_: NonNull<NSTimer>| {
            view.ivars().blink.set(false);
            view.setNeedsDisplay(true);
        });
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.14, false, &unblink) };
    }

    fn draw(&self) {
        let b = self.bounds();
        let (w, h) = (b.size.width, b.size.height);
        let iv = self.ivars();
        let notch_h = iv.notch_h.get().max(1.0);
        let expanded = iv.expanded.get();
        let snap = snapshot();
        let five = snap.as_ref().and_then(|s| s.five_hour);
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

        if !expanded {
            // Left ear: critter. Right ear: % left in the 5h window.
            let s = ((notch_h - 12.0) * 24.0 / 15.0).clamp(18.0, 30.0);
            critter(FLARE + WING / 2.0 + 4.0, notch_h / 2.0, s, mood, iv.blink.get());
            // Right ear: session (5h) and weekly (7d) left, stacked.
            let week = snap.as_ref().and_then(|s| s.seven_day);
            let size = (notch_h / 3.0).clamp(9.0, theme::MICRO.size);
            let lh = size * 1.22;
            let y0 = (notch_h - 2.0 * lh) / 2.0;
            let (label_x, num_r) = (w - FLARE - WING + 4.0, w - FLARE - 9.0);
            for (i, (tag, l)) in [("5h", five), ("7d", week)].into_iter().enumerate() {
                let y = y0 + i as f64 * lh;
                let (num, color) = match l {
                    Some(l) => (format!("{:.0}%", left_pct(l)), theme::remaining(left_pct(l))),
                    None => ("–".to_string(), theme::TEXT_TERTIARY),
                };
                let micro_label = Type { size: size - 1.5, ..theme::MICRO_LABEL };
                text(tag, label_x, y + 1.0, &micro_label, theme::TEXT_TERTIARY, Align::Left);
                text(&num, num_r, y, &Type { size, ..theme::MICRO }, color, Align::Right);
            }
            return;
        }

        // Header: critter + mood line + status line.
        let top = notch_h + 10.0;
        critter(FLARE + PAD + 18.0, top + 18.0, 36.0, mood, iv.blink.get());
        let x = FLARE + PAD + 48.0;
        text(mood.title(), x, top + 4.0, &theme::TITLE, theme::TEXT_PRIMARY, Align::Left);
        let sub = match snap.as_ref() {
            None => "Loading…".to_string(),
            Some(s) => match &s.error {
                Some(e) => e.clone(),
                None => format!("Updated {} · click to refresh", ago(usage::now() - s.fetched_at)),
            },
        };
        text(&sub, x, top + 23.0, &theme::CAPTION, theme::TEXT_SECONDARY, Align::Left);

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
            let left_pct = left_pct(l);
            let c = theme::remaining(left_pct);
            text(name, left, y, &theme::LABEL, theme::TEXT_SECONDARY, Align::Left);
            // The bar shows what's LEFT, so it shrinks as you use Claude — same as the number.
            let by = y + 4.0;
            pill(bar_x, by, bar_w, theme::BAR_H, theme::TRACK);
            if left_pct > 0.0 {
                pill(bar_x, by, (bar_w * left_pct / 100.0).max(theme::BAR_H), theme::BAR_H, c);
            }
            text(&format!("{left_pct:.0}%"), right - theme::COL_PCT_R, y - 1.0, &theme::NUMERAL, c, Align::Right);
            let reset = l.resets_at.map_or("—".to_string(), reset_label);
            text(&reset, right, y, &theme::CAPTION, theme::TEXT_SECONDARY, Align::Right);
        }
    }
}

fn wake_worker() {
    if let Some(tx) = WAKE.get() {
        let _ = tx.send(());
    }
}

fn left_pct(l: Limit) -> f64 {
    (100.0 - l.used).clamp(0.0, 100.0)
}

fn snapshot() -> Option<Snapshot> {
    STATE.lock().ok()?.clone()
}

fn rows(s: &Snapshot) -> Vec<(&'static str, Limit)> {
    [("Session", s.five_hour), ("Weekly", s.seven_day), ("Opus", s.seven_day_opus), ("Sonnet", s.seven_day_sonnet)]
        .into_iter()
        .filter_map(|(n, l)| Some((n, l?)))
        .collect()
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
        let notched = screens.iter().find(|s| s.safeAreaInsets().top > 0.0);
        let screen = notched.or_else(|| NSScreen::mainScreen(mtm))?;
        let f = screen.frame();
        let inset = screen.safeAreaInsets().top;
        let (notch_w, notch_h) = if inset > 0.0 {
            let l = screen.auxiliaryTopLeftArea();
            let r = screen.auxiliaryTopRightArea();
            (f.size.width - l.size.width - r.size.width, inset)
        } else {
            // No notch: a small pill hanging from the menu bar.
            let vf = screen.visibleFrame();
            (0.0, (f.origin.y + f.size.height - (vf.origin.y + vf.size.height)).clamp(22.0, 32.0))
        };
        Some(Geometry { center_x: f.origin.x + f.size.width / 2.0, top: f.origin.y + f.size.height, notch_w, notch_h })
    }
}

// ---------- the critter ----------

#[derive(Clone, Copy, PartialEq)]
enum Mood {
    Sleepy,
    Happy,
    Busy,
    Careful,
    Dizzy,
}

impl Mood {
    fn of(s: Option<&Snapshot>) -> Self {
        match s.and_then(|s| s.five_hour.map(left_pct)) {
            None => Mood::Sleepy,
            Some(l) if l > 50.0 => Mood::Happy,
            Some(l) if l > 30.0 => Mood::Busy,
            Some(l) if l > 10.0 => Mood::Careful,
            Some(_) => Mood::Dizzy,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Mood::Sleepy => "zzz… waiting for data",
            Mood::Happy => "Plenty of room ♪",
            Mood::Busy => "Busy crafting!",
            Mood::Careful => "Let's pace ourselves…",
            Mood::Dizzy => "Running on fumes @_@",
        }
    }
}

/// Clawd, drawn from the official Claude Code icon (24×24 viewBox, body spans y 5..20).
/// `s` is the sprite width; the sprite is 24×15 units, centred on (cx, cy).
fn critter(cx: f64, cy: f64, s: f64, mood: Mood, blink: bool) {
    let u = s / 24.0;
    let (x0, y0) = (cx - 12.0 * u, cy - 7.5 * u);
    let rect = |x: f64, y: f64, w: f64, h: f64| {
        NSBezierPath::fillRect(NSRect::new(pt(x0 + x * u, y0 + (y - 5.0) * u), NSSize::new(w * u, h * u)))
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
        s => format!("{}h ago", s / 3600),
    }
}

/// "1h 23m · 14:30" / "2d 5h · Fri 09:00"
fn reset_label(at: i64) -> String {
    let left = (at - usage::now()).max(0);
    let (d, h, m) = (left / 86400, left % 86400 / 3600, left % 3600 / 60);
    let rel = match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    };
    thread_local! {
        static FMT: Retained<NSDateFormatter> = {
            let f = NSDateFormatter::new();
            f.setLocale(Some(&NSLocale::initWithLocaleIdentifier(NSLocale::alloc(), ns_string!("en_US_POSIX"))));
            f
        };
    }
    let abs = FMT.with(|f| {
        f.setDateFormat(Some(if d == 0 { ns_string!("HH:mm") } else { ns_string!("EEE HH:mm") }));
        f.stringFromDate(&NSDate::dateWithTimeIntervalSince1970(at as f64)).to_string()
    });
    format!("{rel} · {abs}")
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

    let v = view.clone();
    let tick = RcBlock::new(move |_: NonNull<NSTimer>| v.tick());
    let timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(4.0, true, &tick) };
    timer.setTolerance(1.0);

    app.run();
}
