//! The settings design system: layout, rows, lists and controls shared by
//! every settings destination.
//!
//! Pages are a left settings sidebar plus one left-aligned content column.
//! Inside the column a page is a header (optional breadcrumb, title, one-line
//! description) followed by sections; a section is a heading and a run of
//! setting rows separated by hairlines. Bordered containers are reserved for
//! grouped lists (providers, environments, plugins, approval rules). The
//! measurements live in the constants below and are written up for page
//! authors in `SETTINGS_DESIGN.md`; colours come from `crate::ui` so both
//! themes follow the shared palette.
//!
//! Every clipped label uses [`SingleLine::single_line`] inside a row that
//! centres its items, so descenders never clip. Builders that render text
//! inside a button draw their own single-line child rather than the toolkit
//! button label, whose 1.25 line box is too short.

use super::*;
use crate::icons::{self, AppIcon};
use crate::motion;
use gpui_kit::TestSupportExt;
use gpui_kit::base::{Radio, RadioGroup};
use gpui_kit::component::button::Button as MenuButton;
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// A click handler stored by a builder until render.
type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
/// A handler for a menu choice, select option or radio preset.
type ChoiceHandler = Rc<dyn Fn(&mut Window, &mut App)>;

// ---------------------------------------------------------------------------
// Scale
// ---------------------------------------------------------------------------

/// Width of the settings navigation sidebar.
pub(super) const SIDEBAR_WIDTH: f32 = 220.;
/// Height of one settings navigation row.
pub(super) const NAV_ROW_HEIGHT: f32 = 32.;
/// Widest the content column grows; it stays left-aligned beyond this.
pub(super) const CONTENT_MAX_WIDTH: f32 = 680.;
/// Horizontal padding between the sidebar edge and the content column.
pub(super) const CONTENT_GUTTER: f32 = 48.;
/// Space above the page header.
pub(super) const CONTENT_TOP: f32 = 32.;
/// Vertical space between sections (and between the header and the first).
pub(super) const SECTION_GAP: f32 = 32.;
/// Minimum height of a setting row; descriptions may grow it.
pub(super) const ROW_MIN_HEIGHT: f32 = 56.;
/// Vertical padding inside a setting row.
pub(super) const ROW_PADDING_Y: f32 = 13.;
/// Fixed height of a list item, so long lists can be virtualized with
/// `uniform_list` at exactly this item size.
pub(super) const LIST_ITEM_HEIGHT: f32 = 56.;
/// Horizontal padding of rows inside a bordered list container.
pub(super) const LIST_PADDING_X: f32 = 14.;
/// Height of every compact control: buttons, selects, search fields.
pub(super) const CONTROL_HEIGHT: f32 = 32.;
/// Side of the monogram tile that leads a list item.
pub(super) const MONOGRAM_SIZE: f32 = 28.;
/// Corner radius of bordered list containers.
pub(super) const CONTAINER_RADIUS: f32 = 10.;

/// Page title size.
pub(super) const TITLE_SIZE: f32 = 22.;
/// Section heading size.
pub(super) const SECTION_TITLE_SIZE: f32 = 14.;
/// Row and list item label size.
pub(super) const LABEL_SIZE: f32 = 14.;
/// Description size under titles, labels and fields.
pub(super) const DESCRIPTION_SIZE: f32 = 13.;
/// Captions, pills, chips, breadcrumbs and sidebar group headers.
pub(super) const CAPTION_SIZE: f32 = 12.;

fn label_text(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(LABEL_SIZE))
        .line_height(relative(1.4))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(crate::ui::text()))
        .child(text.into())
}
fn description_text(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(DESCRIPTION_SIZE))
        .line_height(relative(1.45))
        .text_color(rgb(muted()))
        .child(text.into())
}

// ---------------------------------------------------------------------------
// Shell and navigation
// ---------------------------------------------------------------------------

/// A titled group of navigation rows in the settings sidebar.
pub(super) struct NavGroup {
    label: Option<SharedString>,
    items: Vec<AnyElement>,
}
impl NavGroup {
    /// A group under a small muted header.
    pub(super) fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: Some(label.into()),
            items: Vec::new(),
        }
    }
    /// A group without a header, for the first rows of the sidebar.
    pub(super) fn untitled() -> Self {
        Self {
            label: None,
            items: Vec::new(),
        }
    }
    /// Appends a row, normally a [`settings_nav_item`].
    pub(super) fn item(mut self, item: impl IntoElement) -> Self {
        self.items.push(item.into_any_element());
        self
    }
}

/// The settings sidebar: an optional header (back link, "Settings") above
/// grouped navigation rows. It scrolls so every destination stays reachable in
/// short windows.
pub(super) fn settings_sidebar(header: impl IntoElement, groups: Vec<NavGroup>) -> Stateful<Div> {
    let mut sidebar = div()
        .id("settings-navigation")
        .w(px(SIDEBAR_WIDTH))
        .flex_shrink_0()
        .h_full()
        .overflow_y_scroll()
        .bg(rgb(crate::ui::sidebar()))
        .border_r_1()
        .border_color(rgb(border()))
        .px(px(8.))
        .pb(px(12.))
        .flex()
        .flex_col()
        .child(header);
    for (index, group) in groups.into_iter().enumerate() {
        let mut column = div()
            .flex()
            .flex_col()
            .gap(px(1.))
            .when(index > 0, |column| column.mt(px(16.)));
        if let Some(label) = group.label {
            column = column.child(
                div()
                    .h(px(28.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .text_size(px(CAPTION_SIZE))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(muted()))
                    .child(div().min_w_0().single_line().child(label)),
            );
        }
        sidebar = sidebar.child(column.children(group.items));
    }
    sidebar
}

/// One settings destination: icon and label in a 32 px row; the active row
/// carries a subtle fill.
pub(super) fn settings_nav_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    glyph: AppIcon,
    active: bool,
) -> base::Button {
    let label: SharedString = label.into();
    focus_ring(base::Button::new(id))
        .accessibility_label(label.clone())
        .w_full()
        .h(px(NAV_ROW_HEIGHT))
        .px(px(10.))
        .rounded(px(7.))
        .flex()
        .items_center()
        .gap(px(10.))
        .text_size(px(13.5))
        .font_weight(if active {
            FontWeight::MEDIUM
        } else {
            FontWeight::NORMAL
        })
        .text_color(rgb(if active { text() } else { text_secondary() }))
        .when(active, |row| row.bg(rgb(selected())))
        .cursor_pointer()
        .hover(move |row| {
            if active {
                row
            } else {
                row.bg(rgb(hover())).text_color(rgb(text()))
            }
        })
        .active(|row| row.bg(rgb(pressed())))
        .child(icons::icon(glyph).text_color(rgb(if active {
            icons::hover_color()
        } else {
            icons::rest_color()
        })))
        .child(div().flex_1().min_w_0().single_line().child(label))
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

/// A settings page: scrolls as a whole, with the header and sections in one
/// left-aligned column. Children are sections; the page spaces them.
#[derive(IntoElement)]
pub(super) struct SettingsPage {
    id: ElementId,
    title: SharedString,
    description: Option<SharedString>,
    breadcrumb: Option<AnyElement>,
    actions: Vec<AnyElement>,
    children: Vec<AnyElement>,
}
/// Starts a page with its title. `id` keys the page's scroll position.
pub(super) fn settings_page(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
) -> SettingsPage {
    SettingsPage {
        id: id.into(),
        title: title.into(),
        description: None,
        breadcrumb: None,
        actions: Vec::new(),
        children: Vec::new(),
    }
}
impl SettingsPage {
    /// The one-line muted description under the title.
    pub(super) fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }
    /// The trail above the title on sub-pages; see [`breadcrumb`].
    pub(super) fn breadcrumb(mut self, trail: impl IntoElement) -> Self {
        self.breadcrumb = Some(trail.into_any_element());
        self
    }
    /// A page-level action shown right of the title (at most one primary).
    pub(super) fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }
}
impl ParentElement for SettingsPage {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl RenderOnce for SettingsPage {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        // Each page (and each sub-page, which differs by title) plays its own
        // entrance: top-level pages rise into place, sub-pages slide in from
        // the breadcrumb side.
        let entrance_id: SharedString = format!("{:?}-{}-in", self.id, self.title).into();
        let sub_page = self.breadcrumb.is_some();
        let mut heading = div().flex().items_start().gap(px(16.)).child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(px(TITLE_SIZE))
                        .line_height(relative(1.3))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(text()))
                        .child(self.title),
                )
                .when_some(self.description, |column, description| {
                    column.child(
                        div()
                            .text_size(px(13.5))
                            .line_height(relative(1.5))
                            .text_color(rgb(muted()))
                            .child(description),
                    )
                }),
        );
        if !self.actions.is_empty() {
            heading = heading.child(
                div()
                    .flex_shrink_0()
                    .pt(px(2.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .children(self.actions),
            );
        }
        let header = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .when_some(self.breadcrumb, |header, trail| header.child(trail))
            .child(heading);
        let column = div()
            .w_full()
            .max_w(px(CONTENT_MAX_WIDTH))
            .flex()
            .flex_col()
            .gap(px(SECTION_GAP))
            .child(header)
            .children(self.children);
        let entrance = if sub_page {
            motion::slide_in_x(entrance_id, column, 16.).into_any_element()
        } else {
            motion::fade_slide_in(entrance_id, column, 6.).into_any_element()
        };
        div()
            .id(self.id)
            .test_support()
            .size_full()
            .overflow_y_scroll()
            .px(px(CONTENT_GUTTER))
            .pt(px(CONTENT_TOP))
            .pb(px(64.))
            .child(entrance)
    }
}

/// A link back to a parent page inside a [`breadcrumb`].
pub(super) fn crumb(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(format!("Back to {label}"))
        .flex_shrink_0()
        .px(px(4.))
        .mx(px(-4.))
        .rounded(px(5.))
        .text_color(rgb(muted()))
        .cursor_pointer()
        .hover(|link| link.text_color(rgb(text())))
        .child(div().single_line().child(label))
}
/// The sub-page trail, "Providers / ChatGPT": parent [`crumb`]s then the
/// current page. It sits above the title, aligned with its left edge.
pub(super) fn breadcrumb(
    parents: impl IntoIterator<Item = gpui_kit::base::Button>,
    current: impl Into<SharedString>,
) -> Div {
    let mut trail = div()
        .flex()
        .items_center()
        .gap(px(6.))
        .min_w_0()
        .text_size(px(DESCRIPTION_SIZE));
    for parent in parents {
        trail = trail
            .child(parent)
            .child(div().text_color(rgb(faint())).child("/"));
    }
    trail.child(
        div()
            .min_w_0()
            .single_line()
            .text_color(rgb(text_secondary()))
            .child(current.into()),
    )
}

// ---------------------------------------------------------------------------
// Sections and rows
// ---------------------------------------------------------------------------

/// A titled run of content: a 14 px heading, an optional muted description,
/// then the body (usually [`rows`] or a [`list_container`]).
pub(super) fn section(
    title: impl Into<SharedString>,
    description: Option<SharedString>,
    body: impl IntoElement,
) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_size(px(SECTION_TITLE_SIZE))
                .line_height(relative(1.4))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(text()))
                .child(title.into()),
        )
        .when_some(description, |section, description| {
            section.child(description_text(description))
        })
        .child(div().mt(px(6.)).child(body))
}

/// Setting rows stacked with a hairline between neighbours (none above the
/// first or below the last).
pub(super) fn rows(items: impl IntoIterator<Item = AnyElement>) -> Div {
    let mut column = div().w_full().flex().flex_col();
    for (index, item) in items.into_iter().enumerate() {
        if index > 0 {
            column = column.child(row_divider());
        }
        column = column.child(item);
    }
    column
}

/// The hairline between rows.
pub(super) fn row_divider() -> Div {
    div().w_full().h(px(1.)).flex_shrink_0().bg(rgb(border()))
}

/// Inline outcome of a row's last change, shown under its description.
#[derive(Clone)]
pub(super) enum Feedback {
    /// The change is in flight.
    Pending(SharedString),
    /// The change was saved.
    Saved(SharedString),
    /// The change failed; the message says what to do next.
    Error(SharedString),
}
/// The feedback line, announced as a status; `owner` (the row or field
/// label) keys its accessibility node.
fn feedback_line(owner: &str, feedback: Feedback) -> Stateful<Div> {
    let (glyph, color, message) = match feedback {
        Feedback::Pending(message) => (None, muted(), message),
        Feedback::Saved(message) => (Some(AppIcon::Success), success(), message),
        Feedback::Error(message) => (Some(AppIcon::Error), danger(), message),
    };
    div()
        .id(SharedString::from(format!("feedback-{owner}")))
        .role(Role::Status)
        .aria_label(message.clone())
        .mt(px(4.))
        .flex()
        .items_start()
        .gap(px(6.))
        .text_size(px(12.5))
        .line_height(relative(1.45))
        .text_color(rgb(color))
        .when_some(glyph, |line, glyph| {
            line.child(icons::small(glyph).mt(px(2.)).text_color(rgb(color)))
        })
        .child(div().flex_1().min_w_0().child(message))
}

/// A page- or section-level failure: a danger notice in product wording.
/// Row-level failures use [`SettingRow::feedback`] instead.
pub(super) fn inline_error(error: &str) -> Div {
    notice(BadgeTone::Danger, product_error(error))
}

impl Xylo {
    /// The one destructive confirmation: Cancel first, the destructive action
    /// second. `confirmed` runs only when the destructive action is chosen.
    pub(super) fn confirm_destructive(
        &mut self,
        title: &str,
        detail: &str,
        action: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
        confirmed: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let answer = window.prompt(
            PromptLevel::Warning,
            title,
            Some(detail),
            &[PromptButton::cancel("Cancel"), PromptButton::new(action)],
            cx,
        );
        cx.spawn_in(window, async move |view, cx| {
            if matches!(answer.await, Ok(1)) {
                let _ = view.update_in(cx, |view, window, cx| confirmed(view, window, cx));
            }
        })
        .detach();
    }
}

/// A setting: label and description on the left, the control right-aligned
/// and vertically centred. Rows have no horizontal padding of their own; they
/// align with the section heading.
#[derive(IntoElement)]
pub(super) struct SettingRow {
    label: SharedString,
    description: Option<SharedString>,
    control: Option<AnyElement>,
    feedback: Option<Feedback>,
    inset: bool,
}
/// A row with an optional description and control.
pub(super) fn setting_row(
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    control: Option<AnyElement>,
) -> SettingRow {
    SettingRow {
        label: label.into(),
        description,
        control,
        feedback: None,
        inset: false,
    }
}
impl SettingRow {
    /// Shows the outcome of the row's last change under its description.
    pub(super) fn feedback(mut self, feedback: Option<Feedback>) -> Self {
        self.feedback = feedback;
        self
    }
    /// Pads the row horizontally for use inside a [`list_container`].
    pub(super) fn inset(mut self) -> Self {
        self.inset = true;
        self
    }
}
impl RenderOnce for SettingRow {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .w_full()
            .min_h(px(ROW_MIN_HEIGHT))
            .py(px(ROW_PADDING_Y))
            .when(self.inset, |row| row.px(px(LIST_PADDING_X)))
            .flex()
            .items_center()
            .gap(px(24.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(label_text(self.label.clone()))
                    .when_some(self.description, |text, description| {
                        text.child(description_text(description).line_clamp(2))
                    })
                    .when_some(self.feedback, |text, feedback| {
                        text.child(feedback_line(&self.label, feedback))
                    }),
            )
            .when_some(self.control, |row, control| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(control),
                )
            })
    }
}

/// The final section of a page for irreversible actions: rows inside a
/// danger-tinted container. Each row's action is a [`destructive_button`]
/// whose handler confirms first (`Xylo::confirm_destructive`, Cancel first).
pub(super) fn danger_zone(body: impl IntoIterator<Item = AnyElement>) -> Div {
    section(
        "Danger zone",
        None,
        container_frame()
            .border_color(rgba((danger() << 8) | 0x55))
            .child(rows(body)),
    )
}

// ---------------------------------------------------------------------------
// Lists
// ---------------------------------------------------------------------------

fn container_frame() -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .rounded(px(CONTAINER_RADIUS))
        .border_1()
        .border_color(rgb(border()))
        .bg(rgb(panel()))
        .overflow_hidden()
}
/// A bordered container for grouped items, with hairlines between them. For
/// hundreds of items put a `uniform_list` of [`ListItem`]s (each
/// [`ListItem::divider`] after the first) inside [`list_frame`] instead.
pub(super) fn list_container(items: impl IntoIterator<Item = AnyElement>) -> Div {
    container_frame().child(rows(items))
}
/// The empty bordered frame of a [`list_container`], for virtualized lists.
pub(super) fn list_frame() -> Div {
    container_frame()
}

/// The letters shown on a monogram: the first letter of the first two words,
/// upper-cased; "?" when the name has no letters or digits.
pub(super) fn monogram_text(name: &str) -> String {
    let mut initials = name
        .split(|c: char| !c.is_alphanumeric())
        .filter_map(|word| word.chars().next());
    match (initials.next(), initials.next()) {
        (None, _) => "?".into(),
        (Some(first), None) => first.to_uppercase().collect(),
        (Some(first), Some(second)) => first.to_uppercase().chain(second.to_uppercase()).collect(),
    }
}

/// Monogram tints as (light background, light text, dark background, dark
/// text). Each text colour meets 4.5:1 on its background.
const MONOGRAM_TINTS: [(u32, u32, u32, u32); 8] = [
    (0xe6e7fd, 0x3f43c4, 0x2a2c52, 0xb4b6ff),
    (0xdcf3ee, 0x0e6b57, 0x173a33, 0x7fdcc4),
    (0xfbefd6, 0x7f4f00, 0x3a2f17, 0xf2c56b),
    (0xfbe3e6, 0xa02f46, 0x41222a, 0xff9fb0),
    (0xdff0fb, 0x1f6391, 0x172f40, 0x8dc8f0),
    (0xefe2fa, 0x6c33a0, 0x33224a, 0xd0a6f5),
    (0xe5f2dc, 0x3c661c, 0x24331a, 0xa9d68a),
    (0xe7eaf0, 0x3b4352, 0x262b36, 0xbcc4d3),
];
/// Picks the tint for `seed` (a stable id, not the display name, so renaming
/// an item keeps its colour). FNV-1a keeps the choice identical across runs.
pub(super) fn tint_index(seed: &str) -> usize {
    let hash = seed.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    hash as usize % MONOGRAM_TINTS.len()
}
/// The rounded-square tile that leads a list item: the item's initials on a
/// tint chosen from `seed`. Used instead of brand logos.
pub(super) fn monogram(text: &str, seed: &str) -> Div {
    let (light_bg, light_fg, dark_bg, dark_fg) = MONOGRAM_TINTS[tint_index(seed)];
    let dark = is_dark();
    let letters = monogram_text(text);
    div()
        .size(px(MONOGRAM_SIZE))
        .flex_shrink_0()
        .rounded(px(7.))
        .bg(rgb(if dark { dark_bg } else { light_bg }))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(if letters.chars().count() > 1 {
            11.5
        } else {
            13.
        }))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(if dark { dark_fg } else { light_fg }))
        .child(letters)
}

/// A grouped item: monogram, title with a muted subtitle, then trailing
/// status and actions. With [`ListItem::on_click`] the leading part becomes
/// a button that opens the item and a chevron is shown; trailing controls stay
/// siblings so their clicks never open the item.
#[derive(IntoElement)]
pub(super) struct ListItem {
    id: ElementId,
    leading: Option<AnyElement>,
    title: SharedString,
    subtitle: Option<SharedString>,
    detail: Option<SharedString>,
    tags: Vec<(SharedString, Tone)>,
    title_tag: Option<(SharedString, Tone)>,
    trailing: Vec<AnyElement>,
    divider: bool,
    selected: bool,
    on_click: Option<ClickHandler>,
}
/// An item with an optional leading tile (normally a [`monogram`]).
pub(super) fn list_item(
    id: impl Into<ElementId>,
    leading: Option<AnyElement>,
    title: impl Into<SharedString>,
    subtitle: Option<SharedString>,
) -> ListItem {
    ListItem {
        id: id.into(),
        leading,
        title: title.into(),
        subtitle,
        detail: None,
        tags: Vec::new(),
        title_tag: None,
        trailing: Vec::new(),
        divider: false,
        selected: false,
        on_click: None,
    }
}
impl ListItem {
    /// Muted text after the title revealed on hover, such as a raw id.
    pub(super) fn hover_detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }
    /// Small chips after the subtitle (component kinds, transports).
    pub(super) fn tags(mut self, tags: impl IntoIterator<Item = SharedString>) -> Self {
        self.tags
            .extend(tags.into_iter().map(|label| (label, Tone::Neutral)));
        self
    }
    /// A chip after the subtitle in a status colour, such as a warning that
    /// an item is held back.
    pub(super) fn toned_tag(mut self, label: impl Into<SharedString>, tone: Tone) -> Self {
        self.tags.push((label.into(), tone));
        self
    }
    /// A chip right after the title, for a property of the item itself
    /// ("Trusted").
    pub(super) fn title_tag(mut self, label: impl Into<SharedString>, tone: Tone) -> Self {
        self.title_tag = Some((label.into(), tone));
        self
    }
    /// A trailing element: status pill, version, switch, button or menu.
    pub(super) fn trailing(mut self, element: impl IntoElement) -> Self {
        self.trailing.push(element.into_any_element());
        self
    }
    /// Draws the hairline above this item (every item but the first) when it
    /// is rendered by a virtualized list rather than [`list_container`].
    pub(super) fn divider(mut self, divider: bool) -> Self {
        self.divider = divider;
        self
    }
    /// Marks the item that is currently open or chosen.
    pub(super) fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    /// Makes the item open something; adds a trailing chevron.
    pub(super) fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}
impl RenderOnce for ListItem {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let group: SharedString = format!("list-item-{:?}", self.id).into();
        let title_line = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .min_w_0()
            .child(label_text(self.title.clone()).min_w_0().single_line())
            .when_some(self.title_tag, |line, (label, tone)| {
                line.child(toned_tag(label, tone))
            })
            .when_some(self.detail, |line, detail| {
                line.child(
                    // Takes no width until hovered, so the title keeps the
                    // whole line at rest.
                    div()
                        .min_w_0()
                        .max_w(px(0.))
                        .single_line()
                        .text_size(px(CAPTION_SIZE))
                        .text_color(rgb(faint()))
                        .invisible()
                        .group_hover(group.clone(), |style| style.visible().max_w(px(240.)))
                        .child(detail),
                )
            });
        let mut second_line = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .min_w_0()
            .overflow_hidden();
        if let Some(subtitle) = self.subtitle {
            second_line = second_line.child(
                div()
                    .min_w_0()
                    .single_line()
                    .text_size(px(DESCRIPTION_SIZE))
                    .text_color(rgb(muted()))
                    .child(subtitle),
            );
        }
        for (label, tone) in self.tags {
            second_line = second_line.child(toned_tag(label, tone));
        }
        let main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .gap(px(12.))
            .when_some(self.leading, |main, leading| main.child(leading))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(title_line)
                    .child(second_line),
            );
        let clickable = self.on_click.is_some();
        let leading: AnyElement = match self.on_click {
            Some(handler) => focus_ring(gpui_kit::base::Button::new(self.id.clone()))
                .accessibility_label(self.title)
                .flex_1()
                .min_w_0()
                .h_full()
                .rounded(px(6.))
                .cursor_pointer()
                .on_click(move |event, window, cx| handler(event, window, cx))
                .child(main)
                .into_any_element(),
            None => main.into_any_element(),
        };
        let entrance_id: SharedString = format!("{group}-in").into();
        let row = div()
            .group(group)
            .w_full()
            .h(px(LIST_ITEM_HEIGHT))
            .flex_shrink_0()
            .px(px(LIST_PADDING_X))
            .flex()
            .items_center()
            .gap(px(12.))
            .when(self.divider, |row| {
                row.border_t_1().border_color(rgb(border()))
            })
            .when(self.selected, |row| row.bg(rgb(hover_panel())))
            .when(clickable && !self.selected, |row| {
                row.hover(|row| row.bg(rgb(hover_panel())))
            })
            .child(leading)
            .when(!self.trailing.is_empty(), |row| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .children(self.trailing),
                )
            })
            .when(clickable, |row| {
                row.child(
                    icons::small(AppIcon::ChevronRight)
                        .flex_shrink_0()
                        .text_color(rgb(icons::rest_color())),
                )
            });
        motion::fade_in(entrance_id, row)
    }
}

/// A placeholder list item shown while the first load runs.
pub(super) fn skeleton_list_item(divider: bool) -> Div {
    div()
        .w_full()
        .h(px(LIST_ITEM_HEIGHT))
        .flex_shrink_0()
        .px(px(LIST_PADDING_X))
        .flex()
        .items_center()
        .gap(px(12.))
        .when(divider, |row| row.border_t_1().border_color(rgb(border())))
        .child(motion::skeleton_block(px(MONOGRAM_SIZE), px(MONOGRAM_SIZE)))
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(7.))
                .child(motion::skeleton_line(px(160.)))
                .child(motion::skeleton_block(px(260.), px(8.))),
        )
}

/// A list that is still loading: `rows` placeholder items in a bordered
/// container, then the "Taking longer than usual…" caption once the wait that
/// began at `started_at` passes `motion::SLOW_AFTER` (arm the redraw with
/// [`Xylo::arm_slow_hint`] when the request starts).
pub(super) fn loading_list(rows: usize, started_at: Option<Instant>) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(list_container(
            (0..rows).map(|index| skeleton_list_item(index > 0).into_any_element()),
        ))
        .children(started_at.and_then(slow_caption))
}

/// The slow-wait caption for a wait that began at `started_at`, once it has
/// run past `motion::SLOW_AFTER`; `None` before that.
pub(super) fn slow_caption(started_at: Instant) -> Option<Div> {
    motion::is_slow(started_at, Instant::now())
        .then(|| caption(motion::SLOW_HINT).text_color(rgb(faint())))
}

/// Shows `button` as busy: disabled with a spinner after its label, keeping
/// its width and accessible name. Use for every action that waits on the host
/// (save, test connection, install, refresh).
pub(super) fn busy(button: gpui_kit::base::Button, busy: bool) -> gpui_kit::base::Button {
    motion::busy_button(button, busy)
}

impl Xylo {
    /// Redraws once `motion::SLOW_AFTER` after a wait starts, so a
    /// [`slow_caption`] appears on time even if nothing else changes. Call it
    /// when the request is sent and keep the returned start time in the page
    /// state.
    pub(super) fn arm_slow_hint(&self, cx: &mut Context<Self>) -> Instant {
        let started_at = Instant::now();
        cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(motion::SLOW_AFTER + Duration::from_millis(50))
                .await;
            let _ = view.update(cx, |_, cx| cx.notify());
        })
        .detach();
        started_at
    }
}

/// A small chip for kinds and attributes ("Skills", "MCP"), neutral or in
/// a status colour ("Not used automatically").
pub(super) fn toned_tag(label: impl Into<SharedString>, tone: Tone) -> Div {
    let (background, foreground) = tone_colors(tone);
    // Short tags keep their width; a long value (a licence, a path) stops at
    // the cap and truncates, so tags never push into the trailing controls.
    div()
        .flex_shrink_0()
        .max_w(px(160.))
        .overflow_hidden()
        .h(px(18.))
        .px(px(6.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .bg(rgb(background))
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(foreground))
        .child(div().min_w_0().single_line().child(label.into()))
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Colour family of a [`pill`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    /// Informational or inactive.
    Neutral,
    /// Something new to act on, such as an available update.
    Accent,
    /// Working as intended.
    Success,
    /// Usable but incomplete.
    Warning,
    /// Broken until the person acts.
    Danger,
}
/// The states list items report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Status {
    /// Signed in or reachable.
    Connected,
    /// Installed and usable.
    Ready,
    /// Saved but missing a credential or required setting.
    NeedsSetup,
    /// A newer version can be installed.
    UpdateAvailable,
    /// Turned off by the person.
    Disabled,
    /// The last check or action failed.
    Error,
}
impl Status {
    /// The pill text.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Connected => "Connected",
            Self::Ready => "Ready",
            Self::NeedsSetup => "Needs setup",
            Self::UpdateAvailable => "Update available",
            Self::Disabled => "Disabled",
            Self::Error => "Error",
        }
    }
    /// The pill colour.
    pub(super) fn tone(self) -> Tone {
        match self {
            Self::Connected | Self::Ready => Tone::Success,
            Self::NeedsSetup => Tone::Warning,
            Self::UpdateAvailable => Tone::Accent,
            Self::Disabled => Tone::Neutral,
            Self::Error => Tone::Danger,
        }
    }
}
/// A status pill for one of the standard [`Status`]es.
pub(super) fn status_pill(status: Status) -> Div {
    pill(status.label(), status.tone())
}
/// The (background, text) colours of a tone, shared by pills and tags.
fn tone_colors(tone: Tone) -> (u32, u32) {
    match tone {
        Tone::Neutral => (well(), text_secondary()),
        Tone::Accent => (brand_soft(), accent_text()),
        Tone::Success => (success_bg(), success()),
        Tone::Warning => (warning_bg(), warning()),
        Tone::Danger => (danger_bg(), danger()),
    }
}
/// A status pill with custom wording ("Disabled after update").
pub(super) fn pill(label: impl Into<SharedString>, tone: Tone) -> Div {
    let (background, foreground) = tone_colors(tone);
    div()
        .flex_shrink_0()
        .h(px(22.))
        .px(px(8.))
        .flex()
        .items_center()
        .gap(px(6.))
        .rounded_full()
        .bg(rgb(background))
        .text_size(px(CAPTION_SIZE))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(foreground))
        .child(div().size(px(6.)).rounded_full().bg(rgb(foreground)))
        .child(div().single_line().child(label.into()))
}

/// A muted caption line, such as "Updated 5 min ago".
pub(super) fn caption(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(CAPTION_SIZE))
        .line_height(relative(1.45))
        .text_color(rgb(muted()))
        .child(text.into())
}

// ---------------------------------------------------------------------------
// Empty state
// ---------------------------------------------------------------------------

/// Nothing to list yet: a centred icon, a one-line title, a one-line
/// description and at most one primary action.
pub(super) fn empty_state(
    glyph: AppIcon,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    action: Option<AnyElement>,
) -> Div {
    div()
        .w_full()
        .py(px(32.))
        .px(px(24.))
        .flex()
        .flex_col()
        .items_center()
        .text_center()
        .gap(px(6.))
        .child(
            div()
                .size(px(40.))
                .mb(px(6.))
                .rounded(px(10.))
                .bg(rgb(well()))
                .flex()
                .items_center()
                .justify_center()
                .child(icons::icon(glyph).text_color(rgb(icons::rest_color()))),
        )
        .child(
            div()
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(text()))
                .child(title.into()),
        )
        .child(description_text(description).max_w(px(380.)))
        .when_some(action, |state, action| {
            state.child(div().mt(px(10.)).child(action))
        })
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

fn settings_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(label.clone())
        .flex_shrink_0()
        .h(px(CONTROL_HEIGHT))
        .px(px(12.))
        .rounded(px(8.))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .text_size(px(13.))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(div().single_line().child(label))
}
/// The page's one main action: filled, inverted per theme.
pub(super) fn primary_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui_kit::base::Button {
    settings_button(id, label)
        .bg(rgb(text()))
        .text_color(rgb(bg()))
        .hover(|style| style.opacity(0.88))
        .active(|style| style.opacity(0.76))
}
/// A supporting action: outlined on the panel surface.
pub(super) fn secondary_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui_kit::base::Button {
    settings_button(id, label)
        .bg(rgb(panel()))
        .border_color(rgb(border_strong()))
        .focus_visible(|style| style.border_color(rgb(brand())))
        .text_color(rgb(text()))
        .hover(|style| style.bg(rgb(hover_panel())))
        .active(|style| style.bg(rgb(pressed())))
}
/// A quiet action inside rows ("Revoke", "Refresh"): text until hovered.
pub(super) fn text_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui_kit::base::Button {
    settings_button(id, label)
        .px(px(8.))
        .text_color(rgb(text_secondary()))
        .hover(|style| style.bg(rgb(hover())).text_color(rgb(text())))
        .active(|style| style.bg(rgb(pressed())))
}
/// A removal: red text, tinted on hover. Its handler must confirm first.
pub(super) fn destructive_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui_kit::base::Button {
    settings_button(id, label)
        .px(px(8.))
        .text_color(rgb(danger()))
        .hover(|style| style.bg(rgb(danger_bg())))
        .active(|style| style.bg(rgb(danger_bg())))
}

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

/// A boolean control for a setting row: the track only, named by `label`
/// (normally the row label) for assistive technology. Chain `.disabled()` and
/// `.on_change()` on the returned switch.
pub(super) fn switch(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
) -> XyloSwitch {
    XyloSwitch::new(id)
        .label(label)
        .hide_label()
        .checked(checked)
}

/// One choice of a [`select`].
#[derive(Clone)]
pub(super) struct SelectOption {
    id: Option<ElementId>,
    label: SharedString,
    selected: bool,
    choose: ChoiceHandler,
}
impl SelectOption {
    /// A choice; `choose` runs when it is picked (also when already selected,
    /// so handlers should ignore a no-op choice).
    pub(super) fn new(
        label: impl Into<SharedString>,
        selected: bool,
        choose: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: None,
            label: label.into(),
            selected,
            choose: Rc::new(choose),
        }
    }
    /// The element id of this option's segment in a [`segmented`] control,
    /// when tests or callers address it directly ("theme-light").
    pub(super) fn id(mut self, id: impl Into<ElementId>) -> Self {
        self.id = Some(id.into());
        self
    }
}
/// A compact dropdown for an enumerated setting: the current choice and a
/// caret; the menu checks the selected option.
pub(super) fn select(
    id: impl Into<ElementId>,
    accessible_label: impl Into<SharedString>,
    options: Vec<SelectOption>,
) -> AnyElement {
    let current = options
        .iter()
        .find(|option| option.selected)
        .map(|option| option.label.clone())
        .unwrap_or_else(|| "Choose…".into());
    let accessible_label: SharedString = accessible_label.into();
    let options: Rc<[SelectOption]> = options.into();
    MenuButton::new(id)
        .outline()
        .h(px(CONTROL_HEIGHT))
        .px(px(10.))
        .gap(px(6.))
        .text_size(px(13.))
        .flex_shrink_0()
        .accessibility_label(format!("{accessible_label}: {current}"))
        .child(div().max_w(px(220.)).single_line().child(current))
        .child(icons::small(AppIcon::ChevronDown).text_color(rgb(icons::rest_color())))
        .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
            for option in options.iter() {
                let choose = option.choose.clone();
                menu = menu.item(
                    PopupMenuItem::new(option.label.clone())
                        .checked(option.selected)
                        .on_click(move |_, window, cx| choose(window, cx)),
                );
            }
            menu
        })
        .into_any_element()
}

/// A segmented control for switching views or short enums ("Discover ·
/// Installed (4) · Sources", "Light · Dark"). Each option's handler runs when
/// it is chosen.
pub(super) fn segmented(id: &str, options: Vec<SelectOption>) -> Div {
    let mut control = segments();
    for (index, option) in options.into_iter().enumerate() {
        let choose = option.choose.clone();
        control = control.child(
            segment(
                option
                    .id
                    .unwrap_or_else(|| SharedString::from(format!("{id}-{index}")).into()),
                option.label,
                option.selected,
            )
            .on_click(move |_, window, cx| choose(window, cx)),
        );
    }
    // Sized to its segments, never stretched by a column parent.
    control.flex_shrink_0().self_start()
}

/// A search box: a leading magnifier, a clear button and the compact control
/// height. Filtering happens as the person types.
pub(super) fn search_field(state: &Entity<InputState>, label: impl Into<SharedString>) -> Input {
    Input::new(state)
        .prefix(icons::small(AppIcon::Search).text_color(rgb(icons::rest_color())))
        .cleanable(true)
        .aria_label(label.into())
        .h(px(CONTROL_HEIGHT))
}

/// A read-only value on the right of a setting row ("1.4.2", a path).
pub(super) fn value_text(value: impl Into<SharedString>) -> Div {
    div()
        .max_w(px(320.))
        .min_w_0()
        .single_line()
        .text_size(px(DESCRIPTION_SIZE))
        .text_color(rgb(text_secondary()))
        .child(value.into())
}

/// One entry of an [`overflow_menu`].
#[derive(Clone)]
pub(super) struct MenuAction {
    label: SharedString,
    run: ChoiceHandler,
}
impl MenuAction {
    /// An entry; destructive entries must confirm in `run`.
    pub(super) fn new(
        label: impl Into<SharedString>,
        run: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            run: Rc::new(run),
        }
    }
}
/// The "…" menu at the end of a list item for secondary actions (Remove).
pub(super) fn overflow_menu(
    id: impl Into<ElementId>,
    accessible_label: impl Into<SharedString>,
    actions: Vec<MenuAction>,
) -> AnyElement {
    let actions: Rc<[MenuAction]> = actions.into();
    icons::menu_trigger(id, AppIcon::More, accessible_label)
        .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
            for action in actions.iter() {
                let run = action.run.clone();
                menu = menu.item(
                    PopupMenuItem::new(action.label.clone())
                        .on_click(move |_, window, cx| run(window, cx)),
                );
            }
            menu
        })
        .into_any_element()
}

/// One preset of a [`radio_list`]: label, description and the radio dot.
#[derive(IntoElement)]
pub(super) struct RadioOption {
    id: ElementId,
    label: SharedString,
    description: SharedString,
    checked: bool,
    caution: bool,
    choose: Option<ChoiceHandler>,
}
/// A preset with a description; `checked` marks the current one.
pub(super) fn radio_option(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    checked: bool,
) -> RadioOption {
    RadioOption {
        id: id.into(),
        label: label.into(),
        description: description.into(),
        checked,
        caution: false,
        choose: None,
    }
}
impl RadioOption {
    /// Shows the label in the warning colour for a risky preset.
    pub(super) fn caution(mut self, caution: bool) -> Self {
        self.caution = caution;
        self
    }
    /// Runs when the person picks this unchecked option.
    pub(super) fn on_choose(mut self, choose: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.choose = Some(Rc::new(choose));
        self
    }
}
impl RenderOnce for RadioOption {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let checked = self.checked;
        let choose = self.choose;
        Radio::new(self.id)
            .checked(checked)
            .accessibility_label(format!("{}: {}", self.label, self.description))
            .w_full()
            .px(px(LIST_PADDING_X))
            .py(px(12.))
            .flex()
            .items_start()
            .gap(px(12.))
            .border_1()
            .border_color(transparent_black())
            .focus_visible(|style| style.border_color(rgb(brand())))
            .cursor_pointer()
            .hover(move |style| {
                if checked {
                    style
                } else {
                    style.bg(rgb(hover_panel()))
                }
            })
            .styles(|style| style.disabled(|style| style.cursor_default().opacity(0.5)))
            .child(
                div()
                    .mt(px(2.))
                    .size(px(16.))
                    .flex_shrink_0()
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(if checked { brand() } else { border_strong() }))
                    .bg(rgb(panel()))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(checked, |dot| {
                        dot.child(div().size(px(8.)).rounded_full().bg(rgb(brand())))
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        label_text(self.label)
                            .when(self.caution, |label| label.text_color(rgb(warning()))),
                    )
                    .child(description_text(self.description)),
            )
            .on_change(move |_, _, window, cx| {
                if let Some(choose) = &choose {
                    choose(window, cx);
                }
            })
    }
}
/// Mutually exclusive presets with descriptions, in a bordered container.
pub(super) fn radio_list(
    id: impl Into<ElementId>,
    options: impl IntoIterator<Item = RadioOption>,
) -> RadioGroup {
    let mut group = RadioGroup::new(id)
        .w_full()
        .flex()
        .flex_col()
        .rounded(px(CONTAINER_RADIUS))
        .border_1()
        .border_color(rgb(border()))
        .bg(rgb(panel()))
        .overflow_hidden();
    for (index, option) in options.into_iter().enumerate() {
        if index > 0 {
            group = group.child(row_divider());
        }
        group = group.child(option);
    }
    group
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

/// A labelled form input: label above, the input full width, then optional
/// help text. Validation failures use the page's [`inline_error`].
#[derive(IntoElement)]
pub(super) struct Field {
    label: SharedString,
    input: AnyElement,
    help: Option<SharedString>,
}
/// A field with optional help text under the input.
pub(super) fn field(
    label: impl Into<SharedString>,
    input: impl IntoElement,
    help: Option<SharedString>,
) -> Field {
    Field {
        label: label.into(),
        input: input.into_any_element(),
        help,
    }
}
impl RenderOnce for Field {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let below = self.help.map(caption);
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(text()))
                    .child(self.label),
            )
            .child(div().w_full().child(self.input))
            .when_some(below, |field, below| field.child(below))
    }
}
/// The column a form's fields stack in; actions go in a final right-aligned
/// row of buttons.
pub(super) fn form_column(children: impl IntoIterator<Item = AnyElement>) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(16.))
        .children(children)
}
/// The right-aligned button row that ends a form.
pub(super) fn form_actions(children: impl IntoIterator<Item = AnyElement>) -> Div {
    div()
        .w_full()
        .pt(px(4.))
        .flex()
        .items_center()
        .justify_end()
        .gap(px(8.))
        .children(children)
}

#[cfg(test)]
#[path = "settings_ui/tests.rs"]
mod tests;
