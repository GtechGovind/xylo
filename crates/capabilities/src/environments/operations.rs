//! Durable intent before dispatch, exact-result reuse, and observation-only
//! recovery. Unknown command effects require explicit human reconciliation.
use super::*;
use native::Inspection;
use std::sync::Arc;

fn cleanup() -> CancellationToken {
    CancellationToken::new()
}
async fn settle_capture(
    manager: &Manager,
    operation: &Operation,
    leases: Arc<capture::RunLease>,
) -> Result<()> {
    let (manager, operation) = (manager.clone(), operation.clone());
    xylo_resources::spawn_blocking(move || {
        let _lease = leases;
        capture::settled(&manager, &operation)
    })
    .await?
}
fn digest(action: EnvironmentAction, args: &impl serde::Serialize) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
        action, args,
    ))?)))
}
fn pending(state: EnvironmentOperationState) -> bool {
    matches!(
        state,
        EnvironmentOperationState::Prepared
            | EnvironmentOperationState::Running
            | EnvironmentOperationState::OutcomeUnknown
    )
}
fn new_operation(
    record: &EnvironmentRecord,
    id: Uuid,
    action: EnvironmentAction,
    digest: String,
    expected: u64,
) -> Result<Operation> {
    ensure!(
        !id.is_nil(),
        "Choose a valid environment operation identity"
    );
    let created_at = now_ms();
    Ok(Operation {
        created_at,
        digest,
        expected_revision: expected,
        profile: record.profile.clone(),
        previous_container_id: record.container_id.clone(),
        receipt: EnvironmentReceipt {
            operation_id: id,
            environment_id: record.profile.id,
            action,
            state: EnvironmentOperationState::Prepared,
            environment: Some(record.clone()),
            output: None,
            message: "Preparing the request.".into(),
            created_at_ms: Some(created_at),
            command_label: None,
        },
    })
}
async fn previous(
    manager: &Manager,
    id: Uuid,
    digest: &str,
    environment_id: Uuid,
    action: EnvironmentAction,
    cancel: CancellationToken,
    leases: Arc<capture::RunLease>,
) -> Result<Option<EnvironmentReceipt>> {
    if let Some(receipt) = manager.cancelled(id)? {
        ensure!(
            receipt.environment_id == environment_id && receipt.action == action,
            "This request identity belongs to another environment or action"
        );
        return Ok(Some(receipt));
    }
    if let Some(operation) = manager.operation(id)? {
        ensure!(
            operation.digest == digest,
            "An environment operation identity was reused for different arguments"
        );
        return Ok(Some(reconcile(manager, operation, cancel, leases).await?));
    }
    Ok(None)
}
async fn ensure_resolved(
    manager: &Manager,
    id: Uuid,
    cancel: &CancellationToken,
    leases: Arc<capture::RunLease>,
) -> Result<()> {
    for operation in manager.operations(id)? {
        if pending(operation.receipt.state) {
            let receipt = reconcile(manager, operation, cancel.clone(), leases.clone()).await?;
            ensure!(
                !pending(receipt.state),
                "An earlier environment operation has an uncertain outcome. Check or explicitly reconcile it before continuing."
            );
        }
    }
    Ok(())
}
async fn commit_record(
    manager: &Manager,
    operation: &mut Operation,
    mut record: EnvironmentRecord,
    removed: bool,
) -> Result<()> {
    record.revision = record
        .revision
        .checked_add(1)
        .context("Environment revision limit reached")?;
    record.updated_at = now_ms();
    manager
        .commit(
            record.clone(),
            removed,
            operation.receipt.operation_id,
            &cleanup(),
        )
        .await?;
    operation.receipt.environment = (!removed).then_some(record);
    operation.receipt.state = EnvironmentOperationState::Succeeded;
    operation.receipt.message = "Environment updated.".into();
    manager.write_operation(operation, &cleanup()).await
}
pub(super) async fn save(
    manager: &Manager,
    request: EnvironmentSaveRequest,
    cancel: CancellationToken,
) -> Result<EnvironmentReceipt> {
    let _operation_lease = manager
        .operation_lease(request.operation_id, &cancel)
        .await?;
    let hash = digest(EnvironmentAction::Save, &request)?;
    let _lease = manager.profile_lease(request.profile.id, &cancel).await?;
    let leases = capture::RunLease::new(manager.root.clone(), _operation_lease, _lease)?;
    if let Some(receipt) = previous(
        manager,
        request.operation_id,
        &hash,
        request.profile.id,
        EnvironmentAction::Save,
        cancel.clone(),
        leases.clone(),
    )
    .await?
    {
        return Ok(receipt);
    }
    ensure_resolved(manager, request.profile.id, &cancel, leases.clone()).await?;
    let profile = normalize(request.profile)?;
    let existing = manager.stored(profile.id, &cancel).await?;
    ensure!(
        existing.as_ref().map_or(0, |stored| stored.record.revision) == request.expected_revision,
        "This environment changed; refresh before saving"
    );
    if let Some(stored) = &existing {
        ensure!(
            !stored.record.connected
                && !(stored.record.container_id.is_some()
                    && matches!(
                        stored.record.profile.target,
                        EnvironmentTarget::Container {
                            container: ContainerTarget::Managed { .. },
                            ..
                        }
                    )),
            "Disconnect and remove any owned container before changing this profile"
        );
        // Only Unix has a persistent local master to check. Other platforms'
        // ssh_check probes the remote host; saving setup must never connect.
        #[cfg(unix)]
        if matches!(stored.record.profile.target, EnvironmentTarget::Ssh { .. }) {
            ensure!(
                !native::ssh_check(manager, &stored.record, cancel.clone()).await?,
                "An SSH session is still active; disconnect it before changing this profile"
            );
        }
    }
    let record = EnvironmentRecord {
        profile,
        revision: request.expected_revision,
        connected: false,
        container_id: None,
        updated_at: now_ms(),
    };
    let mut operation = new_operation(
        &record,
        request.operation_id,
        EnvironmentAction::Save,
        hash,
        request.expected_revision,
    )?;
    manager.write_operation(&operation, &cleanup()).await?;
    if cancel.is_cancelled() {
        operation.receipt.state = EnvironmentOperationState::Cancelled;
        operation.receipt.message = "Save cancelled before commit.".into();
        manager.write_operation(&operation, &cleanup()).await?;
        return Ok(operation.receipt);
    }
    commit_record(manager, &mut operation, record, false).await?;
    Ok(operation.receipt)
}
fn managed(record: &EnvironmentRecord) -> Result<(&str, &[String])> {
    let EnvironmentTarget::Container {
        container: ContainerTarget::Managed { image, command },
        ..
    } = &record.profile.target
    else {
        bail!("Lifecycle changes are permitted only for containers created by this application")
    };
    Ok((image, command))
}
async fn inspected_owned(
    manager: &Manager,
    record: &EnvironmentRecord,
    cancel: CancellationToken,
) -> Result<Inspection> {
    managed(record)?;
    let id = record
        .container_id
        .as_ref()
        .context("Create this managed container first")?;
    let inspection = native::inspect(manager, record, id, cancel.clone())
        .await?
        .context("The owned container is no longer present")?;
    ensure!(
        &inspection.id == id,
        "Container identity changed; no lifecycle action was dispatched"
    );
    native::owned(
        &inspection,
        manager.installation(&cancel).await?,
        record.profile.id,
    )?;
    Ok(inspection)
}
pub(super) async fn probe(
    manager: &Manager,
    record: &EnvironmentRecord,
    cancel: CancellationToken,
) -> Result<EnvironmentState> {
    match &record.profile.target {
        EnvironmentTarget::Ssh { .. } => {
            let live = native::ssh_check(manager, record, cancel).await?;
            Ok(if record.connected && live {
                EnvironmentState::Connected
            } else {
                EnvironmentState::Disconnected
            })
        }
        EnvironmentTarget::Container { container, .. } => {
            let target = match (&record.container_id, container) {
                (Some(id), _) => id.clone(),
                (None, ContainerTarget::Existing { target }) => target.clone(),
                (None, ContainerTarget::Managed { .. }) => {
                    native::container_name(manager.installation(&cancel).await?, record.profile.id)
                }
            };
            let Some(inspection) =
                native::inspect(manager, record, &target, cancel.clone()).await?
            else {
                return Ok(EnvironmentState::Missing);
            };
            if let Some(id) = &record.container_id {
                ensure!(id == &inspection.id, "Container identity changed");
            }
            if matches!(container, ContainerTarget::Managed { .. }) {
                native::owned(
                    &inspection,
                    manager.installation(&cancel).await?,
                    record.profile.id,
                )?;
            }
            Ok(if !inspection.running {
                EnvironmentState::Stopped
            } else if record.connected {
                EnvironmentState::Connected
            } else {
                EnvironmentState::Disconnected
            })
        }
    }
}
enum Plan {
    Local(EnvironmentRecord, bool),
    Ssh(EnvironmentRecord, bool),
    Container(EnvironmentRecord, Vec<String>),
}
async fn prepare(
    manager: &Manager,
    record: &EnvironmentRecord,
    action: EnvironmentAction,
    operation: Uuid,
    cancel: &CancellationToken,
) -> Result<Plan> {
    let mut next = record.clone();
    match action {
        EnvironmentAction::Connect => match &record.profile.target {
            EnvironmentTarget::Ssh { .. } => Ok(Plan::Ssh(next, true)),
            EnvironmentTarget::Container { container, .. } => {
                let target = record
                    .container_id
                    .as_deref()
                    .or({
                        if let ContainerTarget::Existing { target } = container {
                            Some(target.as_str())
                        } else {
                            None
                        }
                    })
                    .context("Create this managed container first")?;
                let inspection = native::inspect(manager, record, target, cancel.clone())
                    .await?
                    .context("This container does not exist")?;
                if let Some(id) = &record.container_id {
                    ensure!(*id == inspection.id, "Container identity changed");
                }
                if matches!(container, ContainerTarget::Managed { .. }) {
                    native::owned(
                        &inspection,
                        manager.installation(cancel).await?,
                        record.profile.id,
                    )?;
                }
                ensure!(inspection.running, "Start the container before connecting");
                next.container_id = Some(inspection.id);
                next.connected = true;
                Ok(Plan::Local(next, false))
            }
        },
        EnvironmentAction::Disconnect => {
            next.connected = false;
            #[cfg(unix)]
            if matches!(record.profile.target, EnvironmentTarget::Ssh { .. }) {
                return Ok(Plan::Ssh(next, false));
            }
            Ok(Plan::Local(next, false))
        }
        EnvironmentAction::Create => {
            let (image, command) = managed(record)?;
            ensure!(
                record.container_id.is_none() && !record.connected,
                "Remove the previous managed container before creating another"
            );
            let installation = manager.installation(cancel).await?;
            let name = native::container_name(installation, record.profile.id);
            ensure!(
                native::inspect(manager, record, &name, cancel.clone())
                    .await?
                    .is_none(),
                "A container already occupies the reserved name; check the previous operation"
            );
            let mut args = vec![
                "container".into(),
                "create".into(),
                "--pull=never".into(),
                "--name".into(),
                name,
                "--label".into(),
                format!("{}={installation}", native::OWNER),
                "--label".into(),
                format!("{}={}", native::PROFILE, record.profile.id),
                "--label".into(),
                format!("{}={operation}", native::CREATED),
                "--".into(),
                image.into(),
            ];
            args.extend_from_slice(command);
            Ok(Plan::Container(next, args))
        }
        EnvironmentAction::Start | EnvironmentAction::Stop | EnvironmentAction::RemoveContainer => {
            let inspection = inspected_owned(manager, record, cancel.clone()).await?;
            let verb = match action {
                EnvironmentAction::Start => "start",
                EnvironmentAction::Stop => "stop",
                EnvironmentAction::RemoveContainer => "rm",
                _ => unreachable!(),
            };
            ensure!(
                action != EnvironmentAction::RemoveContainer || !inspection.running,
                "Stop the owned container before removing it; removal never forces or deletes volumes"
            );
            if action == EnvironmentAction::Stop {
                next.connected = false;
            }
            if (action == EnvironmentAction::Start && inspection.running)
                || (action == EnvironmentAction::Stop && !inspection.running)
            {
                return Ok(Plan::Local(next, false));
            }
            let mut args = vec!["container".into(), verb.into()];
            if action == EnvironmentAction::Stop {
                args.extend(["--time".into(), "10".into()]);
            }
            args.extend(["--".into(), inspection.id]);
            Ok(Plan::Container(next, args))
        }
        EnvironmentAction::Remove => {
            ensure!(!record.connected, "Disconnect before removing this profile");
            if matches!(
                record.profile.target,
                EnvironmentTarget::Container {
                    container: ContainerTarget::Managed { .. },
                    ..
                }
            ) {
                ensure!(
                    record.container_id.is_none(),
                    "Remove the owned container before removing its profile"
                );
            }
            if matches!(record.profile.target, EnvironmentTarget::Ssh { .. }) {
                ensure!(
                    !native::ssh_check(manager, record, cancel.clone()).await?,
                    "Disconnect the active SSH master before removing its profile"
                );
            }
            Ok(Plan::Local(next, true))
        }
        EnvironmentAction::Save | EnvironmentAction::Execute => {
            bail!("This action uses a separate typed request")
        }
    }
}
fn output(value: crate::process::ProcessOutput) -> EnvironmentOutput {
    EnvironmentOutput {
        success: value.success,
        exit_code: value.exit_code,
        stdout: value.stdout,
        stderr: value.stderr,
        truncated: value.truncated,
        artifacts: Vec::new(),
    }
}
async fn verify_effect(
    manager: &Manager,
    operation: &Operation,
    mut record: EnvironmentRecord,
    cancel: CancellationToken,
) -> Result<EnvironmentRecord> {
    match operation.receipt.action {
        EnvironmentAction::Connect | EnvironmentAction::Disconnect => {
            let connected = operation.receipt.action == EnvironmentAction::Connect;
            if matches!(record.profile.target, EnvironmentTarget::Ssh { .. })
                && (connected || cfg!(unix))
            {
                ensure!(
                    native::ssh_check(manager, &record, cancel).await? == connected,
                    "SSH connection outcome is not confirmed"
                );
            }
            record.connected = connected;
        }
        EnvironmentAction::Create => {
            let installation = manager.installation(&cancel).await?;
            let name = native::container_name(installation, record.profile.id);
            let inspection = native::inspect(manager, &record, &name, cancel)
                .await?
                .context("Container creation outcome is not confirmed")?;
            native::owned(&inspection, installation, record.profile.id)?;
            ensure!(
                inspection.labels.get(native::CREATED)
                    == Some(&operation.receipt.operation_id.to_string()),
                "The reserved container belongs to another create operation"
            );
            record.container_id = Some(inspection.id);
            record.connected = false;
        }
        EnvironmentAction::Start | EnvironmentAction::Stop => {
            let inspection = inspected_owned(manager, &record, cancel).await?;
            ensure!(
                inspection.running == (operation.receipt.action == EnvironmentAction::Start),
                "Container lifecycle outcome is not confirmed"
            );
            if !inspection.running {
                record.connected = false;
            }
        }
        EnvironmentAction::RemoveContainer => {
            let id = record
                .container_id
                .as_ref()
                .context("Missing owned container identity")?;
            ensure!(
                native::inspect(manager, &record, id, cancel)
                    .await?
                    .is_none(),
                "Container removal outcome is not confirmed"
            );
            record.container_id = None;
            record.connected = false;
        }
        _ => bail!("This effect requires its original durable record"),
    }
    Ok(record)
}
pub(super) async fn action(
    manager: &Manager,
    action: EnvironmentAction,
    request: EnvironmentActionRequest,
    cancel: CancellationToken,
) -> Result<EnvironmentReceipt> {
    let _operation_lease = manager
        .operation_lease(request.operation_id, &cancel)
        .await?;
    let hash = digest(action, &request)?;
    let _lease = manager
        .profile_lease(request.environment_id, &cancel)
        .await?;
    let leases = capture::RunLease::new(manager.root.clone(), _operation_lease, _lease)?;
    if let Some(receipt) = previous(
        manager,
        request.operation_id,
        &hash,
        request.environment_id,
        action,
        cancel.clone(),
        leases.clone(),
    )
    .await?
    {
        return Ok(receipt);
    }
    ensure_resolved(manager, request.environment_id, &cancel, leases.clone()).await?;
    let record = manager.record(request.environment_id, &cancel).await?;
    ensure!(
        record.revision == request.expected_revision,
        "This environment changed; refresh before continuing"
    );
    let mut operation = new_operation(
        &record,
        request.operation_id,
        action,
        hash,
        request.expected_revision,
    )?;
    manager.write_operation(&operation, &cleanup()).await?;
    let plan = match prepare(manager, &record, action, request.operation_id, &cancel).await {
        Ok(plan) => plan,
        Err(error) => {
            operation.receipt.state = if cancel.is_cancelled() {
                EnvironmentOperationState::Cancelled
            } else {
                EnvironmentOperationState::Failed
            };
            operation.receipt.message = error.to_string();
            manager.write_operation(&operation, &cleanup()).await?;
            return Ok(operation.receipt);
        }
    };
    if let Plan::Local(record, removed) = plan {
        commit_record(manager, &mut operation, record, removed).await?;
        return Ok(operation.receipt);
    }
    if cancel.is_cancelled() {
        operation.receipt.state = EnvironmentOperationState::Cancelled;
        operation.receipt.message = "Operation cancelled before native dispatch.".into();
        manager.write_operation(&operation, &cleanup()).await?;
        return Ok(operation.receipt);
    }
    operation.receipt.state = EnvironmentOperationState::Running;
    manager.write_operation(&operation, &cleanup()).await?;
    let result = match plan {
        Plan::Ssh(ref record, true) => native::ssh_connect(manager, record, cancel.clone()).await,
        Plan::Ssh(ref record, false) => {
            native::ssh_disconnect(manager, record, cancel.clone()).await
        }
        Plan::Container(ref record, args) => {
            match native::run(manager, record, args, 30, cancel.clone()).await {
                Ok(value) => {
                    let success = value.success;
                    operation.receipt.output = Some(output(value));
                    if success {
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!(
                            "The container client did not confirm completion"
                        ))
                    }
                }
                Err(error) => Err(error),
            }
        }
        Plan::Local(..) => unreachable!(),
    };
    // Cancellation kills the local client, not a remote/server-side effect.
    // A separate bounded probe may prove the desired state without replaying it.
    match verify_effect(manager, &operation, record, cleanup()).await {
        Ok(record) => commit_record(manager, &mut operation, record, false).await?,
        Err(probe) => {
            operation.receipt.state = EnvironmentOperationState::OutcomeUnknown;
            operation.receipt.message = result.err().map_or_else(
                || probe.to_string(),
                |error| format!("{error}. The remote outcome remains unconfirmed."),
            );
            manager.write_operation(&operation, &cleanup()).await?;
        }
    }
    Ok(operation.receipt)
}
pub(super) async fn execute(
    manager: &Manager,
    request: EnvironmentExecuteRequest,
    default_timeout: u64,
    budget: usize,
    cancel: CancellationToken,
) -> Result<EnvironmentReceipt> {
    let _operation_lease = manager
        .operation_lease(request.operation_id, &cancel)
        .await?;
    let hash = digest(EnvironmentAction::Execute, &request)?;
    let _lease = manager
        .profile_lease(request.environment_id, &cancel)
        .await?;
    let lease = capture::RunLease::new(manager.root.clone(), _operation_lease, _lease)?;
    if let Some(receipt) = previous(
        manager,
        request.operation_id,
        &hash,
        request.environment_id,
        EnvironmentAction::Execute,
        cancel.clone(),
        lease.clone(),
    )
    .await?
    {
        return Ok(receipt);
    }
    ensure_resolved(manager, request.environment_id, &cancel, lease.clone()).await?;
    let record = manager.record(request.environment_id, &cancel).await?;
    // Only this pure validation region proves a fresh command was rejected.
    // Lease/receipt lookup, earlier uncertain effects and all persistence remain
    // outside the typed boundary and therefore conservative at the outer journal.
    let (timeout, operation, output_limit) = (|| -> Result<_> {
        ensure!(
            record.revision == request.expected_revision,
            "This environment changed after command approval; review the current profile before executing"
        );
        ensure!(
            record.connected,
            "Connect and verify this environment before executing"
        );
        native::validate_command(&request.command)?;
        let timeout = request
            .timeout_seconds
            .unwrap_or(default_timeout)
            .min(default_timeout);
        ensure!(
            (1..=3600).contains(&timeout),
            "Environment timeout must be between 1 and 3600 seconds"
        );
        let mut operation = new_operation(
            &record,
            request.operation_id,
            EnvironmentAction::Execute,
            hash,
            request.expected_revision,
        )?;
        operation.receipt.command_label = request.command.first().and_then(|program| {
            let label: String = program.rsplit(['/', '\\']).next().unwrap_or(program)
                .chars().filter(|character| !character.is_control()).take(160).collect();
            (!label.is_empty()).then_some(label)
        });
        let base = capture::response_overhead(manager, &operation)?;
        ensure!(
            budget > base + 2048,
            "The output budget cannot hold this environment's structured receipt; increase it before executing"
        );
        Ok((timeout, operation, ((budget - base - 2048) / 12).clamp(1, 8192)))
    })()
    .map_err(CommandNotDispatched)?;
    // A definitive disconnected observation also precedes dispatch. Probe I/O
    // errors, including its metadata/lease failures, retain their original type.
    if probe(manager, &record, cancel.clone()).await? != EnvironmentState::Connected {
        return Err(CommandNotDispatched(anyhow::anyhow!(
            "Connect and verify this environment before executing"
        ))
        .into());
    }
    let args = native::execution_args(manager, &record, &request.command, &cancel).await?;
    let spec = native::spec(manager, &record, args, timeout, output_limit)?;
    // Cancel this child operation on a dropped caller without cancelling the
    // parent task. Actual work retains its leases and quota until it finishes.
    let cancel = cancel.child_token();
    let _cancel_on_drop = capture::CancelOnDrop(cancel.clone());
    let (storage, intent, token, guard) = (
        manager.clone(),
        operation.clone(),
        cancel.clone(),
        lease.clone(),
    );
    let prepared = xylo_resources::spawn_blocking(move || {
        let _lease = guard;
        capture::prepare(&storage, &intent, output_limit, &token)
    })
    .await?;
    let session = prepared.map_err(|error| {
        if error.downcast_ref::<capture::QuotaUnavailable>().is_some() {
            anyhow::Error::from(CommandNotDispatched(error))
        } else {
            error
        }
    })?;
    let manager = manager.clone();
    let mut finishing = operation.receipt.clone();
    finishing.state = EnvironmentOperationState::OutcomeUnknown;
    finishing.message = "The command's retained output is still finishing. Check this saved result before another command; it will not be replayed automatically.".into();
    let wait_cancel = cancel.clone();
    let mut retained = tokio::spawn(async move {
        let _lease = lease.clone();
        let mut operation = operation;
        manager.write_operation(&operation, &cleanup()).await?;
        operation.receipt.state = EnvironmentOperationState::Running;
        manager.write_operation(&operation, &cleanup()).await?;
        match session.run(spec, output_limit, cancel, lease).await {
            Ok(completed) => operation = completed,
            Err(error) => {
                tracing::warn!(%error, "Native output finalization requires reconciliation");
                operation.receipt.state = EnvironmentOperationState::OutcomeUnknown;
                operation.receipt.message = "Command output could not be finalized. Check this result before another command; the command will not be replayed automatically.".into();
            }
        }
        manager.write_operation(&operation, &cleanup()).await?;
        settle_capture(&manager, &operation, _lease).await?;
        Ok::<_, anyhow::Error>(operation.receipt)
    });
    tokio::select! {
        result = &mut retained => result?,
        _ = wait_cancel.cancelled() => match tokio::time::timeout(Duration::from_secs(2), &mut retained).await {
            Ok(result) => result?,
            Err(_) => Ok(finishing),
        },
        _ = tokio::time::sleep(Duration::from_secs(timeout.saturating_add(8))) => Ok(finishing),
    }
}
async fn reconcile(
    manager: &Manager,
    mut operation: Operation,
    cancel: CancellationToken,
    leases: Arc<capture::RunLease>,
) -> Result<EnvironmentReceipt> {
    if operation.receipt.action == EnvironmentAction::Execute {
        let (storage, original, guard) = (manager.clone(), operation.clone(), leases.clone());
        let recovered = xylo_resources::spawn_blocking(move || {
            let _lease = guard;
            if pending(original.receipt.state) {
                capture::recover(&storage, &original)
            } else {
                capture::verify(&storage, &original)?;
                Ok(None)
            }
        })
        .await??;
        if let Some(recovered) = recovered {
            operation = recovered;
            manager.write_operation(&operation, &cleanup()).await?;
        }
    }
    if !pending(operation.receipt.state) {
        settle_capture(manager, &operation, leases).await?;
        return Ok(operation.receipt);
    }
    let stored = manager.stored(operation.profile.id, &cancel).await?;
    if let Some(stored) = &stored
        && stored.last_operation == operation.receipt.operation_id
        && operation.receipt.action != EnvironmentAction::Execute
    {
        operation.receipt.state = EnvironmentOperationState::Succeeded;
        operation.receipt.environment = (!stored.removed).then_some(stored.record.clone());
        operation.receipt.message = "Recovered the completed environment update.".into();
    } else if operation.receipt.action == EnvironmentAction::Execute {
        operation.receipt.state = EnvironmentOperationState::OutcomeUnknown;
        operation.receipt.message="This interrupted command may have executed. Inspect the actual target and explicitly reconcile its outcome before running another command.".into();
    } else if let Some(stored) = stored
        .filter(|stored| !stored.removed && stored.record.revision == operation.expected_revision)
    {
        match verify_effect(manager, &operation, stored.record, cancel).await {
            Ok(record) => {
                commit_record(manager, &mut operation, record, false).await?;
                return Ok(operation.receipt);
            }
            Err(_) => {
                operation.receipt.state = EnvironmentOperationState::OutcomeUnknown;
                operation.receipt.message="The previous operation's effect is not confirmed. Check the target before explicitly reconciling it.".into();
            }
        }
    } else {
        operation.receipt.state = EnvironmentOperationState::OutcomeUnknown;
        operation.receipt.message="The previous operation needs explicit reconciliation; its profile changed before the receipt was completed.".into();
    }
    manager.write_operation(&operation, &cleanup()).await?;
    Ok(operation.receipt)
}
pub(super) async fn operation_status(
    manager: &Manager,
    id: Uuid,
    context: Option<(Uuid, EnvironmentAction)>,
    cancel: CancellationToken,
) -> Result<EnvironmentReceipt> {
    let lease = manager.try_operation_lease(id)?;
    if let Some(receipt) = manager.cancelled(id)? {
        if let Some((environment_id, action)) = context {
            ensure!(
                receipt.environment_id == environment_id && receipt.action == action,
                "This request identity belongs to another environment or action"
            );
        }
        return Ok(receipt);
    }
    let Some(operation) = manager.operation(id)? else {
        let (environment_id,action)=context.context("No durable receipt is available yet. Retain this operation ID; absence does not authorize a new request.")?;
        ensure!(
            !environment_id.is_nil(),
            "Checking a request requires its original environment identity"
        );
        let mut receipt = EnvironmentReceipt {
            operation_id: id,
            environment_id,
            action,
            state: EnvironmentOperationState::Prepared,
            environment: None,
            output: None,
            message: "The request is still being admitted. Check it again before continuing."
                .into(),
            created_at_ms: None,
            command_label: None,
        };
        if lease.is_some() {
            receipt.state = EnvironmentOperationState::Cancelled;
            receipt.message =
                "This request was cancelled before dispatch. Your changes remain here.".into();
            return manager.cancel_undispatched(&receipt, &cleanup()).await;
        }
        return Ok(receipt);
    };
    if let Some((environment_id, action)) = context {
        ensure!(
            operation.receipt.environment_id == environment_id
                && operation.receipt.action == action,
            "This request identity belongs to another environment or action"
        );
    }
    if lease.is_none() {
        return Ok(operation.receipt);
    }
    let Some(profile_lease) = manager.try_profile_lease(operation.profile.id)? else {
        return Ok(operation.receipt);
    };
    let leases = capture::RunLease::new(manager.root.clone(), lease.unwrap(), profile_lease)?;
    reconcile(manager, operation, cancel, leases).await
}
pub(super) async fn resolve(
    manager: &Manager,
    id: Uuid,
    succeeded: bool,
    note: &str,
    cancel: CancellationToken,
) -> Result<EnvironmentReceipt> {
    let _operation_lease = manager.operation_lease(id, &cancel).await?;
    ensure!(
        !note.trim().is_empty() && note.len() <= 1024 && !note.contains('\0'),
        "Record how you inspected the actual target before reconciling this operation"
    );
    let mut operation = manager
        .operation(id)?
        .context("This operation has no durable receipt")?;
    let _lease = manager.profile_lease(operation.profile.id, &cancel).await?;
    let leases = capture::RunLease::new(manager.root.clone(), _operation_lease, _lease)?;
    let current = reconcile(manager, operation.clone(), cancel, leases.clone()).await?;
    if !pending(current.state) {
        return Ok(current);
    }
    operation.receipt = current;
    operation.receipt.state = if succeeded {
        EnvironmentOperationState::Succeeded
    } else {
        EnvironmentOperationState::Failed
    };
    operation.receipt.message =
        format!("Outcome explicitly reconciled by the user after inspecting the target: {note}");
    manager.write_operation(&operation, &cleanup()).await?;
    settle_capture(manager, &operation, leases).await?;
    Ok(operation.receipt)
}

/// Restore only an exact, already terminal command result. Neither a profile
/// mutation nor a liveness probe nor native execution belongs on this path.
pub(super) async fn recover_execution(
    manager: &Manager,
    request: &EnvironmentExecuteRequest,
    cancel: CancellationToken,
) -> Result<Option<EnvironmentReceipt>> {
    let expected = digest(EnvironmentAction::Execute, request)?;
    let Some(operation) = manager.operation(request.operation_id)? else {
        return Ok(None);
    };
    let verify = |operation: &Operation| -> Result<()> {
        ensure!(
            operation.digest == expected
                && operation.profile.id == request.environment_id
                && operation.receipt.environment_id == request.environment_id
                && operation.receipt.action == EnvironmentAction::Execute
                && operation.expected_revision == request.expected_revision,
            "Saved command result does not match the original environment, revision or arguments"
        );
        Ok(())
    };
    verify(&operation)?;
    ensure!(!cancel.is_cancelled(), "Command recovery cancelled");
    let Some(_operation_lease) = manager.try_operation_lease(request.operation_id)? else {
        return Ok(None);
    };
    let Some(_profile_lease) = manager.try_profile_lease(request.environment_id)? else {
        return Ok(None);
    };
    let operation = manager
        .operation(request.operation_id)?
        .context("The saved command receipt disappeared during recovery")?;
    verify(&operation)?;
    if matches!(
        operation.receipt.state,
        EnvironmentOperationState::Succeeded
            | EnvironmentOperationState::Failed
            | EnvironmentOperationState::Cancelled
    ) {
        let guard = capture::RunLease::new(manager.root.clone(), _operation_lease, _profile_lease)?;
        let (storage, original) = (manager.clone(), operation.clone());
        xylo_resources::spawn_blocking(move || {
            let _lease = guard;
            capture::verify(&storage, &original)
        })
        .await??;
        Ok(Some(operation.receipt))
    } else {
        Ok(None)
    }
}
