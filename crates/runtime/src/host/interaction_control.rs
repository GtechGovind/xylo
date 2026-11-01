//! Conversation-owned wakeup admission and explicit native loop controls.
use super::*;

impl HostState {
    pub(super) async fn cap_plan_answer(
        &self,
        session: Uuid,
        mut answer: xylo_core::InteractionAnswer,
    ) -> Result<xylo_core::InteractionAnswer> {
        if let xylo_core::InteractionAnswer::ApprovePlan { mode } = &mut answer {
            if self.scheduled.lock().await.contains_key(&session) {
                let current = self.settings.lock().await.permissions;
                *mode = self
                    .wakeup_permission(
                        session,
                        crate::scheduler::wakeup_mode(*mode, effective_mode(current, true)),
                    )
                    .await?;
            }
            ensure!(
                *mode != PermissionMode::Plan,
                "This task must remain in Plan mode. Keep planning or reject the plan."
            );
        }
        Ok(answer)
    }

    pub(super) async fn wakeup_permission(
        &self,
        session: Uuid,
        current: PermissionMode,
    ) -> Result<PermissionMode> {
        let id = self.scheduled.lock().await.get(&session).copied();
        let Some(id) = id else {
            return Ok(current);
        };
        let task = self
            .default_scheduler()?
            .list()
            .await?
            .into_iter()
            .find(|task| task.id == id)
            .context("Scheduled task was stopped")?;
        ensure!(task.enabled, "Scheduled task was stopped");
        Ok(task.wakeup.map_or(current, |wakeup| {
            crate::scheduler::wakeup_mode(wakeup.permissions, current)
        }))
    }

    pub(super) async fn start_loop(
        &self,
        session: Uuid,
        workspace: PathBuf,
        command: String,
    ) -> Result<()> {
        let (interval, prompt) = crate::scheduler::parse_loop(&command)?;
        let permissions = self.settings.lock().await.permissions;
        ensure!(
            permissions.allows_writes(),
            "Switch out of Plan or Read only before scheduling work"
        );
        let workspace = tokio::fs::canonicalize(workspace).await?;
        let store = self.store.clone();
        let root = workspace.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(chat) = store.conversation(session)? {
                ensure!(!chat.archived, "Unarchive the chat before starting a loop");
                ensure!(
                    std::fs::canonicalize(chat.workspace)? == root,
                    "Chat belongs to another project"
                );
            } else {
                store.upsert_conversation(&xylo_core::Conversation {
                    id: session,
                    title: "Loop".into(),
                    workspace: root,
                    updated_at: xylo_core::now_ms(),
                    title_source: Default::default(),
                    pinned: false,
                    archived: false,
                })?;
            }
            Ok(())
        })
        .await??;
        self.scheduler(workspace)?
            .create_wakeup(
                session,
                permissions,
                interval,
                prompt,
                "Repeating prompt requested with /loop".into(),
                Some(interval),
            )
            .await?;
        self.send_schedules().await?;
        self.send_conversations().await?;
        self.events
            .send(HostEvent::Status(format!(
                "Loop started; runs every {interval} seconds while Xylo is open"
            )))
            .await?;
        Ok(())
    }

    pub(super) async fn stop_wakeups(&self, session: Uuid) -> Result<()> {
        let scheduler = self.default_scheduler()?;
        for task in scheduler.list().await? {
            if task
                .wakeup
                .as_ref()
                .is_some_and(|wakeup| wakeup.session_id == session)
            {
                scheduler.pause(task.id).await?;
                if let Some(claim) = task.claim {
                    let queued = self.queued.lock();
                    if let Some((turn, cancel)) = queued.get(&session)
                        && *turn == claim.occurrence
                    {
                        cancel.cancel();
                    }
                }
            }
        }
        self.send_schedules().await?;
        self.events
            .send(HostEvent::Status("Conversation wakeups stopped".into()))
            .await?;
        Ok(())
    }

    pub(super) async fn dispatch_wakeup(
        self: &Arc<Self>,
        task: crate::scheduler::ScheduledTask,
    ) -> Result<()> {
        let wakeup = task
            .wakeup
            .as_ref()
            .context("Missing conversation wakeup")?;
        let session = wakeup.session_id;
        let occurrence = task.claim.context("Wakeup has no claim")?.occurrence;
        let scheduler = self.default_scheduler()?;
        let store = self.store.clone();
        let chat = tokio::task::spawn_blocking(move || store.conversation(session))
            .await??
            .context("Conversation no longer exists")?;
        ensure!(!chat.archived, "Conversation is archived");
        ensure!(
            tokio::fs::canonicalize(&chat.workspace).await?
                == tokio::fs::canonicalize(&task.workspace).await?,
            "Conversation project changed"
        );
        let workers = self.workers.lock().await;
        let mut scheduled = self.scheduled.lock().await;
        let busy = {
            let mut queued = self.queued.lock();
            if workers
                .get(&session)
                .is_some_and(|worker| worker.active.load(Ordering::Acquire))
                || queued.contains_key(&session)
                || scheduled.contains_key(&session)
            {
                true
            } else {
                queued.insert(session, (occurrence, CancellationToken::new()));
                scheduled.insert(session, task.id);
                false
            }
        };
        drop(scheduled);
        drop(workers);
        if busy {
            scheduler.release(task.id).await?;
            return Ok(());
        }
        let provider = self.settings.lock().await.provider.clone();
        let turn = TurnRequest {
            session_id: session,
            turn_id: occurrence,
            prompt: format!("Scheduled wakeup: {}\n\n{}", wakeup.reason, task.prompt),
            workspace: task.workspace,
            provider,
            permissions: wakeup.permissions,
            attachments: Vec::new(),
            approved_rules: Vec::new(),
        };
        let state = self.clone();
        tokio::spawn(async move {
            let _operation = state.operations.read().await;
            if let Err(error) = state
                .worker_command(Some(session), WorkerCommand::Run { turn })
                .await
            {
                let _ = scheduler.pause(task.id).await;
                state.scheduled.lock().await.remove(&session);
                {
                    let mut queued = state.queued.lock();
                    if queued
                        .get(&session)
                        .is_some_and(|(turn, _)| *turn == occurrence)
                    {
                        queued.remove(&session);
                    }
                }
                let _ = state
                    .events
                    .send(HostEvent::Status(format!(
                        "Conversation wakeup paused: {error:#}"
                    )))
                    .await;
            }
        });
        Ok(())
    }
}

/// Clear failed schedule ownership before a manual follow-up can enter the chat.
pub(super) async fn pause_schedule(
    scheduler: &crate::scheduler::Scheduler,
    scheduled: &Mutex<HashMap<Uuid, Uuid>>,
    session: Uuid,
) -> Result<()> {
    let mut scheduled = scheduled.lock().await;
    if let Some(id) = scheduled.get(&session).copied() {
        scheduler.pause(id).await?;
        scheduled.remove(&session);
    }
    Ok(())
}
