//! OAuth discovery and registration against a local authorization server fixture.
use super::discovery::discovered_endpoint;
use super::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_discovery_registers_public_client_and_binds_resource_on_exchange() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    let origin = base.clone();
    let (record, mut records) = tokio::sync::mpsc::channel(8);
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let base = origin.clone();
            let record = record.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let read = socket.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..end]);
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                let path = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let body = request.split_once("\r\n\r\n").unwrap().1;
                let (status, extra, value) = match path {
                    "/mcp" => (
                        "401 Unauthorized",
                        format!(
                            "WWW-Authenticate: Bearer resource_metadata=\"{base}/resource\"\r\n"
                        ),
                        json!({}),
                    ),
                    "/resource" => (
                        "200 OK",
                        String::new(),
                        json!({"resource":format!("{base}/mcp"),"authorization_servers":[format!("{base}/auth")],"scopes_supported":["read"]}),
                    ),
                    "/.well-known/oauth-authorization-server/auth" => (
                        "200 OK",
                        String::new(),
                        json!({"issuer":format!("{base}/auth"),"authorization_endpoint":format!("{base}/authorize"),"token_endpoint":format!("{base}/token"),"registration_endpoint":format!("{base}/register"),"code_challenge_methods_supported":["S256"]}),
                    ),
                    "/register" => {
                        let value: Value = serde_json::from_str(body).unwrap();
                        record.send(("register", value)).await.unwrap();
                        (
                            "201 Created",
                            String::new(),
                            json!({"client_id":"registered-client","token_endpoint_auth_method":"none"}),
                        )
                    }
                    "/token" => {
                        let values = url::form_urlencoded::parse(body.as_bytes())
                            .into_owned()
                            .collect::<std::collections::BTreeMap<_, _>>();
                        record.send(("token", json!(values))).await.unwrap();
                        (
                            "200 OK",
                            String::new(),
                            json!({"access_token":"fixture-access","token_type":"Bearer","expires_in":3600}),
                        )
                    }
                    _ => panic!("Unexpected OAuth fixture path {path}"),
                };
                let body = value.to_string();
                socket.write_all(format!("HTTP/1.1 {status}\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            });
        }
    });
    let resource = format!("{base}/mcp");
    let redirect = "http://127.0.0.1:43210/callback";
    let config = discover_oauth(&resource, redirect, None, &Default::default())
        .await
        .unwrap();
    assert_eq!(config.client_id, "registered-client");
    let registration = records.recv().await.unwrap();
    assert_eq!(registration.1["redirect_uris"], json!([redirect]));
    assert_eq!(registration.1["token_endpoint_auth_method"], "none");
    let flow = PendingOAuth::begin(config, redirect).unwrap();
    let url = url::Url::parse(&flow.authorization_url).unwrap();
    let query = url
        .query_pairs()
        .into_owned()
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(query["resource"], resource);
    assert_eq!(query["code_challenge_method"], "S256");
    let tokens = flow
        .complete("fixture-code", &query["state"], &Default::default())
        .await
        .unwrap();
    assert_eq!(tokens.access_token, "fixture-access");
    let exchange = records.recv().await.unwrap();
    assert_eq!(exchange.1["resource"], resource);
    assert_eq!(exchange.1["client_id"], "registered-client");
    assert!(exchange.1["code_verifier"].as_str().unwrap().len() >= 43);
    server.abort();
}

#[tokio::test]
async fn cancelled_oauth_discovery_does_not_contact_a_server() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        discover_oauth(
            "http://127.0.0.1:1/mcp",
            "http://127.0.0.1:1234/callback",
            None,
            &cancel
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("cancelled")
    );
}

#[test]
fn oauth_discovery_does_not_downgrade_https_to_loopback_http() {
    let resource = url::Url::parse("https://example.com/mcp").unwrap();
    assert!(discovered_endpoint("http://127.0.0.1/token", &resource).is_err());
    assert!(discovered_endpoint("https://issuer.example/token", &resource).is_ok());
}
