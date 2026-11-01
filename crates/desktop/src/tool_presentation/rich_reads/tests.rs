use super::*;
use serde_json::json;

#[test]
fn rich_reads_show_their_actual_units_and_content() {
    let (meta, body) = prepare(&json!({"images":[{"id":"image"}]})).unwrap();
    assert_eq!(meta, "1 image");
    assert!(matches!(body,ToolBody::Message(text) if text=="Image content loaded"));
    let (meta, body) = prepare(&json!({"pages":[{"page":3,"text":"PDF text"}]})).unwrap();
    assert_eq!(meta, "1 page");
    assert!(
        matches!(body,ToolBody::Message(text) if text.contains("Page 3")&&text.contains("PDF text"))
    );
    let (meta,body)=prepare(&json!({"cells":[{"cell_id":"a","cell_type":"code","source":"print(1)","outputs":[{"text":["1\n"]}]}]})).unwrap();
    assert_eq!(meta, "1 cell");
    assert!(
        matches!(body,ToolBody::Message(text) if text.contains("print(1)")&&text.contains("Output: 1"))
    );
    assert!(prepare(&json!({"content":"text","total_lines":1})).is_none());
}
