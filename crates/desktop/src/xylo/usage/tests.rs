use super::{CostEstimate, cost_label};
use crate::xylo::Xylo;
use gpui_kit::{AppContext, TestAppContext, WindowOptions};

#[test]
fn usage_labels_distinguish_free_estimates_unknown_and_partial_prices() {
    assert_eq!(
        cost_label(Some(CostEstimate {
            usd: 0.,
            unpriced_calls: 0
        })),
        "$0.0000 est."
    );
    assert_eq!(cost_label(None), "Cost unavailable");
    assert!(
        cost_label(Some(CostEstimate {
            usd: 1.,
            unpriced_calls: 2
        }))
        .contains("2 unpriced")
    );
}

#[gpui_kit::test]
fn styles_and_price_forms_change_only_their_selected_settings(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    });
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            let before = view.settings.provider.clone();
            view.choose_output_style("learning".into(), cx);
            assert_eq!(view.settings.output_style, "learning");
            assert_eq!(
                serde_json::to_value(&view.settings.provider).unwrap(),
                serde_json::to_value(&before).unwrap()
            );
            view.usage_ui
                .model
                .update(cx, |field, cx| field.set_value("custom-model", window, cx));
            for (field, price) in view.usage_ui.rates.iter().zip(["1", "2", "0.1", "1.25"]) {
                field.update(cx, |field, cx| field.set_value(price, window, cx));
            }
            view.save_price_override(cx);
            assert_eq!(view.settings.pricing_overrides["custom-model"].output, 2.);
            view.usage_ui.rates[0].update(cx, |field, cx| field.set_value("NaN", window, cx));
            view.save_price_override(cx);
            assert_eq!(view.settings.pricing_overrides["custom-model"].input, 1.);
            assert!(view.usage_ui.form_error.is_some());
            view.usage_ui.editing_price = true;
            view.sync_usage_forms(window, cx);
            assert_eq!(
                view.usage_ui.rates[0].read(cx).value().as_str(),
                "NaN",
                "A host update must preserve an edited field"
            );
        })
    })
    .unwrap();
}
