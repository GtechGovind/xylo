# Custom Provider and Diagnostics Implementation Plan

> **For agentic workers:** Implement the bounded logging task independently; integrate provider changes in this session. Use test-driven development and a fresh final review.

**Goal:** Generic capability-aware custom providers and bounded useful diagnostics.
**Architecture:** Existing Rust protocol adapters, explicit custom route/auth configuration, per-model catalog capabilities, existing tracing pipeline with bounded rolling files.
**Tech Stack:** Rust, serde, reqwest, tracing, GPUI.
**Spec:** docs/superpowers/specs/2026-11-03-custom-provider-diagnostics-design.md

## Global Constraints
- No gateway names or host checks in implementation.
- No prompts, bodies, headers or secrets in diagnostics; custom secrets stay in vault.
- Routes cannot silently change origin; no new dependencies.
- Preserve user changes and the currently running app.
- User opted out of further design questions; implement in place on codex/custom-provider-diagnostics.

## Review Focus
- Old settings deserialize without changing behavior.
- Unrecognized model IDs retain explicit catalog capabilities.
- Credentials remain correct when one gateway uses multiple native protocols.
- Fixed protocol requests fail clearly for explicit unsupported combinations.
- Rolling survives restarts, oversized writes, day changes and unrelated/symlink files.

## Task 1: Custom provider contracts and adapters
- [x] Add failing core/services tests for routing capability selection, defaults, same-origin routes and vault-backed auth headers.
- [x] Implement CustomProviderConfig and ModelCapabilities, discovery metadata and normalized custom routes.
- [x] Add failing engine HTTP/request tests for route/auth defaults, unsupported combinations and safe diagnostics.
- [x] Implement capability-aware protocol choice and request defaults/limits; preserve existing adapters.
- [x] Run core/services/engine focused suites and checks.

## Task 2: Catalog persistence and editor
- [x] Carry per-model capabilities in HostEvent::Models and saved ProviderConfig.
- [x] Expose validated advanced custom connection JSON; preserve it on editing/model selection.
- [x] Add configuration round-trip and catalog routing tests, run desktop/runtime checks.
- [x] Document custom routes/authentication/defaults and migration via refresh.

## Task 3: Rolling logs (delegated independent files)
- [x] Add actual-file tests for rotation/retention/privacy and lifecycle/worker diagnostics.
- [x] Implement daily/10 MiB rotation, seven-day/100 MiB retention and correct worker severity.
- [x] Run runtime tests and report results.

## Final validation
- [x] Review integrated diff with fresh reviewer, resolve findings.
- [x] Run affected suites and workspace checks, report any limitations.

## Added at the user's request
- [x] Paste OpenAPI 3 / Swagger 2 JSON to import supported routes and header authentication without network access.
- [x] Keep the custom provider's main form minimal; put manual protocol, routes, headers and model overrides in Advanced settings.
- [x] Exercise actual supplied API specification and native settings import flow.

## Verification notes
- Runtime: 253 passed, 3 ignored.
- Worker unit/integration suites: 20 passed.
- Core: 23 passed; desktop provider unit tests: 12 passed; native account/settings interactions: 12 passed.
- Broad engine/services runs each encountered one unrelated subprocess timing failure; both failed tests passed on isolated Cargo reruns. Final focused suites pass: engine providers 102, services providers/importer 73 (including 12 importer tests). Workspace all-target check and desktop/worker/CLI builds pass.
- Live diagnostic: tool requests to the selected reasoning model require Responses or explicit reasoning none on Chat Completions. The tested restricted native-family model returned HTTP 403. Client changes cannot grant server access.
- Preserve the currently running process; new binaries require an application restart.

- Final native Messages live recheck could not run: the current profile no longer contains the previously tested gateway connection. No saved connection settings were written by this diagnostic. Earlier HTTP 403 remains the only live evidence for that model.

## Default-branch integration verification

Rebased onto `8a81a02` (RC5) before committing. Preserved the default branch's validation lockfile versions. Formatting and strict workspace Clippy pass; two test-only lint issues from the new base were corrected without changing application behavior. Native account/settings interactions: 12 passed. Workspace test groups were exercised with `--no-default-features` to exclude the unrelated bundled-browser source build. Three existing process/cancellation timing tests failed under parallel execution and passed on isolated serial reruns; remaining workspace tests and doctests passed. Unrelated local deletions and build artifacts are excluded from the integration commit.
