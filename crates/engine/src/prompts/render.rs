//! A deliberately tiny `{{name}}` template renderer.
//!
//! Templates are scanned once; substituted values are never rescanned, so
//! user-controlled text (project instructions, skill descriptions, task
//! text) that happens to contain `{{` is inserted verbatim. A template that
//! names a placeholder without a value, supplies a value no placeholder uses,
//! or contains a malformed `{{` is rejected rather than rendered partially.

use std::fmt;

/// Why a template could not be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenderError {
    /// The template uses a placeholder for which no value was supplied.
    MissingValue(String),
    /// A value was supplied that the template never uses.
    UnusedValue(String),
    /// A `{{` is not followed by `name}}` with a lowercase snake_case name.
    Malformed(usize),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::MissingValue(name) => write!(f, "no value for placeholder {{{{{name}}}}}"),
            RenderError::UnusedValue(name) => write!(f, "value {name} is not used by the template"),
            RenderError::Malformed(offset) => write!(f, "malformed placeholder at byte {offset}"),
        }
    }
}

impl std::error::Error for RenderError {}

/// Substitutes every `{{name}}` in `template` with its value from `values`.
pub(crate) fn render(template: &str, values: &[(&str, &str)]) -> Result<String, RenderError> {
    let mut output =
        String::with_capacity(template.len() + values.iter().map(|(_, v)| v.len()).sum::<usize>());
    let mut used = vec![false; values.len()];
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        output.push_str(&rest[..start]);
        let offset = template.len() - rest.len() + start;
        let after = &rest[start + 2..];
        let end = after.find("}}").ok_or(RenderError::Malformed(offset))?;
        let name = &after[..end];
        if !is_placeholder_name(name) {
            return Err(RenderError::Malformed(offset));
        }
        let index = values
            .iter()
            .position(|(key, _)| *key == name)
            .ok_or_else(|| RenderError::MissingValue(name.to_owned()))?;
        output.push_str(values[index].1);
        used[index] = true;
        rest = &after[end + 2..];
    }
    output.push_str(rest);
    match values.iter().zip(&used).find(|(_, used)| !**used) {
        Some(((name, _), _)) => Err(RenderError::UnusedValue((*name).to_owned())),
        None => Ok(output),
    }
}

/// Placeholder names a template may use, in order of first appearance.
#[cfg(test)]
pub(crate) fn placeholders(template: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        if !names.contains(&&after[..end]) {
            names.push(&after[..end]);
        }
        rest = &after[end + 2..];
    }
    names
}

fn is_placeholder_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
}
