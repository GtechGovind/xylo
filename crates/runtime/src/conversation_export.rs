//! Complete conversation exports, paginated from durable history.

use anyhow::{Result, ensure};
use serde_json::json;
use std::{io::Write, path::Path};
use uuid::Uuid;
use xylo_core::ChatMessage;
use xylo_services::Store;

/// Encodes display messages as independent versioned JSONL records.
pub fn messages_jsonl(session: Uuid, messages: &[ChatMessage]) -> Result<String> {
    let mut output = String::new();
    for message in messages {
        output.push_str(&serde_json::to_string(
            &json!({"version":1,"session_id":session,"type":"message","message":message}),
        )?);
        output.push('\n');
    }
    Ok(output)
}

/// Writes Markdown and the adjacent JSONL export using the same history snapshot.
pub fn write_pair(path: &Path, markdown: &str, jsonl: &str) -> Result<()> {
    ensure!(
        path.extension().is_some_and(|extension| extension == "md"),
        "Markdown export requires an .md path"
    );
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut md = tempfile::NamedTempFile::new_in(parent)?;
    let mut lines = tempfile::NamedTempFile::new_in(parent)?;
    md.write_all(markdown.as_bytes())?;
    lines.write_all(jsonl.as_bytes())?;
    md.as_file().sync_all()?;
    lines.as_file().sync_all()?;
    lines.persist(path.with_extension("jsonl"))?;
    md.persist(path)?;
    Ok(())
}

/// Exports every persisted message, including chats longer than one database page.
pub fn export(store: &Store, session: Uuid, path: &Path) -> Result<()> {
    let conversation = store
        .conversation(session)?
        .ok_or_else(|| anyhow::anyhow!("Conversation does not exist"))?;
    let mut pages = Vec::new();
    let mut cursor = None;
    loop {
        let page = store.messages_page(session, cursor, 200)?;
        cursor = page.before;
        pages.push(page.messages);
        if !page.has_more {
            break;
        }
    }
    let messages = pages.into_iter().rev().flatten().collect::<Vec<_>>();
    let mut markdown = format!("# {}\n", conversation.title);
    for message in &messages {
        markdown.push_str(&format!(
            "\n## {}\n\n{}\n",
            if message.role == "user" {
                "You"
            } else {
                "Xylo"
            },
            message.content
        ));
    }
    write_pair(path, &markdown, &messages_jsonl(session, &messages)?)
}

#[cfg(test)]
#[path = "conversation_export/tests.rs"]
mod tests;
