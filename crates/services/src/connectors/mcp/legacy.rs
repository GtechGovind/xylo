//! Legacy HTTP+SSE transport, bounded and restricted to a same-origin POST endpoint.
use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub(super) async fn connect(
    endpoint: &str,
    secret: Option<&str>,
) -> Result<tokio::io::DuplexStream> {
    let url = validate_endpoint(endpoint)?;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
        .build()?;
    let mut request = http.get(url.clone()).header(ACCEPT, EVENT_STREAM_MIME_TYPE);
    if let Some(secret) = secret {
        request = request.bearer_auth(secret);
    }
    let response = request.send().await?;
    ensure!(
        response.status().is_success(),
        "Legacy MCP stream returned HTTP {}",
        response.status()
    );
    let mut stream = bounded_sse(response, HTTP_BODY_LIMIT);
    let post = loop {
        let event = stream
            .next()
            .await
            .context("Legacy MCP stream closed before its endpoint")??;
        if event.event.as_deref() == Some("endpoint") {
            let target = url.join(
                event
                    .data
                    .as_deref()
                    .context("Legacy MCP endpoint is absent")?,
            )?;
            validate_endpoint(target.as_str())?;
            ensure!(
                target.origin() == url.origin(),
                "Legacy MCP POST endpoint changed origin"
            );
            break target;
        }
    };
    let (client, server) = tokio::io::duplex(65536);
    let (reader, mut writer) = tokio::io::split(server);
    let secret = secret.map(str::to_owned);
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        loop {
            let room = HTTP_BODY_LIMIT + 1 - line.len();
            let mut bounded = (&mut reader).take(room as u64);
            tokio::select! {
                read = bounded.read_until(b'\n', &mut line) => {
                    if !matches!(read, Ok(size) if size > 0 && line.len() <= HTTP_BODY_LIMIT) { break }
                    let mut request = http.post(post.clone()).header(CONTENT_TYPE, JSON_MIME_TYPE).body(line.clone()).timeout(CALL_TIMEOUT);
                    if let Some(secret) = &secret { request = request.bearer_auth(secret); }
                    if !request.send().await.is_ok_and(|response| response.status().is_success()) { break }
                    line.clear();
                }
                event = stream.next() => {
                    let Some(Ok(event)) = event else { break };
                    if event.event.as_deref().is_none_or(|name| name == "message") && let Some(data) = event.data {
                        if serde_json::from_str::<ServerJsonRpcMessage>(&data).is_err() { break }
                        if writer.write_all(data.as_bytes()).await.is_err() || writer.write_all(b"\n").await.is_err() { break }
                    }
                }
            }
        }
    });
    Ok(client)
}
