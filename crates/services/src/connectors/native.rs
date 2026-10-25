use super::{OAuthTokens, mcp::validate_endpoint};
use crate::CredentialVault;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use reqwest::{Client, Method, RequestBuilder};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use workbench_core::{ConnectorConfig, ToolDefinition};

/// A definitive local rejection: no HTTP execution future was polled.
///
/// The private constructor is used only during preparation. It deliberately
/// drops underlying error chains, which can contain credential-store details or
/// malformed authentication material. Errors after HTTP dispatch never use it.
#[derive(Debug)]
pub struct ConnectorNotDispatched {
    message: String,
}
impl ConnectorNotDispatched {
    fn preparation(error: anyhow::Error) -> Self {
        Self {
            message: error.to_string().chars().take(2048).collect(),
        }
    }
}
impl std::fmt::Display for ConnectorNotDispatched {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for ConnectorNotDispatched {}

struct PreparedRequest {
    request: reqwest::Request,
    token: String,
    page: u64,
    _permit: std::sync::Arc<tokio::sync::OwnedSemaphorePermit>,
}

#[cfg(test)]
mod credential_tests;
#[cfg(test)]
mod dispatch_tests;
mod pagination;
#[cfg(test)]
mod tests;
mod validation;

/// Native REST connectors reuse one pooled HTTP client. Every mutation is labelled
/// `writes`; authorization and action journaling remain in the application registry.
#[derive(Clone)]
pub struct NativeConnectors {
    http: Client,
    vault: CredentialVault,
    capacity: std::sync::Arc<tokio::sync::Semaphore>,
}
impl NativeConnectors {
    pub fn new(vault: CredentialVault) -> Result<Self> {
        Ok(Self {
            http: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(60))
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .user_agent(concat!("Xylo/", env!("CARGO_PKG_VERSION")))
                .build()?,
            vault,
            capacity: std::sync::Arc::new(tokio::sync::Semaphore::new(8)),
        })
    }
    pub fn tools(transport: &str) -> Result<Vec<ToolDefinition>> {
        let s = |description: &str| json!({"type":"string","description":description});
        let tool = |name: &str,
                    description: &str,
                    properties: Value,
                    required: Vec<&str>,
                    writes: bool| ToolDefinition {
            name: name.into(),
            description: description.into(),
            parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
            writes,
        };
        Ok(match transport {
            "github" => vec![
                tool(
                    "github_search_issues",
                    "Search issues and pull requests using GitHub search syntax",
                    json!({"query":s("GitHub issue search expression"),"page":{"type":"integer","minimum":1,"maximum":1000},"limit":{"type":"integer","minimum":1,"maximum":100}}),
                    vec!["query"],
                    false,
                ),
                tool(
                    "github_get_issue",
                    "Read a repository issue and its metadata",
                    json!({"owner":s("Repository owner"),"repo":s("Repository name"),"number":{"type":"integer","minimum":1}}),
                    vec!["owner", "repo", "number"],
                    false,
                ),
                tool(
                    "github_create_issue",
                    "Create an issue in a repository",
                    json!({"owner":s("Repository owner"),"repo":s("Repository name"),"title":s("Issue title"),"body":s("Issue Markdown body")}),
                    vec!["owner", "repo", "title"],
                    true,
                ),
                tool(
                    "github_list_pull_requests",
                    "List pull requests in a repository",
                    json!({"owner":s("Repository owner"),"repo":s("Repository name"),"state":{"type":"string","enum":["open","closed","all"]},"page":{"type":"integer","minimum":1,"maximum":1000},"limit":{"type":"integer","minimum":1,"maximum":100}}),
                    vec!["owner", "repo"],
                    false,
                ),
            ],
            "google_drive" => vec![
                tool(
                    "drive_list_files",
                    "Find Drive files with the Drive v3 query language",
                    json!({"query":s("Drive query, for example trashed=false"),"page_token":s("Next page token"),"limit":{"type":"integer","minimum":1,"maximum":100}}),
                    vec![],
                    false,
                ),
                tool(
                    "drive_get_file",
                    "Read a file's Drive metadata",
                    json!({"file_id":s("Drive file ID")}),
                    vec!["file_id"],
                    false,
                ),
                tool(
                    "drive_export_text",
                    "Export a Google Workspace document as plain text",
                    json!({"file_id":s("Google Workspace file ID")}),
                    vec!["file_id"],
                    false,
                ),
                tool(
                    "drive_create_folder",
                    "Create a named folder in Drive",
                    json!({"name":s("Folder name"),"parent_id":s("Optional parent folder ID")}),
                    vec!["name"],
                    true,
                ),
            ],
            "gmail" => vec![
                tool(
                    "gmail_list_messages",
                    "Search Gmail messages; returns message IDs and a page token",
                    json!({"query":s("Gmail search expression"),"page_token":s("Next page token"),"limit":{"type":"integer","minimum":1,"maximum":100}}),
                    vec![],
                    false,
                ),
                tool(
                    "gmail_get_message",
                    "Read a Gmail message, including MIME parts",
                    json!({"message_id":s("Gmail message ID")}),
                    vec!["message_id"],
                    false,
                ),
                tool(
                    "gmail_create_draft",
                    "Create an unsent Gmail draft from a complete RFC 5322 email",
                    json!({"raw_email":s("Complete RFC 5322 email including headers, blank line and body")}),
                    vec!["raw_email"],
                    true,
                ),
                tool(
                    "gmail_send_draft",
                    "Send an existing Gmail draft",
                    json!({"draft_id":s("Existing Gmail draft ID")}),
                    vec!["draft_id"],
                    true,
                ),
            ],
            "google_calendar" => vec![
                tool(
                    "calendar_list_events",
                    "List calendar events, optionally bounded by RFC 3339 timestamps",
                    json!({"calendar_id":s("Calendar ID; defaults to primary"),"time_min":s("Inclusive lower time bound"),"time_max":s("Exclusive upper time bound"),"page_token":s("Next page token"),"limit":{"type":"integer","minimum":1,"maximum":100}}),
                    vec![],
                    false,
                ),
                tool(
                    "calendar_get_event",
                    "Read a calendar event",
                    json!({"calendar_id":s("Calendar ID; defaults to primary"),"event_id":s("Event ID")}),
                    vec!["event_id"],
                    false,
                ),
                tool(
                    "calendar_create_event",
                    "Create a Calendar v3 event from its provider JSON representation",
                    json!({"calendar_id":s("Calendar ID; defaults to primary"),"event":{"type":"object","description":"Google Calendar event with summary, start and end"}}),
                    vec!["event"],
                    true,
                ),
                tool(
                    "calendar_delete_event",
                    "Delete an event from a calendar",
                    json!({"calendar_id":s("Calendar ID; defaults to primary"),"event_id":s("Event ID")}),
                    vec!["event_id"],
                    true,
                ),
            ],
            "slack" => vec![
                tool(
                    "slack_search_messages",
                    "Search Slack messages visible to the connected account",
                    json!({"query":s("Slack search expression"),"page":{"type":"integer","minimum":1,"maximum":100},"cursor":s("Use * for the first cursor page, then the returned next_cursor; omit page when using a cursor"),"limit":{"type":"integer","minimum":1,"maximum":100}}),
                    vec!["query"],
                    false,
                ),
                tool(
                    "slack_channel_history",
                    "Read recent channel messages",
                    json!({"channel":s("Channel ID"),"cursor":s("Next cursor"),"limit":{"type":"integer","minimum":1,"maximum":100}}),
                    vec!["channel"],
                    false,
                ),
                tool(
                    "slack_send_message",
                    "Post a message to a Slack channel",
                    json!({"channel":s("Channel ID"),"text":s("Message text"),"thread_ts":s("Optional parent message timestamp")}),
                    vec!["channel", "text"],
                    true,
                ),
                tool(
                    "slack_update_message",
                    "Update a message authored by the connected app",
                    json!({"channel":s("Channel ID"),"timestamp":s("Message timestamp"),"text":s("Replacement text")}),
                    vec!["channel", "timestamp", "text"],
                    true,
                ),
            ],
            other => bail!("Unsupported native connector {other}"),
        })
    }
    /// Pure preflight for an application-owned native definition. The broker
    /// calls this before journaling/approval; direct callers use the same check.
    /// It performs no credential, network or process work.
    pub fn validate_arguments(tool: &ToolDefinition, args: &Value) -> Result<()> {
        ensure!(
            args.is_object() && serde_json::to_vec(args)?.len() <= 1024 * 1024,
            "Arguments must be a JSON object no larger than 1 MiB"
        );
        validation::arguments(args, &tool.parameters)?;
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20);
        let page = args.get("page").and_then(Value::as_u64).unwrap_or(1);
        if tool.name == "github_search_issues" {
            ensure!(
                page.checked_sub(1)
                    .and_then(|page| page.checked_mul(limit))
                    .is_some_and(|offset| offset < 1000),
                "GitHub search exposes at most 1,000 results; narrow the query"
            );
        }
        if tool.name == "slack_search_messages" {
            ensure!(
                args.get("page").is_none() || args.get("cursor").is_none(),
                "Choose either a Slack page or cursor"
            );
        }
        for key in [
            "owner",
            "repo",
            "file_id",
            "calendar_id",
            "message_id",
            "event_id",
        ] {
            if let Some(identifier) = args.get(key).and_then(Value::as_str) {
                ensure!(
                    !identifier.is_empty()
                        && !matches!(identifier, "." | "..")
                        && !identifier.chars().any(char::is_control),
                    "Invalid connector resource identifier {key}"
                );
            }
        }
        if tool.name == "calendar_create_event" {
            let event = args.get("event").context("Event requires start and end")?;
            ensure!(
                event.get("start").is_some_and(Value::is_object)
                    && event.get("end").is_some_and(Value::is_object),
                "Event requires start and end objects"
            );
        }
        Ok(())
    }
    /// Calls do not retry on 401, 429, 5xx, timeout, or cancellation. In particular,
    /// a failed network exchange after sending a write has an uncertain outcome.
    pub async fn call(
        &self,
        config: &ConnectorConfig,
        name: &str,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let prepared = self
            .prepare_request(config, name, args, cancel)
            .await
            .map_err(ConnectorNotDispatched::preparation)?;
        self.execute_prepared(config, name, prepared, cancel).await
    }

    /// Everything here is local preparation. Credential jobs retain their own
    /// capacity guard if cancellation stops the caller before the real read ends.
    async fn prepare_request(
        &self,
        config: &ConnectorConfig,
        name: &str,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<PreparedRequest> {
        ensure!(!cancel.is_cancelled(), "Connector request cancelled");
        let permit = tokio::select! {biased; _ = cancel.cancelled()=>bail!("Connector request cancelled"),permit=self.capacity.clone().acquire_owned()=>permit?};
        let _permit = std::sync::Arc::new(permit);
        let allowed = Self::tools(&config.transport)?;
        let tool = allowed
            .iter()
            .find(|tool| tool.name == name)
            .context("Tool is not exposed by this connector")?;
        Self::validate_arguments(tool, &args)?;
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20);
        let page = args.get("page").and_then(Value::as_u64).unwrap_or(1);
        let reference = config
            .credential_ref
            .clone()
            .context("Choose an OS credential for this connector")?;
        let vault = self.vault.clone();
        ensure!(!cancel.is_cancelled(), "Connector request cancelled");
        let credential_permit = _permit.clone();
        let credentials = tokio::task::spawn_blocking(move || {
            // Native credential prompts cannot be interrupted by dropping the
            // async request. Keep their admission until the real read returns.
            let _permit = credential_permit;
            vault.get(&reference)
        });
        let secret = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("Connector request cancelled during credential access"),
            result = credentials => result
                .map_err(|_| anyhow::anyhow!("Connector credential access could not complete"))?
                .map_err(|_| anyhow::anyhow!("Connector credential could not be read"))?,
        }
        .context("Connector credential is missing")?;
        let token = if secret.starts_with('{') {
            let t: OAuthTokens =
                serde_json::from_str(&secret).context("Invalid stored OAuth token")?;
            ensure!(
                !t.is_expired(),
                "OAuth token expired; refresh the connection before running a tool"
            );
            t.access_token
        } else {
            secret
        };
        ensure!(
            !token.is_empty()
                && token.len() <= 64 * 1024
                && token.bytes().all(|byte| (0x21..=0x7e).contains(&byte)),
            "Connector credential is invalid"
        );
        ensure!(!cancel.is_cancelled(), "Connector request cancelled");
        let default = match config.transport.as_str() {
            "github" => "https://api.github.com",
            "slack" => "https://slack.com/api",
            _ => "https://www.googleapis.com",
        };
        let base = validate_endpoint(if config.endpoint.trim().is_empty() {
            default
        } else {
            &config.endpoint
        })?;
        ensure!(
            base.query().is_none() && base.fragment().is_none(),
            "Connector base URLs cannot contain queries or fragments"
        );
        let str_arg = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .with_context(|| format!("Missing string argument {key}"))
        };
        let request = |method: Method, segments: &[&str]| -> Result<RequestBuilder> {
            let mut url = base.clone();
            {
                let mut path = url
                    .path_segments_mut()
                    .map_err(|_| anyhow::anyhow!("Invalid API base URL"))?;
                path.pop_if_empty();
                for segment in segments {
                    ensure!(
                        !segment.is_empty()
                            && !matches!(*segment, "." | "..")
                            && !segment.chars().any(char::is_control),
                        "Invalid connector resource identifier"
                    );
                    path.push(segment);
                }
            }
            let request = self
                .http
                .request(method, url)
                .bearer_auth(&token)
                .header("Accept", "application/json");
            Ok(if config.transport == "github" {
                request.header("X-GitHub-Api-Version", "2022-11-28")
            } else {
                request
            })
        };
        let req=match name{
            "github_search_issues"=>request(Method::GET,&["search","issues"])?.query(&[("q",str_arg("query")?.to_owned()),("per_page",limit.to_string()),("page",page.to_string())]),
            "github_get_issue"=>{let n=args.get("number").and_then(Value::as_u64).filter(|n|*n>0).context("Issue number must be positive")?.to_string();request(Method::GET,&["repos",str_arg("owner")?,str_arg("repo")?,"issues",&n])?},
            "github_create_issue"=>request(Method::POST,&["repos",str_arg("owner")?,str_arg("repo")?,"issues"])?.json(&json!({"title":str_arg("title")?,"body":args.get("body").and_then(Value::as_str).unwrap_or("")})),
            "github_list_pull_requests"=>request(Method::GET,&["repos",str_arg("owner")?,str_arg("repo")?,"pulls"])?.query(&[("state",args.get("state").and_then(Value::as_str).unwrap_or("open").to_owned()),("per_page",limit.to_string()),("page",page.to_string())]),
            "drive_list_files"=>request(Method::GET,&["drive","v3","files"])?.query(&[("q",args.get("query").and_then(Value::as_str).unwrap_or("trashed=false").to_owned()),("pageSize",limit.to_string()),("pageToken",args.get("page_token").and_then(Value::as_str).unwrap_or("").into()),("fields","nextPageToken,files(id,name,mimeType,modifiedTime,webViewLink)".into())]),
            "drive_get_file"=>request(Method::GET,&["drive","v3","files",str_arg("file_id")?])?.query(&[("fields","id,name,mimeType,modifiedTime,size,parents,webViewLink,description")]),
            "drive_export_text"=>request(Method::GET,&["drive","v3","files",str_arg("file_id")?,"export"])?.query(&[("mimeType","text/plain")]),
            "drive_create_folder"=>{let mut body=json!({"name":str_arg("name")?,"mimeType":"application/vnd.google-apps.folder"});if let Some(parent)=args.get("parent_id").and_then(Value::as_str){body["parents"]=json!([parent]);}request(Method::POST,&["drive","v3","files"])?.json(&body)},
            "gmail_list_messages"=>request(Method::GET,&["gmail","v1","users","me","messages"])?.query(&[("q",args.get("query").and_then(Value::as_str).unwrap_or("").to_owned()),("maxResults",limit.to_string()),("pageToken",args.get("page_token").and_then(Value::as_str).unwrap_or("").into())]),
            "gmail_get_message"=>request(Method::GET,&["gmail","v1","users","me","messages",str_arg("message_id")?])?.query(&[("format","full")]),
            "gmail_create_draft"=>request(Method::POST,&["gmail","v1","users","me","drafts"])?.json(&json!({"message":{"raw":URL_SAFE_NO_PAD.encode(str_arg("raw_email")?.as_bytes())}})),
            "gmail_send_draft"=>request(Method::POST,&["gmail","v1","users","me","drafts","send"])?.json(&json!({"id":str_arg("draft_id")?})),
            "calendar_list_events"=>{let mut req=request(Method::GET,&["calendar","v3","calendars",args.get("calendar_id").and_then(Value::as_str).unwrap_or("primary"),"events"])?.query(&[("maxResults",limit.to_string()),("singleEvents","true".into())]);for (key,param) in [("time_min","timeMin"),("time_max","timeMax"),("page_token","pageToken")]{if let Some(value)=args.get(key).and_then(Value::as_str){req=req.query(&[(param,value)]);}}req},
            "calendar_get_event"|"calendar_delete_event"=>request(if name=="calendar_get_event"{Method::GET}else{Method::DELETE},&["calendar","v3","calendars",args.get("calendar_id").and_then(Value::as_str).unwrap_or("primary"),"events",str_arg("event_id")?])?,
            "calendar_create_event"=>{let event=args.get("event").filter(|e|e.is_object()).context("event must be an object")?;ensure!(event.get("start").is_some() && event.get("end").is_some(),"Event requires start and end");request(Method::POST,&["calendar","v3","calendars",args.get("calendar_id").and_then(Value::as_str).unwrap_or("primary"),"events"])?.json(event)},
            "slack_search_messages"=>{let req=request(Method::GET,&["search.messages"])?.query(&[("query",str_arg("query")?.to_owned()),("count",limit.to_string())]);if let Some(cursor)=args.get("cursor").and_then(Value::as_str){ensure!(!cursor.is_empty(),"Slack search cursor cannot be empty");req.query(&[("cursor",cursor)])}else{req.query(&[("page",page.to_string())])}},
            "slack_channel_history"=>request(Method::GET,&["conversations.history"])?.query(&[("channel",str_arg("channel")?.to_owned()),("limit",limit.to_string()),("cursor",args.get("cursor").and_then(Value::as_str).unwrap_or("").into())]),
            "slack_send_message"=>{let mut body=json!({"channel":str_arg("channel")?,"text":str_arg("text")?});if let Some(parent)=args.get("thread_ts").and_then(Value::as_str){body["thread_ts"]=json!(parent)}request(Method::POST,&["chat.postMessage"])?.json(&body)},
            "slack_update_message"=>request(Method::POST,&["chat.update"])?.json(&json!({"channel":str_arg("channel")?,"ts":str_arg("timestamp")?,"text":str_arg("text")?})),
            _=>bail!("Unsupported native tool"),
        };
        let request = req.build().context("Build connector request")?;
        Ok(PreparedRequest {
            request,
            token,
            page,
            _permit,
        })
    }

    async fn execute_prepared(
        &self,
        config: &ConnectorConfig,
        name: &str,
        prepared: PreparedRequest,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        // This check precedes creation/polling of the HTTP execution future.
        // Cancellation racing after it remains conservative and never retries.
        if cancel.is_cancelled() {
            return Err(ConnectorNotDispatched::preparation(anyhow::anyhow!(
                "Connector request cancelled before dispatch"
            ))
            .into());
        }
        let PreparedRequest {
            request,
            token,
            page,
            _permit,
        } = prepared;
        let request_url = request.url().clone();
        let task = async {
            let response =
                self.http.execute(request).await.context(
                    "Connector HTTP request failed; a write may have an unknown outcome",
                )?;
            let status = response.status();
            let github_page = if status.is_success()
                && matches!(name, "github_search_issues" | "github_list_pull_requests")
            {
                Some(pagination::github(response.headers(), &request_url, page)?)
            } else {
                None
            };
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map(|seconds| format!("; retry after {seconds} seconds"))
                .unwrap_or_default();
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                ensure!(
                    bytes.len() + chunk.len() <= 8 * 1024 * 1024,
                    "Connector response exceeds 8 MiB; use a smaller page"
                );
                bytes.extend_from_slice(&chunk);
            }
            ensure!(
                status.is_success(),
                "Connector returned HTTP {}{}: {}",
                status,
                retry_after,
                error_excerpt(&String::from_utf8_lossy(&bytes), &token)
            );
            let mut value = if name == "drive_export_text" {
                json!({"text":String::from_utf8(bytes).context("Export is not UTF-8")?})
            } else if bytes.is_empty() {
                json!({"http_status":status.as_u16()})
            } else {
                serde_json::from_slice(&bytes).context("Connector returned invalid JSON")?
            };
            if let Some(pagination) = github_page {
                if name == "github_list_pull_requests" {
                    ensure!(
                        value.is_array(),
                        "GitHub pull requests response is not a list"
                    );
                    value = json!({"items":value,"pagination":pagination});
                } else {
                    ensure!(value.is_object(), "GitHub search response is not an object");
                    value["pagination"] = pagination;
                }
            }
            if config.transport == "slack" {
                ensure!(
                    value.get("ok").and_then(Value::as_bool) == Some(true),
                    "Slack API error: {}",
                    error_excerpt(
                        value
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("invalid response"),
                        &token
                    )
                );
            }
            Ok(value)
        };
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(anyhow::anyhow!("Connector request cancelled; remote outcome may be unknown")),
            result = task => result,
        };
        // Providers, HTTP clients and body decoders are not trusted to keep
        // authentication out of diagnostics. Preserve no raw source chain.
        result.map_err(|error| anyhow::anyhow!("{}", error_excerpt(&format!("{error:#}"), &token)))
    }
}

fn error_excerpt(text: &str, token: &str) -> String {
    // Redact before bounding, so cutting a long error cannot expose a prefix of
    // the credential at the boundary. Providers may put arbitrary text here.
    text.replace(token, "[redacted]")
        .chars()
        .take(2048)
        .collect()
}
