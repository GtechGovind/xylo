use super::super::UiPreferences;
#[test]
fn omitted_ui_preferences_take_workspace_defaults() {
    let prefs: UiPreferences = serde_json::from_value(
        serde_json::json!({"sidebar_width":248.0,"files_width":240.0,"active_page":"chat"}),
    )
    .unwrap();
    assert!(prefs.sidebar_visible);
    assert!(!prefs.work_panel_open);
    assert!(prefs.open_projects.is_empty());
    assert_eq!(prefs.work_panel, "files");
    let saved = serde_json::to_value(&prefs).unwrap();
    let restored: UiPreferences = serde_json::from_value(saved).unwrap();
    assert_eq!(restored.active_page, "chat");
    assert_eq!(restored.sidebar_width, 248.0);
}
