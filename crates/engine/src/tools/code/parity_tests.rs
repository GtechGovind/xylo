use super::*;

#[test]
fn multi_edit_is_atomic_and_sequential() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one two one").unwrap();
    let args = json!({"edits":[{"old_string":"one","new_string":"three","replace_all":true},{"old_string":"two","new_string":"four"}]});
    let plan = plan_edit(&path, "a.txt", &args).unwrap();
    assert_eq!(plan.updated, "three four three");
    assert_eq!(plan.replacements, 3);
    assert!(plan_edit(&path, "a.txt", &json!({"edits":[{"old_string":"two","new_string":"x"},{"old_string":"absent","new_string":"y"}]})).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "one two one");
}

#[test]
fn search_supports_modes_paging_types_and_multiline() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "before\none\ntwo\nafter\none\n").unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    let cancel = CancellationToken::new();
    let count = search(
        dir.path(),
        dir.path(),
        &json!({"query":"one", "output_mode":"count", "type":"rust"}),
        &cancel,
    )
    .unwrap();
    assert_eq!(count["matches"], 2);
    let files = search(
        dir.path(),
        dir.path(),
        &json!({"query":"one","output_mode":"files","type":"rust"}),
        &cancel,
    )
    .unwrap();
    assert_eq!(files["results"], "a.rs\n");
    let asymmetric = search(
        dir.path(),
        dir.path(),
        &json!({"query":"two","before":1,"after":0}),
        &cancel,
    )
    .unwrap();
    assert!(asymmetric["results"].as_str().unwrap().contains("one"));
    assert!(!asymmetric["results"].as_str().unwrap().contains("after"));
    let multiline = search(
        dir.path(),
        dir.path(),
        &json!({"query":"one\\ntwo", "multiline":true,"before":1,"after":1}),
        &cancel,
    )
    .unwrap();
    assert!(multiline["results"].as_str().unwrap().contains("before"));
    assert!(multiline["results"].as_str().unwrap().contains("after"));
    let page = search(
        dir.path(),
        dir.path(),
        &json!({"query":"one","type":"rust","offset":1,"max_results":1}),
        &cancel,
    )
    .unwrap();
    assert!(page["results"].as_str().unwrap().contains(":5:one"));
}
