use super::super::CHATGPT_REF;
use super::{
    PickerRow, choice_positions, effort_caption, effort_label, format_tokens, group_label,
    picker_rows, remember_recent, sticky_header,
};
use xylo_core::{ProviderConfig, RecentModel};

fn entry(provider: &ProviderConfig) -> RecentModel {
    RecentModel {
        endpoint: provider.endpoint.clone(),
        model: provider.model.clone(),
    }
}

fn model(connection: &str, model: &str) -> ProviderConfig {
    ProviderConfig {
        name: connection.into(),
        endpoint: format!("https://{connection}.example/v1"),
        model: model.into(),
        ..ProviderConfig::default()
    }
}

fn models(rows: &[PickerRow]) -> Vec<(usize, bool, bool)> {
    rows.iter()
        .filter_map(|row| match row {
            PickerRow::Model {
                index,
                checked,
                recent,
            } => Some((*index, *checked, *recent)),
            PickerRow::Header(_) => None,
        })
        .collect()
}

fn headers(rows: &[PickerRow]) -> Vec<&str> {
    rows.iter()
        .filter_map(|row| match row {
            PickerRow::Header(name) => Some(name.as_str()),
            PickerRow::Model { .. } => None,
        })
        .collect()
}

#[test]
fn every_registered_model_is_listed_in_catalog_order_grouped_by_connection() {
    let catalog = vec![
        model("alpha", "a-1"),
        model("beta", "b-1"),
        model("alpha", "a-2"),
        model("beta", "b-2"),
    ];
    let rows = picker_rows(&catalog, &catalog[3], &[], "", None);
    assert_eq!(headers(&rows), vec!["Custom · alpha", "Custom · beta"]);
    assert_eq!(
        models(&rows),
        vec![
            (0, false, false),
            (2, false, false),
            (1, false, false),
            (3, true, false)
        ]
    );
}

#[test]
fn recent_group_needs_several_connections_and_no_search() {
    let catalog = vec![
        model("alpha", "a-1"),
        model("beta", "b-1"),
        model("alpha", "a-2"),
        model("beta", "b-2"),
    ];
    let recent = vec![
        entry(&catalog[3]),
        entry(&model("gone", "x")),
        entry(&catalog[0]),
    ];
    let rows = picker_rows(&catalog, &catalog[0], &recent, "", None);
    assert_eq!(rows[0], PickerRow::Header("Recent".into()));
    assert_eq!(models(&rows)[..2], [(3, false, true), (0, true, true)]);
    let searched = picker_rows(&catalog, &catalog[0], &recent, "B-2", None);
    assert_eq!(models(&searched), vec![(3, false, false)]);
    let single = vec![model("alpha", "a-1"), model("alpha", "a-2")];
    let rows = picker_rows(&single, &single[0], &[entry(&single[1])], "", None);
    assert!(!headers(&rows).contains(&"Recent"));
}

#[test]
fn search_matches_group_names_and_empty_catalogs_have_no_choices() {
    let catalog = vec![model("alpha", "a-1"), model("beta", "b-1")];
    assert_eq!(
        models(&picker_rows(&catalog, &catalog[0], &[], "beta", None)),
        vec![(1, false, false)]
    );
    assert!(picker_rows(&[], &ProviderConfig::default(), &[], "", None).is_empty());
    assert!(
        picker_rows(
            &[ProviderConfig::default()],
            &ProviderConfig::default(),
            &[],
            "",
            None
        )
        .is_empty()
    );
}

#[test]
fn keyboard_positions_skip_headers_and_the_pinned_header_follows_scroll() {
    let catalog = vec![
        model("alpha", "a-1"),
        model("beta", "b-1"),
        model("beta", "b-2"),
    ];
    let rows = picker_rows(&catalog, &catalog[0], &[], "", None);
    assert_eq!(choice_positions(&rows), vec![1, 3, 4]);
    assert_eq!(sticky_header(&rows, 1), Some("Custom · alpha"));
    assert_eq!(sticky_header(&rows, 4), Some("Custom · beta"));
}

#[test]
fn token_counts_and_effort_captions_read_as_product_text() {
    assert_eq!(format_tokens(950), "950");
    assert_eq!(format_tokens(8_192), "8.2K");
    assert_eq!(format_tokens(272_000), "272K");
    assert_eq!(format_tokens(1_000_000), "1M");
    assert_eq!(effort_caption("xhigh"), "Extra high");
    assert_eq!(effort_caption("max"), "Max");
    assert_eq!(effort_caption("turbo"), "Turbo");
}

#[test]
fn choosing_moves_the_model_to_the_front_without_duplicates_and_keeps_five() {
    let catalog: Vec<_> = (0..7).map(|i| model("alpha", &format!("m-{i}"))).collect();
    let mut stored = Vec::new();
    for provider in &catalog {
        remember_recent(&mut stored, provider);
    }
    assert_eq!(stored.len(), 5);
    assert_eq!(stored[0], entry(&catalog[6]));
    remember_recent(&mut stored, &catalog[4]);
    assert_eq!(stored.len(), 5);
    assert_eq!(stored[0], entry(&catalog[4]));
    assert_eq!(
        stored
            .iter()
            .filter(|stored| **stored == entry(&catalog[4]))
            .count(),
        1
    );
}

#[test]
fn chatgpt_groups_show_the_signed_in_email_and_fall_back_to_the_name() {
    let chatgpt = ProviderConfig {
        name: "ChatGPT".into(),
        credential_ref: Some(CHATGPT_REF.into()),
        model: "gpt".into(),
        ..ProviderConfig::default()
    };
    assert_eq!(
        group_label(&chatgpt, Some("user@example.com")),
        "ChatGPT · user@example.com"
    );
    assert_eq!(group_label(&chatgpt, None), "ChatGPT");
    let other = model("alpha", "a-1");
    assert_eq!(
        group_label(&other, Some("user@example.com")),
        "Custom · alpha"
    );
}

#[test]
fn the_chip_and_the_picker_share_one_effort_label() {
    let mut provider = model("work", "reasoner");
    provider.reasoning_options = Some(vec!["low".into(), "medium".into(), "high".into()]);
    provider.default_reasoning_effort = Some("medium".into());
    assert_eq!(
        effort_label(&provider).as_deref(),
        Some("Medium"),
        "The catalog default is named, never shown as an opaque 'Default'"
    );
    provider.reasoning_effort = Some("high".into());
    assert_eq!(effort_label(&provider).as_deref(), Some("High"));
    provider.reasoning_effort = None;
    provider.default_reasoning_effort = None;
    assert_eq!(effort_label(&provider).as_deref(), Some("Default"));
    provider.reasoning_options = Some(vec![]);
    assert_eq!(effort_label(&provider), None, "No options, no effort label");
}
