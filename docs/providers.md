# Native providers and account connection

Xylo selects an actual protocol, rather than relabeling every provider as OpenAI Chat Completions. Rust owns HTTP pooling, streaming, credential lookup, cancellation and durable tool-round history. Selecting a provider does not launch Python, Node, a CLI agent, or a model process.

## Configuration and discovery

| UI/API family | Endpoint example | Credential in OS vault | Model field / discovery |
| --- | --- | --- | --- |
| OpenAI Chat | `https://api.openai.com/v1` or an explicitly compatible service | API key | Account `/models` catalog; manually enter custom IDs if needed |
| OpenAI Responses | `https://api.openai.com/v1` | API key, or eligible ChatGPT plan grant described below | Account `/models`; ChatGPT catalog preserves server order and selects `visibility=list` slugs |
| Anthropic | `https://api.anthropic.com` | Anthropic API key | Native `/v1/models`, `has_more` / `after_id` paging |
| Gemini | `https://generativelanguage.googleapis.com` | Gemini API key | Native `/v1beta/models`, `pageToken`; filters entries without `generateContent` support |
| Azure OpenAI | `https://RESOURCE.openai.azure.com/openai/v1` | API key or a current Entra bearer token | Enter the actual deployment name. A foundation model catalog ID does not identify a deployment. Azure management-plane deployment enumeration is not implemented. |
| Vertex Gemini | `https://LOCATION-aiplatform.googleapis.com/v1/projects/PROJECT/locations/LOCATION/publishers/google` | Current Google Cloud OAuth bearer token | Enter publisher model ID. Project deployments are not enumerated. |
| Bedrock | `https://bedrock-runtime.REGION.amazonaws.com` | Bedrock API key, or AWS access key / secret / optional session token | Native regional control-plane foundation models and inference profiles, with API-key bearer or SigV4 auth |

A service root, version root or complete supported inference endpoint can be normalized. Legacy Azure `/openai/deployments/NAME` endpoints require an explicit `api-version` query. Credentials and URL fragments are rejected in endpoint URLs. Remote HTTP is rejected; loopback HTTP supports local models and test fixtures. Authentication and transport headers cannot be replaced through generic custom headers. OpenAI-compatible services remain responsible for implementing their advertised protocol.

The model catalog is read-only, cancellable, capped at 2,000 distinct IDs and eight pages per catalog, with a response cap of 8 MiB and a request deadline up to 60 seconds. Bedrock has two separate catalogs. It never invents a fallback model list. Bedrock control-plane listing can require permissions beyond inference. Without an explicit vault credential, Bedrock inference can read existing AWS environment/static shared profiles, but discovery gives a configuration error and accepts a manually entered model ID. It does not claim the complete AWS SDK credential chain, SSO, ECS or IMDS refresh support.

`workbench_services::providers::{normalize_provider,protocol_capabilities,discover_provider_models}` are the shared host APIs. `ProviderCapabilities` describes protocol support, including streaming, function tools, image input and structured output. `account_or_model_dependent=true` means an actual model/account can reject one of those features. Bedrock structured-output support is currently reported false. Supported protocol does not establish access to every model family.

Credential records can be a plain API-key string, `{"api_key":"..."}`, `{"access_token":"...","expires_at_ms":0}` (use the actual future Unix expiry in milliseconds), or `{"access_key_id":"...","secret_access_key":"...","session_token":"..."}`. Expired records fail before dispatch. For Vertex and Entra, token provisioning/refresh is external to this application; no service-account private key or arbitrary command is executed. A raw key entered for Vertex is sent as bearer authentication. OS vault references, rather than token values, are stored with provider settings.

## Wire behavior

Chat uses native messages, image URL blocks, tool-call/result IDs and structured-output fields. Responses uses input items, instructions, `function_call_output`, `store=false` and encrypted reasoning history. Anthropic uses content blocks and native tool results; signed thinking blocks retain their order and signatures. Gemini/Vertex use `contents`, `systemInstruction`, native function calls/responses, inline image data and JSON response schema; `thoughtSignature` is preserved unchanged in assistant history. Bedrock uses Converse and CRC-validated AWS EventStream framing with native image/tool blocks and signed reasoning replay. Gemini, Anthropic and Bedrock inline images must use model-supported formats. Image payload fixtures test serialization, not a model's ability to interpret their contents.

Streams decode bytes incrementally across UTF-8 and network boundaries. Tool indices are compacted separately from native text/reasoning block indices. A disconnected stream without a protocol terminal event is an error. A late Responses `response.failed` is an error even after text deltas. Native content blocks are retained in the durable engine checkpoint and replayed with their tool observations. Nonstream responses also require an actual terminal reason. Provider transport and status errors omit arbitrary reflected response text.

The engine guard bounds context before dispatch, applies a cancellable generation deadline, and releases requests/streams when cancelled. Protocol cancellation ends local reading; it does not prove a remote mutation was undone. Local single-slot generation serialization applies only to explicitly registered managed endpoints. Other server concurrency remains server-managed. Live resource admission and bounded output remain active for all providers.

## Eligible Sign in with ChatGPT

The current official [local/open-source sign-in guide](https://developers.openai.com/siwc/token-sharing-open-source/sign-in) documents dynamic registration for eligible local and open-source applications. This project implements that path for a local application. Commercial or hosted distribution must separately satisfy OpenAI's eligibility and registration requirements; an API-key configuration is independent. See [SIWC overview](https://developers.openai.com/siwc) and [request a client ID](https://developers.openai.com/siwc/request-client-id). This implementation does not reuse Codex's client IDs, tokens, account cache, browser cookies or private `backend-api` endpoints.

`ChatGptSignInService::new(data_dir)` provides:

- `sign_in(host_id, vault, reference, cancel, authorize_url_callback)`: bind an ephemeral `127.0.0.1` callback listener before handing the authorization URL to the host. The host opens the system browser. The listener bounds request size, pending duration and invalid attempts. It validates exact callback URI, unique fields, random state, PKCE S256 and nonce. Stale or duplicate-state denials cannot terminate a genuine pending attempt.
- `prepare_provider_credential(config, vault, cancel)`: before catalog/inference dispatch, serialize refresh per vault reference across cooperating processes and save rotated refresh credentials before releasing the lease. Refresh does not replay a model request or tool.
- `account(vault, reference)`: return a token-free connection summary. Identity validation alone does not authorize plan usage. UI connection status requires both a connected credential and `chatgpt.tokens.use.direct` scope.
- `sign_out(vault, reference, cancel)`: request official revocation and clear all local access/refresh/ID tokens. Retain the public account/client registration for a future sign-in. Return a warning when remote revocation was not confirmed.

`Store::chatgpt_host_id()` supplies the persistent application host ID. Initial authorization uses the officially documented `dynamic_agent_client` registration hint and application name. Token exchange uses the real issued client ID from the validated callback. ID tokens are verified against published JWKS, permitted asymmetric algorithms, pinned issuer, issued audience, expiry, nonce and returning-account subject. Identity and plan scopes are checked separately. Authorization URLs can contain an ID-token hint; do not log them. Native vault APIs run on blocking workers.

Eligible plan inference uses the public `https://api.openai.com/v1/responses` endpoint only. The [preview restrictions](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations) are enforced: `stream=true`, `store=false`, explicit input history, supported function-tool namespaces, and omission of `temperature`, `max_output_tokens`, previous-response and hosted-tool fields. Account-specific `/models` uses the granted token. Success requires `response.completed`; a plan usage-limit failure remains a failure. See [models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference) and [profiles, refresh and sessions](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions).

The browser listener, signed identity validation, callback binding and token-refresh serialization have offline tests. No real account authorization, subscription access, quota, provider billing or live model inference was exercised in those fixtures. A real sign-in remains a user action and an account-dependent acceptance check.

## Qualification and primary references

Local HTTP fixtures cover Chat/Azure request paths and auth, streamed tool fragments, image encoding, JSON schema, Responses encrypted-history replay and late failures, Anthropic signed thinking and interleaved tool blocks, Gemini thought signatures/tool-result replay, Vertex project URLs/bearer auth, Bedrock bearer/SigV4 requests, binary CRC framing, signed reasoning, image/tool encoding, usage and truncated/corrupt streams. SSE framing tests split Unicode at every byte. AWS catalog signing is checked against AWS's published IAM ListUsers SigV4 reference signature. Credential tests use an injected mock OS store; no real secrets or accounts are required.

These checks establish the application's wire behavior against fixtures. They do not measure provider latency, local model speed, model task quality or external-account acceptance. Host JSONL, UI, native-asset and live-model qualification are tracked separately.

Primary protocol and authentication sources:

- [OpenAI Responses reference](https://developers.openai.com/api/reference/resources/responses/methods/create), [function calling and namespaces](https://developers.openai.com/api/docs/guides/function-calling).
- [Anthropic streaming](https://platform.claude.com/docs/en/build-with-claude/streaming), [structured outputs](https://platform.claude.com/docs/en/build-with-claude/structured-outputs).
- [Gemini generateContent REST API](https://ai.google.dev/api/generate-content), [thinking and signatures](https://ai.google.dev/gemini-api/docs/thinking), [structured output](https://ai.google.dev/gemini-api/docs/structured-output).
- [Vertex inference REST API](https://docs.cloud.google.com/vertex-ai/generative-ai/docs/model-reference/inference), [Google Cloud authentication](https://docs.cloud.google.com/vertex-ai/generative-ai/docs/start/quickstart).
- [Azure endpoint, token scope and deployment-name rules](https://learn.microsoft.com/en-us/azure/foundry-classic/openai/how-to/switching-endpoints).
- [Bedrock ConverseStream](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html), [Bedrock API keys](https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys-use.html), [foundation models](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_ListFoundationModels.html), [inference profiles](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_ListInferenceProfiles.html), [AWS request signing](https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html).
