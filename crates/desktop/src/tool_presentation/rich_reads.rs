//! Human-readable summaries for image, PDF, and notebook file observations.

use super::{ToolBody, short, text};
use serde_json::Value;

fn joined(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

/// Rich reads have their own units and content instead of an empty text excerpt.
pub(super) fn prepare(output: &Value) -> Option<(String, ToolBody)> {
    if let Some(cells) = output.get("cells").and_then(Value::as_array) {
        let rows = cells
            .iter()
            .take(6)
            .map(|cell| {
                let mut preview = format!(
                    "Cell {} ({})\n{}",
                    text(cell, "cell_id"),
                    text(cell, "cell_type"),
                    short(text(cell, "source"), 2000)
                );
                for item in cell["outputs"].as_array().into_iter().flatten().take(3) {
                    let output = joined(&item["text"]);
                    let output = if output.is_empty() {
                        joined(&item["data"]["text/plain"])
                    } else {
                        output
                    };
                    if !output.is_empty() {
                        preview.push_str(&format!("\nOutput: {}", short(&output, 1000)));
                    }
                }
                preview
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        return Some((
            if cells.len() == 1 {
                "1 cell".into()
            } else {
                format!("{} cells", cells.len())
            },
            ToolBody::Message(if rows.is_empty() {
                "No notebook cells".into()
            } else {
                short(&rows, 8000)
            }),
        ));
    }
    if let Some(pages) = output.get("pages").and_then(Value::as_array) {
        let rows = pages
            .iter()
            .take(20)
            .map(|page| format!("Page {}\n{}", page["page"], short(text(page, "text"), 2000)))
            .collect::<Vec<_>>()
            .join("\n\n");
        return Some((
            if pages.len() == 1 {
                "1 page".into()
            } else {
                format!("{} pages", pages.len())
            },
            ToolBody::Message(if rows.is_empty() {
                "No pages returned".into()
            } else {
                short(&rows, 8000)
            }),
        ));
    }
    if let Some(images) = output
        .get("images")
        .and_then(Value::as_array)
        .filter(|images| !images.is_empty())
    {
        let meta = if images.len() == 1 {
            "1 image".to_owned()
        } else {
            format!("{} images", images.len())
        };
        return Some((meta, ToolBody::Message("Image content loaded".into())));
    }
    None
}

#[cfg(test)]
#[path = "rich_reads/tests.rs"]
mod tests;
