mod theme;
mod usage;

use std::cell::{Cell, OnceCell};
use std::ptr::NonNull;
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAccessibilityButtonRole, NSApplication, NSAutoresizingMaskOptions, NSApplicationActivationPolicy,
    NSApplicationDidChangeScreenParametersNotification, NSBackingStoreType, NSBezierPath, NSColor,
    NSEvent, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSMainMenuWindowLevel,
    NSMenu, NSMenuItem, NSPanel, NSScreen, NSStringDrawing, NSTrackingArea, NSTrackingAreaOptions,
    NSView, NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
    NSWorkspaceDidWakeNotification,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSDate, NSDateFormatter, NSDictionary, NSLocale, NSNotification,
    NSNotificationCenter, NSNumber, NSPoint, NSRect, NSSize, NSString, NSTimeZone, NSTimer, NSURL,
};
use objc2_core_graphics::CGPath;
use objc2_quartz_core::{
    kCAFillModeBoth, kCAMediaTimingFunctionEaseOut, CABasicAnimation, CACurrentMediaTime,
    CAMediaTiming, CAMediaTimingFunction, CAShapeLayer, CASpringAnimation, CATransaction,
};

use theme::{Color, Type, EXPANDED_W, FLARE, PAD};
use usage::{snapshot, Limit, Snapshot};

const USAGE_PAGE: &str = "https://claude.ai/settings/usage";
const HOVER_IN_DELAY: f64 = 0.12;
const HOVER_OUT_DELAY: f64 = 0.12;

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
    /// Layout the content is drawn in. Lags `expanded` while collapsing, so the
    /// expanded content can fade out before the collapsed content appears.
    shown_expanded: Cell<bool>,
    /// A morph is in flight; idle relayouts must not fight it.
    animating: Cell<bool>,
    anim_gen: Cell<u64>,
    /// The black blob behind the content (lives on a sibling background view).
    shape: OnceCell<Retained<CAShapeLayer>>,
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
    fn new(mtm: MainThreadMarker, shape: Retained<CAShapeLayer>) -> Retained<Self> {
        let ivars = Ivars::default();
        let _ = ivars.shape.set(shape);
        let this = Self::alloc(mtm).set_ivars(ivars);
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        this.setWantsLayer(true); // content opacity is animated on its layer
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

    /// Settled (collapsed, expanded) panel frames in screen coordinates.
    fn frames(&self) -> Option<(NSRect, NSRect)> {
        let g = Geometry::current(self.mtm())?;
        self.ivars().notch_h.set(g.notch_h);
        let snap = snapshot();
        // Collapsed ears hug their own content, so the two sides may differ in width.
        let (lw, rw) = Ears::new(snap.as_ref(), g.notch_h, false).widths();
        let rows = snap.map_or(2, |s| rows(&s).len().max(1)) as f64;
        let content = theme::HEADER_H + theme::OVERLINE_H + rows * theme::ROW_H + theme::BOTTOM_PAD;
        let (cw, ew) = (lw + g.notch_w + rw + 2.0 * FLARE, (g.notch_w + 2.0 * FLARE + 2.0 * lw.max(rw)).max(EXPANDED_W));
        let eh = g.notch_h + 10.0 + content;
        let notch_left = g.center_x - g.notch_w / 2.0;
        let collapsed = NSRect::new(pt(notch_left - FLARE - lw, g.top - g.notch_h), NSSize::new(cw, g.notch_h));
        let expanded = NSRect::new(pt(g.center_x - ew / 2.0, g.top - eh), NSSize::new(ew, eh));
        Some((collapsed, expanded))
    }

    fn radius(&self, expanded: bool) -> f64 {
        if expanded {
            theme::RADIUS_EXPANDED
        } else {
            (self.ivars().notch_h.get() * theme::RADIUS_COLLAPSED_RATIO).min(10.0)
        }
    }

    /// Dynamic-Island style morph: the window jumps to the larger frame, the black
    /// shape springs between notch-sized and panel-sized, content fades around it.
    fn set_expanded(&self, on: bool) {
        let iv = self.ivars();
        if iv.expanded.replace(on) == on {
            return;
        }
        let (Some(window), Some((collapsed, expanded)), Some(shape)) = (self.window(), self.frames(), iv.shape.get())
        else {
            return;
        };
        if reduce_motion() {
            iv.animating.set(false);
            self.settle();
            fade(self, 0.0, 1.0, 0.0, 0.18);
            return;
        }
        let gen = iv.anim_gen.get() + 1;
        iv.anim_gen.set(gen);
        iv.animating.set(true);

        // Morph inside the expanded frame; start from wherever the shape is now.
        let from = if window.frame() == expanded {
            presented_path(shape)
        } else {
            window.setFrame_display(expanded, false);
            Some(notch_path(local(collapsed, expanded), self.radius(false)))
        };
        let target = if on { expanded } else { collapsed };
        let to = notch_path(local(target, expanded), self.radius(on));
        let settle_in = morph(shape, from.as_deref(), &to, on);

        if on {
            iv.shown_expanded.set(true);
            self.setNeedsDisplay(true);
            fade(self, 0.0, 1.0, 0.1, 0.22);
        } else {
            fade(self, -1.0, 0.0, 0.0, 0.1);
        }
        let view = self.retain();
        after(settle_in, move || {
            let iv = view.ivars();
            if iv.anim_gen.get() != gen {
                return; // superseded by a newer morph
            }
            iv.animating.set(false);
            if !iv.expanded.get() {
                view.settle();
                fade(&view, 0.0, 1.0, 0.0, 0.16);
            }
        });
    }

    /// Snap window, shape and content to the current state without animation.
    fn settle(&self) {
        let iv = self.ivars();
        if iv.animating.get() {
            return;
        }
        let (Some(window), Some((collapsed, expanded)), Some(shape)) = (self.window(), self.frames(), iv.shape.get())
        else {
            return;
        };
        let on = iv.expanded.get();
        let frame = if on { expanded } else { collapsed };
        if frame != window.frame() {
            window.setFrame_display(frame, true);
        }
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        shape.setPath(Some(&notch_path(local(frame, frame), self.radius(on))));
        CATransaction::commit();
        iv.shown_expanded.set(on);
        self.setNeedsDisplay(true);
    }

    /// Fires every few seconds (randomised): blink and pick up new data.
    fn tick(&self) {
        let iv = self.ivars();
        let version = snapshot().map_or(0, |s| s.version);
        if iv.seen_version.replace(version) != version {
            iv.refreshing.set(false);
            self.update_accessibility();
        }
        // Row count (expanded) or ear text width (collapsed) may have changed.
        self.settle();
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
        let w = b.size.width;
        let iv = self.ivars();
        let notch_h = iv.notch_h.get().max(1.0);
        let expanded = iv.shown_expanded.get();
        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor());
        let snap = snapshot();
        let stale = snap.as_ref().is_some_and(|s| s.error.is_some() || usage::now() - s.fetched_at > 600);
        let mood = Mood::of(snap.as_ref());

        if !expanded {
            let ears = Ears::new(snap.as_ref(), notch_h, stale);
            // Every piece sits on one baseline, centred on the hero number's cap height.
            let baseline = notch_h / 2.0 + ears.session_ty.size * 0.36;
            let top = |ty: &Type| baseline - ty.size * 0.97;

            let [session, week] = &ears.sides;

            // Left ear: Clawd · [weekly left], anchored to the outer edge.
            let mut x = FLARE + theme::EAR_OUTER;
            critter(x + ears.sprite / 2.0, notch_h / 2.0, ears.sprite, scale, mood, iv.blink.get());
            x += ears.sprite + theme::BADGE_GAP;
            let bw = measure(&week.value, &ears.week_ty) + 2.0 * theme::BADGE_PAD_X;
            let bh = ears.week_ty.size + 2.0 * theme::BADGE_PAD_Y;
            theme::TRACK.ns().set();
            let br = (bh / 2.0).min(4.0);
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                NSRect::new(pt(x, (notch_h - bh) / 2.0), NSSize::new(bw, bh)),
                br,
                br,
            )
            .fill();
            text(&week.value, x + theme::BADGE_PAD_X, top(&ears.week_ty), &ears.week_ty, week.color, Align::Left);

            // Right ear: session left · session reset, anchored to the outer edge.
            let mut x = w - FLARE - theme::EAR_OUTER;
            if let Some(r) = &session.reset {
                x -= text(r, x, top(&ears.reset_ty), &ears.reset_ty, theme::TEXT_TERTIARY, Align::Right) + theme::RESET_GAP;
            }
            text(&session.value, x, top(&ears.session_ty), &ears.session_ty, session.color, Align::Right);
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

/// What the collapsed ears show: `[session, weekly]` → "Clawd [63%] | notch | 94% 4h42".
struct Ears {
    sides: [Side; 2],
    /// Session % — the hero number.
    session_ty: Type,
    reset_ty: Type,
    /// Weekly % in the badge — lighter, secondary.
    week_ty: Type,
    sprite: f64,
}

struct Side {
    value: String,
    color: Color,
    reset: Option<String>,
}

impl Ears {
    fn new(snap: Option<&Snapshot>, notch_h: f64, stale: bool) -> Self {
        let size = (notch_h / 2.9).clamp(9.5, theme::MICRO.size);
        let side = |l: Option<Limit>| match l {
            Some(l) => Side {
                value: pct(l.left()),
                color: if stale { theme::TEXT_TERTIARY } else { theme::remaining(l.left()) },
                reset: l.resets_at.filter(|&at| at > usage::now()).map(short_countdown),
            },
            None => Side { value: "–".to_string(), color: theme::TEXT_TERTIARY, reset: None },
        };
        let (five, week) = snap.map_or((None, None), |s| (s.five_hour, s.seven_day));
        Ears {
            sides: [side(five), side(week)],
            session_ty: Type { size: (size + 1.5).min(notch_h * 0.45), ..theme::MICRO_HERO },
            reset_ty: Type { size: size - 1.5, ..theme::MICRO_LABEL },
            week_ty: Type { size: size - 0.5, ..theme::MICRO_SECONDARY },
            sprite: ((notch_h - 14.0) * 24.0 / 15.0).clamp(16.0, 24.0),
        }
    }

    /// Content widths of the (left, right) ears, excluding the flares.
    /// Both ears get the wider one's width so the blob stays centred on the notch.
    fn widths(&self) -> (f64, f64) {
        let [session, week] = &self.sides;
        let pad = theme::EAR_OUTER + theme::EAR_INNER;
        let left = self.sprite + theme::BADGE_GAP + measure(&week.value, &self.week_ty) + 2.0 * theme::BADGE_PAD_X;
        let right = measure(&session.value, &self.session_ty)
            + session.reset.as_ref().map_or(0.0, |r| theme::RESET_GAP + measure(r, &self.reset_ty));
        let ear = pad + left.max(right);
        (ear, ear)
    }
}

/// Notch-shaped outline for `r` (view coords, y up): square top with concave flares,
/// rounded bottom. Every state uses the same element list so paths interpolate.
fn notch_path(r: NSRect, radius: f64) -> Retained<CGPath> {
    let (l, rt) = (r.origin.x, r.origin.x + r.size.width);
    let (b, t) = (r.origin.y, r.origin.y + r.size.height);
    let p = NSBezierPath::new();
    p.moveToPoint(pt(l, t));
    p.curveToPoint_controlPoint1_controlPoint2(pt(l + FLARE, t - FLARE), pt(l + FLARE, t), pt(l + FLARE, t));
    p.lineToPoint(pt(l + FLARE, b + radius));
    p.curveToPoint_controlPoint1_controlPoint2(pt(l + FLARE + radius, b), pt(l + FLARE, b), pt(l + FLARE, b));
    p.lineToPoint(pt(rt - FLARE - radius, b));
    p.curveToPoint_controlPoint1_controlPoint2(pt(rt - FLARE, b + radius), pt(rt - FLARE, b), pt(rt - FLARE, b));
    p.lineToPoint(pt(rt - FLARE, t - FLARE));
    p.curveToPoint_controlPoint1_controlPoint2(pt(rt, t), pt(rt - FLARE, t), pt(rt - FLARE, t));
    p.closePath();
    p.CGPath()
}

/// `r` (screen) expressed in the coordinates of a window whose frame is `win`.
fn local(r: NSRect, win: NSRect) -> NSRect {
    NSRect::new(pt(r.origin.x - win.origin.x, r.origin.y - win.origin.y), r.size)
}

fn cf_obj(path: &CGPath) -> &AnyObject {
    // CGPath is toll-free bridged to an Objective-C object.
    unsafe { &*(path as *const CGPath as *const AnyObject) }
}

/// The shape's path as currently on screen (mid-animation aware).
fn presented_path(shape: &CAShapeLayer) -> Option<Retained<CGPath>> {
    let presented = unsafe { shape.presentationLayer() }.and_then(|l| l.downcast::<CAShapeLayer>().ok());
    presented.and_then(|l| l.path()).or_else(|| shape.path())
}

/// Spring the shape from `from` to `to`; returns how long until it settles.
fn morph(shape: &CAShapeLayer, from: Option<&CGPath>, to: &CGPath, expanding: bool) -> f64 {
    let anim = CASpringAnimation::animationWithKeyPath(Some(ns_string!("path")));
    // Slightly underdamped on the way out (a hint of bounce), near-critical on the way in.
    anim.setMass(1.0);
    anim.setStiffness(if expanding { theme::SPRING_OPEN.0 } else { theme::SPRING_CLOSE.0 });
    anim.setDamping(if expanding { theme::SPRING_OPEN.1 } else { theme::SPRING_CLOSE.1 });
    unsafe {
        anim.setFromValue(from.map(cf_obj));
        anim.setToValue(Some(cf_obj(to)));
    }
    let settle = anim.settlingDuration().min(0.7);
    anim.setDuration(settle);
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    shape.setPath(Some(to));
    shape.addAnimation_forKey(&anim, Some(ns_string!("morph")));
    CATransaction::commit();
    settle
}

/// Animate a view's opacity. `from < 0` means "from whatever is on screen now".
fn fade(view: &NSView, from: f32, to: f32, delay: f64, duration: f64) {
    let Some(layer) = view.layer() else { return };
    let from = if from < 0.0 {
        unsafe { layer.presentationLayer() }.map_or(layer.opacity(), |p| p.opacity())
    } else {
        from
    };
    let anim = CABasicAnimation::animationWithKeyPath(Some(ns_string!("opacity")));
    unsafe {
        anim.setFromValue(Some(&NSNumber::numberWithFloat(from)));
        anim.setToValue(Some(&NSNumber::numberWithFloat(to)));
        anim.setFillMode(kCAFillModeBoth);
    }
    anim.setBeginTime(CACurrentMediaTime() + delay);
    anim.setDuration(duration.max(0.01));
    anim.setTimingFunction(Some(&CAMediaTimingFunction::functionWithName(unsafe { kCAMediaTimingFunctionEaseOut })));
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    view.setAlphaValue(to as f64);
    layer.addAnimation_forKey(&anim, Some(ns_string!("fade")));
    CATransaction::commit();
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
        let (center_x, notch_w, notch_h) = if inset > 0.0 {
            // Centre on the actual camera cutout, between the two usable top areas.
            let (l, r) = (screen.auxiliaryTopLeftArea(), screen.auxiliaryTopRightArea());
            let cut_l = f.origin.x + l.size.width;
            let cut_r = f.origin.x + f.size.width - r.size.width;
            ((cut_l + cut_r) / 2.0, cut_r - cut_l, inset)
        } else {
            // No notch: a small pill hanging from the menu bar.
            let vf = screen.visibleFrame();
            let h = (f.origin.y + f.size.height - (vf.origin.y + vf.size.height)).clamp(24.0, 32.0);
            (f.origin.x + f.size.width / 2.0, 0.0, h)
        };
        Some(Geometry { center_x, top: f.origin.y + f.size.height, notch_w, notch_h })
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

fn attrs(ty: &Type, color: Color) -> Retained<NSDictionary<NSString, AnyObject>> {
    let font = if ty.mono {
        NSFont::monospacedDigitSystemFontOfSize_weight(ty.size, (ty.weight)())
    } else {
        NSFont::systemFontOfSize_weight(ty.size, (ty.weight)())
    };
    let color = color.ns();
    unsafe {
        NSDictionary::from_slices(
            &[NSFontAttributeName, NSForegroundColorAttributeName],
            &[&*font as &AnyObject, &*color as &AnyObject],
        )
    }
}

fn measure(s: &str, ty: &Type) -> f64 {
    unsafe { NSString::from_str(s).sizeWithAttributes(Some(&attrs(ty, theme::TEXT_PRIMARY))).width }
}

/// Draws `s` and returns its width.
fn text(s: &str, x: f64, y: f64, ty: &Type, color: Color, align: Align) -> f64 {
    let ns = NSString::from_str(s);
    let attrs = attrs(ty, color);
    unsafe {
        let width = ns.sizeWithAttributes(Some(&attrs)).width;
        let x = match align {
            Align::Left => x,
            Align::Right => x - width,
        };
        ns.drawAtPoint_withAttributes(pt(x, y), Some(&attrs));
        width
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

    // Background view owns the black shape layer; the content view draws on top.
    let root = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
    let bg = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
    bg.setWantsLayer(true);
    let shape = CAShapeLayer::new();
    shape.setFillColor(Some(&theme::SURFACE.ns().CGColor()));
    shape.setAutoresizingMask(
        objc2_quartz_core::CAAutoresizingMask::LayerWidthSizable | objc2_quartz_core::CAAutoresizingMask::LayerHeightSizable,
    );
    if let Some(layer) = bg.layer() {
        shape.setFrame(layer.bounds());
        layer.addSublayer(&shape);
    }
    let view = NotchView::new(mtm, shape);
    for v in [&*bg, &**view] {
        v.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        root.addSubview(v);
    }
    panel.setContentView(Some(&root));
    view.settle();
    panel.orderFrontRegardless();

    // Displays added/removed/rearranged → re-hug the notch.
    let v = view.clone();
    let on_screens = RcBlock::new(move |_: NonNull<NSNotification>| v.settle());
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
