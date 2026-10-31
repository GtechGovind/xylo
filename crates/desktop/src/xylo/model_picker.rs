//! The composer's model picker, the only place a model is chosen. Provider
//! settings register every model a connection discovers in
//! `AppSettings::providers` (one entry per model); the picker lists them all,
//! grouped by connection in catalog order, with search, the active model
//! checked, each model's context size, and a "Recent" group when several
//! connections exist. Reasoning effort is set once, for the active model, below
//! the list. Choosing a model saves settings directly and never opens the
//! provider editor. Arrow keys move the highlight, Enter chooses and Escape
//! closes, all without moving focus out of the search field.
use super::*;
use crate::icons::{self, AppIcon};
use gpui_kit::base::TestSupportExt;
use gpui_kit::component::{
    button::{Button as MenuButton, ButtonVariants},
    input::{Enter, Escape, MoveDown, MoveUp},
    menu::{DropdownMenu, PopupMenuItem},
    popover::Popover,
};

/// How many recently chosen models lead the list.
const RECENT_LIMIT: usize = 3;
/// How many recent choices settings keep.
const RECENT_STORED: usize = 5;
/// Height of every single-line row, so the list reads as one rhythm.
const ROW_HEIGHT: f32 = 32.;
/// Rows inset their content by this much; group headers align with the names.
const ROW_INSET: f32 = 6.;
/// The leading check column: icon width; names follow after an 8 px gap.
const CHECK_WIDTH: f32 = 14.;
const NAME_INSET: f32 = ROW_INSET + CHECK_WIDTH + 8.;
/// The list scrolls on its own so the effort control and footer stay visible.
const LIST_MAX_HEIGHT: f32 = 320.;

/// Picker state that outlives the popover: the open flag, the search field,
/// the keyboard highlight and the list scroll. Recent choices live in
/// `UiPreferences::recent_models`.
pub(super) struct ModelPickerUi {
    pub open: bool,
    query: Entity<InputState>,
    /// The query the highlight was computed for; edits reset the highlight.
    last_query: String,
    /// Position among the selectable rows.
    highlighted: usize,
    /// The highlight is drawn only once arrow keys have moved it.
    keyboard_active: bool,
    scroll: ScrollHandle,
    _query_subscription: Subscription,
}

impl ModelPickerUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Xylo>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search models"));
        let subscription = cx.subscribe(&query, |view, query, event: &InputEvent, cx| {
            let value = query.read(cx).value().to_string();
            if matches!(event, InputEvent::Change) && value != view.model_picker.last_query {
                view.model_picker.last_query = value;
                view.model_picker.highlighted = 0;
                view.model_picker.scroll.scroll_to_item(0);
                cx.notify();
            }
        });
        Self {
            open: false,
            query,
            last_query: String::new(),
            highlighted: 0,
            keyboard_active: false,
            scroll: ScrollHandle::new(),
            _query_subscription: subscription,
        }
    }
}

/// One line of the picker list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PickerRow {
    Header(String),
    /// `index` points into `AppSettings::providers`.
    Model {
        index: usize,
        checked: bool,
        recent: bool,
    },
}

/// Moves a chosen model to the front of the stored recent list.
fn remember_recent(recent: &mut Vec<RecentModel>, provider: &ProviderConfig) {
    let chosen = RecentModel {
        endpoint: provider.endpoint.clone(),
        model: provider.model.clone(),
    };
    recent.retain(|entry| *entry != chosen);
    recent.insert(0, chosen);
    recent.truncate(RECENT_STORED);
}

fn same_model(left: &ProviderConfig, right: &ProviderConfig) -> bool {
    left.model == right.model && providers::same_connection(left, right)
}

/// The heading a connection's models are grouped under: the provider and the
/// signed-in account's email for ChatGPT ("ChatGPT · user@example.com"),
/// otherwise the connection's own name when it says more ("OpenAI · Work").
fn group_label(provider: &ProviderConfig, account_email: Option<&str>) -> String {
    let kind = providers::provider_kind(provider);
    if provider.credential_ref.as_deref() == Some(CHATGPT_REF)
        && let Some(email) = account_email.filter(|email| !email.trim().is_empty())
    {
        return format!("{kind} · {}", email.trim());
    }
    let name = provider.name.trim();
    if name.is_empty() || name.eq_ignore_ascii_case(kind) {
        kind.to_owned()
    } else {
        format!("{kind} · {name}")
    }
}

/// Builds the list. Models keep catalog order inside their connection group;
/// with more than one connection and no search, up to three recent choices
/// lead in a "Recent" group. Search matches model and group names.
fn picker_rows(
    models: &[ProviderConfig],
    active: &ProviderConfig,
    recent: &[RecentModel],
    query: &str,
    account_email: Option<&str>,
) -> Vec<PickerRow> {
    let query = query.trim().to_lowercase();
    let matches = |provider: &ProviderConfig| {
        !provider.model.is_empty()
            && (query.is_empty()
                || provider.model.to_lowercase().contains(&query)
                || group_label(provider, account_email)
                    .to_lowercase()
                    .contains(&query))
    };
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    for (index, provider) in models.iter().enumerate().filter(|(_, p)| matches(p)) {
        match groups
            .iter_mut()
            .find(|(first, _)| providers::same_connection(&models[*first], provider))
        {
            Some((_, members)) => members.push(index),
            None => groups.push((index, vec![index])),
        }
    }
    let mut rows = Vec::new();
    if query.is_empty() && groups.len() > 1 {
        let recent: Vec<usize> = recent
            .iter()
            .filter_map(|recent| {
                models.iter().position(|model| {
                    model.endpoint == recent.endpoint && model.model == recent.model
                })
            })
            .take(RECENT_LIMIT)
            .collect();
        if !recent.is_empty() {
            rows.push(PickerRow::Header("Recent".into()));
            rows.extend(recent.into_iter().map(|index| PickerRow::Model {
                index,
                checked: same_model(&models[index], active),
                recent: true,
            }));
        }
    }
    for (first, members) in groups {
        rows.push(PickerRow::Header(group_label(
            &models[first],
            account_email,
        )));
        rows.extend(members.into_iter().map(|index| PickerRow::Model {
            index,
            checked: same_model(&models[index], active),
            recent: false,
        }));
    }
    rows
}

/// List positions of the selectable rows, in order.
fn choice_positions(rows: &[PickerRow]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(position, row)| matches!(row, PickerRow::Model { .. }).then_some(position))
        .collect()
}

/// The group header to pin above the list once `top` (the first visible row)
/// has scrolled past it.
fn sticky_header(rows: &[PickerRow], top: usize) -> Option<&str> {
    rows.iter()
        .take(top.saturating_add(1))
        .rev()
        .find_map(|row| match row {
            PickerRow::Header(name) => Some(name.as_str()),
            PickerRow::Model { .. } => None,
        })
}

/// Compact token count for context sizes ("8K", "200K", "1M").
pub(super) fn format_tokens(tokens: u64) -> String {
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => compact(tokens as f64 / 1_000., "K"),
        _ => compact(tokens as f64 / 1_000_000., "M"),
    }
}

fn compact(value: f64, unit: &str) -> String {
    if value >= 100. || value.fract() < 0.05 {
        format!("{value:.0}{unit}")
    } else {
        format!("{value:.1}{unit}")
    }
}

/// Product caption for a reasoning effort value.
pub(super) fn effort_caption(value: &str) -> String {
    match value {
        "none" => "None".into(),
        "minimal" => "Minimal".into(),
        "low" => "Low".into(),
        "medium" => "Medium".into(),
        "high" => "High".into(),
        "xhigh" => "Extra high".into(),
        "max" => "Max".into(),
        other => {
            let mut chars = other.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        }
    }
}

/// The effort in force for a model: its explicit choice, else the catalog default.
fn active_effort(provider: &ProviderConfig) -> Option<&str> {
    provider
        .effective_reasoning_effort()
        .or_else(|| catalog_default_effort(provider))
}

/// The catalog's default effort, when the model offers it.
fn catalog_default_effort(provider: &ProviderConfig) -> Option<&str> {
    let efforts = provider.supported_reasoning_efforts();
    provider
        .default_reasoning_effort
        .as_deref()
        .filter(|value| efforts.contains(value))
}

/// The one effort label shown everywhere (composer chip and picker): the
/// effort in force, "Default" when the provider decides, nothing for a model
/// without reasoning options.
pub(super) fn effort_label(provider: &ProviderConfig) -> Option<String> {
    if provider.supported_reasoning_efforts().is_empty() {
        return None;
    }
    Some(
        active_effort(provider)
            .map(effort_caption)
            .unwrap_or_else(|| "Default".into()),
    )
}

fn header_text(name: &str) -> Div {
    div()
        .pl(px(NAME_INSET))
        .pr_2()
        .h(px(26.))
        .flex()
        .items_center()
        .text_size(px(11.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(muted()))
        .child(div().single_line().child(name.to_owned()))
}

/// A plain single-line menu row: a check or icon column so names align with
/// the group headers, then the content. Hover fills lightly; the keyboard
/// highlight draws a ring only after arrow-key navigation.
fn picker_row(id: impl Into<ElementId>, keyboard_highlight: bool) -> gpui_kit::base::Button {
    focus_ring(gpui_kit::base::Button::new(id))
        .w_full()
        .h(px(ROW_HEIGHT))
        .px(px(ROW_INSET))
        .rounded(px(8.))
        .bg(rgb(panel()))
        .when(keyboard_highlight, |row| row.border_color(rgb(accent())))
        .flex()
        .items_center()
        .gap(px(8.))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(hover())))
}

fn leading_column() -> Div {
    div()
        .w(px(CHECK_WIDTH))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
}

impl Xylo {
    fn picker_rows(&self, cx: &App) -> Vec<PickerRow> {
        picker_rows(
            &self.settings.providers,
            &self.settings.provider,
            &self.settings.ui.recent_models,
            self.model_picker.query.read(cx).value().as_ref(),
            self.account_email(),
        )
    }

    pub(super) fn set_model_picker_open(
        &mut self,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.model_picker.open == open {
            return;
        }
        self.model_picker.open = open;
        if open {
            self.model_picker.last_query.clear();
            self.model_picker
                .query
                .update(cx, |input, cx| input.set_value("", window, cx));
            let rows = self.picker_rows(cx);
            self.model_picker.highlighted = rows
                .iter()
                .filter(|row| matches!(row, PickerRow::Model { .. }))
                .position(|row| matches!(row, PickerRow::Model { checked: true, .. }))
                .unwrap_or(0);
            self.model_picker.keyboard_active = false;
            self.model_picker.scroll.scroll_to_item(0);
            self.model_picker
                .query
                .update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    fn move_model_highlight(&mut self, step: isize, cx: &mut Context<Self>) {
        let positions = choice_positions(&self.picker_rows(cx));
        if positions.is_empty() {
            return;
        }
        let count = positions.len() as isize;
        let current = (self.model_picker.highlighted as isize).min(count - 1);
        let next = (current + step).rem_euclid(count) as usize;
        self.model_picker.highlighted = next;
        self.model_picker.keyboard_active = true;
        self.model_picker.scroll.scroll_to_item(positions[next]);
        cx.notify();
    }

    fn choose_highlighted_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.picker_rows(cx);
        let chosen = choice_positions(&rows)
            .get(self.model_picker.highlighted)
            .and_then(|position| match rows[*position] {
                PickerRow::Model { index, .. } => Some(index),
                PickerRow::Header(_) => None,
            });
        if let Some(index) = chosen {
            self.choose_model(index, window, cx);
        }
    }

    /// Switches the active model immediately and persists the choice.
    fn choose_model(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(provider) = self.settings.providers.get(index).cloned() else {
            return;
        };
        remember_recent(&mut self.settings.ui.recent_models, &provider);
        self.settings.provider = provider;
        let settings = self.settings.clone();
        self.host_call(cx, |host| host.save_settings(settings));
        self.set_model_picker_open(false, window, cx);
    }

    fn set_reasoning_effort(&mut self, effort: Option<String>, cx: &mut Context<Self>) {
        let provider = &self.settings.provider;
        if effort
            .as_deref()
            .is_some_and(|effort| !provider.supported_reasoning_efforts().contains(&effort))
        {
            return;
        }
        self.settings.provider.reasoning_effort = effort;
        self.settings.remember_model(self.settings.provider.clone());
        let settings = self.settings.clone();
        self.host_call(cx, |host| host.save_settings(settings));
        cx.notify();
    }

    fn open_providers_from_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_model_picker_open(false, window, cx);
        self.settings_open = true;
        self.navigate(Page::Providers, cx);
    }

    /// The composer trigger and its popover. `label` is the active model's
    /// name, or a prompt to choose one.
    pub(super) fn unified_model_control(&self, label: String, cx: &Context<Self>) -> AnyElement {
        let effort_label = effort_label(&self.settings.provider);
        let content = cx.entity().downgrade();
        let change = cx.entity().downgrade();
        let mut trigger_layout = div().min_w_0().max_w(px(250.)).flex_shrink(1.);
        Popover::new("composer-model-popover")
            .appearance(false)
            .trigger_style(trigger_layout.style().clone())
            .anchor(Anchor::BottomRight)
            .offset(px(8.))
            .open(self.model_picker.open)
            .track_focus(&self.model_picker.query.read(cx).focus_handle(cx))
            .on_open_change(move |open, window, cx| {
                let _ = change.update(cx, |view, cx| view.set_model_picker_open(*open, window, cx));
            })
            .trigger(
                MenuButton::new("composer-model")
                    .ghost()
                    .accessibility_label(label.clone())
                    .child(div().min_w_0().single_line().child(label))
                    .h(px(30.))
                    .rounded_full()
                    .text_size(px(12.5))
                    .text_color(rgb(text_secondary()))
                    .max_w(px(250.))
                    .w_full()
                    .min_w_0()
                    .flex_shrink(1.)
                    .gap_1()
                    .when_some(effort_label, |button, effort| {
                        button.child(
                            div()
                                .flex_shrink_0()
                                .single_line()
                                .text_color(rgb(muted()))
                                .child(format!("· {effort}")),
                        )
                    })
                    .child(icons::small(AppIcon::Select).text_color(rgb(icons::rest_color())))
                    .tooltip("Model and reasoning"),
            )
            .content(move |_, _, cx| {
                let panel = content
                    .update(cx, |view, cx| view.model_picker_panel(cx))
                    .unwrap_or_else(|_| div().into_any_element());
                crate::motion::popover_surface("composer-model-surface", panel).into_any_element()
            })
            .into_any_element()
    }

    fn model_picker_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let (up, down, enter, escape) = (weak.clone(), weak.clone(), weak.clone(), weak.clone());
        let rows = self.picker_rows(cx);
        let mut list = div()
            .id("composer-model-list")
            .test_support()
            .max_h(px(LIST_MAX_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&self.model_picker.scroll)
            .on_scroll(move |_, _, cx| {
                // Re-render so the pinned group header follows the scroll.
                let _ = weak.update(cx, |_, cx| cx.notify());
            })
            .flex()
            .flex_col();
        let mut choice = 0usize;
        for row in &rows {
            list = list.child(match row {
                PickerRow::Header(name) => header_text(name).into_any_element(),
                PickerRow::Model {
                    index,
                    checked,
                    recent,
                } => {
                    let highlighted = self.model_picker.keyboard_active
                        && self.model_picker.highlighted == choice;
                    choice += 1;
                    self.model_row(*index, *checked, *recent, highlighted, cx)
                }
            });
        }
        let query = self.model_picker.query.read(cx).value().trim().to_owned();
        if choice == 0 && query.is_empty() {
            list = list.child(
                picker_row("composer-connect-provider", false)
                    .child(leading_column().child(icons::small(AppIcon::Add)))
                    .child(div().text_size(px(13.)).child("Connect a provider"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.open_providers_from_picker(window, cx)
                    })),
            );
        } else if choice == 0 {
            list = list.child(
                div()
                    .px_2()
                    .py_3()
                    .child(subtitle(format!("No models match “{query}”."))),
            );
        }
        let pinned = (self.model_picker.scroll.offset().y < px(0.))
            .then(|| sticky_header(&rows, self.model_picker.scroll.top_item()))
            .flatten()
            .map(|name| {
                header_text(name)
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .bg(rgb(panel()))
            });
        div()
            .id("composer-model-panel")
            .test_support()
            .w(px(340.))
            .flex()
            .flex_col()
            .gap_2()
            .pt(px(8.))
            .px(px(6.))
            .pb(px(4.))
            .capture_action(move |_: &MoveUp, _, cx| {
                cx.stop_propagation();
                let _ = up.update(cx, |view, cx| view.move_model_highlight(-1, cx));
            })
            .capture_action(move |_: &MoveDown, _, cx| {
                cx.stop_propagation();
                let _ = down.update(cx, |view, cx| view.move_model_highlight(1, cx));
            })
            .capture_action(move |_: &Enter, window, cx| {
                cx.stop_propagation();
                let _ = enter.update(cx, |view, cx| view.choose_highlighted_model(window, cx));
            })
            .capture_action(move |_: &Escape, window, cx| {
                cx.stop_propagation();
                let _ = escape.update(cx, |view, cx| view.set_model_picker_open(false, window, cx));
            })
            .child(
                div()
                    .px(px(2.))
                    .child(Input::new(&self.model_picker.query).aria_label("Search models")),
            )
            .child(div().relative().child(list).children(pinned))
            .child(self.reasoning_effort_control(cx))
            .child(divider())
            .child(
                picker_row("composer-manage-models", false)
                    .child(leading_column().child(
                        icons::small(AppIcon::Settings).text_color(rgb(icons::rest_color())),
                    ))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(text_secondary()))
                            .child("Add or manage providers…"),
                    )
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.open_providers_from_picker(window, cx)
                    })),
            )
            .into_any_element()
    }

    fn model_row(
        &self,
        index: usize,
        checked: bool,
        recent: bool,
        highlighted: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let provider = &self.settings.providers[index];
        let id: ElementId = if recent {
            ("composer-model-recent", index).into()
        } else {
            ("composer-model-choice", index).into()
        };
        let reasoning = !provider.supported_reasoning_efforts().is_empty();
        picker_row(id, highlighted)
            .accessibility_label(format!(
                "{}, {}, {} context{}{}",
                provider.model,
                group_label(provider, self.account_email()),
                format_tokens(u64::from(provider.context_tokens)),
                if reasoning {
                    ", supports reasoning"
                } else {
                    ""
                },
                if checked { ", selected" } else { "" }
            ))
            .text_size(px(13.))
            .child(leading_column().when(checked, |column| {
                column.child(icons::small(AppIcon::Done).text_color(rgb(accent_text())))
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .single_line()
                    .font_weight(if checked {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(rgb(if checked { accent_text() } else { text() }))
                    .child(provider.model.clone()),
            )
            .when(reasoning, |row| {
                row.child(
                    icons::small(AppIcon::Reasoning)
                        .flex_shrink_0()
                        .text_color(rgb(icons::rest_color())),
                )
            })
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.5))
                    .text_color(rgb(muted()))
                    .child(format_tokens(u64::from(provider.context_tokens))),
            )
            .on_click(cx.listener(move |view, _, window, cx| view.choose_model(index, window, cx)))
            .into_any_element()
    }

    /// One compact row, "Reasoning  [Medium ▾]", for the active model. It
    /// shows exactly what the composer chip shows; its menu lists every
    /// option with the catalog default marked, and choosing that default
    /// follows the catalog again. Hidden when the model has no options.
    fn reasoning_effort_control(&self, cx: &Context<Self>) -> AnyElement {
        let provider = &self.settings.provider;
        let Some(current) = effort_label(provider) else {
            return div().into_any_element();
        };
        let weak = cx.entity().downgrade();
        div()
            // Rows carry a 1 px focus border; match it so the label lines up
            // with the model names.
            .px(px(ROW_INSET + 1.))
            .h(px(ROW_HEIGHT + 4.))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                leading_column()
                    .child(icons::small(AppIcon::Reasoning).text_color(rgb(icons::rest_color()))),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .text_color(rgb(text_secondary()))
                    .child("Reasoning"),
            )
            .child(
                MenuButton::new("composer-effort")
                    .ghost()
                    .gap_1()
                    .child(div().single_line().child(current.clone()))
                    .h(px(28.))
                    .text_size(px(12.5))
                    .flex_shrink_0()
                    .child(icons::small(AppIcon::Select).text_color(rgb(icons::rest_color())))
                    .accessibility_label(format!("Reasoning effort: {current}"))
                    .dropdown_menu(move |mut menu, _, cx| {
                        let Ok(provider) =
                            weak.update(cx, |view, _| view.settings.provider.clone())
                        else {
                            return menu;
                        };
                        let active = active_effort(&provider).map(str::to_owned);
                        let default = catalog_default_effort(&provider).map(str::to_owned);
                        if default.is_none() {
                            // Without a catalog default, "Default" lets the
                            // provider decide.
                            let view = weak.clone();
                            menu = menu.item(
                                PopupMenuItem::new("Default")
                                    .checked(active.is_none())
                                    .on_click(move |_, _, cx| {
                                        let _ = view.update(cx, |view, cx| {
                                            view.set_reasoning_effort(None, cx)
                                        });
                                    }),
                            );
                        }
                        for effort in provider.supported_reasoning_efforts() {
                            let is_default = default.as_deref() == Some(effort);
                            let label = if is_default {
                                format!("{} (default)", effort_caption(effort))
                            } else {
                                effort_caption(effort)
                            };
                            // Choosing the default follows the catalog again.
                            let choice = (!is_default).then(|| effort.to_owned());
                            let view = weak.clone();
                            menu = menu.item(
                                PopupMenuItem::new(label)
                                    .checked(active.as_deref() == Some(effort))
                                    .on_click(move |_, _, cx| {
                                        let choice = choice.clone();
                                        let _ = view.update(cx, |view, cx| {
                                            view.set_reasoning_effort(choice, cx)
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "model_picker/tests.rs"]
mod tests;
