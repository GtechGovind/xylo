//! Conversation-local checkout selection and approved worktree transitions.
use super::ToolBroker;
use anyhow::{Context, Result, ensure};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use xylo_runtime::workspace::worktree;

type UseCounts = Arc<Mutex<HashMap<(uuid::Uuid, PathBuf), usize>>>;

/// Owned background work keeps its original checkout alive across root changes.
pub struct WorkspaceLease {
    uses: UseCounts,
    key: (uuid::Uuid, PathBuf),
}
impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        let mut counts = self.uses.lock();
        if let Some(count) = counts.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.key);
            }
        }
    }
}
#[derive(Clone, Default)]
pub(super) struct WorkspaceUses(UseCounts);
impl WorkspaceUses {
    fn key(session: uuid::Uuid, root: &Path) -> (uuid::Uuid, PathBuf) {
        (
            session,
            root.canonicalize().unwrap_or_else(|_| root.to_owned()),
        )
    }
    pub fn retain(&self, session: uuid::Uuid, root: &Path) -> WorkspaceLease {
        let key = Self::key(session, root);
        *self.0.lock().entry(key.clone()).or_default() += 1;
        WorkspaceLease {
            uses: self.0.clone(),
            key,
        }
    }
    pub fn in_use(&self, session: uuid::Uuid, root: &Path) -> bool {
        self.0.lock().contains_key(&Self::key(session, root))
    }
}

pub(super) struct LiveWorkspace {
    pub root: Mutex<PathBuf>,
    pub gate: RwLock<()>,
    pub revision: std::sync::atomic::AtomicU64,
    pub prompt: Mutex<Option<String>>,
}
impl LiveWorkspace {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root: Mutex::new(root),
            gate: RwLock::new(()),
            revision: std::sync::atomic::AtomicU64::new(0),
            prompt: Mutex::new(None),
        }
    }
}
pub(super) fn execute(broker: &ToolBroker, name: &str, args: &Value) -> Result<Value> {
    let root = broker.workspace_root();
    let (selected, branch, kept) = if name == "worktree_enter" {
        ensure!(
            worktree::inspect(&root).is_err(),
            "This chat already uses a worktree; exit it first"
        );
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("chat-{}", uuid::Uuid::new_v4().simple()));
        let base = args.get("base").and_then(Value::as_str).unwrap_or("HEAD");
        let tree = broker
            .runtime
            .block_on(worktree::create(&root, &name, base))?;
        (tree.path, Some(tree.branch), false)
    } else {
        let tree = worktree::inspect(&root)?;
        // Capability helpers must finish before their checkout can disappear.
        broker.runtime.block_on(broker.capabilities.shutdown())?;
        let keep = args
            .get("keep")
            .and_then(Value::as_bool)
            .context("keep must be a boolean")?;
        let shared = broker.store.worktree_conversations()?.iter().any(|chat| {
            chat.id != broker.request.session_id && !chat.archived && chat.workspace == root
        });
        let kept = keep
            || shared
            // Persistent MCP and native language-server processes belong to
            // this worker, not its most recent turn. Manual removal retires it.
            || broker.connectors.may_have_local_processes()
            || broker
                .session
                .lsp
                .get()
                .is_some_and(|service| service.may_have_processes(&root))
            || broker
                .session
                .workspace_in_use(broker.request.session_id, &root)
            || !broker
                .runtime
                .block_on(worktree::status(&root))?
                .removable();
        if !kept {
            broker.runtime.block_on(worktree::remove(&root, false))?;
        }
        (tree.repository, None, kept)
    };
    broker
        .store
        .set_conversation_workspace(broker.request.session_id, selected.clone())?;
    *broker.workspace.root.lock() = selected.clone();
    broker.rules.lock().clear();
    if name == "worktree_enter"
        && let Err(error) = broker.runtime.block_on(broker.capabilities.shutdown())
    {
        tracing::warn!(%error, "Previous checkout helpers need cleanup");
    }
    let mut context = crate::prompts::PromptContext::detect(
        &selected,
        broker.mode(),
        xylo_core::now_ms(),
        broker.window().prompt_budget(),
    );
    broker.record_instruction_sources(&context.instruction_sources)?;
    context.set_skills_index(broker.project_skill_context().unwrap_or_default());
    let base_prompt = crate::prompts::system(&context);
    let (system, style_notice) = match broker.store.settings().and_then(|settings| {
        xylo_runtime::workspace::output_styles::current(&selected, &settings.output_style)
    }) {
        Ok(style) => (
            crate::prompts::output_styles::apply(base_prompt, &style),
            None,
        ),
        Err(_) => (
            base_prompt,
            Some("The selected output style is unavailable in this checkout; using Default."),
        ),
    };
    *broker.workspace.prompt.lock() = Some(system.clone());
    broker
        .workspace
        .revision
        .fetch_add(1, std::sync::atomic::Ordering::Release);
    broker.emit(xylo_core::EngineEvent::WorkspaceChanged {
        workspace: selected.clone(),
        branch: branch.clone(),
    })?;
    Ok(
        json!({"workspace":selected,"branch":branch,"kept":kept,"instructions":system,"output_style_notice":style_notice,"note":"Future tools use this checkout. Re-read files before editing. Existing background tools keep their original checkout until they stop."}),
    )
}

#[cfg(test)]
mod tests;
