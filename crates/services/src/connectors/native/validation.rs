//! Validate the small schema vocabulary used by built-in connector tools before
//! opening credentials or sending requests. Provider-specific payload objects
//! remain open only where the published tool schema explicitly allows them.
use anyhow::{Result, ensure};
use serde_json::Value;

pub(super) fn arguments(value: &Value, schema: &Value) -> Result<()> {
    check(value, schema, "arguments")
}

fn check(value: &Value, schema: &Value, path: &str) -> Result<()> {
    let valid_type = match schema.get("type").and_then(Value::as_str) {
        Some("object") => value.is_object(),
        Some("string") => value.is_string(),
        Some("integer") => value.as_u64().is_some(),
        None => true,
        _ => false,
    };
    ensure!(valid_type, "Invalid type for {path}");
    if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
        ensure!(choices.contains(value), "Unsupported value for {path}");
    }
    if let Some(number) = value.as_u64() {
        if let Some(minimum) = schema.get("minimum").and_then(Value::as_u64) {
            ensure!(number >= minimum, "{path} is below its minimum");
        }
        if let Some(maximum) = schema.get("maximum").and_then(Value::as_u64) {
            ensure!(number <= maximum, "{path} exceeds its maximum");
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                ensure!(object.contains_key(key), "Missing argument {path}.{key}");
                ensure!(
                    !object
                        .get(key)
                        .and_then(Value::as_str)
                        .is_some_and(str::is_empty),
                    "Missing string argument {path}.{key}"
                );
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        for (key, child) in object {
            if let Some(child_schema) = properties.and_then(|properties| properties.get(key)) {
                check(child, child_schema, &format!("{path}.{key}"))?;
            } else {
                ensure!(
                    schema.get("additionalProperties").and_then(Value::as_bool) != Some(false),
                    "Unknown argument {path}.{key}"
                );
            }
        }
    }
    Ok(())
}
