# Custom provider compatibility and diagnostics

The user requested generic customizable gateway support without gateway branding, and approved daily/10 MiB rolling logs with seven-day/100 MiB retention and redacted diagnostics. They explicitly opted out of further design questions.

A live reproduction found a model accepts streaming, but rejects tools plus default reasoning on Chat Completions; Responses succeeds with both. Another model returns HTTP 403 with the application's credential. Configuration cannot grant service authorization.

Extend existing protocol adapters, rather than adding a vendor adapter or arbitrary executable request transforms. Custom connections expose automatic versus fixed protocol routing, relative API route templates, authentication header/prefix, extra generation defaults, and per-model capability overrides. Secrets remain in the vault, including optional additional credential headers. Catalog metadata supplies supported protocols, native family, token parameter, output cap, and reasoning/tool compatibility. Automatic routing prefers an advertised native Messages/GenerateContent format, otherwise Responses, then Chat Completions. Unknown catalogs preserve the selected protocol. Routes stay on the configured origin. Existing non-custom settings retain behavior.

The custom provider editor exposes the advanced configuration as validated JSON, keeping ordinary name/base URL/secret/metadata headers/catalog URL fields. Refresh propagates catalog capabilities into saved models and the active selection. Unsupported combinations fail clearly, without silently reducing reasoning or changing models.

Extend the existing logger rather than introduce another logging framework. Rotate on day or 10 MiB, keep seven days and at most 100 MiB, preserve private permissions and unrelated files. Capture provider metadata and lifecycle failures at useful levels; never log prompts, bodies, headers, tokens or raw provider error text. Forward worker severity instead of downgrading everything to DEBUG.

Validate with actual-file rotation tests, local HTTP fixtures for generic route/auth/discovery/request behavior, UI configuration round-trip tests, and relevant Rust suites. Preserve existing unrelated deletions. Live gateway tests are diagnostic evidence, not hermetic regression tests. No automatic restart of the user's running app.

## OpenAPI import and simpler form

The user additionally requested pasting OpenAPI JSON for automatic setup and removing unnecessary custom-provider options. Import recognizes supported wire protocols, catalog routes, server URLs and header security schemes from OpenAPI 3 / Swagger 2. It never fetches remote references or imports example credentials. Multiple required authentication headers are explained with a vault-backed credential-record notice. Ambiguous routes and incompatible operation servers fail clearly; unsupported authentication or missing discovery produces a manual-setup notice. The main form shows name, endpoint, credential and import; manual controls are collapsed under Advanced settings. Import fills the editor only; Save remains the storage/discovery action.
