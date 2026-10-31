//! Settings design system tests: pure helpers, then rendered layout.
use super::{MONOGRAM_TINTS, Status, Tone, monogram_text, slow_caption, tint_index};
use std::time::{Duration, Instant};

fn luminance(color: u32) -> f64 {
    let channel = |shift: u32| {
        let value = f64::from((color >> shift) & 0xff) / 255.;
        if value <= 0.039_28 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
}
fn contrast(foreground: u32, background: u32) -> f64 {
    let (a, b) = (luminance(foreground), luminance(background));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[test]
fn monogram_uses_the_first_letters_of_the_first_two_words() {
    assert_eq!(monogram_text("ChatGPT"), "C");
    assert_eq!(monogram_text("Build iOS apps"), "BI");
    assert_eq!(monogram_text("my-plugin"), "MP");
    assert_eq!(monogram_text("  local model  "), "LM");
    assert_eq!(monogram_text("über tool"), "ÜT");
    assert_eq!(monogram_text("2fa helper"), "2H");
}

#[test]
fn monogram_without_letters_shows_a_question_mark() {
    assert_eq!(monogram_text(""), "?");
    assert_eq!(monogram_text("  -- / "), "?");
}

#[test]
fn tint_is_stable_and_spread_across_the_palette() {
    assert_eq!(tint_index("openai"), tint_index("openai"));
    assert!(tint_index("") < MONOGRAM_TINTS.len());
    let seeds = [
        "chatgpt",
        "openai",
        "anthropic",
        "gemini",
        "local",
        "custom",
        "ssh",
        "docker",
    ];
    let distinct: std::collections::BTreeSet<_> =
        seeds.iter().map(|seed| tint_index(seed)).collect();
    assert!(distinct.len() >= 4, "tints collapsed: {distinct:?}");
}

#[test]
fn every_monogram_tint_is_legible_in_both_themes() {
    for (light_bg, light_fg, dark_bg, dark_fg) in MONOGRAM_TINTS {
        assert!(
            contrast(light_fg, light_bg) >= 4.5,
            "{light_fg:06x} on {light_bg:06x}"
        );
        assert!(
            contrast(dark_fg, dark_bg) >= 4.5,
            "{dark_fg:06x} on {dark_bg:06x}"
        );
    }
}

#[test]
fn statuses_read_with_matching_tones() {
    assert_eq!(Status::Connected.tone(), Tone::Success);
    assert_eq!(Status::NeedsSetup.label(), "Needs setup");
    assert_eq!(Status::NeedsSetup.tone(), Tone::Warning);
    assert_eq!(Status::UpdateAvailable.tone(), Tone::Accent);
    assert_eq!(Status::Error.tone(), Tone::Danger);
    assert_eq!(Status::Disabled.tone(), Tone::Neutral);
}

#[test]
fn the_slow_caption_appears_only_after_the_threshold() {
    assert!(slow_caption(Instant::now()).is_none());
    let started = Instant::now() - crate::motion::SLOW_AFTER - Duration::from_millis(10);
    assert!(slow_caption(started).is_some());
}

mod layout {
    // Named imports only: a glob of the parent would bring gpui's `test`
    // attribute into scope and make `#[test]` expand recursively.
    use super::super::{
        CONTENT_GUTTER, CONTENT_MAX_WIDTH, CONTROL_HEIGHT, Feedback, LIST_ITEM_HEIGHT, MenuAction,
        NavGroup, ROW_MIN_HEIGHT, SIDEBAR_WIDTH, SelectOption, Status, Tone, breadcrumb, busy,
        caption, crumb, danger_zone, destructive_button, empty_state, field, form_actions,
        form_column, inline_error, list_container, list_item, loading_list, monogram,
        overflow_menu, pill, primary_button, radio_list, radio_option, rows, secondary_button,
        section, segmented, select, setting_row, settings_nav_item, settings_page,
        settings_sidebar, skeleton_list_item, status_pill, switch, text_button, value_text,
    };
    use crate::icons::AppIcon;
    use crate::ui::set_dark;
    use gpui_kit::prelude::*;
    use gpui_kit::{
        Bounds, Context, Pixels, TestAppContext, VisualTestContext, Window, div, px, size,
    };

    /// Every primitive in one settings window, with debug selectors on
    /// the parts whose geometry the design fixes.
    struct Gallery;
    impl Render for Gallery {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let theme = select(
                "gallery-theme",
                "Theme",
                vec![
                    SelectOption::new("Light", true, |_, _| {}),
                    SelectOption::new("Dark", false, |_, _| {}),
                ],
            );
            let rows_section = section(
                "Appearance",
                Some("How the workspace looks.".into()),
                rows([
                    div()
                        .debug_selector(|| "row".into())
                        .child(setting_row(
                            "Theme",
                            Some("Choose the appearance of your workspace.".into()),
                            Some(
                                div()
                                    .debug_selector(|| "control".into())
                                    .child(theme)
                                    .into_any_element(),
                            ),
                        ))
                        .into_any_element(),
                    setting_row(
                        "Notifications",
                        None,
                        Some(switch("gallery-switch", "Notifications", true).into_any_element()),
                    )
                    .feedback(Some(Feedback::Error("Could not save.".into())))
                    .into_any_element(),
                ]),
            );
            let list = list_container([
                div()
                    .debug_selector(|| "item".into())
                    .child(
                        list_item(
                            "gallery-openai",
                            Some(monogram("OpenAI", "openai").into_any_element()),
                            "OpenAI",
                            Some("2 models".into()),
                        )
                        .hover_detail("openai")
                        .title_tag("Trusted", Tone::Success)
                        .toned_tag("Not used automatically", Tone::Warning)
                        .tags(["Skills".into(), "MCP".into()])
                        .trailing(status_pill(Status::Connected))
                        .trailing(overflow_menu(
                            "gallery-more",
                            "More actions for OpenAI",
                            vec![MenuAction::new("Remove", |_, _| {})],
                        ))
                        .on_click(|_, _, _| {}),
                    )
                    .into_any_element(),
                skeleton_list_item(true).into_any_element(),
            ]);
            let page = settings_page("gallery-page", "Providers")
                .description("Models and accounts Xylo can use.")
                .breadcrumb(breadcrumb(
                    [crumb("gallery-crumb", "Settings")],
                    "Providers",
                ))
                .action(secondary_button("gallery-secondary", "Install from folder"))
                .action(primary_button("gallery-primary", "Add provider"))
                .child(
                    div()
                        .debug_selector(|| "section".into())
                        .child(rows_section),
                )
                .child(section("Connections", None, list))
                .child(radio_list(
                    "gallery-presets",
                    [
                        radio_option("gallery-ask", "Ask", "Ask before acting.", true),
                        radio_option("gallery-full", "Full access", "Never ask.", false)
                            .caution(true),
                    ],
                ))
                .child(empty_state(
                    AppIcon::Folder,
                    "No environments",
                    "Add an SSH host or container to run commands there.",
                    Some(primary_button("gallery-empty", "New environment").into_any_element()),
                ))
                .child(form_column([
                    field(
                        "Name",
                        div().h(px(CONTROL_HEIGHT)),
                        Some("Shown in lists.".into()),
                    )
                    .into_any_element(),
                    field("Host", div().h(px(CONTROL_HEIGHT)), None).into_any_element(),
                    form_actions([
                        text_button("gallery-cancel", "Cancel").into_any_element(),
                        primary_button("gallery-save", "Save").into_any_element(),
                    ])
                    .into_any_element(),
                ]))
                .child(caption("Updated 5 min ago"))
                .child(segmented(
                    "gallery-tabs",
                    vec![
                        SelectOption::new("Discover", true, |_, _| {}).id("gallery-discover"),
                        SelectOption::new("Installed (2)", false, |_, _| {}),
                    ],
                ))
                .child(value_text("1.4.2"))
                .child(inline_error("Could not reach the server."))
                .child(loading_list(2, None))
                .child(busy(primary_button("gallery-busy", "Install"), true))
                .child(pill("Disabled after update", Tone::Warning))
                .child(danger_zone([setting_row(
                    "Reset settings",
                    Some("Restore every preference to its default.".into()),
                    Some(destructive_button("gallery-reset", "Reset").into_any_element()),
                )
                .into_any_element()]));
            let sidebar = settings_sidebar(
                div().h(px(52.)),
                vec![
                    NavGroup::untitled().item(settings_nav_item(
                        "gallery-nav-general",
                        "General",
                        AppIcon::General,
                        false,
                    )),
                    NavGroup::new("Models & accounts").item(settings_nav_item(
                        "gallery-nav-providers",
                        "Providers",
                        AppIcon::Model,
                        true,
                    )),
                ],
            );
            // The app shell places the sidebar beside the page the same way.
            div()
                .size_full()
                .flex()
                .child(
                    div()
                        .debug_selector(|| "sidebar".into())
                        .h_full()
                        .child(sidebar),
                )
                .child(div().flex_1().min_w_0().h_full().child(page))
        }
    }

    fn gallery(cx: &mut TestAppContext, width: f32) -> &mut VisualTestContext {
        cx.update(gpui_kit::init);
        // Entrances finish at once, so geometry is measured at rest.
        cx.update(|cx| cx.set_reduce_motion(true));
        let (_, cx) = cx.add_window_view(|_, _| Gallery);
        cx.simulate_resize(size(px(width), px(2400.)));
        cx.run_until_parked();
        cx
    }
    fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} was not drawn"))
    }

    #[gpui_kit::test]
    fn the_content_column_sits_beside_the_sidebar_at_its_maximum_width(cx: &mut TestAppContext) {
        let cx = gallery(cx, 1400.);
        assert_eq!(bounds(cx, "sidebar").size.width, px(SIDEBAR_WIDTH));
        let section = bounds(cx, "section");
        assert_eq!(section.origin.x, px(SIDEBAR_WIDTH + CONTENT_GUTTER));
        assert_eq!(section.size.width, px(CONTENT_MAX_WIDTH));
    }

    #[gpui_kit::test]
    fn rows_keep_their_height_and_right_align_controls_in_a_narrow_window(cx: &mut TestAppContext) {
        let cx = gallery(cx, 800.);
        let section = bounds(cx, "section");
        assert_eq!(
            section.size.width,
            px(800. - SIDEBAR_WIDTH - 2. * CONTENT_GUTTER)
        );
        let row = bounds(cx, "row");
        assert!(row.size.height >= px(ROW_MIN_HEIGHT), "{row:?}");
        let control = bounds(cx, "control");
        assert_eq!(control.right(), section.right());
        assert!(control.size.height >= px(CONTROL_HEIGHT), "{control:?}");
        let item = bounds(cx, "item");
        assert_eq!(item.size.height, px(LIST_ITEM_HEIGHT));
    }

    #[gpui_kit::test]
    fn the_dark_theme_renders_every_primitive(cx: &mut TestAppContext) {
        set_dark(true);
        let cx = gallery(cx, 1000.);
        let item = bounds(cx, "item");
        set_dark(false);
        assert_eq!(item.size.height, px(LIST_ITEM_HEIGHT));
    }
}
