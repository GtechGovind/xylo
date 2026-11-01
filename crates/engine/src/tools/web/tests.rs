//! Web security and conversion tests use no external services.
use super::*;

#[test]
fn upgrades_http_and_rejects_credentials_and_non_web_schemes() {
    assert_eq!(
        normalize_url("http://example.com/a#b").unwrap().as_str(),
        "https://example.com/a"
    );
    for input in [
        "file:///etc/passwd",
        "ftp://example.com",
        "https://user:pass@example.com",
        "https://example.com/?api_key=secret",
    ] {
        assert!(normalize_url(input).is_err(), "{input}");
    }
}

#[test]
fn private_and_special_addresses_require_explicit_authority() {
    for ip in [
        "127.0.0.1",
        "10.0.0.1",
        "172.16.0.1",
        "192.168.0.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "192.0.2.1",
        "224.0.0.1",
        "::1",
        "::",
        "fc00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "2001:db8::1",
        "64:ff9b::7f00:1",
    ] {
        assert!(!public_address(ip.parse().unwrap()), "{ip}");
    }
    for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
        assert!(public_address(ip.parse().unwrap()), "{ip}");
    }
}

#[test]
fn markdown_preserves_content_without_scripts() {
    let page = markdown(
        "<h1>Docs</h1><p>Hello <a href=\"/guide\">guide</a>.</p><script>steal()</script>",
        "text/html",
    )
    .unwrap();
    assert!(page.contains("# Docs"));
    assert!(page.contains("[guide](/guide)"));
    assert!(!page.contains("steal"));
}

#[test]
fn domain_filters_use_host_boundaries() {
    let args = SearchArguments::parse(&json!({"query":"rust", "allowed_domains":["example.com"], "blocked_domains":["bad.example.com"]})).unwrap();
    assert!(args.permits("https://docs.example.com/a"));
    assert!(!args.permits("https://bad.example.com/a"));
    assert!(!args.permits("https://example.com.evil.test/a"));
    assert!(
        SearchArguments::parse(&json!({"query":"x", "allowed_domains":["https://example.com"]}))
            .is_err()
    );
}

async fn fixture_server(response: &'static str) -> (String, tokio::task::JoinHandle<usize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/page", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut count = 0;
        while let Ok(Ok((mut socket, _))) =
            tokio::time::timeout(Duration::from_millis(300), listener.accept()).await
        {
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await;
            socket.write_all(response.as_bytes()).await.unwrap();
            count += 1;
        }
        count
    });
    (url, task)
}

async fn run_approved(
    broker: Arc<ToolBroker>,
    events: &mut tokio::sync::mpsc::Receiver<EngineEvent>,
    call: &str,
    tool: &str,
    args: Value,
    allow: bool,
    remember: bool,
) -> (Result<String>, Vec<Value>) {
    let execution = broker.clone();
    let call = call.to_owned();
    let tool = tool.to_owned();
    let mut task = tokio::task::spawn_blocking(move || execution.execute(&call, &tool, args));
    let mut approvals = Vec::new();
    let result=tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            tokio::select! {
                result=&mut task => break result.unwrap(),
                event=events.recv() => if let Some(EngineEvent::ApprovalRequired{action_id,arguments,..})=event {
                    approvals.push(arguments);
                    broker.session.approvals().decide_with(action_id,allow,remember).unwrap();
                },
            }
        }
    }).await.expect("web test must finish promptly");
    (result, approvals)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_access_still_asks_for_private_network_and_denial_never_connects() {
    let (_directory, broker, mut events) = crate::tools::tests::fixture(PermissionMode::FullAccess);
    let (url, server) = fixture_server(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 12\r\n\r\n<h1>Doc</h1>",
    )
    .await;
    broker.session.register_web_fixture(&url).unwrap();
    let (result, approvals) = run_approved(
        broker,
        &mut events,
        "denied",
        "web_fetch",
        json!({"url":url}),
        false,
        false,
    )
    .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Private network access was denied")
    );
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0]["private_network"], true);
    assert_eq!(server.await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cached_page_keeps_private_approval_and_untrusted_memory_provenance() {
    let (_directory, broker, mut events) =
        crate::tools::tests::fixture(PermissionMode::AskForApproval);
    let (url,server)=fixture_server("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 12\r\nConnection: close\r\n\r\n<h1>Doc</h1>").await;
    broker.session.register_web_fixture(&url).unwrap();
    let (first, approvals) = run_approved(
        broker.clone(),
        &mut events,
        "first",
        "web_fetch",
        json!({"url":url}),
        true,
        true,
    )
    .await;
    assert!(first.unwrap().contains("# Doc"));
    assert_eq!(
        approvals.len(),
        2,
        "domain and private network need separate decisions"
    );
    assert!(broker.rules.lock().contains("web-domain:127.0.0.1"));
    let (second, approvals) = run_approved(
        broker.clone(),
        &mut events,
        "second",
        "web_fetch",
        json!({"url":url}),
        true,
        false,
    )
    .await;
    assert!(second.unwrap().contains("\"cached\":true"));
    assert_eq!(
        approvals.len(),
        1,
        "a domain grant never grants private network access"
    );
    assert_eq!(approvals[0]["private_network"], true);
    assert_eq!(broker.exposure().unwrap(), MemoryOrigin::WebContent);
    assert_eq!(server.await.unwrap(), 1, "the second read uses the cache");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn body_caps_and_unregistered_fixture_paths_are_not_bypassed() {
    let (_directory, broker, mut events) = crate::tools::tests::fixture(PermissionMode::FullAccess);
    let (url,server)=fixture_server("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 20\r\n\r\n01234567890123456789").await;
    broker.session.register_web_fixture(&url).unwrap();
    assert_eq!(
        request_url(&broker, &url.replace("/page", "/other"))
            .unwrap()
            .scheme(),
        "https"
    );
    let (result, _) = run_approved(
        broker,
        &mut events,
        "cap",
        "web_fetch",
        json!({"url":url,"max_bytes":10}),
        true,
        false,
    )
    .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("exceeds max_bytes")
    );
    assert_eq!(server.await.unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_web_tools_are_discoverable() {
    let (_directory, broker, _events) = crate::tools::tests::fixture(PermissionMode::ReadOnly);
    for name in ["web_fetch", "web_search"] {
        let result = broker.search_tools(None, &json!({"query":name}));
        assert!(result.to_string().contains(name), "{result}");
    }
}

struct ExtractionProvider(Mutex<Vec<ModelRequest>>);
impl ModelProvider for ExtractionProvider {
    fn stream(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
    ) -> crate::model::EventStream {
        self.0.lock().push(request);
        Box::pin(futures_util::stream::iter([
            Ok(crate::model::StreamEvent::Text("Extracted answer".into())),
            Ok(crate::model::StreamEvent::Finished(
                crate::model::FinishReason::Stop,
            )),
        ]))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn page_prompt_uses_the_turn_provider_with_lowest_effort_and_source_url() {
    let (_directory, broker, mut events) = crate::tools::tests::fixture(PermissionMode::FullAccess);
    let provider = Arc::new(ExtractionProvider(Mutex::new(Vec::new())));
    broker.set_web_provider(provider.clone());
    let (url,server)=fixture_server("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 12\r\nConnection: close\r\n\r\n<h1>Doc</h1>").await;
    broker.session.register_web_fixture(&url).unwrap();
    let (result, _) = run_approved(
        broker,
        &mut events,
        "prompt",
        "web_fetch",
        json!({"url":url,"prompt":"What is this page?"}),
        true,
        false,
    )
    .await;
    let result = result.unwrap();
    assert!(result.contains("Extracted answer") && result.contains(&url));
    {
        let requests = provider.0.lock();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].reasoning, Reasoning::Lowest);
        assert!(requests[0].tools.is_empty());
        assert!(requests[0].messages[1].content.contains("<untrusted_page>"));
        assert!(requests[0].messages[1].content.contains(&url));
    }
    assert_eq!(server.await.unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn searxng_fallback_uses_saved_configuration_and_filters_results() {
    let (_directory, broker, mut events) = crate::tools::tests::fixture(PermissionMode::FullAccess);
    broker.set_web_provider(Arc::new(ExtractionProvider(Mutex::new(Vec::new()))));
    let payload = r#"{"results":[{"title":"Docs","url":"https://example.com/a","content":"Snippet"},{"title":"Other","url":"https://evil.test/a","content":"Excluded"}]}"#;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/search", listener.local_addr().unwrap());
    broker.session.register_web_fixture(&url).unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 8192];
        let count = socket.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..count]).into_owned();
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",payload.len()).as_bytes()).await.unwrap();
        request
    });
    let mut settings = broker.store.settings().unwrap();
    settings.web_search = WebSearchSettings {
        backend: WebSearchBackend::Searxng,
        endpoint: url,
        credential_ref: Some("search-key".into()),
    };
    broker
        .session
        .vault()
        .set("search-key", "fixture-search-secret")
        .unwrap();
    broker.store.save_settings(&settings).unwrap();
    let (result, approvals) = run_approved(
        broker,
        &mut events,
        "fallback",
        "web_search",
        json!({"query":"docs","allowed_domains":["example.com"]}),
        true,
        false,
    )
    .await;
    let result = result.unwrap();
    assert!(result.contains("Snippet") && !result.contains("Excluded"));
    assert!(!result.contains("fixture-search-secret"));
    assert_eq!(approvals.len(), 1);
    let request = server.await.unwrap();
    assert!(request.contains("format=json") && request.contains("q=docs"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-search-secret")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redirects_stop_after_ten_hops_and_recheck_private_authority() {
    let (_directory, broker, mut events) = crate::tools::tests::fixture(PermissionMode::FullAccess);
    let (url, server) = fixture_server(
        "HTTP/1.1 302 Found\r\nLocation: /page\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    broker.session.register_web_fixture(&url).unwrap();
    let (result, approvals) = run_approved(
        broker,
        &mut events,
        "redirects",
        "web_fetch",
        json!({"url":url}),
        true,
        false,
    )
    .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("exceeded 10 redirects")
    );
    assert_eq!(
        approvals.len(),
        11,
        "the initial request and each redirect need private authority"
    );
    assert_eq!(server.await.unwrap(), 11);
}
