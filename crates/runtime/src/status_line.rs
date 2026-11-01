//! Bounded execution of an explicitly configured status command.
use anyhow::{Context, Result, ensure};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// One runner serializes status commands and throttles their start times.
#[derive(Default)]
pub struct Runner {
    last: tokio::sync::Mutex<Option<Instant>>,
}

impl Runner {
    /// Runs a saved command with session JSON on stdin, returning its first line.
    pub async fn run(
        &self,
        workspace: &Path,
        command: &str,
        session: &serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<Option<String>> {
        ensure!(command.len() <= 8192, "Status command exceeds 8 KiB");
        let Ok(mut last) = self.last.try_lock() else {
            return Ok(None);
        };
        if last.is_some_and(|last| last.elapsed() < Duration::from_millis(300)) {
            return Ok(None);
        }
        let input = serde_json::to_vec(session)?;
        ensure!(input.len() <= 32 * 1024, "Status input exceeds 32 KiB");
        let root = workspace.to_owned();
        let script = command.to_owned();
        let result = async {
            let (temporary, prepared) = tokio::task::spawn_blocking(move || -> Result<_> {
                let root = root.canonicalize()?;
                let temporary = tempfile::tempdir()?;
                // The actual project is a read root; only the private temporary
                // workspace is writable. Positional arguments preserve path bytes.
                let args = vec![
                    "-c".into(),
                    "cd -- \"$1\" || exit 1; exec /bin/sh -c \"$2\"".into(),
                    "xylo-status".into(),
                    root.to_str().context("Project path is not UTF-8")?.into(),
                    script,
                ];
                let prepared = xylo_services::sandbox::prepare(
                    "/bin/sh",
                    &args,
                    temporary.path(),
                    &[root],
                    &[],
                )?;
                Ok((temporary, prepared))
            })
            .await??;
            let output = crate::process::run_prepared_sandboxed_input(
                prepared,
                temporary.path(),
                Duration::from_secs(2),
                cancel,
                &input,
            )
            .await?
            .result;
            ensure!(
                output.code == Some(0),
                "Status command did not finish successfully"
            );
            let text = output
                .stdout
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .filter(|character| !character.is_control())
                .take(1024)
                .collect::<String>();
            Ok(Some(text))
        }
        .await;
        *last = Some(Instant::now());
        result
    }
}

#[cfg(all(test, unix))]
mod tests;
