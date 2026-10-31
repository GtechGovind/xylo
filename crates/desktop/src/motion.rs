//! Motion and loading states: one quiet vocabulary for every surface.
//!
//! Motion in Xylo explains a change — something appeared, opened, moved or is
//! still working — and is never decoration. Every helper here is short
//! ([`FAST`] to [`SLOW`], 120–240 ms), eases out when something enters and in
//! when it leaves, never bounces, and never delays input: an element is
//! interactive from its first frame, whatever its opacity.
//!
//! # Reduced motion
//!
//! `gpui_kit::init` reads the operating system's "Reduce motion" setting
//! (macOS: `NSWorkspace.accessibilityDisplayShouldReduceMotion`) into
//! [`App::reduce_motion`], and GPUI's animation elements, value transitions
//! and spinners all honour that flag: a one-shot animation renders its end
//! state on the first frame, a looping one (spinner, shimmer, pulse) renders
//! its resting state and asks for no frames. Read the flag directly
//! (`cx.reduce_motion()`) in the rare place that must choose different
//! content; [`refresh_motion_preference`] re-reads it, because macOS posts no
//! change notification GPUI can follow without a window.
//!
//! # Cost
//!
//! An animation asks GPUI to redraw the view that rendered it on the next
//! frame, and Xylo's workspace is one view, so every running animation
//! re-renders the workspace. Entrances are therefore one-shot and short, and
//! the looping decorations (shimmer, pulse, the slow-hint timer) are throttled
//! with [`Animation::with_max_fps`] or a single timer. Animate the small thing
//! that changed (a row, a popover, a chevron), never a whole list.
//!
//! GPUI has no transform for ordinary elements (only SVGs rotate and scale),
//! so a "scale" entrance is a fade with a short rise, and a slide offsets the
//! element with `top`/`left` while it stays in flow.

use std::f32::consts::FRAC_PI_2;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

use gpui_kit::base::{Transition, transition};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::{Icon, Sizable};
use gpui_kit::{prelude::*, *};

use crate::icons::{self, AppIcon};
use crate::ui::{faint, hover, muted, text_secondary, well};

/// Small state changes: hover feedback, a chevron turning, a value cross-fade.
pub const FAST: Duration = Duration::from_millis(120);
/// The default for things entering: rows, popovers, toasts.
pub const BASE: Duration = Duration::from_millis(180);
/// Larger movements: panels, expanding groups.
pub const SLOW: Duration = Duration::from_millis(240);
/// How long a wait runs before [`slow_hint`] says it is taking a while.
pub const SLOW_AFTER: Duration = Duration::from_secs(3);
/// The caption [`slow_hint`] shows once a wait passes [`SLOW_AFTER`].
pub const SLOW_HINT: &str = "Taking longer than usual…";

/// How far a [`scale_fade_in`] surface rises into place, in pixels.
const SURFACE_RISE: f32 = 4.;
/// Looping decorations never need more than this many redraws a second.
const AMBIENT_FPS: f32 = 30.;
/// One breath of [`pulse`].
const PULSE_PERIOD: Duration = Duration::from_millis(1600);
/// One sweep of the skeleton shimmer.
const SHIMMER_PERIOD: Duration = Duration::from_millis(1400);
/// The dimmest a pulsing element gets.
const PULSE_FLOOR: f32 = 0.35;
/// The dimmest a shimmering skeleton gets.
const SHIMMER_FLOOR: f32 = 0.55;

// ---------------------------------------------------------------------------
// Easing
// ---------------------------------------------------------------------------

/// Ease-out cubic for things entering: quick to respond, settles gently.
pub fn enter(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    1. - (1. - t).powi(3)
}

/// Ease-in cubic for things leaving: lingers a moment, then gets out of the way.
pub fn exit(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    t * t * t
}

/// Ease-in-out cubic for things that move or change value in place.
pub fn standard(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    if t < 0.5 {
        4. * t * t * t
    } else {
        1. - (-2. * t + 2.).powi(3) / 2.
    }
}

/// A breath that starts and ends at full strength and dips to `floor` halfway,
/// so a reduced-motion render (which samples the start) shows it fully lit.
fn breathe(floor: f32) -> impl Fn(f32) -> f32 {
    move |t| {
        let dip = 0.5 - 0.5 * (t * std::f32::consts::TAU).cos();
        1. - (1. - floor) * dip
    }
}

// ---------------------------------------------------------------------------
// Reduced motion
// ---------------------------------------------------------------------------

/// Re-reads the system "Reduce motion" setting. macOS posts no notification
/// GPUI can subscribe to, so call this when the window becomes active to pick
/// up a change made while Xylo was in the background.
pub fn refresh_motion_preference(cx: &mut App) {
    gpui_kit::base::apply_system_reduce_motion(cx);
}

// ---------------------------------------------------------------------------
// Entrances
// ---------------------------------------------------------------------------

/// Fades `element` in over [`BASE`] the first time `id` renders.
///
/// The animation belongs to the id: a new id (a new row, a reopened popover
/// with a fresh generation) plays it again, while re-rendering the same id
/// does not.
pub fn fade_in<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: IntoElement + Styled + 'static,
{
    element.with_animation(id, Animation::new(BASE).with_easing(enter), |this, t| {
        this.opacity(t)
    })
}

/// Fades `element` in while sliding it `from_px` pixels into place
/// (positive starts below and rises, negative starts above and drops).
///
/// For in-flow elements: the slide sets `relative` positioning and `top`, so
/// do not use it on an absolutely positioned element.
pub fn fade_slide_in<E>(id: impl Into<ElementId>, element: E, from_px: f32) -> AnimationElement<E>
where
    E: IntoElement + Styled + 'static,
{
    element.with_animation(
        id,
        Animation::new(BASE).with_easing(enter),
        move |this, t| this.relative().top(px(from_px * (1. - t))).opacity(t),
    )
}

/// Fades `element` in while sliding it horizontally `from_px` pixels into
/// place (positive starts to the right). Used for toasts and side panels.
/// Like [`fade_slide_in`], only for in-flow elements.
pub fn slide_in_x<E>(id: impl Into<ElementId>, element: E, from_px: f32) -> AnimationElement<E>
where
    E: IntoElement + Styled + 'static,
{
    element.with_animation(
        id,
        Animation::new(SLOW).with_easing(enter),
        move |this, t| this.relative().left(px(from_px * (1. - t))).opacity(t),
    )
}

/// The entrance for popovers and menus: a [`FAST`]-to-[`BASE`] fade with a
/// short rise, and a drop shadow that ramps in with it.
///
/// GPUI cannot scale an element, so the rise stands in for the zoom. The
/// helper owns the element's shadow: GPUI fades each primitive on its own, so
/// a full-strength shadow would show through the half-transparent surface as a
/// dark slab; ramping it by the cube of the fade keeps it hidden until the
/// surface is nearly opaque. Give the surface its background and border as
/// usual, and give its `id` a generation when the same popover reopens so the
/// entrance plays each time.
pub fn scale_fade_in<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: IntoElement + Styled + 'static,
{
    element.with_animation(id, Animation::new(BASE).with_easing(enter), |this, t| {
        this.relative()
            .top(px(SURFACE_RISE * (1. - t)))
            .opacity(t)
            .shadow(surface_shadow(t * t * t))
    })
}

/// A popover or menu panel that animates in with [`scale_fade_in`]: panel
/// fill, hairline border, 16 px corners and 12 px padding around `content`.
///
/// Use it as a toolkit `Popover`'s content with `.appearance(false)`, so the
/// panel the reader sees is this element and the whole surface (fill, border,
/// shadow and content) enters together. A closed popover stops rendering its
/// content, so the entrance replays on every open without a generation.
pub fn popover_surface(
    id: impl Into<ElementId>,
    content: impl IntoElement,
) -> AnimationElement<Div> {
    scale_fade_in(
        id,
        div()
            .bg(rgb(crate::ui::panel()))
            .border_1()
            .border_color(rgb(crate::ui::border()))
            .rounded(px(16.))
            .p(px(12.))
            .child(content),
    )
}

/// The resting elevation of a floating surface at `strength` (0 to 1) of its
/// full ink: a soft two-layer drop shadow.
pub fn surface_shadow(strength: f32) -> Vec<BoxShadow> {
    let strength = strength.clamp(0., 1.);
    let ink = |alpha: f32| hsla(0., 0., 0., alpha * strength);
    vec![
        BoxShadow::new(px(0.), px(8.), ink(0.10))
            .blur_radius(px(12.))
            .spread_radius(px(-4.)),
        BoxShadow::new(px(0.), px(2.), ink(0.08))
            .blur_radius(px(4.))
            .spread_radius(px(-2.)),
    ]
}

/// Fades `element` in again whenever `key` changes: a cross-fade for text or
/// values that are replaced in place (a phase label, a renamed title).
///
/// GPUI keeps no copy of the outgoing content, so the old value is replaced at
/// once and the new one fades up over [`FAST`] — short enough to read as a
/// cross-fade.
pub fn fade_on_change<E>(
    id: impl Into<SharedString>,
    key: &impl Hash,
    element: E,
) -> AnimationElement<E>
where
    E: IntoElement + Styled + 'static,
{
    element.with_animation(
        keyed_id(id, key),
        Animation::new(FAST).with_easing(enter),
        |this, t| this.opacity(0.35 + 0.65 * t),
    )
}

/// An element id that changes whenever `key` does: give an entrance one of
/// these to replay it for each new value (a new notice, a new error).
pub fn keyed_id(name: impl Into<SharedString>, key: &impl Hash) -> ElementId {
    ElementId::NamedInteger(name.into(), key_hash(key))
}

fn key_hash(key: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

/// A gentle, endless breath for small "this is live" indicators such as a
/// running dot. Throttled, and fully lit when motion is reduced.
pub fn pulse<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: IntoElement + Styled + 'static,
{
    element.with_animation(
        id,
        Animation::new(PULSE_PERIOD)
            .repeat_synced()
            .with_max_fps(AMBIENT_FPS)
            .with_easing(breathe(PULSE_FLOOR)),
        |this, t| this.opacity(t),
    )
}

/// The same emphasis an element gets when it needs attention as it appears
/// (an approval card): it fades and rises in, then its `accent` border glows
/// once and settles to `resting`.
pub fn emphasize_in<E>(
    id: impl Into<ElementId>,
    element: E,
    accent: Hsla,
    resting: Hsla,
) -> AnimationElement<E>
where
    E: IntoElement + Styled + 'static,
{
    element.with_animations(
        id,
        vec![
            Animation::new(BASE).with_easing(enter),
            Animation::new(Duration::from_millis(600)).with_easing(standard),
        ],
        move |this, step, t| match step {
            0 => this
                .relative()
                .top(px(6. * (1. - t)))
                .opacity(t)
                .border_color(accent),
            _ => this.border_color(accent.blend(resting.opacity(t))),
        },
    )
}

// ---------------------------------------------------------------------------
// Value transitions
// ---------------------------------------------------------------------------

/// Progress from 0 (closed) to 1 (open) for a disclosure keyed by `id`:
/// [`SLOW`] with ease-out while opening, [`BASE`] with ease-in while closing.
pub fn open_progress(
    id: impl Into<ElementId>,
    open: bool,
    window: &mut Window,
    cx: &mut App,
) -> f32 {
    let policy = if open {
        Transition::new(SLOW).ease(enter)
    } else {
        Transition::new(BASE).ease(exit)
    };
    transition(
        (id.into(), "open"),
        if open { 1. } else { 0. },
        policy,
        window,
        cx,
    )
}

/// The rotation, in radians, of a disclosure chevron at `progress`: pointing
/// right when closed, down when open.
pub fn chevron_angle(progress: f32) -> f32 {
    progress.clamp(0., 1.) * FRAC_PI_2
}

/// One expand/collapse control: its open state, its eased progress, and the
/// pieces that animate with it (the chevron and the revealed content).
///
/// ```ignore
/// let disclosure = Disclosure::new(("tool-group", key), expanded, window, cx);
/// header.child(disclosure.chevron());
/// if disclosure.is_visible() {
///     column.child(disclosure.reveal(build_rows(), window, cx));
/// }
/// ```
///
/// Settled open content renders at its natural height with no wrapper
/// effects, so a virtualised list that measures rows off screen sees the
/// true height. While moving, the content is clipped to its last measured
/// height times the progress and faded with it.
#[derive(Clone, Debug)]
pub struct Disclosure {
    id: ElementId,
    open: bool,
    progress: f32,
}

/// The natural height a [`Disclosure`] last measured for its content.
struct RevealHeight(Option<Pixels>);

impl Disclosure {
    /// Samples the disclosure keyed by `id` at `open`; call while rendering.
    pub fn new(id: impl Into<ElementId>, open: bool, window: &mut Window, cx: &mut App) -> Self {
        let id = id.into();
        let progress = open_progress(id.clone(), open, window, cx);
        Self { id, open, progress }
    }

    /// Whether the content should be built: open, or still closing.
    pub fn is_visible(&self) -> bool {
        self.open || self.progress > 0.
    }

    /// The small chevron for this disclosure, turned to match its progress.
    pub fn chevron(&self) -> Icon {
        icons::small(AppIcon::ChevronRight).rotate(radians(chevron_angle(self.progress)))
    }

    /// Wraps `content` so it reveals and hides with the disclosure.
    pub fn reveal(
        &self,
        content: impl IntoElement,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let measured = window.use_keyed_state((self.id.clone(), "reveal-height"), cx, |_, _| {
            RevealHeight(None)
        });
        let record = measured.clone();
        let remember = move |bounds: Vec<Bounds<Pixels>>, _: &mut Window, cx: &mut App| {
            if let Some(child) = bounds.first() {
                record.update(cx, |height, _| height.0 = Some(child.size.height));
            }
        };
        let inner = div()
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .child(content);
        if self.open && self.progress >= 1. {
            return div()
                .w_full()
                .child(inner)
                .on_children_prepainted(remember)
                .into_any_element();
        }
        let height = measured.read(cx).0.unwrap_or_default();
        div()
            .w_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .h(height * self.progress)
            .opacity(self.progress)
            .child(inner)
            .on_children_prepainted(remember)
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Skeletons
// ---------------------------------------------------------------------------

/// One placeholder line of text, `width` wide (pixels or a fraction via
/// `relative`). Shimmers gently; a muted static block under reduced motion.
pub fn skeleton_line(width: impl Into<Length>) -> AnimationElement<Div> {
    skeleton_block(width, px(10.))
}

/// A placeholder block of any size, with the same shimmer as [`skeleton_line`].
pub fn skeleton_block(width: impl Into<Length>, height: Pixels) -> AnimationElement<Div> {
    let width: Length = width.into();
    div()
        .w(width)
        .h(height)
        .flex_shrink_0()
        .rounded(px(4.))
        .bg(rgb(skeleton_fill()))
        .with_animation(
            "skeleton-shimmer",
            Animation::new(SHIMMER_PERIOD)
                .repeat_synced()
                .with_max_fps(AMBIENT_FPS)
                .with_easing(breathe(SHIMMER_FLOOR)),
            |this, t| this.opacity(t),
        )
}

fn skeleton_fill() -> u32 {
    if crate::ui::is_dark() {
        hover()
    } else {
        well()
    }
}

/// The fraction of the row width the `index`th skeleton row fills: varied so
/// a list of placeholders reads as text, and the same on every render so it
/// does not flicker.
pub fn skeleton_width(index: usize) -> f32 {
    const WIDTHS: [f32; 5] = [0.72, 0.54, 0.86, 0.62, 0.45];
    WIDTHS[index % WIDTHS.len()]
}

/// A placeholder list row: a small glyph block and one line of text, at the
/// height of a sidebar or file-tree row.
pub fn skeleton_row(index: usize) -> Div {
    div()
        .h(px(30.))
        .w_full()
        .px(px(10.))
        .flex()
        .items_center()
        .gap(px(10.))
        .child(skeleton_block(px(14.), px(14.)))
        .child(skeleton_line(relative(skeleton_width(index))))
}

/// `rows` placeholder rows for a list that is still loading.
pub fn skeleton_list(rows: usize) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(2.))
        .children((0..rows).map(skeleton_row))
}

/// A placeholder paragraph of `lines` lines, the last one shorter.
pub fn skeleton_paragraph(lines: usize) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(9.))
        .children((0..lines).map(|line| {
            let width = if line + 1 == lines {
                0.48
            } else {
                0.92 - 0.06 * (line % 3) as f32
            };
            skeleton_line(relative(width))
        }))
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// A small spinner for inline "working" states, in muted ink.
pub fn spinner_inline() -> Spinner {
    spinner(icons::SMALL).color(rgb(muted()).into())
}

/// One full turn of a [`Spinner`].
const SPIN_PERIOD: Duration = Duration::from_millis(800);

/// A spinner `size` across (an `icons` size token) that turns at an even
/// speed (GPUI's default eases each turn, which reads as stuttering at small
/// sizes).
pub fn spinner(size: Pixels) -> Spinner {
    Spinner { size, color: None }
}

/// A turning loading glyph. Every animation re-renders the workspace view,
/// so the turn is throttled to [`AMBIENT_FPS`] like the other looping
/// decorations; GPUI holds it still when motion is reduced.
#[derive(IntoElement)]
pub struct Spinner {
    size: Pixels,
    color: Option<Hsla>,
}

impl Spinner {
    /// The glyph's colour; it inherits the text colour otherwise.
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }
}

impl RenderOnce for Spinner {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div().flex_shrink_0().child(
            Icon::new(AppIcon::Loading)
                .size(self.size)
                .when_some(self.color, |icon, color| icon.text_color(color))
                .with_animation("spinner-turn", spin_animation(), |icon, delta| {
                    icon.transform(Transformation::rotate(percentage(delta)))
                }),
        )
    }
}

/// The spinner's endless, evenly paced, frame-capped turn.
fn spin_animation() -> Animation {
    Animation::new(SPIN_PERIOD)
        .repeat()
        .with_max_fps(AMBIENT_FPS)
        .with_easing(gpui_kit::linear)
}

/// Shows `button` as busy: disabled, with a spinner after its label. The
/// button keeps its width and accessible name, so nothing jumps and assistive
/// technology still knows what is running.
pub fn busy_button(button: gpui_kit::base::Button, busy: bool) -> gpui_kit::base::Button {
    if !busy {
        return button;
    }
    button
        .disabled(true)
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .child(spinner(icons::SMALL).color(rgb(text_secondary()).into()))
}

/// A thin determinate progress bar at `fraction` (0 to 1); value changes ease.
pub fn progress_bar(id: impl Into<ElementId>, fraction: f32) -> Progress {
    Progress::new(id)
        .value(fraction.clamp(0., 1.) * 100.)
        .color(rgb(crate::ui::brand()))
        .with_size(px(4.))
}

/// A thin bar for work whose progress is unknown. Under reduced motion it
/// holds still in the middle of its track.
pub fn indeterminate_bar(id: impl Into<ElementId>) -> Progress {
    Progress::new(id)
        .loading(true)
        .color(rgb(crate::ui::brand()))
        .with_size(px(3.))
}

/// A calm, centred loading state: a spinner over one caption ("Starting
/// Xylo…"), with the [`slow_hint`] below once the wait runs long.
pub fn loading_state(
    id: impl Into<SharedString>,
    caption: impl Into<SharedString>,
    started_at: Instant,
    window: &mut Window,
    cx: &mut App,
) -> Div {
    let id = id.into();
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(10.))
        .child(spinner(icons::LARGE).color(rgb(muted()).into()))
        .child(
            div()
                .text_size(px(13.))
                .text_color(rgb(text_secondary()))
                .child(caption.into()),
        )
        .children(slow_hint(id, started_at, window, cx))
}

/// When `id` was first rendered in its current unbroken run of frames: the
/// start of a wait that Xylo does not otherwise time ("Sending…"). The clock
/// restarts once the element stops rendering and comes back.
pub fn shown_since(id: impl Into<ElementId>, window: &mut Window, cx: &mut App) -> Instant {
    let key = ElementId::NamedChild(std::sync::Arc::new(id.into()), "shown-since".into());
    *window
        .use_keyed_state(key, cx, |_, _| Instant::now())
        .read(cx)
}

/// Whether a wait that began at `started_at` has run past [`SLOW_AFTER`].
pub fn is_slow(started_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(started_at) >= SLOW_AFTER
}

/// The "Taking longer than usual…" caption once a wait that began at
/// `started_at` passes [`SLOW_AFTER`], and `None` before that.
///
/// While the wait is still short it arms one timer (per `id` and start time)
/// that redraws the view when the threshold passes, so the caption appears on
/// time even if nothing else changes.
pub fn slow_hint(
    id: impl Into<SharedString>,
    started_at: Instant,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnimationElement<Div>> {
    let id = id.into();
    let now = Instant::now();
    if is_slow(started_at, now) {
        return Some(fade_in(
            ElementId::Name(format!("{id}-slow-hint").into()),
            div()
                .text_size(px(12.))
                .text_color(rgb(faint()))
                .child(SLOW_HINT),
        ));
    }
    let armed = window.use_keyed_state(
        ElementId::Name(format!("{id}-slow-timer").into()),
        cx,
        |_, _| None::<Instant>,
    );
    if *armed.read(cx) != Some(started_at) {
        armed.update(cx, |armed, _| *armed = Some(started_at));
        let remaining = SLOW_AFTER.saturating_sub(now.saturating_duration_since(started_at));
        let view = window.current_view();
        window
            .spawn(cx, async move |cx| {
                cx.background_executor().timer(remaining).await;
                cx.update(move |_, cx| cx.notify(view)).ok();
            })
            .detach();
    }
    None
}

#[cfg(test)]
#[path = "motion/tests.rs"]
mod tests;
