use super::*;
#[test]
fn notebook_reads_outputs_and_edits_preserve_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.ipynb");
    let book = json!({"nbformat":4,"nbformat_minor":5,"metadata":{"kernelspec":{"name":"python3"}},"cells":[{"id":"first","cell_type":"code","source":["print(1)\n"],"metadata":{"tag":"kept"},"outputs":[{"output_type":"stream","text":["1\n"]}],"execution_count":1}]});
    std::fs::write(&path, book.to_string()).unwrap();
    assert_eq!(
        read(&path, "test.ipynb", &json!({})).unwrap()["cells"][0]["outputs"],
        book["cells"][0]["outputs"]
    );
    let plan=plan(&path,"test.ipynb",&json!({"cell_id":"first","new_source":"# title","cell_type":"markdown","edit_mode":"replace"})).unwrap();
    let next: Value = serde_json::from_str(&plan.updated).unwrap();
    assert_eq!(next["metadata"], book["metadata"]);
    assert_eq!(next["cells"][0]["metadata"], book["cells"][0]["metadata"]);
    assert!(next["cells"][0].get("outputs").is_none());
    let code_replaced = plan_fn(
        &path,
        &json!({"cell_id":"first","new_source":"print(2)","edit_mode":"replace"}),
    );
    assert_eq!(code_replaced["cells"][0]["outputs"], json!([]));
    assert_eq!(code_replaced["cells"][0]["execution_count"], Value::Null);
    let inserted = plan_fn(
        &path,
        &json!({"cell_id":"first","new_source":"2","edit_mode":"insert"}),
    );
    assert_eq!(inserted["cells"].as_array().unwrap().len(), 2);
    let deleted = plan_fn(
        &path,
        &json!({"cell_id":"first","new_source":"","edit_mode":"delete"}),
    );
    assert_eq!(deleted["cells"], json!([]));
}
fn plan_fn(path: &Path, args: &Value) -> Value {
    serde_json::from_str(&plan(path, "test.ipynb", args).unwrap().updated).unwrap()
}

#[test]
fn malformed_cells_are_refused_without_panicking_or_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.ipynb");
    let original = json!({"nbformat":4,"cells":[42]}).to_string();
    std::fs::write(&path, &original).unwrap();
    assert!(
        plan(
            &path,
            "broken.ipynb",
            &json!({"cell_id":"0","new_source":"x","edit_mode":"replace"})
        )
        .is_err()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
}
