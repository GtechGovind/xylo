//! Primitive MCP forms reject malformed responses and retain decline semantics.
use super::*;
use serde_json::json;
#[test]
fn mcp_form_validation_checks_required_types_enum_and_bounds() {
    let schema = json!({"type":"object","properties":{"name":{"type":"string","minLength":2},"age":{"type":"integer","minimum":18},"choice":{"type":"string","enum":["a","b"]}},"required":["name","age"]});
    assert!(validate_mcp_form(&schema, &json!({"name":"Ada","age":20,"choice":"b"})).is_ok());
    for bad in [
        json!({}),
        json!({"name":"A","age":20}),
        json!({"name":"Ada","age":17}),
        json!({"name":"Ada","age":18.5}),
        json!({"name":"Ada","age":20,"choice":"x"}),
    ] {
        assert!(validate_mcp_form(&schema, &bad).is_err());
    }
}
