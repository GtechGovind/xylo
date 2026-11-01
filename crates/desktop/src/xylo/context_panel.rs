//! The composer's visible Context control. It reads the engine's `ContextUsage` for the open chat (the accounting of
//! the latest request), never summed per-turn usage or a character estimate.
use super::*;
use gpui_kit::base::TestSupportExt;
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

/// Segment colors of the bar, shared with the row swatches.
const MESSAGES: fn() -> u32 = brand;
const SYSTEM: fn() -> u32 = lavender;
const TOOLS: fn() -> u32 = accent_text;
const SUMMARY: fn() -> u32 = success;
const BUFFER: fn() -> u32 = warning_bg;
const FREE: fn() -> u32 = hover;

/// Context and usage receipts share one scroll bound. The surrounding
/// popover contributes padding and a border outside this viewport.
pub(super) fn receipt_viewport(
    id: &'static str,
    width: f32,
    window: &Window,
) -> impl IntoElement + ParentElement {
    div()
        .id(id)
        .test_support()
        .w(px(width).min((window.viewport_size().width - px(80.)).max(px(0.))))
        .max_h((window.viewport_size().height - px(180.)).clamp(px(100.), px(480.)))
        .overflow_y_scroll()
}

impl Xylo {
    pub(super) fn composer_context(&self, cx: &Context<Self>) -> AnyElement {
        use gpui_kit::component::WindowExt;
        let label = self
            .current_context()
            .filter(|usage| usage.capacity > 0)
            .map(|usage| format!("Context {}", ContextBreakdown::new(usage).percent_label()))
            .unwrap_or_else(|| "Context".into());
        button("composer-context", label.clone())
            .accessibility_label(format!("{label}. View context details"))
            .h(px(26.))
            .min_h(px(26.))
            .px(px(5.))
            .text_size(px(11.))
            .font_weight(FontWeight::NORMAL)
            .text_color(rgb(muted()))
            .on_click(cx.listener(|_view, _, window, cx| {
                let weak = cx.entity().downgrade();
                window.open_dialog(cx, move |dialog, window, cx| {
                    let (content, width) = weak
                        .update(cx, |view, _| {
                            (view.context_panel(window), view.context_panel_width())
                        })
                        .unwrap_or_else(|_| (div().into_any_element(), 480.));
                    dialog
                        .title("Context")
                        .width(px(width))
                        .rounded(px(16.))
                        .bg(rgb(panel()))
                        .border_color(rgb(border_strong()))
                        .child(content)
                });
            }))
            .into_any_element()
    }

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

    pub(super) fn context_panel_width(&self) -> f32 {
        if self.current_context().is_some() {
            720.
        } else {
            480.
        }
    }

    pub(super) fn context_panel(&self, window: &Window) -> AnyElement {
        let viewport = receipt_viewport(
            "composer-context-panel",
            self.context_panel_width() - 60.,
            window,
        );
        let panel = div()
            .id("composer-context-content")
            .test_support()
            .w_full()
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
        let panel = panel.child(subtitle(
            "The configured limit for this connection. It may differ from the model’s maximum.",
        ));
        let Some(usage) = self.current_context() else {
            let message = if self.settings.model_ready() {
                format!(
                    "{} · {} tokens. Usage appears after the first request.",
                    self.settings.provider.model,
                    format_tokens(u64::from(self.settings.provider.context_window()))
                )
            } else {
                "Context details appear after you choose a model and start a task.".into()
            };
            return viewport
                .child(panel.child(subtitle(message)))
                .into_any_element();
        };
        let model = self
            .turns
            .get(&self.session)
            .and_then(|turn| turn.context_model.as_deref());
        let panel = panel.child(subtitle(match model {
            Some(model) => format!("Latest request · {model}"),
            None => "Latest request".into(),
        }));
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
            "Earlier messages were summarized to make room for new work."
        } else {
            ""
        };
        let content = panel
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
                        "App instructions",
                        tokens(usage.system),
                    ))
                    .child(context_row(
                        Some(TOOLS()),
                        &format!("Tools ({})", usage.tool_count),
                        tokens(usage.tools),
                    ))
                    .child(context_row(
                        None,
                        &format!("Tools available on demand ({})", usage.deferred_tool_count),
                        "not counted".into(),
                    ))
                    .child(context_row(
                        Some(SUMMARY()),
                        "Earlier message summary",
                        if usage.compacted {
                            tokens(usage.summary)
                        } else {
                            "none".into()
                        },
                    ))
                    .child(context_row(
                        Some(BUFFER()),
                        "Reserved space",
                        tokens(breakdown.buffer),
                    ))
                    .child(context_row(
                        Some(FREE()),
                        "Free space",
                        tokens(breakdown.free),
                    )),
            )
            .child(divider())
            .when(usage.compacted, |panel| panel.child(subtitle(compaction)))
            .child(subtitle(updated));
        viewport.child(content).into_any_element()
    }
    pub(super) fn quota_rows(&self) -> AnyElement {
        let mut rows = div().flex().flex_col().gap_1();
        if let Some((model, limits)) = self
            .turns
            .get(&self.session)
            .and_then(|turn| turn.rate_limits.as_ref())
        {
            rows = rows.child(subtitle(format!("Remaining quota · {model}")));
            for (label, value) in [
                ("Requests", limits.requests),
                ("Tokens", limits.tokens),
                ("Input tokens", limits.input_tokens),
                ("Output tokens", limits.output_tokens),
            ] {
                if let Some(value) = value {
                    rows = rows.child(context_row(None, label, value.to_string()));
                }
            }
            rows = rows.child(subtitle(format!(
                "Reported {}",
                updated_label(limits.observed_at_ms, now_ms())
            )));
        }
        rows.into_any_element()
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

#[cfg(all(test, not(feature = "validation")))]
#[path = "context_panel/tests.rs"]
mod tests;
