//! Streaming, resumable, integrity-checked downloads. Only catalog URLs reach
//! production; fixtures use an explicitly isolated loopback-only test policy.

use super::catalog::Download;
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use reqwest::{StatusCode, header};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

pub(super) type Progress = Arc<dyn Fn(u64) + Send + Sync>;

#[derive(Clone)]
pub(super) struct Downloader {
    client: reqwest::Client,
    cache: PathBuf,
    #[cfg(test)]
    allow_loopback_http: bool,
}

impl Downloader {
    pub fn new(cache: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&cache)?;
        let cache = std::fs::canonicalize(cache)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(3600))
            .user_agent(concat!("Xylo/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            client,
            cache,
            #[cfg(test)]
            allow_loopback_http: false,
        })
    }

    fn valid_url(&self, url: &url::Url) -> bool {
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return false;
        }
        #[cfg(test)]
        if self.allow_loopback_http && url.scheme() == "http" && url.host_str() == Some("127.0.0.1")
        {
            return true;
        }
        url.scheme() == "https"
    }

    pub async fn fetch(
        &self,
        spec: &Download,
        cancel: &CancellationToken,
        progress: Progress,
    ) -> Result<PathBuf> {
        ensure!(
            spec.sha256.len() == 64 && spec.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid pinned asset digest"
        );
        ensure!(
            spec.size_bytes > 0 && spec.size_bytes <= 16 * 1024 * 1024 * 1024,
            "Asset download exceeds budget"
        );
        let digest = spec.sha256.to_ascii_lowercase();
        let complete = self.cache.join(format!("{digest}.blob"));
        let partial = self.cache.join(format!("{digest}.part"));
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.cache.join(format!("{digest}.lock")))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match fs2::FileExt::try_lock_exclusive(&lock) {
                Ok(()) => break,
                Err(error)
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {}
                Err(error) => return Err(error.into()),
            }
            tokio::select! {
                _=cancel.cancelled()=>anyhow::bail!("Asset installation cancelled"),
                _=tokio::time::sleep_until(deadline)=>anyhow::bail!("This verified asset is already downloading in another helper. Wait and retry."),
                _=tokio::time::sleep(Duration::from_millis(50))=>(),
            }
        }
        for path in [&complete, &partial] {
            if let Ok(metadata) = tokio::fs::symlink_metadata(path).await {
                ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "Asset cache entry is not a regular owned file"
                );
            }
        }
        if complete.is_file() {
            if verify(&complete, spec.size_bytes, &digest, cancel)
                .await
                .is_ok()
            {
                progress(spec.size_bytes);
                return Ok(complete);
            }
            ensure!(!cancel.is_cancelled(), "Asset installation cancelled");
            tokio::fs::remove_file(&complete).await?;
        }
        let mut offset = tokio::fs::metadata(&partial)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if offset > spec.size_bytes {
            tokio::fs::remove_file(&partial).await?;
            offset = 0;
        }
        if offset == spec.size_bytes {
            if verify(&partial, spec.size_bytes, &digest, cancel)
                .await
                .is_ok()
            {
                tokio::fs::rename(&partial, &complete).await?;
                progress(offset);
                return Ok(complete);
            }
            ensure!(!cancel.is_cancelled(), "Asset installation cancelled");
            tokio::fs::remove_file(&partial).await?;
            offset = 0;
        }
        let available = fs2::available_space(&self.cache)?;
        ensure!(
            available >= spec.size_bytes - offset + 64 * 1024 * 1024,
            "Not enough free space for this asset download"
        );
        let mut url = url::Url::parse(&spec.url)?;
        let mut redirects = 0;
        let response = loop {
            ensure!(
                self.valid_url(&url),
                "Asset download and redirects require credential-free HTTPS"
            );
            let mut request = self
                .client
                .get(url.clone())
                .header(header::ACCEPT_ENCODING, "identity");
            if offset > 0 {
                request = request.header(header::RANGE, format!("bytes={offset}-"));
            }
            let response = tokio::select! {result=request.send()=>result.context("Download verified asset")?,_=cancel.cancelled()=>anyhow::bail!("Asset installation cancelled")};
            if response.status().is_redirection() {
                redirects += 1;
                ensure!(redirects <= 5, "Asset redirect limit exceeded");
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .context("Asset redirect has no location")?
                    .to_str()?;
                url = url.join(location)?;
                continue;
            }
            break response.error_for_status()?;
        };
        match response.status() {
            StatusCode::PARTIAL_CONTENT => {
                let range = response
                    .headers()
                    .get(header::CONTENT_RANGE)
                    .context("Resumed asset response has no Content-Range")?
                    .to_str()?;
                let expected_prefix = format!("bytes {offset}-");
                let (bounds, total) = range.split_once('/').context("Malformed Content-Range")?;
                ensure!(
                    bounds.starts_with(&expected_prefix)
                        && total.parse::<u64>()? == spec.size_bytes,
                    "Asset resume range does not match pinned size/offset"
                );
                let end = bounds
                    .strip_prefix(&expected_prefix)
                    .context("Malformed range")?
                    .parse::<u64>()?;
                ensure!(
                    end >= offset && end < spec.size_bytes,
                    "Asset resume range exceeds pinned size"
                );
            }
            StatusCode::OK => {
                offset = 0;
            }
            status => anyhow::bail!("Unexpected asset HTTP status: {status}"),
        }
        if let Some(length) = response.content_length() {
            ensure!(
                length <= spec.size_bytes - offset,
                "Asset HTTP body exceeds pinned byte count"
            );
        }
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(offset == 0)
            .open(&partial)
            .await?;
        if offset > 0 {
            use tokio::io::AsyncSeekExt;
            file.seek(std::io::SeekFrom::Start(offset)).await?;
        }
        let mut hash = Sha256::new();
        if offset > 0 {
            let mut previous = tokio::fs::File::open(&partial).await?;
            let mut left = offset;
            let mut buffer = vec![0; 1024 * 1024];
            while left > 0 {
                ensure!(!cancel.is_cancelled(), "Asset installation cancelled");
                let n = previous
                    .read(&mut buffer[..left.min(1024 * 1024) as usize])
                    .await?;
                ensure!(n > 0, "Partial asset changed during resume");
                hash.update(&buffer[..n]);
                left -= n as u64;
            }
        }
        progress(offset);
        let mut stream = response.bytes_stream();
        let result=async {
            loop {
                let chunk=tokio::select!{chunk=stream.next()=>chunk,_=cancel.cancelled()=>anyhow::bail!("Asset installation cancelled")};
                let Some(chunk)=chunk else{break;};let chunk=chunk?;
                ensure!(offset.saturating_add(chunk.len() as u64)<=spec.size_bytes,"Asset stream exceeds pinned size");
                file.write_all(&chunk).await?;hash.update(&chunk);offset+=chunk.len() as u64;progress(offset);
            }
            ensure!(offset==spec.size_bytes,"Asset download interrupted before the pinned byte count; retry to resume");
            let actual=hex::encode(hash.finalize());
            ensure!(actual==digest,"Asset SHA256 mismatch; unverified bytes will not be installed");
            Ok::<_,anyhow::Error>(())
        }.await;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        if let Err(error) = result {
            if offset == spec.size_bytes {
                let _ = tokio::fs::remove_file(&partial).await;
            }
            return Err(error);
        }
        tokio::fs::rename(&partial, &complete).await?;
        Ok(complete)
    }
}

pub(super) async fn verify(
    path: &Path,
    size: u64,
    expected: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut file = tokio::fs::File::open(path).await?;
    ensure!(
        file.metadata().await?.len() == size,
        "Installed asset size changed"
    );
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        ensure!(!cancel.is_cancelled(), "Asset verification cancelled");
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    ensure!(
        hex::encode(hash.finalize()) == expected.to_ascii_lowercase(),
        "Installed asset SHA256 mismatch"
    );
    Ok(())
}

#[cfg(test)]
pub(super) mod tests;
