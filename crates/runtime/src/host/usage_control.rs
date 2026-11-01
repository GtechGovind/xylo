//! Usage and optional status-command work stays off the UI thread.
use super::*;

impl HostClient {
    pub fn load_usage(
        &self,
        id: Uuid,
        session: Uuid,
        messages: Vec<Uuid>,
        start: i64,
        end: i64,
        workspace: Option<PathBuf>,
    ) -> Result<()> {
        self.enqueue(HostCommand::Usage(
            id, session, messages, start, end, workspace,
        ))
    }
    pub fn output_styles(&self, id: Uuid, workspace: PathBuf) -> Result<()> {
        self.enqueue(HostCommand::OutputStyles(id, workspace))
    }
    pub fn status_line(&self, id: Uuid, workspace: PathBuf, input: Value) -> Result<()> {
        self.enqueue(HostCommand::StatusLine(id, workspace, input))
    }
}

impl HostState {
    pub(super) async fn load_usage(
        &self,
        id: Uuid,
        session: Uuid,
        messages: Vec<Uuid>,
        start: i64,
        end: i64,
        workspace: Option<PathBuf>,
    ) -> Result<()> {
        let store = self.store.clone();
        let result = async {
            let report = tokio::task::spawn_blocking(move || {
                store.usage_report(session, messages, start, end)
            })
            .await??;
            let branch = if let Some(workspace) = workspace {
                crate::workspace::git::status(&workspace)
                    .await
                    .ok()
                    .map(|status| status.branch)
            } else {
                None
            };
            Ok::<_, anyhow::Error>((report, branch))
        }
        .await
        .map_err(|error| format!("{error:#}"));
        self.events
            .send(HostEvent::Usage {
                request_id: id,
                session_id: session,
                result,
            })
            .await?;
        Ok(())
    }
    pub(super) async fn output_styles(&self, id: Uuid, workspace: PathBuf) -> Result<()> {
        let result = tokio::task::spawn_blocking(move || {
            let mut styles = crate::workspace::output_styles::current_catalog(&workspace)?;
            for style in &mut styles {
                style.prompt.clear();
            }
            Ok::<_, anyhow::Error>(styles)
        })
        .await?
        .map_err(|error| format!("{error:#}"));
        self.events
            .send(HostEvent::OutputStyles {
                request_id: id,
                result,
            })
            .await?;
        Ok(())
    }
    pub(super) async fn status_line(
        &self,
        id: Uuid,
        workspace: PathBuf,
        input: Value,
    ) -> Result<()> {
        let command = if self.foreground.load(Ordering::Acquire) {
            self.settings.lock().await.status_line_command.clone()
        } else {
            None
        };
        let result = match command.filter(|command| !command.trim().is_empty()) {
            Some(command) => self
                .status_line
                .run(&workspace, &command, &input, CancellationToken::new())
                .await
                .map_err(|_| "Status command failed".to_owned()),
            None => Ok(None),
        };
        self.events
            .send(HostEvent::StatusLine {
                request_id: id,
                result,
            })
            .await?;
        Ok(())
    }
}
