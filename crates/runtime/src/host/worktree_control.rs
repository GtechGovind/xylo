//! Host-owned worktree creation, startup cleanup and manual removal.
use super::*;
use crate::workspace::worktree;

const WORKTREE_SHUTDOWN_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(10)
};

impl HostState {
    pub(super) async fn create_chat_worktree(&self, session: Uuid, repo: PathBuf) -> Result<()> {
        ensure!(
            !self.workers.lock().await.contains_key(&session)
                && !self.queued.lock().contains_key(&session),
            "Start a new chat to create a worktree"
        );
        ensure!(
            self.store.conversation(session)?.is_none(),
            "Start a new chat to create a worktree"
        );
        let root = worktree::inspect(&repo).map_or(repo, |tree| tree.repository);
        let tree = worktree::create(&root, &format!("chat-{}", session.simple()), "HEAD").await?;
        self.store.upsert_conversation(&Conversation {
            id: session,
            title: "New chat".into(),
            workspace: tree.path.clone(),
            updated_at: now_ms(),
            title_source: TitleSource::Prompt,
            pinned: false,
            archived: false,
        })?;
        self.events
            .send(HostEvent::ChatWorktree {
                session,
                workspace: tree.path,
                branch: Some(tree.branch),
            })
            .await?;
        self.send_conversations().await
    }
    pub(super) async fn list_worktrees(&self, root: Option<PathBuf>) -> Result<()> {
        let workspaces = self.store.conversation_workspaces()?;
        let mut roots = std::collections::BTreeSet::new();
        if let Some(root) = root {
            roots.insert(worktree::inspect(&root).map_or(root, |tree| tree.repository));
        }
        for workspace in workspaces {
            if let Ok(tree) = worktree::inspect(&workspace) {
                roots.insert(tree.repository);
            } else if workspace.join(".git").is_dir() {
                roots.insert(workspace.canonicalize()?);
            }
        }
        let mut trees = Vec::new();
        for root in roots {
            trees.extend(worktree::list(&root).await?);
        }
        self.events.send(HostEvent::Worktrees(trees)).await?;
        Ok(())
    }
    pub(super) async fn remove_worktree(&self, path: PathBuf, force: bool) -> Result<()> {
        let tree = worktree::inspect(&path)?;
        let path = tree.path.clone();
        let chats = self.store.worktree_conversations()?;
        let associated = chats
            .iter()
            .filter(|chat| chat.workspace == path)
            .collect::<Vec<_>>();
        let mut workers = self.workers.lock().await;
        let owners: std::collections::BTreeSet<_> = associated
            .iter()
            .map(|chat| chat.id)
            .chain(
                workers
                    .iter()
                    .filter(|(_, worker)| worker.owns_workspace(&path))
                    .map(|(id, _)| *id),
            )
            .collect();
        for id in &owners {
            ensure!(
                !self.queued.lock().contains_key(id),
                "This worktree has a queued chat; stop it first"
            );
            ensure!(
                workers
                    .get(id)
                    .is_none_or(|worker| !worker.active.load(Ordering::Acquire)),
                "This worktree has a running chat; stop it first"
            );
        }
        for id in &owners {
            if let Some(worker) = workers.get(id).cloned() {
                // Publish retirement before sending: failed sends and timeouts
                // retain both process ownership and the new-turn admission fence.
                worker.worktree_retiring.store(true, Ordering::Release);
                tokio::time::timeout(WORKTREE_SHUTDOWN_TIMEOUT, async {
                    if !*worker.exited.borrow() {
                        // Graceful shutdown lets background job owners terminate
                        // and reap their process trees before deletion.
                        worker
                            .commands
                            .send(Envelope::new(
                                Uuid::new_v4(),
                                Some(*id),
                                0,
                                WorkerCommand::Shutdown,
                            ))
                            .await
                            .context("The worktree worker could not stop its background jobs")?;
                        let mut exited = worker.exited.clone();
                        exited
                            .wait_for(|done| *done)
                            .await
                            .context("The worktree worker's exit could not be confirmed")?;
                    }
                    Ok::<(), anyhow::Error>(())
                })
                .await
                .context("Wait for the worktree's background jobs to stop before removing it")??;
                // Only a positive exit notification releases ownership.
                workers.remove(id);
            }
        }
        // Keep worker admission blocked until the checkout and durable chat roots
        // have changed together. A new turn cannot restart in the removed tree.
        worktree::remove(&path, force).await?;
        for chat in associated {
            self.store
                .set_conversation_workspace(chat.id, tree.repository.clone())?;
            self.events
                .send(HostEvent::ChatWorktree {
                    session: chat.id,
                    workspace: tree.repository.clone(),
                    branch: None,
                })
                .await?;
        }
        drop(workers);
        self.send_conversations().await?;
        self.list_worktrees(Some(tree.repository)).await
    }
    pub(super) async fn cleanup_archived_worktrees(&self) -> Result<()> {
        let chats = self.store.worktree_conversations()?;
        let mut paths = std::collections::BTreeSet::new();
        for chat in &chats {
            if chat.archived
                && !chats
                    .iter()
                    .any(|other| other.workspace == chat.workspace && !other.archived)
            {
                paths.insert(chat.workspace.clone());
            }
        }
        for path in paths {
            let Ok(tree) = worktree::inspect(&path) else {
                continue;
            };
            if worktree::status(&path)
                .await
                .is_ok_and(|status| status.removable())
                && worktree::remove(&path, false).await.is_ok()
            {
                for chat in chats.iter().filter(|chat| chat.workspace == path) {
                    self.store
                        .set_conversation_workspace(chat.id, tree.repository.clone())?;
                }
            }
        }
        self.list_worktrees(None).await
    }
}

#[cfg(test)]
mod tests;
