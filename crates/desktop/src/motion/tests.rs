//! Motion helpers: timing and easing contracts, then rendered behaviour under
//! normal and reduced motion.
use super::{
    BASE, Disclosure, FAST, SLOW, SLOW_AFTER, breathe, chevron_angle, enter, exit, fade_in,
    fade_slide_in, is_slow, key_hash, scale_fade_in, skeleton_list, skeleton_width, spinner_inline,
    standard,
};
use gpui_kit::{
    Context, IntoElement, ParentElement, Render, Styled, TestAppContext, Window, div, px, size,
};
use std::cell::Cell;
use std::f32::consts::FRAC_PI_2;
use std::rc::Rc;
use std::time::{Duration, Instant};

#[test]
fn durations_stay_short_and_ordered() {
    assert_eq!(FAST, Duration::from_millis(120));
    assert_eq!(BASE, Duration::from_millis(180));
    assert_eq!(SLOW, Duration::from_millis(240));
    assert!(FAST < BASE && BASE < SLOW);
}

#[test]
fn easings_start_at_zero_end_at_one_and_never_overshoot() {
    for easing in [enter, exit, standard] {
        assert_eq!(easing(0.), 0.);
        assert!((easing(1.) - 1.).abs() < f32::EPSILON);
        let mut previous = 0.;
        for step in 0..=100 {
            let value = easing(step as f32 / 100.);
            assert!((0. ..=1.).contains(&value), "{value} escaped 0..=1");
            assert!(
                value >= previous,
                "easing must not move backwards (no bounce)"
            );
            previous = value;
        }
        // Out-of-range time is clamped rather than extrapolated.
        assert_eq!(easing(-1.), 0.);
        assert!((easing(2.) - 1.).abs() < f32::EPSILON);
    }
}

#[test]
fn entering_responds_quickly_and_leaving_lingers() {
    assert!(
        enter(0.25) > 0.5,
        "ease-out should cover most of the way early"
    );
    assert!(exit(0.25) < 0.05, "ease-in should barely move at first");
    assert!((standard(0.5) - 0.5).abs() < 1e-6);
}

#[test]
fn breathing_rests_fully_lit_and_dips_to_its_floor_halfway() {
    let breath = breathe(0.4);
    assert!((breath(0.) - 1.).abs() < 1e-6);
    assert!((breath(0.5) - 0.4).abs() < 1e-6);
    assert!((breath(1.) - 1.).abs() < 1e-5);
}

#[test]
fn chevron_turns_a_quarter_turn_from_closed_to_open() {
    assert_eq!(chevron_angle(0.), 0.);
    assert!((chevron_angle(1.) - FRAC_PI_2).abs() < 1e-6);
    assert!((chevron_angle(4.) - FRAC_PI_2).abs() < 1e-6);
}

#[test]
fn the_slow_hint_waits_three_seconds() {
    let start = Instant::now();
    assert!(!is_slow(start, start));
    assert!(!is_slow(start, start + Duration::from_millis(2_999)));
    assert!(is_slow(start, start + SLOW_AFTER));
    // A clock that reads earlier than the start never counts as slow.
    assert!(!is_slow(start + Duration::from_secs(10), start));
}

#[test]
fn skeleton_widths_vary_but_repeat_deterministically() {
    let widths: Vec<f32> = (0..10).map(skeleton_width).collect();
    assert!(widths.iter().all(|width| (0.3..=0.9).contains(width)));
    assert_ne!(widths[0], widths[1]);
    assert_eq!(widths[0..5], widths[5..10]);
}

#[test]
fn a_change_key_produces_a_new_animation_identity() {
    assert_eq!(key_hash(&"Reading files"), key_hash(&"Reading files"));
    assert_ne!(key_hash(&"Reading files"), key_hash(&"Editing files"));
}

/// Renders a representative set of helpers and records whether the
/// disclosure built its content for the current `open` state.
struct Gallery {
    open: bool,
    collapsed: Rc<Cell<Option<bool>>>,
}

impl Render for Gallery {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let disclosure = Disclosure::new("group", self.open, window, cx);
        let content = disclosure
            .is_visible()
            .then(|| disclosure.reveal(div().h(px(40.)), window, cx));
        self.collapsed.set(Some(content.is_some()));
        div()
            .size_full()
            .child(fade_in("fade", div().h(px(10.))))
            .child(fade_slide_in("slide", div().h(px(10.)), 8.))
            .child(scale_fade_in("surface", div().h(px(10.))))
            .child(disclosure.chevron())
            .child(skeleton_list(3))
            .child(spinner_inline())
            .children(content)
    }
}

fn open_gallery(
    cx: &mut TestAppContext,
    reduce_motion: bool,
) -> (gpui_kit::WindowHandle<Gallery>, Rc<Cell<Option<bool>>>) {
    cx.update(gpui_kit::init);
    cx.update(|cx| cx.set_reduce_motion(reduce_motion));
    let collapsed = Rc::new(Cell::new(None));
    let window = cx.open_window(size(px(400.), px(400.)), {
        let collapsed = collapsed.clone();
        move |_, _| Gallery {
            open: false,
            collapsed,
        }
    });
    cx.run_until_parked();
    (window, collapsed)
}

#[gpui_kit::test]
fn reduced_motion_renders_settled_and_requests_no_frames(cx: &mut TestAppContext) {
    let (window, _) = open_gallery(cx, true);
    let frames = window
        .update(cx, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    assert_eq!(frames, 0, "reduced motion must not keep redrawing");
}

#[gpui_kit::test]
fn entrances_and_loaders_animate_with_motion_on(cx: &mut TestAppContext) {
    let (window, _) = open_gallery(cx, false);
    let frames = window
        .update(cx, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    assert!(frames > 0, "entrances should ask for the next frame");
}

#[gpui_kit::test]
fn a_closed_disclosure_mounts_nothing_and_opening_mounts_its_content(cx: &mut TestAppContext) {
    let (window, collapsed) = open_gallery(cx, true);
    assert_eq!(collapsed.get(), Some(false));
    window
        .update(cx, |gallery, _, cx| {
            gallery.open = true;
            cx.notify();
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(collapsed.get(), Some(true));
}

#[test]
fn the_spinner_redraws_at_the_ambient_rate_not_every_display_frame() {
    // Each animation frame re-renders the whole workspace view, so a spinner
    // that runs for a whole turn must be capped like the other loops.
    let turn = super::spin_animation();
    assert_eq!(turn.max_fps, Some(super::AMBIENT_FPS));
}
