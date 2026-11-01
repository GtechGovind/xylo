//! Bounded process shutdown while discarding unread events without callbacks.
use super::{Command, Control};
use anyhow::{Context, Result};
use std::time::Duration;
use tokio::{
    io::{BufReader, copy, sink},
    process::{Child, ChildStdout},
    time::timeout,
};

/// A responsive CLI gets time to cancel its active engine and close connections.
const GRACE_PERIOD: Duration = Duration::from_secs(5);
/// Reaping after a forced stop must also have a deadline.
const REAP_PERIOD: Duration = Duration::from_secs(5);

pub(super) async fn close(
    child: &mut Child,
    output: &mut BufReader<ChildStdout>,
    control: &Control,
) -> Result<i32> {
    let graceful = timeout(GRACE_PERIOD, async {
        let finish = async {
            // The process may already have exited. Its exit status still matters
            // when the final shutdown write encounters a closed pipe.
            let _ = control.send(Command::Shutdown).await;
            control.input.lock().await.take();
            child.wait().await
        };
        tokio::pin!(finish);
        let mut discarded = sink();
        tokio::select! {
            status = &mut finish => status,
            _ = copy(output, &mut discarded) => finish.await,
        }
    })
    .await;
    if let Ok(Ok(status)) = graceful {
        return Ok(status.code().unwrap_or(1));
    }

    // This also covers a blocked shutdown write or a failed wait. Dropping the
    // graceful future releases its borrows before signalling the owned child.
    if let Err(error) = child.start_kill() {
        if let Some(status) = child.try_wait().context("Read CLI exit status")? {
            return Ok(status.code().unwrap_or(1));
        }
        return Err(error).context("Stop CLI after shutdown did not finish");
    }
    let status = timeout(REAP_PERIOD, child.wait())
        .await
        .context("CLI did not exit after a forced stop")?
        .context("Reap CLI after a forced stop")?;
    // A concurrent control write may still own the lock while observing the
    // closed pipe. Never introduce another unbounded lock wait during close.
    if let Ok(mut input) = control.input.try_lock() {
        input.take();
    }
    Ok(status.code().unwrap_or(1))
}
