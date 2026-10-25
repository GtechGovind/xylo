# Native service connectors

The built-in GitHub, Google Drive, Gmail, Calendar and Slack tools use one pooled
native HTTP client with at most eight concurrent requests. Construction opens no
credentials and contacts no service. The application broker still owns tool
discovery, approval, journaling and recovery. Connector writes do not retry after
an HTTP error, timeout, cancellation or uncertain exchange. The client explicitly
disables its protocol retry policy as well as redirects.

Each call requests one bounded page. The model or user explicitly requests the
next page; the connector never walks an entire account in the background. Page
sizes are 1–100, with a default of 20. The response body limit is 8 MiB. Use a
smaller page when that limit is exceeded. Tool arguments are checked against the
published schema before credential access, including required fields, types,
enums, numeric bounds and unknown fields. Provider JSON objects, such as a Calendar
event, stay open only where the tool schema permits them.

| Service | Continuation input | Returned continuation |
| --- | --- | --- |
| GitHub issue search | `page`, `limit` | Original search object plus `pagination: {page,next_page,last_page}` |
| GitHub pull requests | `page`, `limit` | `{items: [...], pagination: {page,next_page,last_page}}` |
| Drive files | `page_token`, `limit` | Original `nextPageToken` |
| Gmail messages | `page_token`, `limit` | Original `nextPageToken` |
| Calendar events | `page_token`, `limit` | Original `nextPageToken` |
| Slack message search | Either `page` or `cursor`, plus `limit` | Original Slack paging or cursor metadata |
| Slack channel history | `cursor`, `limit` | Original `response_metadata.next_cursor` |

GitHub pages are bounded to 1,000 by the application. Issue search also respects
the provider's 1,000-result search ceiling; narrow the search to retrieve a more
specific set. Continuations come from `Link` headers. Only numeric next/last page
values are exposed, after checking the original origin, route and all non-page
query parameters. Link URLs never become request authority. Pull request lists
now return the wrapper shown above rather than a bare array, so consumers can
inspect continuation without guessing from the number of returned items.

Google page tokens remain opaque and are encoded through the HTTP client's query
builder. An empty page with a next token is still incomplete. Slack message search
accepts the legacy numeric page (1–100) or an explicit cursor; use `*` to begin
cursor paging and then pass the returned cursor. Combining page and cursor is
rejected. Provider response metadata is preserved.

GitHub requests pin API version `2022-11-28`. HTTP redirects are disabled. Base URLs
may use HTTPS or loopback HTTP, contain no credentials, query or fragment, and may
include an explicit path prefix for enterprise gateways. Resource identifiers are
encoded as individual path segments; empty, control-character and dot-segment
identifiers are refused. Numeric `Retry-After` information is reported to the
caller without triggering a retry. HTTP error excerpts are bounded, and exact
credential tokens are redacted. This does not substitute for provider-side error
sanitization or real-account permission qualification.

Cancellation also returns while an OS credential read is blocked. The native
credential prompt itself may still require the user's response; dropping an async
request cannot dismiss that platform operation. Its real blocking job retains its
concurrency slot until it finishes, keeping repeated cancellation from creating
unbounded background credential reads. No HTTP request starts after cancellation
while credentials are being retrieved.

## Definitive local rejection and uncertain writes

`NativeConnectors::call` prepares a complete request before polling HTTP execution.
Only this preparation phase can return the typed `ConnectorNotDispatched` error.
It covers missing, inaccessible, malformed or expired credentials; argument,
endpoint and request construction errors; cancellation before admission or while
waiting for capacity/credentials; and a final cancellation check after the request
is built. Credentials must fit the bounded visible-ASCII bearer representation.
Credential-store error chains and malformed authentication material are not
forwarded into tool diagnostics.

The broker still requires the actual write approval. A typed local rejection
records the admitted action as failed, so it does not leave an unresolvable remote
write behind. Pure malformed arguments are rejected before intent creation or
approval. An exact cached call identity never causes a new execution, approval or
credential read, whether its previous state is failed or unknown.

Once HTTP execution is attempted, transport failures, response cancellation,
non-success status, malformed/oversized bodies and decoding errors remain
conservative. A write's journal outcome stays unknown even when its response says
that it failed; this phase does not use the local-rejection marker. Races after
the final pre-send cancellation check also remain conservative. No write is
automatically retried. Bounded diagnostics redact the exact access token and
discard raw underlying error chains. Authentication is attached to the request
header, rather than added to tool arguments or error observations.

## Verification and limits

`cargo test -p workbench-services connectors::native` runs local loopback fixtures
against the production adapter and a mock credential store. The cases cover every
built-in tool route and mutation body, GitHub continuation scope, opaque Google
tokens and empty pages, Slack paging, schema validation, permission/rate-limit/
server errors, malformed and oversized responses, empty success responses and
cancellation during a write response. Fixtures do not contact real accounts or
send real messages.
The suite also fills all eight request slots and tests queued cancellation, plus
a blocking mock credential store to verify prompt cancellation and retained
admission until the actual credential jobs finish.

The focused adapter suite contains 19 passing local tests. It also verifies the
typed preparation boundary, final pre-send cancellation, attempted-connection
failure remaining uncertain, and malicious credential-store diagnostics without
leaking raw authentication data. Three additional actual-broker tests run with
isolated credentials and loopback HTTP:

```sh
cargo test --locked --offline -p workbench-services --lib connectors::native::
cargo test --locked --offline -p workbench-engine --lib native_connector_dispatch_tests
```

Those broker tests exercise real approval and journal transitions, preserve
failed versus unknown results, and verify that exact-ID retries send no additional
requests. Their credential-cancellation case returns while the real credential
job is still blocked. The mock-vault constructor is enabled only by unit tests or
the explicit `test-support` feature on the engine's development dependency;
normal application dependency edges do not enable it. These are focused protocol
checks, not a claim that the entire application or a live account was validated.

Live account setup, OAuth refresh/revocation, API permissions and provider behavior
remain separate acceptance checks. This suite is not a 100-connector load test or
a throughput benchmark. See [the completion audit](completion-audit.md).

The provider contracts were checked against the official
[GitHub pagination guide](https://docs.github.com/en/rest/using-the-rest-api/using-pagination-in-the-rest-api),
[GitHub search reference](https://docs.github.com/en/rest/search/search?apiVersion=2022-11-28),
[Drive files.list](https://developers.google.com/workspace/drive/api/reference/rest/v3/files/list),
[Gmail messages.list](https://developers.google.com/workspace/gmail/api/reference/rest/v1/users.messages/list),
[Calendar events.list](https://developers.google.com/calendar/api/v3/reference/events/list),
[Slack pagination guide](https://docs.slack.dev/apis/web-api/pagination/) and
[Slack search.messages](https://docs.slack.dev/reference/methods/search.messages/).
