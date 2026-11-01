//! Explicit user edits to model prices and the optional status command.
use super::*;

impl Xylo {
    pub(in crate::xylo) fn sync_usage_forms(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.usage_ui.editing_price {
            self.load_price_form(window, cx);
        }
        if !self.usage_ui.editing_command {
            let command = self
                .settings
                .status_line_command
                .clone()
                .unwrap_or_default();
            self.usage_ui
                .command
                .update(cx, |input, cx| input.set_value(command, window, cx));
        }
    }

    fn load_price_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.settings.provider.model.clone();
        let price = self
            .settings
            .pricing_overrides
            .get(&model)
            .copied()
            .or_else(|| xylo_core::pricing::builtin(&model, 0));
        self.usage_ui
            .model
            .update(cx, |input, cx| input.set_value(model, window, cx));
        for (field, value) in self.usage_ui.rates.iter().zip(
            price
                .map(|price| {
                    [
                        price.input,
                        price.output,
                        price.cache_read,
                        price.cache_write,
                    ]
                })
                .unwrap_or([0.; 4]),
        ) {
            field.update(cx, |input, cx| {
                input.set_value(value.to_string(), window, cx)
            });
        }
    }

    pub(in crate::xylo) fn save_price_override(&mut self, cx: &mut Context<Self>) {
        let model = self.usage_ui.model.read(cx).value().trim().to_owned();
        let values = self
            .usage_ui
            .rates
            .iter()
            .map(|field| field.read(cx).value().trim().parse::<f64>())
            .collect::<Result<Vec<_>, _>>();
        let Ok(values) = values else {
            self.usage_ui.form_error = Some("Enter a number for each price.".into());
            cx.notify();
            return;
        };
        let price = ModelPrice {
            input: values[0],
            output: values[1],
            cache_read: values[2],
            cache_write: values[3],
        };
        if model.is_empty() || model.len() > 200 || !price.valid() {
            self.usage_ui.form_error =
                Some("Enter a model and nonnegative prices up to 1,000,000.".into());
            cx.notify();
            return;
        }
        self.settings.pricing_overrides.insert(model, price);
        self.usage_ui.editing_price = false;
        self.usage_ui.form_error = None;
        let settings = self.settings.clone();
        self.host_call(cx, |host| host.save_settings(settings));
        cx.notify();
    }

    pub(in crate::xylo) fn pricing_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let fields = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(Input::new(&self.usage_ui.model).aria_label("Pricing model"));
        let fields = [
            "Input tokens",
            "Output tokens",
            "Cache reads",
            "Cache writes",
        ]
        .into_iter()
        .zip(&self.usage_ui.rates)
        .fold(fields, |body, (label, field)| {
            body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(115.)).child(label))
                    .child(Input::new(field).aria_label(label)),
            )
        });
        settings_ui::section("Token prices",Some("USD per million tokens. Published standard prices are built in; overrides affect future estimates; recorded totals stay unchanged.".into()),
            fields.child(div().flex().gap_2()
                .child(settings_ui::primary_button("save-price-override","Save override").on_click(cx.listener(|view,_,_,cx|view.save_price_override(cx))))
                .child(settings_ui::secondary_button("current-price-model","Use current model").on_click(cx.listener(|view,_,window,cx|view.load_price_form(window,cx))))
                .child(settings_ui::secondary_button("restore-published-price","Restore published price").on_click(cx.listener(|view,_,_,cx|{let model=view.usage_ui.model.read(cx).value().trim().to_owned();view.settings.pricing_overrides.remove(&model);view.usage_ui.editing_price=false;let settings=view.settings.clone();view.host_call(cx,|host|host.save_settings(settings));cx.notify();}))))
                .children(self.usage_ui.form_error.as_ref().map(|error|settings_ui::inline_error(error)))
                .children(self.settings.pricing_overrides.iter().map(|(model,price)|settings_ui::caption(format!("{model} · input {} / output {} / cache read {} / cache write {}",price.input,price.output,price.cache_read,price.cache_write))))
        ).into_any_element()
    }

    pub(in crate::xylo) fn status_command_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let changed = self.usage_ui.command.read(cx).value().trim()
            != self.settings.status_line_command.as_deref().unwrap_or("");
        settings_ui::section(
            "Custom status",
            Some("Show a short message below the composer using a local command.".into()),
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(div().id("status-command-control").test_support().flex_1().min_w_0().child(
                            Input::new(&self.usage_ui.command)
                                .aria_label("Status command")
                                .min_h(px(settings_ui::CONTROL_HEIGHT))
                                .max_h(px(settings_ui::CONTROL_HEIGHT)),
                        ))
                        .child(settings_ui::primary_button("save-status-command", "Save")
                            .disabled(!changed)
                            .on_click(cx.listener(|view, _, _, cx| {
                                let command = view.usage_ui.command.read(cx).value().trim().to_owned();
                                if command.len() > 8192 {
                                    view.usage_ui.command_error = Some("This command is too long. Shorten it and try again.".into());
                                    cx.notify();
                                    return;
                                }
                                view.settings.status_line_command = (!command.is_empty()).then_some(command);
                                view.usage_ui.editing_command = false;
                                view.usage_ui.status_text = None;
                                view.usage_ui.command_error = None;
                                let settings = view.settings.clone();
                                view.host_call(cx, |host| host.save_settings(settings));
                                cx.notify();
                            }))),
                )
                .child(settings_ui::caption("Leave empty to use the built-in status. Commands receive chat data as JSON and return one line; they run offline for up to two seconds."))
                .children(self.usage_ui.command_error.as_ref().map(|error| settings_ui::inline_error(error))),
        ).into_any_element()
    }
}
