use super::{InstalledPlugin, PluginCatalog, Request, RequestKind};
use crate::xylo::Xylo;
use gpui_kit::{AppContext, TestAppContext, WindowOptions};
use serde_json::json;

fn plugin(id: &str) -> InstalledPlugin {
    serde_json::from_value(json!({
        "manifest": {
            "format": 1, "id": id, "name": id, "version": "1",
            "entrypoint": "plugin.wasm", "sha256": "fixture",
            "capabilities": [], "budgets": xylo_core::PluginBudgets::default()
        },
        "package_sha256": "fixture", "enabled": true, "grants": [],
        "installed_at": 0, "updated_at": 0
    }))
    .unwrap()
}

#[gpui_kit::test]
fn run_results_belong_to_the_module_and_project_that_produced_them(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    });
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.plugins.catalog = Some(PluginCatalog {
                revision: "1".into(),
                plugins: vec![plugin("module-a"), plugin("module-b")],
            });
            view.plugin_apply_selection("module-a");
            let id = uuid::Uuid::new_v4();
            view.plugins.pending.insert(
                id,
                Request {
                    epoch: view.plugins.epoch,
                    kind: RequestKind::Run,
                    tool: "plugin_run_installed".into(),
                    args: json!({}),
                },
            );
            view.plugin_result(
                id,
                Ok(json!({"output": "A's result", "artifacts": []})),
                window,
                cx,
            );
            assert!(view.plugins.has_selected_output());
            assert_eq!(view.plugins.output.read(cx).value().as_str(), "A's result");
            view.plugin_apply_selection("module-b");
            assert!(
                !view.plugins.has_selected_output(),
                "B must not show A's output"
            );
            view.plugin_apply_selection("module-a");
            assert!(view.plugins.has_selected_output());
            view.plugin_scope_changed(cx);
            assert!(
                !view.plugins.has_selected_output(),
                "Another project must not inherit the result"
            );
        });
    })
    .unwrap();
}
