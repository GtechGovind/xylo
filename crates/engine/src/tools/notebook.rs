//! Bounded notebook reads and atomic cell edits that preserve notebook metadata.

use super::code::{self, EditPlan};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::Path;

fn load(path: &Path) -> Result<Value> {
    ensure!(
        std::fs::metadata(path)?.len() <= 8 * 1024 * 1024,
        "Notebook exceeds 8 MiB"
    );
    let notebook: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        notebook["nbformat"] == 4 && notebook["cells"].is_array(),
        "Expected a version 4 notebook"
    );
    ensure!(
        notebook["cells"]
            .as_array()
            .unwrap()
            .iter()
            .all(Value::is_object),
        "Notebook cells must be objects"
    );
    Ok(notebook)
}
fn source(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(lines) => lines.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}
/// Returns stable cell identities, cell sources and their stored outputs.
pub(crate) fn read(path: &Path, shown: &str, args: &Value) -> Result<Value> {
    let notebook = load(path)?;
    let offset = args
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as usize
        - 1;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(200)
        .clamp(1, 1000) as usize;
    let total = notebook["cells"].as_array().unwrap().len();
    ensure!(
        offset < total || (offset == 0 && total == 0),
        "Notebook offset exceeds its cell count"
    );
    let cells:Vec<Value>=notebook["cells"].as_array().unwrap().iter().enumerate().skip(offset).take(limit).map(|(index,cell)|json!({
        "cell_id":cell.get("id").cloned().unwrap_or(json!(index.to_string())),"cell_type":cell["cell_type"],
        "source":source(&cell["source"]),"outputs":cell.get("outputs").cloned().unwrap_or(json!([])),"execution_count":cell["execution_count"]
    })).collect();
    Ok(
        json!({"path":shown,"cells":cells,"total_cells":total,"truncated":offset+limit<total,"next_offset":if offset+limit<total {Some(offset+limit+1)} else {None}}),
    )
}
/// Plans a cell replacement, insertion after a cell, or deletion without writing.
pub(crate) fn plan(path: &Path, shown: &str, args: &Value) -> Result<EditPlan> {
    let mut notebook = load(path).map_err(|e| code::unapplied(e.to_string()))?;
    let original = std::fs::read_to_string(path)?;
    let cells = notebook["cells"].as_array_mut().unwrap();
    let id = args["cell_id"].as_str().context("cell_id is required")?;
    let mode = args["edit_mode"]
        .as_str()
        .context("edit_mode is required")?;
    let position = cells.iter().enumerate().position(|(index, cell)| {
        cell["id"]
            .as_str()
            .map_or_else(|| index.to_string() == id, |value| value == id)
    });
    let position = if mode == "insert" && id.is_empty() {
        None
    } else {
        Some(position.ok_or_else(|| {
            code::unapplied(format!("Cell {id} was not found in {shown}; read it again"))
        })?)
    };
    let kind = args["cell_type"]
        .as_str()
        .unwrap_or_else(|| {
            position
                .and_then(|index| cells[index]["cell_type"].as_str())
                .unwrap_or("code")
        })
        .to_owned();
    ensure!(
        matches!(kind.as_str(), "code" | "markdown" | "raw"),
        "Invalid cell_type"
    );
    let text = args["new_source"]
        .as_str()
        .context("new_source is required")?;
    match mode {
        "delete" => {
            cells.remove(position.unwrap());
        }
        "insert" => {
            let mut cell = json!({"id":uuid::Uuid::new_v4().to_string(),"cell_type":kind,"metadata":{},"source":text});
            if kind == "code" {
                cell["outputs"] = json!([]);
                cell["execution_count"] = Value::Null;
            }
            cells.insert(position.map_or(0, |index| index + 1), cell);
        }
        "replace" => {
            let cell = &mut cells[position.unwrap()];
            cell["source"] = json!(text);
            cell["cell_type"] = json!(kind);
            if kind == "code" {
                cell["outputs"] = json!([]);
                cell["execution_count"] = Value::Null;
            } else {
                cell.as_object_mut().unwrap().remove("outputs");
                cell.as_object_mut().unwrap().remove("execution_count");
            }
        }
        _ => return Err(code::unapplied("Unknown edit_mode")),
    }
    let updated = serde_json::to_string_pretty(&notebook)? + "\n";
    ensure!(
        updated.len() <= 8 * 1024 * 1024,
        "Edited notebook exceeds 8 MiB"
    );
    Ok(EditPlan {
        original,
        updated,
        replacements: 1,
    })
}

#[cfg(test)]
mod tests;
