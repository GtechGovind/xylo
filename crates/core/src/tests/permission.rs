use super::super::*;

#[test]
fn permission_modes_use_snake_case_names() {
    for (mode, name) in [
        (PermissionMode::AskForApproval, "\"ask_for_approval\""),
        (PermissionMode::ApproveForMe, "\"approve_for_me\""),
        (PermissionMode::FullAccess, "\"full_access\""),
        (PermissionMode::ReadOnly, "\"read_only\""),
    ] {
        assert_eq!(serde_json::to_string(&mode).unwrap(), name);
        assert_eq!(serde_json::from_str::<PermissionMode>(name).unwrap(), mode);
    }
    assert!(serde_json::from_str::<PermissionMode>("\"auto\"").is_err());
}

#[test]
fn only_full_access_runs_unsandboxed_and_only_read_only_refuses_writes() {
    for mode in PermissionMode::ALL {
        assert_eq!(mode.sandboxed(), mode != PermissionMode::FullAccess);
        assert_eq!(mode.allows_writes(), mode != PermissionMode::ReadOnly);
    }
}
