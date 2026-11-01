//! Adds the explicitly selected response style to the stable system prompt.
pub(crate) fn apply(
    mut system: String,
    style: &xylo_runtime::workspace::output_styles::OutputStyle,
) -> String {
    let body = match style.key.as_str() {
        "default" => return system,
        "explanatory" => include_str!("../../prompts/output-style-explanatory.md"),
        "learning" => include_str!("../../prompts/output-style-learning.md"),
        _ => &style.prompt,
    };
    system.push_str("\n\n");
    system.push_str(body.trim());
    system
}

#[cfg(test)]
mod tests;
