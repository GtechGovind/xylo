use super::*;
use xylo_core::ProviderConfig;

fn window(context_tokens: u32) -> EffectiveWindow {
    EffectiveWindow::of(&ProviderConfig {
        context_tokens,
        output_tokens: 2048,
        ..Default::default()
    })
}

#[test]
fn ranged_reads_number_lines_and_report_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    let body: String = (1..=50).map(|n| format!("line {n}\n")).collect();
    std::fs::write(&file, body).unwrap();
    let first = read(&file, "a.txt", &json!({"limit": 10}), &window(200_000)).unwrap();
    assert_eq!(first["end_line"], 10);
    assert_eq!(first["total_lines"], 50);
    assert_eq!(first["next_offset"], 11);
    assert!(
        first["content"]
            .as_str()
            .unwrap()
            .starts_with("     1\tline 1\n")
    );
    let tail = read(&file, "a.txt", &json!({"offset": 45}), &window(200_000)).unwrap();
    assert_eq!(tail["start_line"], 45);
    assert_eq!(tail["truncated"], false);
    assert!(read(&file, "a.txt", &json!({"offset": 51}), &window(200_000)).is_err());
}

#[test]
fn large_reads_stop_on_the_byte_budget() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("big.txt");
    std::fs::write(&file, format!("{}\n", "x".repeat(100)).repeat(5000)).unwrap();
    let large = window(200_000);
    let value = read(&file, "big.txt", &json!({}), &large).unwrap();
    assert!(value["content"].as_str().unwrap().len() <= large.file_read_bytes());
    assert_eq!(value["truncated"], true);
}

#[test]
fn default_reads_shrink_with_the_context_window() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("long.rs");
    std::fs::write(&file, "let x = 1;\n".repeat(5000)).unwrap();
    let small = window(8192);
    let large = window(200_000);
    let narrow = read(&file, "long.rs", &json!({}), &small).unwrap();
    let wide = read(&file, "long.rs", &json!({}), &large).unwrap();
    let narrow_end = narrow["end_line"].as_u64().unwrap();
    assert!(narrow_end <= small.file_read_lines());
    assert!(narrow["content"].as_str().unwrap().len() <= small.file_read_bytes());
    assert!(wide["end_line"].as_u64().unwrap() > narrow_end);
    assert_eq!(narrow["next_offset"], narrow_end + 1);
    // An explicit limit still pages, within the window's byte bound.
    let paged = read(
        &file,
        "long.rs",
        &json!({"offset": 4990, "limit": 5}),
        &small,
    )
    .unwrap();
    assert_eq!(paged["end_line"], 4994);
}

#[test]
fn edits_require_a_unique_exact_match() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.rs");
    std::fs::write(&file, "fn a() {}\nfn b() {}\nfn a() {}\n").unwrap();
    let error = plan_edit(
        &file,
        "a.rs",
        &json!({"old_string":"fn a() {}","new_string":"x"}),
    )
    .err()
    .unwrap();
    assert!(error.downcast_ref::<Unapplied>().is_some());
    assert!(error.to_string().contains("2 places"));
    let all = plan_edit(
        &file,
        "a.rs",
        &json!({"old_string":"fn a() {}","new_string":"fn c() {}","replace_all":true}),
    )
    .unwrap();
    assert_eq!(all.replacements, 2);
    assert_eq!(all.updated, "fn c() {}\nfn b() {}\nfn c() {}\n");
    let missing = plan_edit(
        &file,
        "a.rs",
        &json!({"old_string":"nope","new_string":"x"}),
    );
    assert!(missing.err().unwrap().downcast_ref::<Unapplied>().is_some());
}

#[test]
fn edits_keep_crlf_line_endings() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("w.txt");
    std::fs::write(&file, "one\r\ntwo\r\nthree\r\n").unwrap();
    let plan = plan_edit(
        &file,
        "w.txt",
        &json!({"old_string":"one\ntwo","new_string":"uno\ndos"}),
    )
    .unwrap();
    assert_eq!(plan.updated, "uno\r\ndos\r\nthree\r\n");
    let single = plan_edit(
        &file,
        "w.txt",
        &json!({"old_string":"two","new_string":"two\nand a half"}),
    )
    .unwrap();
    assert_eq!(single.updated, "one\r\ntwo\r\nand a half\r\nthree\r\n");
}

#[test]
fn search_and_find_respect_ignore_files_and_filters() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn needle() {}\n").unwrap();
    std::fs::write(root.join("src/notes.md"), "needle\n").unwrap();
    std::fs::write(root.join("target/out.rs"), "fn needle() {}\n").unwrap();
    let cancel = CancellationToken::new();
    let hits = search(
        root,
        root,
        &json!({"query":"needle","glob":"*.rs"}),
        &cancel,
    )
    .unwrap();
    assert_eq!(hits["matches"], 1);
    assert!(hits["results"].as_str().unwrap().contains("src/lib.rs:1:"));
    let files = find(root, root, &json!({"pattern":"*.rs"}), &cancel).unwrap();
    assert_eq!(files["files"], json!(["src/lib.rs"]));
    let none = search(root, root, &json!({"query":"absent"}), &cancel).unwrap();
    assert_eq!(none["results"], "No matches");
}

#[test]
fn missing_programs_are_definitive() {
    let dir = tempfile::tempdir().unwrap();
    let error = resolve_program("definitely-not-a-real-program-xyz", dir.path())
        .err()
        .unwrap();
    assert!(error.downcast_ref::<Unapplied>().is_some());
}

#[test]
fn diffs_count_changes() {
    let value = diff("a.txt", "a\nb\nc\n", "a\nB\nc\nd\n");
    assert_eq!(value["added"], 2);
    assert_eq!(value["removed"], 1);
    assert!(value["diff"].as_str().unwrap().contains("+B"));
}
