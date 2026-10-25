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
            let actual=format!("{:x}",hash.finalize());
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
        format!("{:x}", hash.finalize()) == expected.to_ascii_lowercase(),
        "Installed asset SHA256 mismatch"
    );
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::catalog::Format;
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{io::AsyncWriteExt, net::TcpListener};

    pub async fn fixture(
        body: Vec<u8>,
        mode: &'static str,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let count = Arc::new(AtomicUsize::new(0));
        let body = Arc::new(body);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let body = body.clone();
                let count = count.fetch_add(1, Ordering::SeqCst);
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut byte = [0; 1];
                    while request.len() < 16 * 1024 {
                        if stream.read_exact(&mut byte).await.is_err() {
                            return;
                        }
                        request.push(byte[0]);
                        if request.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    let request = String::from_utf8(request).unwrap();
                    recorded.lock().unwrap().push(request.clone());
                    let range = request.lines().find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("range: bytes=")
                            .map(|range| range.trim_end_matches('-').parse::<usize>().unwrap())
                    });
                    let offset = if mode == "ignore" {
                        0
                    } else {
                        range.unwrap_or(0)
                    };
                    let length = body.len() - offset;
                    let header = if range.is_some() && mode != "ignore" {
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {length}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                            if mode == "wrong-range" {
                                offset + 1
                            } else {
                                offset
                            },
                            body.len() - 1,
                            body.len()
                        )
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                        )
                    };
                    if stream.write_all(header.as_bytes()).await.is_err() {
                        return;
                    }
                    if mode == "truncate" && count == 0 {
                        let _ = stream.write_all(&body[offset..offset + length / 2]).await;
                        return;
                    }
                    if mode == "slow" && count == 0 {
                        let end = (offset + 4096).min(body.len());
                        if stream.write_all(&body[offset..end]).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let _ = stream.write_all(&body[end..]).await;
                    } else {
                        let _ = stream.write_all(&body[offset..]).await;
                    }
                });
            }
        });
        (format!("http://{address}/asset"), requests, task)
    }

    pub fn specification(url: String, body: &[u8]) -> Download {
        Download {
            url,
            sha256: format!("{:x}", Sha256::digest(body)),
            size_bytes: body.len() as u64,
            format: Format::Raw,
            destination: "model.bin".into(),
        }
    }
    pub fn local_downloader(path: PathBuf) -> Downloader {
        let mut downloader = Downloader::new(path).unwrap();
        downloader.allow_loopback_http = true;
        downloader
    }

    #[tokio::test]
    async fn interrupted_transfer_resumes_exact_range_then_reuses_verified_cache() {
        let root = tempfile::tempdir().unwrap();
        let body = vec![19u8; 32768];
        let (url, requests, server) = fixture(body.clone(), "truncate").await;
        let spec = specification(url, &body);
        let downloader = local_downloader(root.path().to_path_buf());
        let token = CancellationToken::new();
        assert!(
            downloader
                .fetch(&spec, &token, Arc::new(|_| {}))
                .await
                .is_err()
        );
        let partial = root.path().join(format!("{}.part", spec.sha256));
        assert_eq!(std::fs::metadata(&partial).unwrap().len(), 16384);
        let path = downloader
            .fetch(&spec, &token, Arc::new(|_| {}))
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), body);
        {
            let headers = requests.lock().unwrap();
            assert_eq!(headers.len(), 2);
            assert!(
                headers[1]
                    .to_ascii_lowercase()
                    .contains("range: bytes=16384-")
            );
        }
        assert_eq!(
            downloader
                .fetch(&spec, &token, Arc::new(|_| {}))
                .await
                .unwrap(),
            path
        );
        assert_eq!(requests.lock().unwrap().len(), 2);
        server.abort();
    }

    #[tokio::test]
    async fn cancellation_preserves_only_partial_bytes_for_explicit_resume() {
        let root = tempfile::tempdir().unwrap();
        let body = vec![21u8; 65536];
        let (url, requests, server) = fixture(body.clone(), "slow").await;
        let spec = specification(url, &body);
        let downloader = local_downloader(root.path().to_path_buf());
        let token = CancellationToken::new();
        let signal = token.clone();
        assert!(
            downloader
                .fetch(
                    &spec,
                    &token,
                    Arc::new(move |bytes| {
                        if bytes >= 4096 {
                            signal.cancel();
                        }
                    })
                )
                .await
                .is_err()
        );
        assert!(!root.path().join(format!("{}.blob", spec.sha256)).exists());
        assert_eq!(
            std::fs::metadata(root.path().join(format!("{}.part", spec.sha256)))
                .unwrap()
                .len(),
            4096
        );
        downloader
            .fetch(&spec, &CancellationToken::new(), Arc::new(|_| {}))
            .await
            .unwrap();
        assert!(
            requests.lock().unwrap()[1]
                .to_ascii_lowercase()
                .contains("range: bytes=4096-")
        );
        server.abort();
    }

    #[tokio::test]
    async fn hash_mismatch_never_promotes_unverified_bytes() {
        let root = tempfile::tempdir().unwrap();
        let body = vec![30u8; 8192];
        let (url, _, server) = fixture(body.clone(), "normal").await;
        let mut spec = specification(url, &body);
        spec.sha256 = "0".repeat(64);
        let downloader = local_downloader(root.path().to_path_buf());
        assert!(
            downloader
                .fetch(&spec, &CancellationToken::new(), Arc::new(|_| {}))
                .await
                .unwrap_err()
                .to_string()
                .contains("SHA256")
        );
        assert!(!root.path().join(format!("{}.blob", spec.sha256)).exists());
        assert!(!root.path().join(format!("{}.part", spec.sha256)).exists());
        server.abort();
    }

    #[tokio::test]
    async fn ignored_range_restarts_safely_and_wrong_range_is_rejected() {
        for mode in ["ignore", "wrong-range"] {
            let root = tempfile::tempdir().unwrap();
            let body = vec![42u8; 8192];
            let (url, _, server) = fixture(body.clone(), mode).await;
            let spec = specification(url, &body);
            let downloader = local_downloader(root.path().to_path_buf());
            std::fs::write(
                root.path().join(format!("{}.part", spec.sha256)),
                &body[..2048],
            )
            .unwrap();
            let result = downloader
                .fetch(&spec, &CancellationToken::new(), Arc::new(|_| {}))
                .await;
            if mode == "ignore" {
                assert_eq!(std::fs::read(result.unwrap()).unwrap(), body);
            } else {
                assert!(result.is_err());
                assert_eq!(
                    std::fs::metadata(root.path().join(format!("{}.part", spec.sha256)))
                        .unwrap()
                        .len(),
                    2048
                );
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn production_policy_refuses_http_even_for_loopback() {
        let root = tempfile::tempdir().unwrap();
        let spec = specification("http://127.0.0.1:1/asset".into(), b"fixture");
        assert!(
            Downloader::new(root.path().to_path_buf())
                .unwrap()
                .fetch(&spec, &CancellationToken::new(), Arc::new(|_| {}))
                .await
                .unwrap_err()
                .to_string()
                .contains("HTTPS")
        );
    }
}
