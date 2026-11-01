//! User-visible MCP elicitation forms. Server content never grants tool authority.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpFormRequest {
    pub id: Uuid,
    pub server_id: Uuid,
    pub server_name: String,
    pub message: String,
    pub schema: Value,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpFormAction {
    Accept,
    Decline,
    Cancel,
}

/// Validates the supported flat primitive form before sending a user's response.
pub fn validate_mcp_form(schema: &Value, content: &Value) -> Result<(), String> {
    let properties = schema["properties"]
        .as_object()
        .ok_or("MCP form properties are missing")?;
    let content = content
        .as_object()
        .ok_or("MCP form response must be an object")?;
    for required in schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !content.contains_key(required) {
            return Err(format!("{required} is required"));
        }
    }
    for (name, value) in content {
        let property = properties
            .get(name)
            .ok_or_else(|| format!("Unknown form field {name}"))?;
        let valid = match property["type"].as_str() {
            Some("string") => value.as_str().is_some_and(|text| {
                property["minLength"]
                    .as_u64()
                    .is_none_or(|min| text.chars().count() as u64 >= min)
                    && property["maxLength"]
                        .as_u64()
                        .is_none_or(|max| text.chars().count() as u64 <= max)
            }),
            Some("number" | "integer") => value.as_f64().is_some_and(|number| {
                (property["type"] != "integer" || number.fract() == 0.0)
                    && property["minimum"].as_f64().is_none_or(|min| number >= min)
                    && property["maximum"].as_f64().is_none_or(|max| number <= max)
            }),
            Some("boolean") => value.is_boolean(),
            _ => false,
        };
        if !valid
            || property["enum"]
                .as_array()
                .is_some_and(|values| !values.contains(value))
        {
            return Err(format!("Enter a valid value for {name}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
