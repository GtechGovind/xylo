//! Native review actions for the selected project, outside model tool execution.
use super::*;
use crate::workspace::git::hunks;
impl HostClient {
    /// Applies one explicitly selected hunk from an unchanged diff preview.
    pub fn apply_git_hunk(
        &self,
        request_id: Uuid,
        root: PathBuf,
        request: hunks::Request,
    ) -> Result<()> {
        self.enqueue(HostCommand::ApplyGitHunk(request_id, root, request))
    }
}
impl HostState {
    pub(super) async fn apply_git_hunk(
        &self,
        request_id: Uuid,
        root: PathBuf,
        request: hunks::Request,
    ) -> Result<()> {
        let result = async {
            self.authorized_git_root(&root).await?;
            let canonical = tokio::fs::canonicalize(&root).await?;
            // Worker admission uses this same map. Keep it locked through the
            // short mutation so a new turn cannot start between the check and apply.
            let workers = self.workers.lock().await;
            ensure!(
                !workers
                    .values()
                    .any(|worker| worker.owns_workspace(&canonical) && worker.has_live_work()),
                "Stop active work in this project before changing its Git index or files"
            );
            hunks::apply(&canonical, request).await
        }
        .await
        .map_err(|error: anyhow::Error| format!("{error:#}"));
        self.events
            .send(HostEvent::GitHunkApplied {
                request_id,
                root,
                result,
            })
            .await?;
        Ok(())
    }
}
