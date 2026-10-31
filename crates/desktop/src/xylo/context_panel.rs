//! The composer's context window ring and its breakdown popover. The ring is a
//! static arc over a visible track, hidden until a chat uses 5% of the window.
//! Both read the engine's `ContextUsage` for the open chat (the accounting of
//! the latest request), never summed per-turn usage or a character estimate.
use super::*;
use gpui_kit::component::{
    button::{Button as MenuButton, ButtonVariants},
    popover::Popover,
};
use model_picker::format_tokens;

/// The figures the panel shows, derived once from a `ContextUsage`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ContextBreakdown {
    used: u64,
    capacity: u64,
    /// Space kept free so the context is condensed before it overflows.
    buffer: u64,
    free: u64,
}

impl ContextBreakdown {
    fn new(usage: &ContextUsage) -> Self {
        let used = usage.used();
        let capacity = usage.capacity;
        let buffer = capacity.saturating_sub(usage.autocompact_threshold);
        Self {
            used,
            capacity,
            buffer,
            free: capacity.saturating_sub(used).saturating_sub(buffer),
        }
    }

    /// Share of the window in use, rounded down; `None` when capacity is unknown.
    fn percent(&self) -> Option<u64> {
        (self.capacity > 0).then(|| (self.used.saturating_mul(100) / self.capacity).min(100))
    }

    fn percent_label(&self) -> String {
        match self.percent() {
            Some(0) if self.used > 0 => "<1%".into(),
            Some(percent) => format!("{percent}%"),
            None => "—".into(),
        }
    }

    fn fraction(&self, tokens: u64) -> f32 {
        if self.capacity == 0 {
            0.
        } else {
            (tokens as f64 / self.capacity as f64).clamp(0., 1.) as f32
        }
    }
}

/// "just now", "3 min ago", "2 h ago" for the panel's freshness line.
fn updated_label(updated_at_ms: i64, now_ms: i64) -> String {
    let seconds = (now_ms - updated_at_ms).max(0) / 1000;
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", seconds / 60),
        _ => format!("{} h ago", seconds / 3600),
    }
}

/// Below this share of the window the ring is noise and stays hidden.
const RING_THRESHOLD: u64 = 5;
/// Diameter and stroke of the ring.
const RING_SIZE: f32 = 14.;
const RING_STROKE: f32 = 2.;

/// A static progress arc over a full track, starting at twelve o'clock. It
/// never animates, so it cannot be mistaken for a spinner; the arc turns to
/// the warning colour once the window is nearly full.
fn usage_ring(percent: u64) -> impl IntoElement {
    use gpui_kit::component::plot::shape::{Arc, ArcData};
    use std::f32::consts::TAU;
    let track: Hsla = rgb(border_strong()).into();
    let arc: Hsla = rgb(if percent >= 90 {
        warning()
    } else {
        text_secondary()
    })
    .into();
    let fraction = (percent.min(100) as f32) / 100.;
    canvas(
        |bounds, _, _| bounds,
        move |_, bounds, window, _| {
            let outer = bounds.size.width.min(bounds.size.height).as_f32() / 2.;
            let shape = Arc::new()
                .inner_radius(outer - RING_STROKE)
                .outer_radius(outer);
            shape.paint(&ArcData::new(&(), 0, 100., 0., TAU), track, &bounds, window);
            shape.paint(
                &ArcData::new(&(), 1, percent as f32, 0., fraction * TAU),
                arc,
                &bounds,
                window,
            );
        },
    )
    .size(px(RING_SIZE))
    .flex_shrink_0()
}

/// Segment colors of the bar, shared with the row swatches.
const MESSAGES: fn() -> u32 = brand;
const SYSTEM: fn() -> u32 = lavender;
const TOOLS: fn() -> u32 = accent_text;
const SUMMARY: fn() -> u32 = success;
const BUFFER: fn() -> u32 = warning_bg;
const FREE: fn() -> u32 = hover;

impl Xylo {
    fn current_context(&self) -> Option<&ContextUsage> {
        self.turns
            .get(&self.session)
            .and_then(|turn| turn.context.as_ref())
    }

    /// Notes when context accounting arrives, for the panel's freshness line.
    pub(super) fn note_context_usage(&mut self, envelope: &Envelope<EngineEvent>) {
        if let (EngineEvent::ContextUsage { .. }, Some(session)) =
            (&envelope.payload, envelope.session_id)
        {
            self.context_updated_at
                .insert(session, chrono::Utc::now().timestamp_millis());
        }
    }

    /// The context ring in the composer toolbar, shown once a chat uses at
    /// least [`RING_THRESHOLD`] percent of the window; it opens the breakdown.
    pub(super) fn composer_context(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let breakdown = self.current_context().map(ContextBreakdown::new)?;
        let percent = breakdown
            .percent()
            .filter(|percent| *percent >= RING_THRESHOLD)?;
        let tooltip = format!("Context window · {} used", breakdown.percent_label());
        let weak = cx.entity().downgrade();
        let mut trigger_layout = div().flex_shrink_0();
        Some(
            Popover::new("composer-context-popover")
                .appearance(false)
                .trigger_style(trigger_layout.style().clone())
                .anchor(Anchor::BottomRight)
                .offset(px(8.))
                .trigger(
                    MenuButton::new("composer-context")
                        .accessibility_label("Context window")
                        .ghost()
                        .h(px(30.))
                        .px_1p5()
                        .gap_1p5()
                        .child(usage_ring(percent))
                        // The number makes the ring read as a gauge, never
                        // as a loading spinner.
                        .child(
                            div()
                                .single_line()
                                .text_size(px(12.5))
                                .text_color(rgb(text_secondary()))
                                .child(format!("{percent}%")),
                        )
                        .tooltip(tooltip),
                )
                .content(move |_, _, cx| {
                    let panel = weak
                        .update(cx, |view, _| view.context_panel())
                        .unwrap_or_else(|_| div().into_any_element());
                    crate::motion::popover_surface("composer-context-surface", panel)
                        .into_any_element()
                })
                .into_any_element(),
        )
    }

    fn context_panel(&self) -> AnyElement {
        let panel = div()
            .id("composer-context-panel")
            .w(px(300.))
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(text()))
                    .child("Context window"),
            );
        let Some(usage) = self.current_context() else {
            return panel
                .child(subtitle("Context appears after the first reply."))
                .into_any_element();
        };
        let breakdown = ContextBreakdown::new(usage);
        let summary = if breakdown.capacity > 0 {
            format!(
                "{} used · {} / {}",
                breakdown.percent_label(),
                format_tokens(breakdown.used),
                format_tokens(breakdown.capacity)
            )
        } else {
            format!(
                "{} used · window size unknown",
                format_tokens(breakdown.used)
            )
        };
        let segments = [
            (usage.messages, MESSAGES()),
            (usage.system, SYSTEM()),
            (usage.tools, TOOLS()),
            (usage.summary, SUMMARY()),
            (breakdown.free, FREE()),
            (breakdown.buffer, BUFFER()),
        ];
        let mut bar = div()
            .w_full()
            .h(px(8.))
            .flex()
            .rounded_full()
            .overflow_hidden()
            .bg(rgb(FREE()));
        for (tokens, color) in segments {
            let fraction = breakdown.fraction(tokens);
            if fraction > 0. {
                bar = bar.child(div().h_full().w(relative(fraction)).bg(rgb(color)));
            }
        }
        let updated = self
            .context_updated_at
            .get(&self.session)
            .map(|at| {
                format!(
                    "Last updated {}",
                    updated_label(*at, chrono::Utc::now().timestamp_millis())
                )
            })
            .unwrap_or_else(|| "Last updated with the latest reply".into());
        let compaction = if usage.compacted {
            "Earlier work was condensed to fit the window."
        } else {
            "Nothing has been condensed yet."
        };
        panel
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(rgb(text_secondary()))
                    .child(summary),
            )
            .child(bar)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(context_row(
                        Some(MESSAGES()),
                        "Messages",
                        tokens(usage.messages),
                    ))
                    .child(context_row(
                        Some(SYSTEM()),
                        "System prompt",
                        tokens(usage.system),
                    ))
                    .child(context_row(
                        Some(TOOLS()),
                        &format!("Tools ({})", usage.tool_count),
                        tokens(usage.tools),
                    ))
                    .child(context_row(
                        None,
                        &format!("Deferred tools ({})", usage.deferred_tool_count),
                        "not counted".into(),
                    ))
                    .child(context_row(
                        Some(SUMMARY()),
                        "Condensed earlier work",
                        if usage.compacted {
                            tokens(usage.summary)
                        } else {
                            "none".into()
                        },
                    ))
                    .child(context_row(
                        Some(BUFFER()),
                        "Autocompact buffer",
                        tokens(breakdown.buffer),
                    ))
                    .child(context_row(
                        Some(FREE()),
                        "Free space",
                        tokens(breakdown.free),
                    )),
            )
            .child(divider())
            .child(subtitle(compaction))
            .child(subtitle(updated))
            .into_any_element()
    }
}

fn tokens(count: u64) -> String {
    format!("{} tokens", format_tokens(count))
}

fn context_row(swatch: Option<u32>, label: &str, value: String) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_size(px(12.))
        .child(
            div()
                .size(px(8.))
                .flex_shrink_0()
                .rounded(px(2.))
                .when_some(swatch, |swatch_box, color| swatch_box.bg(rgb(color))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .single_line()
                .text_color(rgb(text_secondary()))
                .child(label.to_owned()),
        )
        .child(div().flex_shrink_0().text_color(rgb(muted())).child(value))
}

#[cfg(test)]
#[path = "context_panel/tests.rs"]
mod tests;
