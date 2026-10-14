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
    NSAccessibilityButtonRole, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDidChangeScreenParametersNotification, NSAffineTransformNSAppKitAdditions,
    NSAutoresizingMaskOptions, NSBackingStoreType, NSBezierPath, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSEvent, NSFont, NSFontAttributeName, NSForegroundColorAttributeName,
    NSGraphicsContext, NSMainMenuWindowLevel, NSMenu, NSMenuItem, NSPanel, NSScreen,
    NSStringDrawing, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindowCollectionBehavior,
    NSWindowStyleMask, NSWorkspace, NSWorkspaceDidWakeNotification,
};
use objc2_core_graphics::CGPath;
use objc2_foundation::{
    ns_string, MainThreadMarker, NSAffineTransform, NSDate, NSDateFormatter, NSDictionary,
    NSLocale, NSNotification, NSNotificationCenter, NSNumber, NSPoint, NSRect, NSSize, NSString,
    NSTimeZone, NSTimer, NSURL,
};
use objc2_quartz_core::{
    kCAFillModeBoth, kCAMediaTimingFunctionEaseOut, CABasicAnimation, CACurrentMediaTime,
    CAMediaTiming, CAMediaTimingFunction, CAShapeLayer, CASpringAnimation, CATransaction,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};

use theme::{Color, Type, EXPANDED_W, FLARE, PAD};
use usage::{
    history_for, save_provider_config, snapshot_for, Limit, Provider, Sample, Snapshot,
};

const HOVER_IN_DELAY: f64 = 0.12;
const HOVER_OUT_DELAY: f64 = 0.12;
const PROVIDER_GAP: f64 = 14.0;

static WAKE: OnceLock<Sender<()>> = OnceLock::new();
/// `--preview-week=N` pins the weekly % left, to see characters' teeter without burning budget.
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
        Walk {
            from: f64::NAN,
            to: f64::NAN,
            start: 0.0,
            duration: 0.0,
        }
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
struct CharacterState {
    /// End of the current tremble burst (CACurrentMediaTime), in the yellow band.
    tremble_until: Cell<f64>,
    /// Walk along the weekly bar, as offsets from the bar's left end.
    walk: Cell<Walk>,
    /// Fell off at 0%: when let go (then pops back up, head only).
    gone: Cell<bool>,
    gone_start: Cell<f64>,
}

#[derive(Default)]
struct Ivars {
    show_claude: Cell<bool>,
    show_antigravity: Cell<bool>,
    claude_char: CharacterState,
    antigravity_char: CharacterState,

    expanded: Cell<bool>,
    /// Pointer currently inside the panel (expansion waits for it to linger).
    hovering: Cell<bool>,
    hover_gen: Cell<u64>,
    blink: Cell<bool>,
    refreshing: Cell<bool>,
    seen_version: Cell<u64>,
    notch_h: Cell<f64>,
    /// Layout the content is drawn in. Lags `expanded` while collapsing.
    shown_expanded: Cell<bool>,
    /// A morph is in flight; idle relayouts must not fight it.
    animating: Cell<bool>,
    anim_gen: Cell<u64>,
    /// The black blob behind the content (lives on a sibling background view).
    shape: OnceCell<Retained<CAShapeLayer>>,
    /// Redraw timer while characters tremble or teeter on the weekly bar.
    teeter: RefCell<Option<Retained<NSTimer>>>,
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

        #[unsafe(method(toggleClaude:))]
        fn toggle_claude(&self, _sender: Option<&AnyObject>) {
            let cur = self.ivars().show_claude.get();
            let other = self.ivars().show_antigravity.get();
            if cur && !other {
                return; // At least one provider must remain selected
            }
            self.set_provider_visibility(Provider::Claude, !cur);
        }

        #[unsafe(method(toggleAntigravity:))]
        fn toggle_antigravity(&self, _sender: Option<&AnyObject>) {
            let cur = self.ivars().show_antigravity.get();
            let other = self.ivars().show_claude.get();
            if cur && !other {
                return; // At least one provider must remain selected
            }
            self.set_provider_visibility(Provider::Antigravity, !cur);
        }

        #[unsafe(method(openClaudePage:))]
        fn open_claude_page(&self, _sender: Option<&AnyObject>) {
            if let Some(url) = NSURL::URLWithString(&NSString::from_str(Provider::Claude.usage_page())) {
                NSWorkspace::sharedWorkspace().openURL(&url);
            }
        }

        #[unsafe(method(openAntigravityPage:))]
        fn open_antigravity_page(&self, _sender: Option<&AnyObject>) {
            if let Some(url) = NSURL::URLWithString(&NSString::from_str(Provider::Antigravity.usage_page())) {
                NSWorkspace::sharedWorkspace().openURL(&url);
            }
        }

        #[unsafe(method(toggleLaunchAtLogin:))]
        fn toggle_launch_at_login(&self, _sender: Option<&AnyObject>) {
            set_launch_at_login(!launch_at_login());
        }
    }
);

impl NotchView {
    pub fn new(mtm: MainThreadMarker, shape: Retained<CAShapeLayer>) -> Retained<Self> {
        let cfg = usage::Config::load();
        Self::with_config(mtm, shape, cfg.show_claude, cfg.show_antigravity)
    }

    pub fn with_config(
        mtm: MainThreadMarker,
        shape: Retained<CAShapeLayer>,
        show_claude: bool,
        show_antigravity: bool,
    ) -> Retained<Self> {
        let ivars = Ivars::default();
        let _ = ivars.shape.set(shape);
        ivars.show_claude.set(show_claude);
        ivars.show_antigravity.set(show_antigravity);
        let this = Self::alloc(mtm).set_ivars(ivars);
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        this.setWantsLayer(true);
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

    fn active_providers(&self) -> Vec<Provider> {
        let iv = self.ivars();
        let mut list = Vec::new();
        if iv.show_claude.get() {
            list.push(Provider::Claude);
        }
        if iv.show_antigravity.get() {
            list.push(Provider::Antigravity);
        }
        if list.is_empty() {
            list.push(Provider::Claude);
        }
        list
    }

    fn char_state(&self, provider: Provider) -> &CharacterState {
        let iv = self.ivars();
        match provider {
            Provider::Claude => &iv.claude_char,
            Provider::Antigravity => &iv.antigravity_char,
        }
    }

    fn set_provider_visibility(&self, provider: Provider, visible: bool) {
        let iv = self.ivars();
        match provider {
            Provider::Claude => iv.show_claude.set(visible),
            Provider::Antigravity => iv.show_antigravity.set(visible),
        }
        save_provider_config(iv.show_claude.get(), iv.show_antigravity.get());
        self.update_accessibility();

        // Immediately update window frame and morph smoothly
        if let (Some(window), Some((collapsed, expanded)), Some(shape)) = (self.window(), self.frames(), iv.shape.get()) {
            let target = if iv.expanded.get() { expanded } else { collapsed };
            let current = window.frame();
            if reduce_motion() {
                window.setFrame_display(target, true);
                shape.setPath(Some(&notch_path(local(target, target), self.radius(iv.expanded.get()))));
                self.setNeedsDisplay(true);
                return;
            }

            let gen = iv.anim_gen.get() + 1;
            iv.anim_gen.set(gen);
            iv.animating.set(true);

            // Bounding box union for smooth expansion/contraction
            let min_x = current.origin.x.min(target.origin.x);
            let min_y = current.origin.y.min(target.origin.y);
            let max_x = (current.origin.x + current.size.width).max(target.origin.x + target.size.width);
            let max_y = (current.origin.y + current.size.height).max(target.origin.y + target.size.height);
            let union_rect = NSRect::new(pt(min_x, min_y), NSSize::new(max_x - min_x, max_y - min_y));

            window.setFrame_display(union_rect, false);

            let from = presented_path(shape).unwrap_or_else(|| notch_path(local(current, union_rect), self.radius(iv.expanded.get())));
            let to = notch_path(local(target, union_rect), self.radius(iv.expanded.get()));
            let settle_in = morph(shape, Some(&from), &to, iv.expanded.get());

            self.setNeedsDisplay(true);
            let view = self.retain();
            after(settle_in, move || {
                let iv = view.ivars();
                if iv.anim_gen.get() != gen {
                    return;
                }
                iv.animating.set(false);
                view.settle();
            });
        } else {
            self.settle();
        }
    }

    fn menu(&self) -> Retained<NSMenu> {
        let mtm = self.mtm();
        let menu = NSMenu::new(mtm);
        let item = |title: &NSString, action, target: Option<&AnyObject>| unsafe {
            let it = NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), title, Some(action), ns_string!(""));
            it.setTarget(target);
            menu.addItem(&it);
            it
        };
        item(ns_string!("Refresh Now"), sel!(refreshNow:), Some(self));

        let show_c = self.ivars().show_claude.get();
        let show_a = self.ivars().show_antigravity.get();

        if show_c {
            item(ns_string!("Open Claude Usage Page"), sel!(openClaudePage:), Some(self));
        }
        if show_a {
            item(ns_string!("Open Antigravity Page"), sel!(openAntigravityPage:), Some(self));
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        // Provider toggles
        let c_item = item(ns_string!("Claude Code"), sel!(toggleClaude:), Some(self));
        c_item.setState(if show_c { NSControlStateValueOn } else { NSControlStateValueOff });
        if show_c && !show_a {
            c_item.setEnabled(false);
        }

        let a_item = item(ns_string!("Antigravity"), sel!(toggleAntigravity:), Some(self));
        a_item.setState(if show_a { NSControlStateValueOn } else { NSControlStateValueOff });
        if show_a && !show_c {
            a_item.setEnabled(false);
        }

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

    fn total_left_width(&self, notch_h: f64) -> f64 {
        let providers = self.active_providers();
        let mut total = theme::EAR_OUTER;
        for (i, &p) in providers.iter().enumerate() {
            if i > 0 {
                total += PROVIDER_GAP;
            }
            let snap = snapshot_for(p);
            let ear = ProviderEar::new(p, snap.as_ref(), notch_h, false);
            total += ear.content_w;
        }
        total + theme::EAR_INNER
    }

    /// Settled (collapsed, expanded) panel frames in screen coordinates.
    fn frames(&self) -> Option<(NSRect, NSRect)> {
        let g = Geometry::current(self.mtm())?;
        self.ivars().notch_h.set(g.notch_h);
        let lw = self.total_left_width(g.notch_h);
        let rw = -(g.notch_w / 2.0);

        let active = self.active_providers();
        let (cw, ew) = (
            lw + g.notch_w + rw + 2.0 * FLARE,
            (g.notch_w + 2.0 * FLARE + 2.0 * lw.max(rw)).max(EXPANDED_W),
        );
        let eh = multi_expanded_height(g.notch_h, &active);
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

        let current = window.frame();
        let (morph_frame, from) = if current.size.height > collapsed.size.height + 0.5 {
            (current, presented_path(shape))
        } else {
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
                return;
            }
            iv.animating.set(false);
            let collapsed = !iv.expanded.get();
            view.settle();
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

    /// Character displayed offset along the bar for a new `target`.
    fn walk_to(&self, provider: Provider, target: f64) -> (f64, bool, f64) {
        let cs = self.char_state(provider);
        let now = media_now();
        let mut walk = cs.walk.get();
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
            cs.walk.set(walk);
            self.update_teeter();
        }
        cs.walk.set(walk);
        let (x, walking) = walk.at(now);
        (x, walking, (walk.to - walk.from).signum())
    }

    /// Run a light redraw timer only while any active character is moving.
    fn update_teeter(&self) {
        let iv = self.ivars();
        let notch_h = iv.notch_h.get();
        let now = media_now();
        let motion = !reduce_motion();

        let mut moving = false;
        for &p in &self.active_providers() {
            let snap = snapshot_for(p);
            let ears = ProviderEar::new(p, snap.as_ref(), notch_h, false);
            let (_, stance) = perch(ears.week_left);
            let cs = self.char_state(p);
            let walking = cs.walk.get().at(now).1;
            let is_moving = walking
                || match stance {
                    Stance::Teeter => true,
                    Stance::Gone => now < cs.gone_start.get() + theme::GONE_ANIM,
                    Stance::Tremble => now < cs.tremble_until.get(),
                    Stance::Calm => false,
                };
            if is_moving {
                moving = true;
                break;
            }
        }

        let need = !iv.shown_expanded.get() && motion && moving;
        let mut timer = iv.teeter.borrow_mut();
        if need && timer.is_none() {
            let view = self.retain();
            let block = RcBlock::new(move |_: NonNull<NSTimer>| {
                view.setNeedsDisplay(true);
                let now = media_now();
                let mut any_moving = false;
                for &p in &view.active_providers() {
                    let cs = view.char_state(p);
                    if cs.tremble_until.get() > now || cs.walk.get().at(now).1 {
                        any_moving = true;
                        break;
                    }
                }
                if !any_moving {
                    view.update_teeter();
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

    /// Fires every few seconds: blink and pick up new data.
    fn tick(&self) {
        let iv = self.ivars();
        let v_c = snapshot_for(Provider::Claude).map_or(0, |s| s.version);
        let v_a = snapshot_for(Provider::Antigravity).map_or(0, |s| s.version);
        let version = v_c + v_a;
        if iv.seen_version.replace(version) != version {
            iv.refreshing.set(false);
            self.update_accessibility();
        }
        self.settle();
        let now = media_now();
        iv.claude_char.tremble_until.set(now + theme::TREMBLE_BURST);
        iv.antigravity_char.tremble_until.set(now + theme::TREMBLE_BURST);
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
        let mut parts: Vec<String> = Vec::new();
        for &p in &self.active_providers() {
            let label = match snapshot_for(p) {
                None => format!("{}: loading", p.display_name()),
                Some(s) => {
                    let rows_str: Vec<String> = rows_for(p, &s)
                        .into_iter()
                        .map(|(name, l)| {
                            let reset = l.resets_at.map_or(String::new(), |at| format!(", resets in {}", countdown(at)));
                            format!("{name} {} left{reset}", pct(l.left()))
                        })
                        .collect();
                    format!("{}: {}", p.display_name(), rows_str.join("; "))
                }
            };
            parts.push(label);
        }
        let total_label = parts.join(" | ");
        unsafe {
            let _: () = msg_send![self, setAccessibilityLabel: &*NSString::from_str(&total_label)];
        }
    }

    fn draw(&self, _dirty: NSRect) {
        let b = self.bounds();
        let w = b.size.width;
        let iv = self.ivars();
        let notch_h = iv.notch_h.get().max(1.0);
        let expanded = iv.shown_expanded.get();
        let scale = self.window().map_or(2.0, |win| win.backingScaleFactor());
        let active = self.active_providers();

        if !expanded {
            let motion = !reduce_motion();
            let now = media_now();

            let mut cur_x = FLARE + theme::EAR_OUTER;
            for (idx, &p) in active.iter().enumerate() {
                let snap = snapshot_for(p);
                let stale = snap.as_ref().is_some_and(|s| s.error.is_some() || usage::now() - s.fetched_at > 600);
                let ear = ProviderEar::new(p, snap.as_ref(), notch_h, stale);
                let cs = self.char_state(p);

                if idx > 0 {
                    // Hairline separator between side-by-side providers
                    let sep_x = cur_x - PROVIDER_GAP / 2.0;
                    let sep_h = notch_h * 0.35;
                    let sep_y = (notch_h - sep_h) / 2.0;
                    pill(sep_x - 0.5, sep_y, 1.0, sep_h, theme::TRACK);
                }

                let bar_x = cur_x;
                let sprite = ear.sprite;
                let u = sprite / 24.0;
                let char_h = sprite * 15.0 / 24.0;
                let bar_y = (notch_h - char_h - theme::WEEK_BAR_H) / 2.0 + char_h;
                week_bar(bar_x, bar_y, ear.week_left, ear.week_color);

                let (segments, stance) = perch(ear.week_left);
                let stand_cy = bar_y - char_h / 2.0;
                let mood = Mood::of(snap.as_ref());

                NSGraphicsContext::saveGraphicsState_class();
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
                    let (was_x, _) = cs.walk.get().at(now);
                    let from_x = if was_x.is_nan() { bar_x } else { bar_x + was_x };
                    let peek_cy = bar_y + theme::WEEK_BAR_H + 0.5 + 7.5 * u;
                    let peek = ((bar_x + 12.0 * u).max(FLARE + self.radius(false) + 12.0 * u), peek_cy);
                    if !cs.gone.replace(true) {
                        cs.gone_start.set(if was_x.is_nan() { f64::NEG_INFINITY } else { now });
                        self.update_teeter();
                    }
                    let t = if motion { now - cs.gone_start.get() } else { f64::INFINITY };
                    let hidden = notch_h + 9.0 * u;
                    let start_y = stand_cy;
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
                    draw_character(p, cx, cy, sprite, scale, Mood::Busy, Pose { blink: iv.blink.get(), ..Pose::default() });
                } else if stance == Stance::Teeter {
                    cs.gone.set(false);
                    let (offset, walking, dir) = self.walk_to(p, lit_edge(ear.week_left));
                    let cx = bar_x + offset;
                    if !walking {
                        rotate((cx, bar_y), teeter_degrees(ear.week_left), 0.0);
                    }
                    let step = walking.then(|| (now / 0.14) as i64 % 2 == 0);
                    let pose = Pose { panic: if walking { 0.0 } else { 1.0 }, step, look: dir * 0.6, ..Pose::default() };
                    draw_character(p, cx, stand_cy, sprite, scale, mood, pose);
                } else {
                    cs.gone.set(false);
                    let (offset, walking, dir) = self.walk_to(p, segments_centre(segments));
                    let cx = bar_x + offset;
                    let shiver = stance == Stance::Tremble && !walking && motion && now < cs.tremble_until.get();
                    if shiver {
                        let wobble = (now * std::f64::consts::TAU * 13.0).sin();
                        rotate((cx, bar_y), 1.6 * wobble, 0.5 * wobble);
                    }
                    let face = if stance == Stance::Tremble { Mood::Careful } else { mood };
                    let step = walking.then(|| (now / 0.14) as i64 % 2 == 0);
                    let bob = if step == Some(true) { -0.4 } else { 0.0 };
                    let pose = Pose { blink: iv.blink.get() && !walking, step, look: dir * 0.6, ..Pose::default() };
                    draw_character(p, cx, stand_cy + bob, sprite, scale, face, pose);
                }
                NSGraphicsContext::restoreGraphicsState_class();

                // Session text
                let baseline = notch_h / 2.0 + ear.session_ty.size * 0.36;
                let top = |ty: &Type| baseline - ty.size * 0.97;
                let tx = cur_x + theme::week_bar_w() + theme::SESSION_GAP;
                let tx = tx + text(&ear.session.value, tx, top(&ear.session_ty), &ear.session_ty, ear.session.color, Align::Left);
                if let Some(r) = &ear.session.reset {
                    text(r, tx + theme::RESET_GAP, top(&ear.reset_ty), &ear.reset_ty, theme::TEXT_TERTIARY, Align::Left);
                }

                cur_x += ear.content_w + PROVIDER_GAP;
            }
            return;
        }

        // Expanded Panel: Render distinct section for each active provider
        let (left, right) = (FLARE + PAD, w - FLARE - PAD);
        let mut section_top = notch_h + 10.0;

        for (idx, &p) in active.iter().enumerate() {
            if idx > 0 {
                let div_y = section_top - 8.0;
                let div_p = NSBezierPath::new();
                div_p.setLineWidth(0.5);
                div_p.moveToPoint(pt(left, div_y));
                div_p.lineToPoint(pt(right, div_y));
                theme::TRACK.ns().set();
                div_p.stroke();
            }

            let snap = snapshot_for(p);
            let stale = snap.as_ref().is_some_and(|s| s.error.is_some() || usage::now() - s.fetched_at > 600);
            let mood = Mood::of(snap.as_ref());

            // Section Header: Character + Title + Status
            draw_character(p, left + 18.0, section_top + 18.0, 36.0, scale, mood, Pose { blink: iv.blink.get(), ..Pose::default() });
            let hx = left + 48.0;
            let title = format!("{} · {}", p.display_name(), mood.title());
            text(&title, hx, section_top + 4.0, &theme::TITLE, theme::TEXT_PRIMARY, Align::Left);
            let status = match snap.as_ref() {
                _ if iv.refreshing.get() => "Refreshing…".to_string(),
                None => "Loading…".to_string(),
                Some(s) => match (s.error, s.fetched_at) {
                    (Some(e), 0) => e.to_string(),
                    (Some(e), t) => format!("{e} · data {}", ago(usage::now() - t)),
                    (None, t) => format!("Updated {} · click to refresh", ago(usage::now() - t)),
                },
            };
            text(&status, hx, section_top + 23.0, &theme::CAPTION, theme::TEXT_SECONDARY, Align::Left);

            // Limit rows
            let rows_list = snap.as_ref().map_or_else(Vec::new, |s| rows_for(p, s));
            let bar_x = left + theme::COL_BAR_X;
            let bar_w = (right - theme::COL_BAR_END) - bar_x;
            let oy = section_top + theme::HEADER_H;
            text("REMAINING", bar_x, oy, &theme::OVERLINE, theme::TEXT_TERTIARY, Align::Left);
            text("RESETS IN", right, oy, &theme::OVERLINE, theme::TEXT_TERTIARY, Align::Right);

            for (i, (name, l)) in rows_list.iter().enumerate() {
                let y = oy + theme::OVERLINE_H + i as f64 * theme::ROW_H;
                let left_pct = l.left();
                let c = if stale { theme::TEXT_TERTIARY } else { theme::remaining(left_pct) };
                text(name, left, y, &theme::LABEL, theme::TEXT_SECONDARY, Align::Left);
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

            let gy = oy + theme::OVERLINE_H + rows_list.len() as f64 * theme::ROW_H + theme::GRAPH_GAP;
            history_graph_for(p, snap.as_ref(), left, gy, right - left, stale);

            section_top = gy + theme::OVERLINE_H + theme::GRAPH_H + theme::GRAPH_AXIS_H + 20.0;
        }
    }
}

/// "THIS SESSION" hairline graph of session % left across the current 5 h window.
fn history_graph_for(provider: Provider, snap: Option<&Snapshot>, x: f64, y: f64, w: f64, stale: bool) {
    let session = snap.and_then(|s| s.five_hour);
    let color = match session {
        Some(l) if !stale => theme::remaining(l.left()),
        _ => theme::TEXT_TERTIARY,
    };
    let graph_title = match provider {
        Provider::Claude => "THIS SESSION (CLAUDE)",
        Provider::Antigravity => "THIS SESSION (ANTIGRAVITY)",
    };
    text(graph_title, x, y, &theme::OVERLINE, theme::TEXT_TERTIARY, Align::Left);
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

    // Hour ticks on local hour boundaries
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
    if now > t0 && now < t1 {
        let nx = px(now);
        line(&[(nx, gy), (nx, axis_y)], theme::GRAPH_NOW, 0.5);
    }

    let samples: Vec<Sample> = history_for(provider).into_iter().filter(|s| s.t >= t0).collect();
    if samples.len() < 2 {
        text("Fills in as this session goes", x + w / 2.0 - 64.0, gy + h / 2.0 - 7.0, &theme::CAPTION, theme::TEXT_TERTIARY, Align::Left);
    }
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

pub struct Ears {
    pub session: Side,
    pub week_left: f64,
    pub week_color: Color,
    pub session_ty: Type,
    pub reset_ty: Type,
    pub sprite: f64,
}

pub struct Side {
    pub value: String,
    pub color: Color,
    pub reset: Option<String>,
}

impl Ears {
    pub fn new(snap: Option<&Snapshot>, notch_h: f64, stale: bool) -> Self {
        let ear = ProviderEar::new(Provider::Claude, snap, notch_h, stale);
        Ears {
            session: ear.session,
            week_left: ear.week_left,
            week_color: ear.week_color,
            session_ty: ear.session_ty,
            reset_ty: ear.reset_ty,
            sprite: ear.sprite,
        }
    }

    pub fn widths(&self) -> (f64, f64) {
        let session = measure(&self.session.value, &self.session_ty)
            + self.session.reset.as_ref().map_or(0.0, |r| theme::RESET_GAP + measure(r, &self.reset_ty));
        (session_text_x() - FLARE + session + theme::EAR_INNER, 0.0)
    }
}

struct ProviderEar {
    #[allow(dead_code)]
    provider: Provider,
    session: Side,
    week_left: f64,
    week_color: Color,
    session_ty: Type,
    reset_ty: Type,
    sprite: f64,
    content_w: f64,
}

impl ProviderEar {
    fn new(provider: Provider, snap: Option<&Snapshot>, notch_h: f64, stale: bool) -> Self {
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
        let session_ty = Type { size: (size + 1.5).min(notch_h * 0.45), ..theme::MICRO_HERO };
        let reset_ty = Type { size: size - 1.5, ..theme::MICRO_LABEL };
        let sprite = ((notch_h - theme::WEEK_BAR_H - 12.0) * 24.0 / 15.0).clamp(16.0, 22.0);

        let text_w = measure(&session.value, &session_ty)
            + session.reset.as_ref().map_or(0.0, |r| theme::RESET_GAP + measure(r, &reset_ty));
        let content_w = theme::week_bar_w() + theme::SESSION_GAP + text_w;

        ProviderEar {
            provider,
            session,
            week_left,
            week_color,
            session_ty,
            reset_ty,
            sprite,
            content_w,
        }
    }
}

/// Where the session text starts in the collapsed panel (after first character's bar).
pub fn session_text_x() -> f64 {
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
    /// 0%: fell off entirely; only head peeks up from the bottom.
    Gone,
}

/// (segments still lit, stance) for a weekly % left.
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
    lean + (5.0 + 6.0 * severity) * phase.sin()
}

/// The collapsed blob's outline in this (flipped) view: square top with flares,
/// rounded bottom — used to clip characters so they never draw outside the black.
pub fn collapsed_outline(w: f64, h: f64, r: f64) -> Retained<NSBezierPath> {
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
/// rounded bottom.
fn notch_path(r: NSRect, radius: f64) -> Retained<CGPath> {
    notch_bezier(r, radius).CGPath()
}

pub fn notch_bezier(r: NSRect, radius: f64) -> Retained<NSBezierPath> {
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
    anim.setMass(1.0);
    anim.setStiffness(if expanding { theme::SPRING_OPEN.0 } else { theme::SPRING_CLOSE.0 });
    anim.setDamping(if expanding { theme::SPRING_OPEN.1 } else { theme::SPRING_CLOSE.1 });
    unsafe {
        anim.setFromValue(from.map(cf_obj));
        anim.setToValue(Some(cf_obj(to)));
    }
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

/// Height of the expanded panel for multiple provider sections.
fn multi_expanded_height(notch_h: f64, active: &[Provider]) -> f64 {
    let graph = theme::GRAPH_GAP + theme::OVERLINE_H + theme::GRAPH_H + theme::GRAPH_AXIS_H;
    let mut total = notch_h + 10.0;
    for (i, &p) in active.iter().enumerate() {
        let snap = snapshot_for(p);
        let rows_count = snap.as_ref().map_or(2, |s| rows_for(p, s).len().max(1)) as f64;
        total += theme::HEADER_H + theme::OVERLINE_H + rows_count * theme::ROW_H + graph;
        if i + 1 < active.len() {
            total += 20.0;
        }
    }
    total + theme::BOTTOM_PAD
}

pub fn expanded_height(notch_h: f64, rows: f64) -> f64 {
    let graph = theme::GRAPH_GAP + theme::OVERLINE_H + theme::GRAPH_H + theme::GRAPH_AXIS_H;
    notch_h + 10.0 + theme::HEADER_H + theme::OVERLINE_H + rows * theme::ROW_H + graph + theme::BOTTOM_PAD
}

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

fn after(secs: f64, f: impl Fn() + 'static) {
    let block = RcBlock::new(move |_: NonNull<NSTimer>| f());
    let timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(secs, false, &block) };
    timer.setTolerance(secs * 0.2);
}

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

pub fn rows(s: &Snapshot) -> Vec<(&'static str, Limit)> {
    rows_for(Provider::Claude, s)
}

pub fn rows_for(provider: Provider, s: &Snapshot) -> Vec<(&'static str, Limit)> {
    match provider {
        Provider::Claude => [
            ("Session", s.five_hour),
            ("Weekly", s.seven_day),
            ("Opus", s.seven_day_opus),
            ("Sonnet", s.seven_day_sonnet),
        ]
        .into_iter()
        .filter_map(|(n, l)| Some((n, l?)))
        .filter(|(n, l)| matches!(*n, "Session" | "Weekly") || l.used > 0.0 || l.resets_at.is_some())
        .collect(),

        Provider::Antigravity => [
            ("Session", s.five_hour),
            ("Weekly", s.seven_day),
        ]
        .into_iter()
        .filter_map(|(n, l)| Some((n, l?)))
        .collect(),
    }
}

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
        let screen = screens.iter().find(|s| s.safeAreaInsets().top > 0.0).or_else(|| screens.firstObject())?;
        let f = screen.frame();
        let inset = screen.safeAreaInsets().top;
        let (center_x, notch_w, notch_h) = if inset > 0.0 {
            let (l, r) = (screen.auxiliaryTopLeftArea(), screen.auxiliaryTopRightArea());
            let cut_l = f.origin.x + l.size.width;
            let cut_r = f.origin.x + f.size.width - r.size.width;
            ((cut_l + cut_r) / 2.0, cut_r - cut_l, inset)
        } else {
            let vf = screen.visibleFrame();
            let h = (f.origin.y + f.size.height - (vf.origin.y + vf.size.height)).clamp(24.0, 32.0);
            (f.origin.x + f.size.width / 2.0, 0.0, h)
        };
        Some(Geometry { center_x, top: f.origin.y + f.size.height, notch_w, notch_h })
    }
}

// ---------- Mood & Characters ----------

#[derive(Clone, Copy, PartialEq)]
pub enum Mood {
    Sleepy,
    Happy,
    Busy,
    Careful,
    Dizzy,
}

impl Mood {
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
pub struct Pose {
    pub blink: bool,
    pub panic: f64,
    pub step: Option<bool>,
    pub look: f64,
}

fn draw_character(provider: Provider, cx: f64, cy: f64, s: f64, scale: f64, mood: Mood, pose: Pose) {
    match provider {
        Provider::Claude => critter(cx, cy, s, scale, mood, pose),
        Provider::Antigravity => antigravity_critter(cx, cy, s, scale, mood, pose),
    }
}

/// Clawd, drawn from the official Claude Code icon.
pub fn critter(cx: f64, cy: f64, s: f64, scale: f64, mood: Mood, pose: Pose) {
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
        let lifted = step.is_some_and(|phase| phase == (i == 0 || i == 2));
        rect(lx, 17.0, 1.51, if lifted { 2.0 } else { 3.0 });
    }

    theme::CLAWD_EYE.ns().set();
    if panicking {
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

const ANTIGRAVITY_PIXELS: &[(f64, f64, u32)] = &[
    (5.0, 0.0, 0xF2922E),
    (6.0, 0.0, 0xF07237),
    (4.0, 1.0, 0xDAB131),
    (5.0, 1.0, 0xF6912E),
    (6.0, 1.0, 0xF37337),
    (7.0, 1.0, 0xF0583B),
    (3.0, 2.0, 0x9DC345),
    (4.0, 2.0, 0xB5B43E),
    (5.0, 2.0, 0xE2993D),
    (6.0, 2.0, 0xF67A34),
    (7.0, 2.0, 0xF86A35),
    (8.0, 2.0, 0xEF5442),
    (3.0, 3.0, 0x86C64E),
    (4.0, 3.0, 0x75B45E),
    (5.0, 3.0, 0xCC954D),
    (6.0, 3.0, 0xEF7947),
    (7.0, 3.0, 0xE16652),
    (8.0, 3.0, 0xE14F59),
    (2.0, 4.0, 0x7CC251),
    (3.0, 4.0, 0x71C25C),
    (4.0, 4.0, 0x5CA98E),
    (5.0, 4.0, 0x5C91B3),
    (6.0, 4.0, 0x8373B0),
    (7.0, 4.0, 0x746FC3),
    (8.0, 4.0, 0x995DA8),
    (9.0, 4.0, 0x9C5B97),
    (2.0, 5.0, 0x61C37D),
    (3.0, 5.0, 0x54B880),
    (4.0, 5.0, 0x4197DE),
    (7.0, 5.0, 0x4197DE),
    (8.0, 5.0, 0x706ECE),
    (9.0, 5.0, 0x8F64B4),
    (2.0, 6.0, 0x61C37D),
    (3.0, 6.0, 0x43AEAB),
    (8.0, 6.0, 0x4A80EA),
    (9.0, 6.0, 0x6C73D8),
    (1.0, 7.0, 0x6DC694),
    (2.0, 7.0, 0x62BAD5),
    (3.0, 7.0, 0x47A8DC),
    (8.0, 7.0, 0x3D89FB),
    (9.0, 7.0, 0x4A81F0),
    (10.0, 7.0, 0x6579E1),
    (1.0, 8.0, 0x6BC7A3),
    (2.0, 8.0, 0x64B6F6),
    (9.0, 8.0, 0x3886FB),
    (10.0, 8.0, 0x4881F4),
    (0.0, 9.0, 0x67B9F4),
    (1.0, 9.0, 0x64B6F6),
    (10.0, 9.0, 0x3883F9),
    (11.0, 9.0, 0x3D85FC),
];

/// Antigravity character: 12x10 pixel art rainbow gradient arch with matching
/// personality states (Calm, Tremble, Teeter, Gone, Panic, Sweat).
pub fn antigravity_critter(cx: f64, cy: f64, s: f64, scale: f64, mood: Mood, pose: Pose) {
    let Pose { blink, panic, step, look } = pose;
    let u = s / 24.0;
    let px_size = 1.45 * u;
    let x0 = cx - 6.0 * px_size;
    let y0 = cy - 5.0 * px_size;
    let snap = |v: f64| (v * scale).round() / scale;
    let rect = |x: f64, y: f64, w: f64, h: f64| {
        let (l, t) = (snap(x), snap(y));
        let (r, b) = (snap(x + w), snap(y + h));
        NSBezierPath::fillRect(NSRect::new(pt(l, t), NSSize::new((r - l).max(1.0 / scale), (b - t).max(1.0 / scale))));
    };

    let left_lifted = step.is_some_and(|phase| phase);
    let right_lifted = step.is_some_and(|phase| !phase);

    // 1. Draw the 50 pixel cells of the rainbow gradient arch
    for &(px, py, hex) in ANTIGRAVITY_PIXELS {
        let mut py_lift = 0.0;
        if left_lifted && px <= 3.0 && py >= 8.0 {
            py_lift = -0.6 * px_size;
        } else if right_lifted && px >= 8.0 && py >= 8.0 {
            py_lift = -0.6 * px_size;
        }
        Color::hex(hex).ns().set();
        rect(x0 + px * px_size, y0 + py * px_size + py_lift, px_size, px_size);
    }

    // 2. Eyes and facial expressions
    let panicking = panic > 0.0;
    let look_offset = (look / 1.45).clamp(-0.4, 0.4) * px_size;

    theme::ANTIGRAVITY_EYE.ns().set();
    if panicking {
        // Wide panic eyes
        let eye_w = 1.3 * px_size;
        let eye_h = 2.0 * px_size;
        let eye_top = y0 + 3.8 * px_size;
        for ex in [3.5, 8.5] {
            rect(x0 + ex * px_size - eye_w / 2.0, eye_top, eye_w, eye_h);
        }
        // Open mouth on the crossbar bridge
        let mouth_w = 1.8 * px_size;
        let mouth_h = 1.4 * px_size;
        rect(x0 + 6.0 * px_size - mouth_w / 2.0, y0 + 4.3 * px_size, mouth_w, mouth_h);
    } else {
        let (eye_top, eye_h) = match mood {
            Mood::Sleepy => (y0 + 4.9 * px_size, 0.5 * px_size),
            Mood::Dizzy => (y0 + 4.6 * px_size, 0.6 * px_size),
            _ if blink => (y0 + 4.9 * px_size, 0.5 * px_size),
            _ => (y0 + 4.2 * px_size, 1.6 * px_size),
        };
        let eye_w = 1.0 * px_size;
        for ex in [3.5, 8.5] {
            rect(x0 + ex * px_size - eye_w / 2.0 + look_offset, eye_top, eye_w, eye_h);
        }
    }

    // 3. Sweat drops during trembling / panicking
    if panicking || matches!(mood, Mood::Careful | Mood::Dizzy) {
        theme::SWEAT.ns().set();
        rect(x0 + 10.3 * px_size, y0 + 1.8 * px_size, 0.9 * px_size, 0.9 * px_size);
        rect(x0 + 10.7 * px_size, y0 + 1.2 * px_size, 0.5 * px_size, 0.6 * px_size);
        if panicking {
            rect(x0 + 0.8 * px_size, y0 + 2.4 * px_size, 0.9 * px_size, 0.9 * px_size);
            rect(x0 + 1.0 * px_size, y0 + 1.8 * px_size, 0.5 * px_size, 0.6 * px_size);
        }
    }

    // 4. Sleepy "z"
    if mood == Mood::Sleepy && !panicking {
        let ty = Type { size: (8.0 * u).max(7.0), ..theme::NUMERAL };
        text("z", x0 + 12.0 * px_size, y0 - 3.5 * px_size, &ty, theme::TEXT_SECONDARY, Align::Left);
    }
}

// ---------- drawing helpers ----------

pub fn pt(x: f64, y: f64) -> NSPoint {
    NSPoint::new(x, y)
}

pub fn pill(x: f64, y: f64, w: f64, h: f64, c: Color) {
    c.ns().set();
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(NSRect::new(pt(x, y), NSSize::new(w, h)), h / 2.0, h / 2.0)
        .fill();
}

pub enum Align {
    Left,
    Right,
}

pub fn attrs(ty: &Type, color: Color) -> Retained<NSDictionary<NSString, AnyObject>> {
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

pub fn measure(s: &str, ty: &Type) -> f64 {
    unsafe { NSString::from_str(s).sizeWithAttributes(Some(&attrs(ty, theme::TEXT_PRIMARY))).width }
}

/// Draws `s` and returns its width.
pub fn text(s: &str, x: f64, y: f64, ty: &Type, color: Color, align: Align) -> f64 {
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

fn countdown(at: i64) -> String {
    let left = (at - usage::now()).max(0);
    let (d, h, m) = (left / 86400, left % 86400 / 3600, left % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

fn short_countdown(at: i64) -> String {
    let left = (at - usage::now()).max(0);
    let (d, h, m) = (left / 86400, left % 86400 / 3600, left % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h{m:02}"),
        _ => format!("{d}d{h}h"),
    }
}

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
        f.setTimeZone(Some(&NSTimeZone::systemTimeZone()));
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
    let show_claude = cfg.show_claude;
    let show_antigravity = cfg.show_antigravity;
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
    let view = NotchView::with_config(mtm, shape, show_claude, show_antigravity);
    for v in [&*bg, &**view] {
        v.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        root.addSubview(v);
    }
    panel.setContentView(Some(&root));
    view.settle();
    panel.orderFrontRegardless();

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
        assert_eq!(perch(60.0), (5, Stance::Calm));
        assert_eq!(perch(35.0).1, Stance::Tremble);
        assert_eq!(perch(10.0), (1, Stance::Teeter));
        assert_eq!(perch(0.0), (0, Stance::Gone));
    }

    #[test]
    fn clawd_only_moves_when_a_whole_segment_goes() {
        assert_eq!(segments_centre(perch(60.0).0), segments_centre(perch(58.0).0));
        assert!(segments_centre(5) > segments_centre(4));
        assert_eq!(segments_centre(7), theme::week_bar_w() / 2.0);
    }

    #[test]
    fn teeters_on_the_lit_edge() {
        assert!((lit_edge(10.0) - 0.7 * theme::WEEK_SEG_W).abs() < 1e-9);
        assert_eq!(lit_edge(100.0), theme::week_bar_w());
    }

    #[test]
    fn multi_provider_dynamic_expansion() {
        let notch_h = 32.0;
        let snap = Snapshot {
            five_hour: Some(Limit { used: 30.0, resets_at: Some(usage::now() + 3600) }),
            seven_day: Some(Limit { used: 40.0, resets_at: None }),
            ..Default::default()
        };
        let ear_claude = ProviderEar::new(Provider::Claude, Some(&snap), notch_h, false);
        let ear_antigravity = ProviderEar::new(Provider::Antigravity, Some(&snap), notch_h, false);

        let single_w = theme::EAR_OUTER + ear_claude.content_w + theme::EAR_INNER;
        let both_w = theme::EAR_OUTER + ear_claude.content_w + PROVIDER_GAP + ear_antigravity.content_w + theme::EAR_INNER;

        assert!(both_w > single_w);
        assert_eq!(both_w, single_w + PROVIDER_GAP + ear_antigravity.content_w);
    }

    #[test]
    fn provider_rows_distinction() {
        let snap = Snapshot {
            five_hour: Some(Limit { used: 10.0, resets_at: None }),
            seven_day: Some(Limit { used: 20.0, resets_at: None }),
            seven_day_opus: Some(Limit { used: 15.0, resets_at: None }),
            seven_day_sonnet: Some(Limit { used: 25.0, resets_at: None }),
            ..Default::default()
        };
        let c_rows = rows_for(Provider::Claude, &snap);
        let a_rows = rows_for(Provider::Antigravity, &snap);

        assert_eq!(c_rows.len(), 4);
        assert_eq!(c_rows[2].0, "Opus");
        assert_eq!(c_rows[3].0, "Sonnet");

        assert_eq!(a_rows.len(), 2);
        assert_eq!(a_rows[0].0, "Session");
        assert_eq!(a_rows[1].0, "Weekly");
    }

    #[test]
    fn test_antigravity_pixel_table() {
        assert_eq!(ANTIGRAVITY_PIXELS.len(), 50);
        for &(px, py, hex) in ANTIGRAVITY_PIXELS {
            assert!(px >= 0.0 && px <= 11.0, "px out of range: {px}");
            assert!(py >= 0.0 && py <= 9.0, "py out of range: {py}");
            assert!(hex <= 0xFFFFFF, "invalid hex color: {hex:#x}");
        }
    }
}
