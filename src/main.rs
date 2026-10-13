mod render;
mod theme;
mod usage;

use std::cell::{Cell, OnceCell, RefCell};
use std::ptr::NonNull;
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAccessibilityButtonRole, NSApplication, NSAutoresizingMaskOptions, NSControlStateValueOff,
    NSControlStateValueOn, NSApplicationActivationPolicy,
    NSAffineTransformNSAppKitAdditions, NSApplicationDidChangeScreenParametersNotification,
    NSBackingStoreType, NSBezierPath, NSColor, NSGraphicsContext,
    NSEvent, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSMainMenuWindowLevel,
    NSMenu, NSMenuItem, NSPanel, NSScreen, NSStringDrawing, NSTrackingArea, NSTrackingAreaOptions,
    NSView, NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
    NSWorkspaceDidWakeNotification,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSAffineTransform, NSDate, NSDateFormatter, NSDictionary, NSLocale, NSNotification,
    NSNotificationCenter, NSNumber, NSPoint, NSRect, NSSize, NSString, NSTimeZone, NSTimer, NSURL,
};
use objc2_core_graphics::CGPath;
use objc2_service_management::{SMAppService, SMAppServiceStatus};
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
/// `--preview-week=N` pins the weekly % left, to see Clawd's teeter without burning budget.
static PREVIEW_WEEK: OnceLock<Option<f64>> = OnceLock::new();
/// `--demo` sweeps the weekly % left from 100 to 0 every 40 s (walk, tremble, teeter).
static DEMO: OnceLock<bool> = OnceLock::new();

/// A walk from `from` to `to` (pt along the bar) starting at `start` (media time).
#[derive(Clone, Copy)]
struct Walk {
    from: f64,
    to: f64,
    start: f64,
    duration: f64,
}

impl Default for Walk {
    fn default() -> Self {
        Walk { from: f64::NAN, to: f64::NAN, start: 0.0, duration: 0.0 }
    }
}

impl Walk {
    /// (position, still walking) at media time `now`, eased in and out.
    fn at(self, now: f64) -> (f64, bool) {
        if self.duration <= 0.0 {
            return (self.to, false);
        }
        let t = ((now - self.start) / self.duration).clamp(0.0, 1.0);
        let ease = t * t * (3.0 - 2.0 * t);
        (self.from + (self.to - self.from) * ease, t < 1.0)
    }
}

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
    /// Redraw timer while Clawd trembles or teeters on the weekly bar.
    teeter: RefCell<Option<Retained<NSTimer>>>,
    /// End of the current tremble burst (CACurrentMediaTime), in the yellow band.
    tremble_until: Cell<f64>,
    /// Clawd's walk along the weekly bar, as offsets from the bar's left end.
    walk: Cell<Walk>,
    /// Fell off at 0%: when he let go (then he pops back up, head only).
    gone: Cell<bool>,
    gone_start: Cell<f64>,
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
        fn draw_rect(&self, dirty: NSRect) {
            self.draw(dirty);
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

        #[unsafe(method(toggleLaunchAtLogin:))]
        fn toggle_launch_at_login(&self, _sender: Option<&AnyObject>) {
            set_launch_at_login(!launch_at_login());
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
        // Launch at Login (only meaningful for the installed .app).
        unsafe {
            let login = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Launch at Login"),
                running_as_app().then_some(sel!(toggleLaunchAtLogin:)),
                ns_string!(""),
            );
            login.setTarget(Some(self));
            login.setEnabled(running_as_app());
            login.setState(if launch_at_login() { NSControlStateValueOn } else { NSControlStateValueOff });
            menu.addItem(&login);
        }
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
        let (lw, _) = Ears::new(snap.as_ref(), g.notch_h, false).widths();
        // Nothing on the right: end the blob inside the camera cutout so our corner and
        // flare hide behind the hardware and only the notch's own curve shows.
        let rw = -(g.notch_w / 2.0);
        let rows = snap.map_or(2, |s| rows(&s).len().max(1)) as f64;
        let (cw, ew) = (lw + g.notch_w + rw + 2.0 * FLARE, (g.notch_w + 2.0 * FLARE + 2.0 * lw.max(rw)).max(EXPANDED_W));
        let eh = expanded_height(g.notch_h, rows);
        let notch_left = g.center_x - g.notch_w / 2.0;
        let collapsed = NSRect::new(pt(notch_left - FLARE - lw, g.top - g.notch_h), NSSize::new(cw, g.notch_h));
        let expanded = NSRect::new(pt(g.center_x - ew / 2.0, g.top - eh), NSSize::new(ew, eh));
        Some((collapsed, expanded))
    }

    fn radius(&self, expanded: bool) -> f64 {
        if expanded {
            theme::RADIUS_EXPANDED
        } else {
            theme::RADIUS_COLLAPSED.min(self.ivars().notch_h.get() * theme::RADIUS_COLLAPSED_MAX_RATIO)
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

        // Morph inside a panel-sized window, starting from whatever is on screen. Only a
        // collapsed-size window is grown first; an already-large one (open, or mid-morph)
        // is kept as is, so text width changes can't make us restart from the wrong shape.
        let current = window.frame();
        let (morph_frame, from) = if current.size.height > collapsed.size.height + 0.5 {
            (current, presented_path(shape))
        } else {
            // Freshly grown: the presentation layer still holds the old small-window path
            // (which would sit at the bottom), so start explicitly from the notch at the top.
            window.setFrame_display(expanded, false);
            (expanded, Some(notch_path(local(collapsed, expanded), self.radius(false))))
        };
        let target = if on { expanded } else { collapsed };
        let to = notch_path(local(target, morph_frame), self.radius(on));
        let settle_in = morph(shape, from.as_deref(), &to, on);

        if on {
            iv.shown_expanded.set(true);
            self.update_teeter();
            self.setNeedsDisplay(true);
            fade(self, 0.0, 1.0, 0.1, 0.22);
        } else {
            fade(self, -1.0, 0.0, 0.0, 0.07);
        }
        let view = self.retain();
        after(settle_in, move || {
            let iv = view.ivars();
            if iv.anim_gen.get() != gen {
                return; // superseded by a newer morph
            }
            iv.animating.set(false);
            let collapsed = !iv.expanded.get();
            view.settle(); // snap the window to the exact final frame
            if collapsed {
                fade(&view, 0.0, 1.0, 0.0, 0.1);
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
        self.update_teeter();
    }

    /// Clawd's displayed offset along the bar for a new `target`: starts a walk when the
    /// spot he should stand on moves (data changed), else continues the current one.
    /// Returns (offset, walking, direction −1/0/1).
    fn walk_to(&self, target: f64) -> (f64, bool, f64) {
        let iv = self.ivars();
        let now = media_now();
        let mut walk = iv.walk.get();
        if walk.to.is_nan() || reduce_motion() {
            walk = Walk { from: target, to: target, start: now, duration: 0.0 };
        } else if (target - walk.to).abs() > 1e-6 {
            let (here, _) = walk.at(now);
            let distance = (target - here).abs();
            walk = if distance < theme::WALK_MIN {
                Walk { from: target, to: target, start: now, duration: 0.0 }
            } else {
                let duration = (distance / theme::WALK_SPEED).clamp(0.4, 2.5);
                Walk { from: here, to: target, start: now, duration }
            };
            iv.walk.set(walk);
            self.update_teeter(); // make sure frames keep coming while he walks
        }
        iv.walk.set(walk);
        let (x, walking) = walk.at(now);
        (x, walking, (walk.to - walk.from).signum())
    }

    /// Run a light redraw timer only while Clawd is moving: always when teetering (red),
    /// during short tremble bursts (yellow), never when calm.
    fn update_teeter(&self) {
        let iv = self.ivars();
        let stance = {
            let ears = Ears::new(snapshot().as_ref(), iv.notch_h.get(), false);
            perch(ears.week_left).1
        };
        let now = media_now();
        let walking = iv.walk.get().at(now).1;
        let moving = walking
            || match stance {
                Stance::Teeter => true,
                Stance::Gone => now < iv.gone_start.get() + theme::GONE_ANIM,
                Stance::Tremble => now < iv.tremble_until.get(),
                Stance::Calm => false,
            };
        let need = !iv.shown_expanded.get() && !reduce_motion() && moving;
        let mut timer = iv.teeter.borrow_mut();
        if need && timer.is_none() {
            let view = self.retain();
            // Only Clawd's area is invalidated, so the session text isn't re-laid out each frame.
            let block = RcBlock::new(move |_: NonNull<NSTimer>| {
                let h = view.bounds().size.height;
                view.setNeedsDisplayInRect(NSRect::new(pt(0.0, 0.0), NSSize::new(session_text_x(), h)));
                let iv = view.ivars();
                let now = media_now();
                if iv.tremble_until.get() <= now && !iv.walk.get().at(now).1 {
                    view.update_teeter(); // burst/walk over → stops unless teetering
                }
            });
            let t = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0 / 12.0, true, &block) };
            t.setTolerance(0.01);
            *timer = Some(t);
        } else if !need {
            if let Some(t) = timer.take() {
                t.invalidate();
            }
        }
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
        // Yellow band: a short shiver every tick.
        iv.tremble_until.set(media_now() + theme::TREMBLE_BURST);
        self.update_teeter();
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

    fn draw(&self, dirty: NSRect) {
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

            let session = &ears.session;

            // Left ear: Clawd standing on the weekly bar (7 day-segments, lit = weekly left).
            let sprite = ears.sprite;
            let u = sprite / 24.0;
            let clawd_h = sprite * 15.0 / 24.0;
            let bar_y = (notch_h - clawd_h - theme::WEEK_BAR_H) / 2.0 + clawd_h;
            let bar_x = FLARE + theme::EAR_OUTER;
            week_bar(bar_x, bar_y, ears.week_left, ears.week_color);
            let now = media_now();
            let motion = !reduce_motion();
            let (segments, stance) = perch(ears.week_left);
            let stand_cy = bar_y - clawd_h / 2.0;

            NSGraphicsContext::saveGraphicsState_class();
            // Nothing may spill outside the black blob (e.g. near the rounded corner).
            collapsed_outline(w, notch_h, self.radius(false)).addClip();
            let rotate = |pivot: (f64, f64), degrees: f64, shift: f64| {
                if degrees != 0.0 || shift != 0.0 {
                    let t = NSAffineTransform::transform();
                    t.translateXBy_yBy(pivot.0 + shift, pivot.1);
                    t.rotateByDegrees(degrees);
                    t.translateXBy_yBy(-pivot.0, -pivot.1);
                    t.concat();
                }
            };
            if stance == Stance::Gone {
                // 0%: he lets go, drops out of sight, then pops back up — only his head shows
                // above the bottom edge. Kept clear of the rounded corner.
                let (was_x, _) = iv.walk.get().at(now);
                let from_x = if was_x.is_nan() { bar_x } else { bar_x + was_x };
                // Head pokes up to just under the bar, eyes showing; the rest is below the edge.
                let peek_cy = bar_y + theme::WEEK_BAR_H + 0.5 + 7.5 * u;
                let peek = ((bar_x + 12.0 * u).max(FLARE + self.radius(false) + 12.0 * u), peek_cy);
                if !iv.gone.replace(true) {
                    iv.gone_start.set(if was_x.is_nan() { f64::NEG_INFINITY } else { now });
                    self.update_teeter();
                }
                let t = if motion { now - iv.gone_start.get() } else { f64::INFINITY };
                let hidden = notch_h + 9.0 * u;
                let start_y = stand_cy; // where he was teetering
                let hang_x = from_x;
                let cy = if t < 0.25 {
                    let k = t / 0.25;
                    start_y + (hidden - start_y) * k * k
                } else if t < 0.8 {
                    hidden
                } else if t < theme::GONE_ANIM {
                    let k = (t - 0.8) / (theme::GONE_ANIM - 0.8);
                    hidden + (peek.1 - hidden) * (1.0 - (1.0 - k) * (1.0 - k))
                } else {
                    peek.1
                };
                let cx = if t < 0.25 { hang_x + (peek.0 - hang_x) * (t / 0.25) } else { peek.0 };
                critter(cx, cy, sprite, scale, Mood::Busy, Pose { blink: iv.blink.get(), ..Pose::default() });
            } else if stance == Stance::Teeter {
                iv.gone.set(false);
                // Red: walk to the very edge of what's left, then teeter over the drop —
                // leaning and wobbling, wide eyes, open mouth, sweat, flapping arms.
                let (offset, walking, dir) = self.walk_to(lit_edge(ears.week_left));
                let cx = bar_x + offset;
                if !walking {
                    rotate((cx, bar_y), teeter_degrees(ears.week_left), 0.0);
                }
                let step = walking.then(|| (now / 0.14) as i64 % 2 == 0);
                let pose = Pose { panic: if walking { 0.0 } else { 1.0 }, step, look: dir * 0.6, ..Pose::default() };
                critter(cx, stand_cy, sprite, scale, mood, pose);
            } else {
                iv.gone.set(false);
                // Walk to the middle of the segments that are left (only moves when a whole
                // segment goes), then tremble there if yellow.
                let (offset, walking, dir) = self.walk_to(segments_centre(segments));
                let cx = bar_x + offset;
                let shiver = stance == Stance::Tremble && !walking && motion && now < iv.tremble_until.get();
                if shiver {
                    let wobble = (now * std::f64::consts::TAU * 13.0).sin();
                    rotate((cx, bar_y), 1.6 * wobble, 0.5 * wobble);
                }
                let face = if stance == Stance::Tremble { Mood::Careful } else { mood }; // sweat drop
                // Walking: alternate leg pairs every 0.14 s, a tiny bob, eyes toward the way he goes.
                let step = walking.then(|| (now / 0.14) as i64 % 2 == 0);
                let bob = if step == Some(true) { -0.4 } else { 0.0 };
                let pose = Pose { blink: iv.blink.get() && !walking, step, look: dir * 0.6, ..Pose::default() };
                critter(cx, stand_cy + bob, sprite, scale, face, pose);
            }
            NSGraphicsContext::restoreGraphicsState_class();

            // Then session left · session reset, still left of the notch.
            let x = session_text_x();
            if dirty.origin.x + dirty.size.width <= x {
                return; // teeter frame: only Clawd needs repainting
            }
            let x = x + text(&session.value, x, top(&ears.session_ty), &ears.session_ty, session.color, Align::Left);
            if let Some(r) = &session.reset {
                text(r, x + theme::RESET_GAP, top(&ears.reset_ty), &ears.reset_ty, theme::TEXT_TERTIARY, Align::Left);
            }
            let _ = w;
            return;
        }

        // Header: Clawd + mood line + status line.
        let top = notch_h + 10.0;
        critter(FLARE + PAD + 18.0, top + 18.0, 36.0, scale, mood, Pose { blink: iv.blink.get(), ..Pose::default() });
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

        let gy = oy + theme::OVERLINE_H + rows(&snap).len() as f64 * theme::ROW_H + theme::GRAPH_GAP;
        history_graph(&snap, left, gy, right - left, stale);
    }
}

/// "THIS SESSION" hairline graph of session % left across the current 5 h window,
/// with a faint even-pace diagonal from 100% to 0% and hour gridlines.
fn history_graph(snap: &Snapshot, x: f64, y: f64, w: f64, stale: bool) {
    let session = snap.five_hour;
    let color = match session {
        Some(l) if !stale => theme::remaining(l.left()),
        _ => theme::TEXT_TERTIARY,
    };
    text("THIS SESSION", x, y, &theme::OVERLINE, theme::TEXT_TERTIARY, Align::Left);
    // Axis times are local; say which zone.
    let tz = NSTimeZone::systemTimeZone();
    if let Some(abbr) = tz.abbreviation() {
        text(&abbr.to_string(), x + w, y, &theme::OVERLINE, theme::TEXT_TERTIARY, Align::Right);
    }

    let gy = y + theme::OVERLINE_H;
    let h = theme::GRAPH_H;
    let now = usage::now();
    let t1 = session.and_then(|l| l.resets_at).filter(|&at| at > now).unwrap_or(now);
    let t0 = t1 - 5 * 3600;
    let px = |t: i64| x + (t - t0) as f64 / (t1 - t0) as f64 * w;
    let py = |left: f64| gy + (1.0 - left.clamp(0.0, 100.0) / 100.0) * h;
    let line = |pts: &[(f64, f64)], color: Color, width: f64| {
        if pts.len() < 2 {
            return;
        }
        let p = NSBezierPath::new();
        p.setLineWidth(width);
        p.setLineJoinStyle(objc2_app_kit::NSLineJoinStyle::Round);
        p.moveToPoint(pt(pts[0].0, pts[0].1));
        for &(a, b) in &pts[1..] {
            p.lineToPoint(pt(a, b));
        }
        color.ns().set();
        p.stroke();
    };

    // Grid: 50 / 100 % faint, 0 % is the x-axis; even-pace guide.
    for left in [50.0, 100.0] {
        line(&[(x, py(left)), (x + w, py(left))], theme::GRAPH_GRID, 0.5);
    }
    line(&[(px(t0), py(100.0)), (px(t1), py(0.0))], theme::GRAPH_PACE, 0.5);
    let axis_y = py(0.0);
    line(&[(x, axis_y), (x + w, axis_y)], theme::GRAPH_AXIS, 0.5);

    // Hour ticks on *local* hour boundaries (handles :30 / :45 offset zones), labelled HH:mm.
    let offset = tz.secondsFromGMT() as i64;
    let mut tick = ((t0 + offset) as f64 / 3600.0).ceil() as i64 * 3600 - offset;
    while tick <= t1 {
        let tx = px(tick);
        line(&[(tx, gy), (tx, axis_y)], theme::GRAPH_GRID, 0.5);
        line(&[(tx, axis_y), (tx, axis_y + 3.0)], theme::GRAPH_AXIS, 0.5);
        let label = clock(tick);
        let lw = measure(&label, &theme::AXIS);
        let lx = (tx - lw / 2.0).clamp(x, x + w - lw);
        text(&label, lx, axis_y + 4.0, &theme::AXIS, theme::TEXT_TERTIARY, Align::Left);
        tick += 3600;
    }
    // Where we are in the window.
    if now > t0 && now < t1 {
        let nx = px(now);
        line(&[(nx, gy), (nx, axis_y)], theme::GRAPH_NOW, 0.5);
    }

    let samples: Vec<_> = usage::history().into_iter().filter(|s| s.t >= t0).collect();
    if samples.len() < 2 {
        text("Fills in as this session goes", x + w / 2.0 - 64.0, gy + h / 2.0 - 7.0, &theme::CAPTION, theme::TEXT_TERTIARY, Align::Left);
    }
    // Gaps > 30 min (app not running) aren't joined.
    let mut runs: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut last_t = i64::MIN;
    for s in &samples {
        let p = (px(s.t), py(100.0 - s.session as f64));
        match runs.last_mut() {
            Some(run) if s.t - last_t <= 1800 => run.push(p),
            _ => runs.push(vec![p]),
        }
        last_t = s.t;
    }
    for run in &runs {
        line(run, color, theme::GRAPH_LINE);
    }
    if let Some(&(cx, cy)) = runs.last().and_then(|r| r.last()) {
        color.ns().set();
        NSBezierPath::bezierPathWithOvalInRect(NSRect::new(pt(cx - 1.75, cy - 1.75), NSSize::new(3.5, 3.5))).fill();
    }
}

/// What the collapsed ears show: "Clawd on the weekly bar | notch | 94% 4h42".
struct Ears {
    session: Side,
    /// Weekly % left (100 when unknown) and the colour its lit segments use.
    week_left: f64,
    week_color: Color,
    /// Session % — the hero number.
    session_ty: Type,
    reset_ty: Type,
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
        let (five, week) = snap.map_or((None, None), |s| (s.five_hour, s.seven_day));
        let session = match five {
            Some(l) => Side {
                value: pct(l.left()),
                color: if stale { theme::TEXT_TERTIARY } else { theme::remaining(l.left()) },
                reset: l.resets_at.filter(|&at| at > usage::now()).map(short_countdown),
            },
            None => Side { value: "–".to_string(), color: theme::TEXT_TERTIARY, reset: None },
        };
        let demo = DEMO.get().copied().unwrap_or(false).then(|| 100.0 - (usage::now() % 40) as f64 * 2.5);
        let week = match PREVIEW_WEEK.get().copied().flatten().or(demo) {
            Some(left) => Some(Limit { used: 100.0 - left, resets_at: None }),
            None => week,
        };
        let (week_left, week_color) = match week {
            Some(l) if !stale => (l.left(), theme::remaining(l.left())),
            Some(l) => (l.left(), theme::TEXT_TERTIARY),
            None => (100.0, theme::TRACK),
        };
        Ears {
            session,
            week_left,
            week_color,
            session_ty: Type { size: (size + 1.5).min(notch_h * 0.45), ..theme::MICRO_HERO },
            reset_ty: Type { size: size - 1.5, ..theme::MICRO_LABEL },
            sprite: ((notch_h - theme::WEEK_BAR_H - 12.0) * 24.0 / 15.0).clamp(16.0, 22.0),
        }
    }

    /// Content widths of the (left, right) ears, excluding the flares.
    /// Everything lives left of the notch; the right side ends at the cutout.
    fn widths(&self) -> (f64, f64) {
        let session = measure(&self.session.value, &self.session_ty)
            + self.session.reset.as_ref().map_or(0.0, |r| theme::RESET_GAP + measure(r, &self.reset_ty));
        (session_text_x() - FLARE + session + theme::EAR_INNER, 0.0)
    }
}

/// Where the session text starts in the collapsed panel (after Clawd's bar).
fn session_text_x() -> f64 {
    FLARE + theme::EAR_OUTER + theme::week_bar_w() + theme::SESSION_GAP
}

/// Seven day-segments; the lit length is the weekly budget left, draining right → left.
fn week_bar(x: f64, y: f64, left: f64, color: Color) {
    let units = 7.0 * left.clamp(0.0, 100.0) / 100.0;
    let (seg, gap, h) = (theme::WEEK_SEG_W, theme::WEEK_SEG_GAP, theme::WEEK_BAR_H);
    for i in 0..7 {
        let sx = x + i as f64 * (seg + gap);
        let seg_rect = |w: f64| NSRect::new(pt(sx, y), NSSize::new(w, h));
        theme::TRACK.ns().set();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(seg_rect(seg), 1.0, 1.0).fill();
        let lit = (units - i as f64).clamp(0.0, 1.0);
        if lit > 0.0 {
            color.ns().set();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(seg_rect(seg * lit), 1.0, 1.0).fill();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Stance {
    /// Green: standing calmly in the middle of what's left.
    Calm,
    /// Yellow: still in the middle, shivering now and then.
    Tremble,
    /// Red: pushed to the very edge, about to fall.
    Teeter,
    /// 0%: fell off entirely; only his head peeks up from the bottom.
    Gone,
}

/// (segments still lit, stance) for a weekly % left. Clawd only moves when the
/// segment count changes; the stance follows the status colour bands.
fn perch(left: f64) -> (usize, Stance) {
    let units = 7.0 * left.clamp(0.0, 100.0) / 100.0;
    let segments = (units.ceil() as usize).min(7);
    let stance = match left {
        l if l >= theme::BAND_GREEN => Stance::Calm,
        l if l >= theme::BAND_YELLOW => Stance::Tremble,
        l if l >= 0.5 => Stance::Teeter,
        _ => Stance::Gone,
    };
    (segments, stance)
}

/// Lean over the drop and wobble — harder and faster the closer to empty.
fn teeter_degrees(left: f64) -> f64 {
    let severity = (1.0 - left / theme::BAND_YELLOW).clamp(0.3, 1.0);
    let lean = 9.0 + 9.0 * severity;
    if reduce_motion() {
        return lean;
    }
    let period = 1.1 - 0.45 * severity;
    let phase = media_now() * std::f64::consts::TAU / period;
    // Mostly leaning toward the drop, with quick catches back.
    lean + (5.0 + 6.0 * severity) * phase.sin()
}

/// The collapsed blob's outline in this (flipped) view: square top with flares,
/// rounded bottom — used to clip Clawd so he never draws outside the black.
fn collapsed_outline(w: f64, h: f64, r: f64) -> Retained<NSBezierPath> {
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
    p
}

/// Offset of the middle of the first `n` segments.
fn segments_centre(n: usize) -> f64 {
    let span = n.max(1) as f64 * (theme::WEEK_SEG_W + theme::WEEK_SEG_GAP) - theme::WEEK_SEG_GAP;
    span / 2.0
}

/// Offset of the end of the lit part of the bar.
fn lit_edge(left: f64) -> f64 {
    let (seg, gap) = (theme::WEEK_SEG_W, theme::WEEK_SEG_GAP);
    let units = 7.0 * left.clamp(0.0, 100.0) / 100.0;
    if units >= 7.0 {
        return theme::week_bar_w();
    }
    let full = units.floor();
    full * (seg + gap) + (units - full) * seg
}

/// Notch-shaped outline for `r` (view coords, y up): square top with concave flares,
/// rounded bottom. Every state uses the same element list so paths interpolate.
fn notch_path(r: NSRect, radius: f64) -> Retained<CGPath> {
    notch_bezier(r, radius).CGPath()
}

fn notch_bezier(r: NSRect, radius: f64) -> Retained<NSBezierPath> {
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
    p
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
    // Closing is critically damped, so it's visually done well before `settlingDuration`.
    let settle = anim.settlingDuration().min(if expanding { 0.7 } else { 0.26 });
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

thread_local!(static CLOCK: Cell<Option<f64>> = const { Cell::new(None) });

/// Animation clock: Core Animation's media time, or a pinned time when rendering
/// README frames offline.
fn media_now() -> f64 {
    CLOCK.with(Cell::get).unwrap_or_else(|| CACurrentMediaTime())
}

/// Height of the expanded panel for `rows` limit rows.
fn expanded_height(notch_h: f64, rows: f64) -> f64 {
    let graph = theme::GRAPH_GAP + theme::OVERLINE_H + theme::GRAPH_H + theme::GRAPH_AXIS_H;
    notch_h + 10.0 + theme::HEADER_H + theme::OVERLINE_H + rows * theme::ROW_H + graph + theme::BOTTOM_PAD
}

/// Launch at Login only works for a bundled .app (SMAppService registers the bundle).
fn running_as_app() -> bool {
    std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
}

fn launch_at_login() -> bool {
    running_as_app() && unsafe { SMAppService::mainAppService().status() } == SMAppServiceStatus::Enabled
}

fn set_launch_at_login(on: bool) {
    let service = unsafe { SMAppService::mainAppService() };
    let result = unsafe { if on { service.registerAndReturnError() } else { service.unregisterAndReturnError() } };
    if let Err(e) = result {
        eprintln!("launch at login: {}", e.localizedDescription());
    }
    // macOS may want the user to approve it in System Settings → Login Items.
    if on && unsafe { service.status() } == SMAppServiceStatus::RequiresApproval {
        unsafe { SMAppService::openSystemSettingsLoginItems() };
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

#[derive(Clone, Copy, Default)]
struct Pose {
    blink: bool,
    panic: f64,
    step: Option<bool>,
    look: f64,
}

/// Clawd, drawn from the official Claude Code icon (24×24 viewBox, body spans y 5..20).
/// `s` is the sprite width; the sprite is 24×15 units, centred on (cx, cy).
/// Edges are snapped to device pixels so the pixel art stays crisp.
/// `pose.panic` (0..1) swaps in a startled face and flailing arms; `pose.step` lifts
/// alternate leg pairs while walking; `pose.look` shifts the eyes (units, ±).
fn critter(cx: f64, cy: f64, s: f64, scale: f64, mood: Mood, pose: Pose) {
    let Pose { blink, panic, step, look } = pose;
    let u = s / 24.0;
    let (x0, y0) = (cx - 12.0 * u, cy - 7.5 * u);
    let snap = |v: f64| (v * scale).round() / scale;
    let rect = |x: f64, y: f64, w: f64, h: f64| {
        let (l, t) = (snap(x0 + x * u), snap(y0 + (y - 5.0) * u));
        let (r, b) = (snap(x0 + (x + w) * u), snap(y0 + (y - 5.0 + h) * u));
        NSBezierPath::fillRect(NSRect::new(pt(l, t), NSSize::new(r - l, b - t)));
    };
    let panicking = panic > 0.0;
    // Arms flap in opposite directions while panicking.
    let flap = if panicking && !reduce_motion() {
        (media_now() * std::f64::consts::TAU / 0.35).sin() * 1.6 * panic
    } else if panicking {
        -1.2
    } else {
        0.0
    };

    theme::CLAWD.ns().set();
    rect(3.0, 5.0, 18.0, 12.08); // body
    rect(0.0, 10.95 - flap, 3.0, 3.1); // left arm
    rect(21.0, 10.95 + flap, 3.0, 3.1); // right arm
    for (i, lx) in [4.487, 7.488, 15.0, 18.0].into_iter().enumerate() {
        // Walking: outer-left + inner-right pair, then the other pair, step up.
        let lifted = step.is_some_and(|phase| phase == (i == 0 || i == 2));
        rect(lx, 17.0, 1.51, if lifted { 2.0 } else { 3.0 }); // legs
    }

    // Eyes are holes in the icon; paint them with the notch black.
    theme::CLAWD_EYE.ns().set();
    if panicking {
        // Wide eyes glancing down at the drop, mouth open in an "o".
        for ex in [6.6, 17.0] {
            rect(ex, 7.6, 1.9, 3.6);
        }
        rect(10.6, 14.2, 2.8, 1.9);
    } else {
        let (eye_top, eye_h) = match mood {
            Mood::Sleepy => (10.1, 0.85),
            Mood::Dizzy => (9.1, 0.9),
            _ if blink => (10.1, 0.85),
            _ => (8.1, 2.85),
        };
        for ex in [6.0, 16.51] {
            rect(ex + look, eye_top, 1.49, eye_h);
        }
    }

    if panicking || matches!(mood, Mood::Careful | Mood::Dizzy) {
        // sweat drops (two when panicking)
        theme::SWEAT.ns().set();
        rect(22.0, 5.5, 1.3, 1.3);
        rect(22.6, 4.6, 0.7, 0.9);
        if panicking {
            rect(1.0, 6.5, 1.3, 1.3);
            rect(1.3, 5.6, 0.7, 0.9);
        }
    }
    if mood == Mood::Sleepy && !panicking {
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

/// Local wall-clock "HH:mm" for axis ticks.
fn clock(at: i64) -> String {
    thread_local! {
        static FMT: Retained<NSDateFormatter> = {
            let f = NSDateFormatter::new();
            f.setLocale(Some(&NSLocale::initWithLocaleIdentifier(NSLocale::alloc(), ns_string!("en_US_POSIX"))));
            f.setDateFormat(Some(ns_string!("HH:mm")));
            f
        };
    }
    FMT.with(|f| {
        f.setTimeZone(Some(&NSTimeZone::systemTimeZone()));
        f.stringFromDate(&NSDate::dateWithTimeIntervalSince1970(at as f64)).to_string()
    })
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
    let preview = std::env::args().find_map(|a| a.strip_prefix("--preview-week=").and_then(|v| v.parse().ok()));
    let _ = PREVIEW_WEEK.set(preview);
    let _ = DEMO.set(std::env::args().any(|a| a == "--demo"));
    let args: Vec<String> = std::env::args().collect();
    // `--launch-at-login=on|off` (run from inside the .app): toggle and exit.
    if let Some(v) = args.iter().find_map(|a| a.strip_prefix("--launch-at-login=")) {
        set_launch_at_login(v == "on");
        println!("launch at login: {}", if launch_at_login() { "on" } else { "off" });
        return;
    }
    if let Some(path) = args.iter().position(|a| a == "--render-icon").and_then(|i| args.get(i + 1)) {
        render::app_icon(path);
        return;
    }
    if let Some(dir) = args.iter().position(|a| a == "--render-readme").and_then(|i| args.get(i + 1)) {
        render::readme_frames(mtm, dir);
        return;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stance_follows_the_colour_bands() {
        assert_eq!(perch(100.0), (7, Stance::Calm));
        assert_eq!(perch(60.0), (5, Stance::Calm)); // 4.2 units → 5 segments still lit
        assert_eq!(perch(35.0).1, Stance::Tremble);
        assert_eq!(perch(10.0), (1, Stance::Teeter));
        assert_eq!(perch(0.0), (0, Stance::Gone));
    }

    #[test]
    fn clawd_only_moves_when_a_whole_segment_goes() {
        // 60% and 58% both leave 5 segments → same spot.
        assert_eq!(segments_centre(perch(60.0).0), segments_centre(perch(58.0).0));
        assert!(segments_centre(5) > segments_centre(4));
        assert_eq!(segments_centre(7), theme::week_bar_w() / 2.0);
    }

    #[test]
    fn teeters_on_the_lit_edge() {
        assert!((lit_edge(10.0) - 0.7 * theme::WEEK_SEG_W).abs() < 1e-9);
        assert_eq!(lit_edge(100.0), theme::week_bar_w());
    }
}
