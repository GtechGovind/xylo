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
        sha256: hex::encode(Sha256::digest(body)),
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
