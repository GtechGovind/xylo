# Model providers (`xylo-engine` `providers/`)

## Purpose and boundaries

`crates/engine/src/providers/` holds the native HTTP adapters that implement `ModelProvider` (`model.rs`) for the model families Xylo supports. This page covers the adapters, the guard that wraps them (`provider_guard.rs`), the error vocabulary (`ModelError` in `model.rs`), fallback and renewal, and how a provider is configured. The loop that calls providers is in [engine](engine.md); tool execution is in [engine-tools](engine-tools.md); helper agents (which share the same provider) are in [engine-agents-and-skills](engine-agents-and-skills.md). Settings types live in [core](core.md), endpoint normalization, credential parsing and discovery in [services](services.md).

The module doc (`providers/mod.rs`) states the contract: **no provider selection starts a subprocess, reads another application's auth cache, or silently changes to another API family.**

## Module map

| Path under `crates/engine/src/providers` | Purpose |
| --- | --- |
| `mod.rs` | `build`, `NativeProvider`, HTTP client, header redaction list, HTTP error mapping, bounded body reads |
| `request.rs` | Request bodies for Chat Completions, Responses, Anthropic and Gemini; Responses output parser |
| `stream.rs` | `SseDecoder` (SSE framing) and `EventState` (per-protocol event decoding) |
| `bedrock.rs` | Bedrock Converse adapter: SigV4, binary event stream, cache points |
| `diagnostics.rs` | Redaction and bounding of provider error text |
| `renewal.rs` | `RenewableProvider` for ChatGPT-plan credentials |
| `fallback.rs` | `WithFallback` (primary plus configured alternative model) |
| `gemini_cache.rs` | Short-lived explicit Gemini prefix caches |
| `thinking.rs` | Capability-checked reasoning/thinking controls, Anthropic binding beta |
| `usage.rs` | Remaining-quota headers exposed as `RateLimits` |
| `search.rs` | Native server-side web search requests (Responses, Anthropic, Gemini) |

Related: `provider_guard.rs` (`GuardedProvider`, `ContextBudget`), `credentials.rs` (`ProviderCredentialResolver`), `agent_loop/retry.rs` and `agent_loop/fallback.rs`, and in other crates `xylo_core::ProviderConfig`, `xylo_services::providers`.

## Wire protocols and `ApiFamily` mapping

`xylo_core::ApiFamily` has seven values; `xylo_services::providers::protocol_capabilities` maps them to five `ProviderProtocol`s:

| `ApiFamily` (snake_case in settings) | `ProviderProtocol` | Wire |
| --- | --- | --- |
| `open_ai_chat` | `ChatCompletions` | `POST …/chat/completions` + SSE |
| `azure` | `ChatCompletions` | same, Azure routing and `api-key` header |
| `open_ai_responses` | `Responses` | `POST …/responses` + SSE |
| `anthropic` | `AnthropicMessages` | `POST …/messages` + SSE |
| `gemini` | `GeminiGenerateContent` | `POST …/models/MODEL:streamGenerateContent?alt=sse` |
| `vertex` | `GeminiGenerateContent` | same, under a Vertex project/location path |
| `bedrock` | `BedrockConverse` | `POST /model/MODEL/converse-stream` (AWS binary event stream) |

`providers::build(config, secret)` parses the secret into a `ProviderCredential` and returns `BedrockProvider` for `ApiFamily::Bedrock`, otherwise `NativeProvider`. `NativeProvider` always streams: `send` posts the JSON body to `endpoint.stream_url` with `accept: text/event-stream`. Capabilities reported for every family: streaming, function tools and image input; structured output for all but Bedrock.

`NativeProvider::new` enforces:

- A credential is required unless the host is `localhost`, `127.0.0.1` or `[::1]` (`"Save a provider credential before connecting"`).
- AWS credentials only with the Bedrock family.
- A ChatGPT-plan credential only with the Responses protocol on exactly `https://api.openai.com/v1`.
- For Anthropic with reasoning efforts and no adaptive thinking, the `anthropic-beta: interleaved-thinking-2025-05-14` header is added (merged with any existing value). A request body that carries `thinking.block_binding` adds the `thinking-binding-controls-2026-08-01` beta per request.

Request bodies (`request.rs`) are built for prompt caching: Anthropic gets four breakpoints; OpenAI Responses (and Chat Completions only on OpenAI itself) carries a `prompt_cache_key` that is a digest of model, system instructions and the first conversation message, so no content leaves in clear. OpenAI functions are `strict` only when the schema already meets strict mode. `reasoning_effort` (Chat) or `reasoning.effort` (Responses) is sent only when the capability check allows it.

## Endpoint rules (`xylo_services::providers::normalize_provider`)

`provider_url` rejects, for inference and for an explicit `discovery_url` alike:

- URLs with embedded credentials or a fragment.
- Schemes other than **HTTPS, or HTTP on loopback only** (`localhost`, `127.0.0.1`, `[::1]`).
- Query parameters whose normalized name is a secret (`key`, `apikey`, `token`, `password`, `signature`, `x-amz-signature`, …): "Keep provider credentials in the OS credential store".
- `chatgpt.com` hosts and `/backend-api` paths: only the official public API is supported.

`normalize_provider` accepts a service root, a version root or a full inference URL and produces `inference_url`, `stream_url` and an optional `models_url`:

- **Chat/Responses/Azure**: trailing `/chat/completions` or `/responses` is stripped; an empty path becomes `/v1` (`/openai/v1` for Azure hosts `*.openai.azure.com`, `*.services.ai.azure.com` or the `azure` family). Legacy `/openai/deployments/` paths require an `api-version` query. Azure and legacy endpoints have no models URL.
- **Anthropic**: trailing `/messages` stripped; default `/v1`.
- **Gemini**: path cut at `/models`; default `/v1beta`. The model must be a plain ID (no `?#:/`; a `models/` prefix is trimmed).
- **Vertex**: the path must contain `/projects/`, `/locations/` and end with `/publishers/google`; there is no models URL.
- **Bedrock**: the path must be empty and the host `bedrock-runtime.REGION.amazonaws.com` (or loopback for tests); region is parsed from the host. Custom catalog URLs are refused for Bedrock; the catalog is `https://bedrock.REGION.amazonaws.com/foundation-models`.

## Credentials and headers

Credentials are secrets passed to `build`; they come from the credential vault (`config.credential_ref` looked up in the vault, see the vault section of [services](services.md)) or, in a worker, from the host-authorized `WorkerCredential` (see Renewal). Settings never hold the secret.

`ProviderCredential::parse` (services) accepts:

- Plain text: an API key. A JSON record (starts with `{`) is also accepted.
- `expires_at_ms` must be more than 30 s ahead ("Provider credential has expired").
- A record with `access_key_id` (+ `secret_access_key`, optional `session_token`): AWS keys.
- `type: "chatgpt"`: requires `access_token`, `client_id`, `subject`, `ext_agent_host_id` and the scope `chatgpt.tokens.use.direct`.
- Otherwise `access_token` (bearer) or `api_key`.

`ProviderCredential::headers` sets `user-agent: Xylo/0.1`, `anthropic-version: 2023-06-01` (Anthropic) and the auth header: `x-api-key` (Anthropic), `x-goog-api-key` (Gemini), `api-key` (Azure), otherwise `authorization: Bearer …`. Bearer and ChatGPT tokens use `authorization`. Secret variants implement neither `Debug` nor `Serialize`.

Custom headers (`ProviderConfig.headers`, `validate_custom_headers`): at most 16; names at most 128 and values at most 4096 bytes; **no transport headers** (`host`, `content-length`, `connection`, `transfer-encoding`, …, `proxy-*`), **no `x-amz-*`, and no name containing** `auth`, `token`, `key`, `secret`, `cookie`, `session`, `signature`, `password`, `credential` or `bearer`. Custom headers are plain metadata (tenant, project, routing).

## HTTP client and transport

`http_client` builds every `NativeProvider` client with the default headers, **no redirects** (`Policy::none()`), **no automatic retries** (`reqwest::retry::never()`) and a **10 s connect timeout**. There is no overall request timeout on the client; deadlines come from `GuardedProvider` (below). The Bedrock client has the same redirect, retry and connect settings plus a `read_timeout` of `timeout_seconds` clamped to 1–3600 s.

A transport failure becomes `ModelError::Provider` that is retryable **only if `error.is_connect()`** ("once request transmission may have started, replay is unsafe"). URLs are removed from reqwest errors (`without_url`). Stream read failures are `ModelError::transient`.

## Streaming and byte bounds

| Bound | Value | Where | On exceeding |
| --- | --- | --- | --- |
| Error / JSON body read | 8 MiB (`read_bounded`, checks `content-length` first) | `mod.rs` | permanent `Provider` error "Provider response exceeds 8 MiB" |
| One SSE event (`data:` accumulated) | 8 MiB | `SseDecoder` | permanent error "Provider SSE event exceeds 8 MiB" |
| Buffered undelivered SSE bytes | 8 MiB | `SseDecoder::feed` | permanent "exceeded its byte budget" |
| Whole SSE stream | 32 MiB | `SseDecoder::feed` | permanent "exceeded its byte budget" |
| Bedrock event-stream frame / buffer | 8 MiB (`MAX_FRAME`) | `bedrock.rs` | permanent "frame exceeds 8 MiB" |

`SseDecoder` splits on `\n` (strips `\r`), handles `data:` and `event:` lines, joins multi-line data with `\n`, requires valid UTF-8, and `finish()` fails with a retryable error if the stream ends inside a frame. Bedrock has no 32 MiB per-stream cap in this adapter; only the frame/buffer limit.

`NativeProvider::stream` first yields `StreamEvent::Metadata { model, rate_limits }` (numeric `x-ratelimit-remaining-*` and `anthropic-ratelimit-*` headers only, `usage.rs`), then decodes events. Cancellation (`CancellationToken`) is checked before sending and on every chunk and ends the stream with `ModelError::Interrupted`.

Completion rules (`stream.rs`): `[DONE]` is accepted only for Chat Completions after a finished choice; every other protocol must reach its terminal event. **A stream that ends before its terminal event is a retryable error** ("Provider stream ended without its terminal event"; for Bedrock, a missing `messageStop`). An SSE `error` event, `event: error` or `type: error` becomes a stream error: codes such as `overloaded_error`, `server_error`, `api_error`, `internal_error`, `service_unavailable`, `UNAVAILABLE`, `INTERNAL` map to `Unavailable { status: 503 }`; `rate_limit_error`, `rate_limit_exceeded` and `RESOURCE_EXHAUSTED` map to retryable `Provider`; anything else is permanent.

## Errors to `ModelError`

`ModelError` (`model.rs`) variants: `Authentication`, `RateLimited { retry_after, message }`, `Provider { retryable, retry_after, message }`, `Unavailable { status, retry_after, message }`, `InvalidRequest`, `ContextOverflow`, `Timeout`, `Interrupted`, `Io`, `Stalled`, `EmptyResponse`. `retryable()` is true for `RateLimited`, `Unavailable`, `Timeout`, `EmptyResponse` and `Provider { retryable: true }`; false for the rest. `ModelError::transient`/`permanent` build `Provider` errors.

`NativeProvider::send` maps a non-2xx response (the body is read up to 8 MiB as JSON; an unreadable body is treated as `null`). The code is `/error/code` or `/error/type` if it passes `diagnostics::identifier`, else `request_failed`. `Retry-After` is parsed as delta seconds or an HTTP date. Mapping order in `http_error`:

1. ChatGPT-plan credential, HTTP 401 and code `token_expired` → `Authentication("ChatGPT access token expired (HTTP 401)")` (the renewal marker).
2. Status 401 or 403, or code `invalid_api_key` → `Authentication`.
3. 429 → `RateLimited` (with `retry_after`).
4. 500–599 → `Unavailable { status, retry_after }`.
5. Context overflow → `ContextOverflow`: codes `context_length_exceeded`, `string_above_max_length`, `request_too_large`, `exceed_context_size_error`; or status 400/413/422 whose reason contains a known phrase ("prompt is too long", "maximum context length", "exceeds the available context size", "input token count", …).
6. Codes `model_not_found`, `invalid_model`, `model_not_available`, `invalid_request_error`, `invalid_request`, `invalid_schema` → `InvalidRequest`. For the model codes the message adds a hint to refresh models in Settings > Providers (the model name is printed only if it is a short, URL-free token).
7. Otherwise `Provider { retryable: status in 408, 409, 5xx }`.

If the status is 400/413/415/422 and the request body contained images, the message adds that the provider rejected images and **no other model was selected**.

Bedrock (`map_error`) never reflects the response body: 401/403 or `AccessDeniedException`/`UnrecognizedClientException`/`InvalidSignatureException` → `Authentication`; 400 with "input is too long", "maximum number of input tokens", "maximum context length" or "too many tokens" → `ContextOverflow`; 429 or `ThrottlingException` → `RateLimited`; 5xx → `Unavailable`; otherwise `Provider` retryable only for 408 and 424. In-stream exceptions use the same mapper through the `EXCEPTIONS` table.

Deadline errors are `Timeout` (from the guard); a cancelled task is `Interrupted`; local lock or serialization faults are `Io`.

## Diagnostics redaction (`diagnostics.rs`)

Provider errors are untrusted text that may echo input or auth. Only bounded diagnostics are exposed, never raw payloads or headers.

- `redactions(headers)` collects every default header value, plus the secret part of `Bearer`/`Basic` values, longest first; these are replaced by `[redacted]` in messages and cause identifiers containing them to be dropped.
- `identifier`: at most 120 bytes of `[A-Za-z0-9_.\-\[\]]`, not starting with `sk-` or `eyJ`, not containing a redaction value. Used for error codes, request IDs (`x-request-id` or body `request_id`) and `type`/`status`.
- `readable` (the provider's reason text from `/error/message`, `/detail` or `/detail/message`): replaces secrets; **replaces every quoted value** (`'`, `"`, backtick) with `[redacted]` unless it is an exact public protocol field (`input`, `messages`, `system`, `thinking`, `tools`, `reasoning`, `instructions`, `model`, `stream`, `store`, `max_output_tokens`, `temperature` and indexed/dotted paths) or one of `signature`, `redacted_thinking`, `drop_block`, `anthropic-beta`, `thinking-binding-controls-2026-08-01`; strips control and bidi characters; replaces words that contain `://`, `@`, a `?…=` query, start with `www.`, `sk-` or `eyj`, exceed 120 bytes, look like `token=`/`password:` assignments, or follow `bearer`, `authorization`, `api_key`, `secret` and similar keys. **The result is bounded to 480 characters** (truncated with `…`).
- `stream_error` builds "Provider stream error (code=…, type=…, field=…, request=…)" from safe identifiers plus the readable message, bounded to 1000 characters.

## Retries and fallback

The adapters never retry: **no automatic HTTP retries** exist, and a failed request is reported as a typed `ModelError`. Retrying is the agent loop's job (`agent_loop/retry.rs`, see [engine](engine.md)):

- `MAX_RETRIES = 5` retries after the first failure, only when `ModelError::retryable()`.
- A provider `retry_after` is honored up to `MAX_RETRY_AFTER = 120 s`; a longer wait fails the request so the user decides. Without it: exponential backoff from 1 s, capped at 32 s, with ±25 % jitter.
- `ContextOverflow` is not retried verbatim; the loop compacts harder (`Pressure::Overflow`) and retries once.

**Fallback.** `ProviderConfig.fallback_model` names an alternative model **on the same authorized connection** (same endpoint, credential and headers). `AgentEngine::provider` builds it only when the name is non-empty and differs from `model`: the alternative config is a clone with `fallback_model`, reasoning settings and `served_context_tokens` cleared. Both are wrapped in `WithFallback { primary, alternative, name }`; `stream` and `search` use the primary, and `fallback()` returns the alternative. `GuardedProvider::fallback` re-wraps the alternative in the same guard. The loop switches only when retries are exhausted and **`permits_fallback` holds, which is `ModelError::Unavailable` only** (an explicit service-unavailable response). It then adds an application note, checkpoints, **persists the choice** (`store.select_checkpoint_fallback`) before the first alternative request, and emits a `NoteKind::Fallback` event. A resumed turn restores the choice from that checkpoint metadata only (`restored_fallback`). There is no cross-family or cross-connection fallback. The app-level `AppSettings.fallback_model` is the default alternative ID for the active connection.

## Guard: deadlines, local admission and lease (`provider_guard.rs`)

`GuardedProvider` wraps the real provider (itself wrapped by the attachment hydrator and usage meter; see [engine](engine.md)):

- **Output clamp**: `max_tokens = min(request.max_tokens, effective output tokens).max(1)`.
- **First-response and idle deadlines**: both use `ProviderConfig.timeout_seconds` (default 120). Each `stream.next()` is wrapped in a timeout of that length: failing before the first event gives `ModelError::Timeout("Model first response deadline exceeded")`, later `"Model stream idle deadline exceeded"`. Timeout is retryable. Context-summary requests use `min(timeout, 90 s)`.
- **Local vs cloud**: a provider is *local* if the endpoint host is `localhost`, `127.0.0.1` or `[::1]`. Local or application-managed endpoints get a `ResourceManager`; **cloud calls reserve nothing**. Before streaming, a local call reserves memory (`TaskDemand::interactive(context)`) and waits at most `timeout_seconds` (`Timeout("Waiting for enough free memory to run the local model")`; cancellation → `Interrupted`).
- **Cross-process lease**: only when the endpoint is local **and** registered as `single_slot` in `<data>/managed-endpoints.json` (`managed_endpoint`), the guard takes an exclusive `fs2` file lock on `<data>/local-inference.lock`, polling every 20 ms up to `timeout_seconds` (`Timeout("Waiting for the local model to finish another generation")`). **The lease is held only while a response streams, never across tool use**, and released when the stream is dropped.
- `ProviderConfig::context_window()` is the configured context lowered to `served_context_tokens` (probed from llama.cpp `/props`, Ollama `/api/ps`/`/api/show`, LM Studio `/api/v0/models` on loopback only, 2.5 s total, no credentials, never loads a model; cached per session in `ServedContextCache`). A fallback alternative clears it.

## Credential renewal (`renewal.rs`, `credentials.rs`)

Only **ChatGPT-plan credentials** renew. `build_renewable(config, credential, resolver)` returns a plain `build` result unless the credential record has `type: "chatgpt"` **and** a `ProviderCredentialResolver` is installed; then it requires `ApiFamily::OpenAiResponses` on exactly `https://api.openai.com/v1` and returns a `RenewableProvider`. The worker receives the host-authorized `WorkerCredential`; the host answers `resolve(generation, force, cancel)` over the worker's private pipe (the engine never refreshes tokens itself).

- A still-valid credential is reused without asking the host. The host is asked only when the credential is within `RENEWAL_MARGIN_MS = 5 min` of `expires_at_ms` (or has no expiry) or when the provider rejected it.
- Rejection means the exact marker `ChatGPT access token expired (HTTP 401)`: HTTP 401 with code `token_expired` on a ChatGPT-plan provider. Raw provider text never makes a request eligible.
- The state lock is held while the host answers, so **concurrent requests wait on one renewal**; generations let a later request reuse a renewal another already obtained.
- The renewed credential must belong to the same account (`subject`, `client_id`, `issuer`, `ext_agent_host_id` all equal and non-empty) and not be an older generation, else `Authentication("The renewed account did not match this task. Reconnect in Providers.")`.
- **A stream is replayed once, and only if no event was emitted yet** (`emitted == false`, not already retried). Once output has been yielded the error propagates. `search` follows the same single-replay rule.
- An invalid renewed connection becomes `Authentication("The renewed connection is invalid. Reconnect in Providers.")`.

Static credentials (API keys, AWS keys, Bedrock/Vertex bearer tokens) are not renewed; an expired record fails at `ProviderCredential::parse`.

## Gemini prefix caching (`gemini_cache.rs`)

For `ApiFamily::Gemini` only (not Vertex), before each send `cached_body` tries to move a stable prefix into an explicit `cachedContents` resource:

- Prefix = `systemInstruction`, `tools` and the first `min(contents.len() − 1, 2)` contents. If the serialized prefix is **under 32 KiB (32768 bytes), no cache is used**.
- Key = SHA-256 of the prefix JSON. Entries are kept per provider instance (so scoped to one authorized client), at most 8 (the map is cleared when full), and **expire locally after 240 s**, while the created resource requests `ttl: "300s"`. The 240 s margin keeps a local entry from outliving the server's.
- A miss POSTs `{base}/cachedContents`; the returned `name` is accepted only if it starts with `cachedContents/`, is at most 256 bytes and uses `[A-Za-z0-9/_-]`. Any failure silently means "no cache".
- The send body then drops `systemInstruction` and `tools`, keeps the remaining contents and adds `cachedContent`.
- `stream` bounds cache preparation to 5 s. If the cached request fails with `InvalidRequest` or a non-retryable `Provider` error, the cache is cleared and the **uncached body is sent once**.

## Bedrock (`bedrock.rs`)

`BedrockProvider` uses the Converse stream API (`/model/{percent-encoded model}/converse-stream`). Credentials come **only from the vault**: a bearer API key (plain secret or `access_token`) or explicit AWS keys signed with SigV4 (service `bedrock`; region from the host, default `us-east-1`). The process environment and shared AWS files are never consulted. A missing credential, or a ChatGPT credential, is refused at construction.

- Redirects are rejected; request size is bounded; Anthropic models get betas `context-1m-2025-08-07`, `interleaved-thinking-2025-05-14`, `fine-grained-tool-streaming-2025-05-14`.
- The binary AWS event stream is decoded frame by frame with **both CRCs validated**; frames over 8 MiB, malformed headers and trailing partial frames are errors (partial frame: retryable).
- The stream must end with `messageStop` carrying a real reason: `end_turn`/`stop_sequence` → stop; `tool_use` → tool calls; `max_tokens`/`model_context_window_exceeded` → length (so the loop continues the partial answer); `content_filtered`/`guardrail_intervened` and unknown reasons → permanent errors. Incomplete tool-argument JSON is a permanent error.
- Exact content blocks (signed reasoning, tool use) are kept for replay.
- Prompt caching `cachePoint` checkpoints: Anthropic models after tools, system and newest message; `amazon.nova*` (also with a region prefix) after system and messages only; other models get none because Bedrock rejects unsupported checkpoints.
- Error messages never include response bodies (see Errors).

## Search and thinking helpers

- `supports_search()` is true for Responses, Anthropic and Gemini adapters (not Chat Completions or Bedrock). `search.rs` builds a separate native search request (4096 output tokens, lowest reasoning) reusing the connection headers; provider citations are kept as untrusted JSON. `WithFallback` and `GuardedProvider` delegate search to the primary/inner provider.
- `thinking.rs` applies reasoning controls only when the model's capability check (`ProviderConfig::supported_reasoning_efforts`, `adaptive_thinking`) allows them, and for Anthropic marks replayed signed thinking so a changed prefix discards only stale reasoning (`thinking-binding-controls-2026-08-01` beta).

## Local vs cloud

| | Local (loopback) | Cloud |
| --- | --- | --- |
| Scheme | HTTP or HTTPS | HTTPS only |
| Credential | optional | required (`Save a provider credential before connecting`) |
| Memory admission | yes (`ResourceManager`) | none |
| Cross-process lease | only for `single_slot` managed endpoints | none |
| Served-context probe | yes (loopback only) | none |
| Prompt-cache key (Chat Completions) | not sent (third-party servers may reject unknown fields) | only on OpenAI itself |

`ProviderConfig.managed_local` means the application owns the local server and restarts it before a turn; external endpoints keep their own lifecycle.

## How a provider is configured

1. **Settings** (`xylo_core::AppSettings`, see [core](core.md)): `provider` is the active `ProviderConfig`; `providers` lists saved connections; `fallback_model` is the default alternative ID. A `ProviderConfig` has `name`, `family`, `endpoint`, `model`, `credential_ref`, `context_tokens` (default 8192), `output_tokens` (2048), `timeout_seconds` (120), `served_context_tokens`, reasoning fields (`reasoning_effort`, `reasoning_options`, `default_reasoning_effort`), `discovery_url`, `managed_local`, `headers` and `fallback_model`. The default is a local Chat Completions server at `http://127.0.0.1:8080/v1`.
2. **Secrets**: `credential_ref` is only a reference (e.g. `account:chatgpt` for the ChatGPT account). The value is in the encrypted `CredentialVault` ([services](services.md)). `normalize_provider` validates metadata (endpoint and headers) before a connection or secret is saved.
3. **Discovery**: `discover_provider_models(config, vault, cancel)` reads the vault, parses the credential and lists models from `models_url` (read-only, at most eight pages / 2,000 models, never starts a model process). Local catalog contexts are lowered to served contexts.
4. **Turn start** (`AgentEngine`): the turn request carries a `ProviderConfig` snapshot; the engine probes the served context, then `configured_provider` builds the adapter: with a host-authorized `WorkerCredential` it calls `build_renewable`; otherwise it reads `credential_ref` from the session vault and calls `build`. Optional `ProviderLayer` (tests/evals only), then `WithFallback`, `AttachmentProvider`, `GuardedProvider` and the usage meter wrap it.
5. **Reconnect**: for ChatGPT accounts the runtime's `RunCredentials`/`ChatGptSignInService` renews through the resolver pipe; failures surface as `Authentication` errors asking the user to reconnect in Providers.

## Guarantees summary

- **HTTPS (or loopback HTTP), no redirects, no automatic HTTP retries, 10 s connect timeout.**
- **No secrets, URLs or quoted values in error text; provider reasons at most 480 characters.**
- **Error bodies at most 8 MiB, SSE events 8 MiB, streams 32 MiB.**
- **A stream that ends without its terminal event is retryable, never a silent success.**
- **Renewal replays a request at most once and only before any output.**
- **Fallback stays on the same connection and family, is persisted before use, and is triggered only by `Unavailable`.**
