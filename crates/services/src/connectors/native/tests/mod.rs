use super::*;
use std::{collections::BTreeMap, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};
use uuid::Uuid;

#[derive(Debug)]
struct Request {
    method: String,
    url: url::Url,
    headers: String,
    body: Vec<u8>,
}
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// Retain the connection after the written prefix to test stream cancellation.
    hold: bool,
}
impl Response {
    fn json(value: Value) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&value).unwrap(),
            hold: false,
        }
    }
}
struct Server {
    base: String,
    requests: mpsc::UnboundedReceiver<Request>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(response: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (sender, requests) = mpsc::unbounded_channel();
        let handler = Arc::new(response);
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted.unwrap();
                        let sender = sender.clone();
                        let handler = handler.clone();
                        connections.spawn(async move {
                            let mut bytes = Vec::new();
                            let mut buffer = [0u8; 4096];
                            let end = loop {
                                let count = socket.read(&mut buffer).await.unwrap();
                                assert!(count > 0 && bytes.len() < 64 * 1024);
                                bytes.extend_from_slice(&buffer[..count]);
                                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") { break end + 4; }
                            };
                            let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
                            let length = headers.lines().find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                            }).unwrap_or(0);
                            while bytes.len() < end + length {
                                let count = socket.read(&mut buffer).await.unwrap();
                                assert!(count > 0);
                                bytes.extend_from_slice(&buffer[..count]);
                            }
                            let first: Vec<_> = headers.lines().next().unwrap().split_whitespace().collect();
                            let request = Request {
                                method: first[0].into(),
                                url: url::Url::parse(&format!("http://localhost{}", first[1])).unwrap(),
                                headers: headers.clone(), body: bytes[end..end + length].to_vec(),
                            };
                            let response = handler(&request);
                            sender.send(request).unwrap();
                            let extra = response.headers.iter().map(|(key, value)| format!("{key}: {value}\r\n")).collect::<String>();
                            let head = format!("HTTP/1.1 {} Fixture\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n", response.status, response.body.len() + usize::from(response.hold));
                            if socket.write_all(head.as_bytes()).await.is_err() { return; }
                            let _ = socket.write_all(&response.body).await;
                            if response.hold { std::future::pending::<()>().await; }
                        });
                    }
                    result = connections.join_next(), if !connections.is_empty() => {
                        if let Some(Err(error)) = result { panic!("Fixture connection failed: {error}"); }
                    }
                }
            }
        });
        Self {
            base,
            requests,
            task,
        }
    }
    async fn request(&mut self) -> Request {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
}
fn connector(base: &str, transport: &str) -> (NativeConnectors, ConnectorConfig) {
    let vault = CredentialVault::for_tests();
    vault.set("fixture", "native-fixture-secret").unwrap();
    (
        NativeConnectors::new(vault).unwrap(),
        ConnectorConfig {
            id: Uuid::new_v4(),
            name: "Fixture".into(),
            transport: transport.into(),
            endpoint: format!("{base}/api"),
            command: None,
            args: Vec::new(),
            credential_ref: Some("fixture".into()),
            trusted: false,
        },
    )
}
fn query(request: &Request) -> BTreeMap<String, String> {
    request
        .url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}
async fn call_case(
    transport: &str,
    name: &str,
    args: Value,
    method: &str,
    path: &str,
    expected_query: &[(&str, &str)],
    expected_body: Option<Value>,
) {
    let mut server = Server::new(|_| Response::json(json!({"ok":true,"id":"fixture"}))).await;
    let (client, config) = connector(&server.base, transport);
    client
        .call(&config, name, args, &CancellationToken::new())
        .await
        .unwrap();
    let request = server.request().await;
    assert_eq!(request.method, method);
    assert_eq!(request.url.path(), path);
    let parameters = query(&request);
    for (key, value) in expected_query {
        assert_eq!(
            parameters.get(*key).map(String::as_str),
            Some(*value),
            "query {key}"
        );
    }
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer native-fixture-secret")
    );
    if transport == "github" {
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains("x-github-api-version: 2022-11-28")
        );
    }
    match expected_body {
        Some(body) => assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            body
        ),
        None => assert!(request.body.is_empty()),
    }
    assert!(
        server.requests.try_recv().is_err(),
        "one tool call must send one request"
    );
}

#[tokio::test]
async fn github_routes_and_write_payloads_remain_explicit() {
    call_case(
        "github",
        "github_get_issue",
        json!({"owner":"owner","repo":"repo","number":7}),
        "GET",
        "/api/repos/owner/repo/issues/7",
        &[],
        None,
    )
    .await;
    call_case(
        "github",
        "github_create_issue",
        json!({"owner":"owner","repo":"repo","title":"Issue","body":"Details"}),
        "POST",
        "/api/repos/owner/repo/issues",
        &[],
        Some(json!({"title":"Issue","body":"Details"})),
    )
    .await;
}

#[tokio::test]
async fn drive_routes_preserve_opaque_tokens_and_file_identity() {
    call_case(
        "google_drive",
        "drive_list_files",
        json!({"query":"name = 'résumé'","page_token":"opaque+/=token","limit":3}),
        "GET",
        "/api/drive/v3/files",
        &[
            ("q", "name = 'résumé'"),
            ("pageToken", "opaque+/=token"),
            ("pageSize", "3"),
        ],
        None,
    )
    .await;
    call_case(
        "google_drive",
        "drive_get_file",
        json!({"file_id":"a/b?c"}),
        "GET",
        "/api/drive/v3/files/a%2Fb%3Fc",
        &[],
        None,
    )
    .await;
    call_case("google_drive", "drive_create_folder", json!({"name":"Reports","parent_id":"parent"}), "POST", "/api/drive/v3/files", &[], Some(json!({"name":"Reports","mimeType":"application/vnd.google-apps.folder","parents":["parent"]}))).await;
    let mut server = Server::new(|_| Response {
        status: 200,
        headers: Vec::new(),
        body: "Résumé\n".as_bytes().to_vec(),
        hold: false,
    })
    .await;
    let (client, config) = connector(&server.base, "google_drive");
    let result = client
        .call(
            &config,
            "drive_export_text",
            json!({"file_id":"doc"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["text"], "Résumé\n");
    let request = server.request().await;
    assert_eq!(request.url.path(), "/api/drive/v3/files/doc/export");
    assert_eq!(query(&request)["mimeType"], "text/plain");
}

#[tokio::test]
async fn gmail_routes_encode_email_and_never_send_during_draft_creation() {
    call_case(
        "gmail",
        "gmail_list_messages",
        json!({"query":"is:unread","page_token":"opaque+/=","limit":4}),
        "GET",
        "/api/gmail/v1/users/me/messages",
        &[
            ("q", "is:unread"),
            ("pageToken", "opaque+/="),
            ("maxResults", "4"),
        ],
        None,
    )
    .await;
    call_case(
        "gmail",
        "gmail_get_message",
        json!({"message_id":"message"}),
        "GET",
        "/api/gmail/v1/users/me/messages/message",
        &[("format", "full")],
        None,
    )
    .await;
    let mail = "To: example@example.test\r\nSubject: Test\r\n\r\nHello";
    call_case(
        "gmail",
        "gmail_create_draft",
        json!({"raw_email":mail}),
        "POST",
        "/api/gmail/v1/users/me/drafts",
        &[],
        Some(json!({"message":{"raw":URL_SAFE_NO_PAD.encode(mail.as_bytes())}})),
    )
    .await;
    call_case(
        "gmail",
        "gmail_send_draft",
        json!({"draft_id":"draft"}),
        "POST",
        "/api/gmail/v1/users/me/drafts/send",
        &[],
        Some(json!({"id":"draft"})),
    )
    .await;
}

#[tokio::test]
async fn calendar_routes_bind_time_bounds_pages_and_mutation_identity() {
    call_case("google_calendar", "calendar_list_events", json!({"calendar_id":"a@example.test","time_min":"2026-10-25T00:00:00Z","time_max":"2026-10-26T00:00:00Z","page_token":"opaque+/=","limit":5}), "GET", "/api/calendar/v3/calendars/a@example.test/events", &[("timeMin","2026-10-25T00:00:00Z"),("timeMax","2026-10-26T00:00:00Z"),("pageToken","opaque+/="),("maxResults","5"),("singleEvents","true")], None).await;
    call_case(
        "google_calendar",
        "calendar_get_event",
        json!({"event_id":"event"}),
        "GET",
        "/api/calendar/v3/calendars/primary/events/event",
        &[],
        None,
    )
    .await;
    let event =
        json!({"summary":"Review","start":{"date":"2026-10-25"},"end":{"date":"2026-10-26"}});
    call_case(
        "google_calendar",
        "calendar_create_event",
        json!({"event":event}),
        "POST",
        "/api/calendar/v3/calendars/primary/events",
        &[],
        Some(event),
    )
    .await;
    call_case(
        "google_calendar",
        "calendar_delete_event",
        json!({"event_id":"event"}),
        "DELETE",
        "/api/calendar/v3/calendars/primary/events/event",
        &[],
        None,
    )
    .await;
}

#[tokio::test]
async fn slack_routes_support_both_pagination_forms_and_explicit_message_writes() {
    call_case(
        "slack",
        "slack_search_messages",
        json!({"query":"release","page":2,"limit":3}),
        "GET",
        "/api/search.messages",
        &[("query", "release"), ("page", "2"), ("count", "3")],
        None,
    )
    .await;
    call_case(
        "slack",
        "slack_search_messages",
        json!({"query":"release","cursor":"*","limit":3}),
        "GET",
        "/api/search.messages",
        &[("cursor", "*"), ("count", "3")],
        None,
    )
    .await;
    call_case(
        "slack",
        "slack_channel_history",
        json!({"channel":"C123","cursor":"opaque+/=","limit":6}),
        "GET",
        "/api/conversations.history",
        &[("channel", "C123"), ("cursor", "opaque+/="), ("limit", "6")],
        None,
    )
    .await;
    call_case(
        "slack",
        "slack_send_message",
        json!({"channel":"C123","text":"Hello","thread_ts":"123.4"}),
        "POST",
        "/api/chat.postMessage",
        &[],
        Some(json!({"channel":"C123","text":"Hello","thread_ts":"123.4"})),
    )
    .await;
    call_case(
        "slack",
        "slack_update_message",
        json!({"channel":"C123","timestamp":"123.4","text":"Updated"}),
        "POST",
        "/api/chat.update",
        &[],
        Some(json!({"channel":"C123","ts":"123.4","text":"Updated"})),
    )
    .await;
}

#[tokio::test]
async fn github_numeric_continuations_are_exposed_without_automatic_requests() {
    let mut server = Server::new(|request| {
        let host = request.headers.lines().find_map(|line| line.strip_prefix("host: ")).unwrap();
        Response { headers: vec![("Link".into(), format!("<http://{host}/api/search/issues?q=repo%3Ao%2Fr&per_page=2&page=2>; rel=\"next\", <http://{host}/api/search/issues?q=repo%3Ao%2Fr&per_page=2&page=4>; rel=\"last\""))], ..Response::json(json!({"total_count":7,"incomplete_results":false,"items":[{"id":1}]})) }
    }).await;
    let (client, config) = connector(&server.base, "github");
    let result = client
        .call(
            &config,
            "github_search_issues",
            json!({"query":"repo:o/r","limit":2}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        result["pagination"],
        json!({"page":1,"next_page":2,"last_page":4})
    );
    assert_eq!(result["total_count"], 7);
    let request = server.request().await;
    assert_eq!(query(&request)["page"], "1");
    assert!(server.requests.try_recv().is_err());
    let mut server = Server::new(|_| Response::json(json!([{ "number":2 }]))).await;
    let (client, config) = connector(&server.base, "github");
    let result = client
        .call(
            &config,
            "github_list_pull_requests",
            json!({"owner":"o","repo":"r","page":2}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["items"][0]["number"], 2);
    assert_eq!(
        result["pagination"],
        json!({"page":2,"next_page":null,"last_page":null})
    );
    assert_eq!(query(&server.request().await)["page"], "2");
}

#[test]
fn github_links_cannot_change_scope_or_authorize_a_url() {
    let original =
        url::Url::parse("https://api.github.com/search/issues?q=repo%3Ao%2Fr&per_page=2&page=1")
            .unwrap();
    for link in [
        "https://other.example/search/issues?q=repo%3Ao%2Fr&per_page=2&page=2",
        "https://api.github.com/repos/o/r/issues?q=repo%3Ao%2Fr&per_page=2&page=2",
        "https://api.github.com/search/issues?q=other&per_page=2&page=2",
        "https://api.github.com/search/issues?q=repo%3Ao%2Fr&per_page=2&page=2&page=3",
        "https://api.github.com/search/issues?q=repo%3Ao%2Fr&per_page=2&page=0",
        "https://api.github.com/search/issues?q=repo%3Ao%2Fr&per_page=2&page=1",
        "https://user@api.github.com/search/issues?q=repo%3Ao%2Fr&per_page=2&page=2",
    ] {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("link", format!("<{link}>; rel=\"next\"").parse().unwrap());
        assert!(
            pagination::github(&headers, &original, 1).is_err(),
            "{link}"
        );
    }
}

#[tokio::test]
async fn invalid_argument_types_ranges_and_unknown_fields_fail_before_credentials() {
    let client = NativeConnectors::new(CredentialVault::for_tests()).unwrap();
    let config = ConnectorConfig {
        id: Uuid::new_v4(),
        name: "Fixture".into(),
        transport: "github".into(),
        endpoint: "http://127.0.0.1:1".into(),
        command: None,
        args: Vec::new(),
        credential_ref: None,
        trusted: false,
    };
    for args in [
        json!({"query":"q","limit":"3"}),
        json!({"query":"q","limit":0}),
        json!({"query":"q","page":-1}),
        json!({"query":"q","page":1.5}),
        json!({"query":"q","unknown":true}),
        json!({"query":"q","page":1000,"limit":2}),
        json!({}),
    ] {
        let error = client
            .call(
                &config,
                "github_search_issues",
                args,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<ConnectorNotDispatched>().is_some());
        let error = format!("{error:#}");
        assert!(
            !error.contains("credential") && !error.contains("HTTP"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn every_google_list_preserves_an_empty_page_with_a_continuation_token() {
    for (transport, name, key) in [
        ("google_drive", "drive_list_files", "files"),
        ("gmail", "gmail_list_messages", "messages"),
        ("google_calendar", "calendar_list_events", "items"),
    ] {
        let mut expected = json!({"nextPageToken":"next+/="});
        expected[key] = json!([]);
        let body = expected.clone();
        let mut server = Server::new(move |_| Response::json(body.clone())).await;
        let (client, config) = connector(&server.base, transport);
        let result = client
            .call(
                &config,
                name,
                json!({"page_token":"previous+/="}),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result, expected);
        assert_eq!(query(&server.request().await)["pageToken"], "previous+/=");
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn http_failures_surface_rate_limits_redact_tokens_and_never_retry_writes() {
    for status in [401, 403, 429, 500] {
        let mut server = Server::new(move |_| Response {
            status,
            headers: vec![("Retry-After".into(), "17".into())],
            ..Response::json(json!({"message":"failure native-fixture-secret"}))
        })
        .await;
        let (client, config) = connector(&server.base, "gmail");
        let error = client
            .call(
                &config,
                "gmail_send_draft",
                json!({"draft_id":"draft"}),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<ConnectorNotDispatched>().is_none());
        let error = format!("{error:#}");
        assert!(
            error.contains(&status.to_string()) && error.contains("retry after 17 seconds"),
            "{error}"
        );
        assert!(!error.contains("native-fixture-secret"));
        server.request().await;
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn malformed_slack_and_oversized_bodies_are_errors_and_204_is_valid() {
    for (transport, name, args, response, expected) in [
        (
            "github",
            "github_get_issue",
            json!({"owner":"o","repo":"r","number":1}),
            Response {
                body: b"{broken".to_vec(),
                ..Response::json(Value::Null)
            },
            "invalid JSON",
        ),
        (
            "slack",
            "slack_send_message",
            json!({"channel":"C1","text":"hello"}),
            Response::json(json!({"ok":false,"error":"missing_scope"})),
            "missing_scope",
        ),
        (
            "gmail",
            "gmail_get_message",
            json!({"message_id":"m"}),
            Response {
                body: vec![b'x'; 8 * 1024 * 1024 + 1],
                ..Response::json(Value::Null)
            },
            "exceeds 8 MiB",
        ),
    ] {
        let response = std::sync::Mutex::new(Some(response));
        let mut server =
            Server::new(move |_| response.lock().unwrap().take().expect("must not retry")).await;
        let (client, config) = connector(&server.base, transport);
        let error = client
            .call(&config, name, args, &CancellationToken::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error}");
        server.request().await;
    }
    let mut server = Server::new(|_| Response {
        status: 204,
        body: Vec::new(),
        ..Response::json(Value::Null)
    })
    .await;
    let (client, config) = connector(&server.base, "google_calendar");
    assert_eq!(
        client
            .call(
                &config,
                "calendar_delete_event",
                json!({"event_id":"event"}),
                &CancellationToken::new()
            )
            .await
            .unwrap(),
        json!({"http_status":204})
    );
    server.request().await;
}

#[tokio::test]
async fn cancellation_during_response_keeps_write_outcome_uncertain_and_sends_once() {
    let mut server = Server::new(|_| Response {
        hold: true,
        body: b"{\"id\":".to_vec(),
        ..Response::json(Value::Null)
    })
    .await;
    let (client, config) = connector(&server.base, "gmail");
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        client
            .call(
                &config,
                "gmail_send_draft",
                json!({"draft_id":"draft"}),
                &task_cancel,
            )
            .await
    });
    server.request().await;
    cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.downcast_ref::<ConnectorNotDispatched>().is_none());
    let error = format!("{error:#}");
    assert!(error.contains("cancelled") && error.contains("unknown"));
    assert!(server.requests.try_recv().is_err());
}

#[test]
fn error_redaction_precedes_unicode_safe_excerpt_limits() {
    let token = "sensitive-token";
    let text = format!("{}{}{}", "é".repeat(2042), token, "尾".repeat(100));
    let excerpt = error_excerpt(&text, token);
    assert_eq!(excerpt.chars().count(), 2048);
    assert!(!excerpt.contains("sensitive") && !excerpt.contains(token));
}

#[tokio::test]
async fn queued_cancellation_never_sends_an_additional_request() {
    let mut server = Server::new(|_| Response {
        hold: true,
        body: b"{".to_vec(),
        ..Response::json(Value::Null)
    })
    .await;
    let (client, config) = connector(&server.base, "gmail");
    let active_cancel = CancellationToken::new();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let (client, config, cancel) = (client.clone(), config.clone(), active_cancel.clone());
        tasks.spawn(async move {
            client
                .call(
                    &config,
                    "gmail_send_draft",
                    json!({"draft_id":"active"}),
                    &cancel,
                )
                .await
        });
    }
    for _ in 0..8 {
        server.request().await;
    }
    let queued_cancel = CancellationToken::new();
    let cancel_task = queued_cancel.clone();
    let pending = tokio::spawn(async move {
        client
            .call(
                &config,
                "gmail_send_draft",
                json!({"draft_id":"queued"}),
                &cancel_task,
            )
            .await
    });
    tokio::task::yield_now().await;
    queued_cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap();
    let error = result.unwrap_err();
    assert!(error.downcast_ref::<ConnectorNotDispatched>().is_some());
    assert!(error.to_string().contains("cancelled"));
    assert!(server.requests.try_recv().is_err());
    active_cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(result) = tasks.join_next().await {
            assert!(result.unwrap().is_err());
        }
    })
    .await
    .unwrap();
}

mod credential;

mod dispatch;
