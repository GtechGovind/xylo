//! Bounded HTTPS transport. Synchronous entrypoints run a small private async
//! reactor so cancellation interrupts both connection and stalled body reads.
use super::*;
use reqwest::Client;
fn client(policy: &UpdatePolicy) -> Result<Client> {
    let policy = policy.clone();
    Ok(Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .read_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 || policy.allow_url(attempt.url()).is_err() {
                attempt.error("Update redirect is not trusted")
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}
fn reactor() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}
pub(super) fn get_bytes(
    policy: &UpdatePolicy,
    url: &Url,
    limit: u64,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    policy.allow_url(url)?;
    reactor()?.block_on(async {
        let mut response = tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("Update cancelled"),
            result = client(policy)?.get(url.clone()).send() => result?.error_for_status()?,
        };
        ensure!(
            response.content_length().is_none_or(|size| size <= limit),
            "Update metadata exceeds limit"
        );
        let mut bytes = Vec::new();
        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => anyhow::bail!("Update cancelled"),
                result = response.chunk() => result?,
            };
            let Some(chunk) = chunk else {
                break;
            };
            ensure!(
                bytes.len() as u64 + chunk.len() as u64 <= limit,
                "Update metadata exceeds limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    })
}
pub(super) fn download(
    policy: &UpdatePolicy,
    artifact: &ReleaseArtifact,
    file: &mut NamedTempFile,
    cancel: &CancellationToken,
    progress: &impl Fn(UpdateProgress),
) -> Result<()> {
    policy.allow_url(&artifact.url)?;
    reactor()?.block_on(async {
        let mut response = tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("Update cancelled"),
            result = client(policy)?.get(artifact.url.clone()).send() => result?.error_for_status()?,
        };
        ensure!(response.content_length().is_none_or(|size| size == artifact.size_bytes), "Update size does not match");
        let mut total = 0;
        let mut last = std::time::Instant::now() - Duration::from_secs(1);
        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => anyhow::bail!("Update cancelled"),
                result = response.chunk() => result?,
            };
            let Some(chunk) = chunk else { break; };
            total += chunk.len() as u64;
            ensure!(total <= artifact.size_bytes && total <= policy.max_artifact_bytes, "Update exceeds download limit");
            file.write_all(&chunk)?;
            if last.elapsed() >= Duration::from_millis(100) {
                progress(UpdateProgress { phase: "downloading", downloaded_bytes: total, total_bytes: artifact.size_bytes });
                last = std::time::Instant::now();
            }
        }
        ensure!(total == artifact.size_bytes, "Update size does not match");
        progress(UpdateProgress { phase: "verifying", downloaded_bytes: total, total_bytes: artifact.size_bytes });
        Ok(())
    })
}
