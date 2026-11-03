use super::secret_storage_detail;
use xylo_core::SecretStorage;

#[test]
fn secret_storage_defaults_to_the_prompt_free_local_file() {
    assert_eq!(SecretStorage::default(), SecretStorage::LocalFile);
    assert!(secret_storage_detail(SecretStorage::LocalFile).contains("No system prompts"));
}

#[gpui_kit::test]
fn failed_rule_dispatch_keeps_the_form_retryable(cx: &mut gpui_kit::TestAppContext) {
    use crate::xylo::Xylo;
    use gpui_kit::{AppContext, WindowOptions};
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    });
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.workspace = Some("/project".into());
            view.permission_pattern.update(cx, |input, cx| {
                input.set_value("Read(*)", window, cx);
            });
            view.add_permission_rule(super::PermissionDecision::Allow, cx);
            assert!(
                !view.permissions.loading,
                "A rejected dispatch has no receipt to wait for"
            );
            assert!(view.permissions.loading_since.is_none());
            assert!(view.permissions.error.is_some());
            assert_eq!(view.permission_pattern.read(cx).value().as_str(), "Read(*)");
            view.add_permission_rule(super::PermissionDecision::Deny, cx);
            assert!(!view.permissions.loading, "Retry must remain possible");
        });
    })
    .unwrap();
}
