# Obscura as Xylo's sole managed browser

Date: 2026-11-02. Status: **historical proposal superseded by the current bundled implementation; not runtime-qualified**.

## Current implementation (supersedes the proposal below)

Xylo now integrates Obscura as its sole browser component: source-built from pinned **v0.2.6**, render-enabled and packaged as the sibling `xylo-obscura` executable (`xylo-obscura.exe` on Windows). Xylo supervises the child through **private local IPC**, not a browser URL, public CDP listener, cloud service or MCP connection. There is no custom runtime path, optional browser asset installation, installed Chrome/Chromium/Edge discovery or fallback.

Exactly **seven** tools are advertised: `browser_navigate`, `browser_snapshot`, `browser_click`, `browser_type`, `browser_wait`, `browser_screenshot`, `browser_close`. `browser_download` is unsupported and rejected before launch, rather than emulated with another HTTP transport. Normal-window engine advertisement includes `browser_*` by default without checking config paths or runtime presence; small/tiny windows keep compact discovery behavior.

`CapabilityConfig.chromium` is removed. Persisted legacy fields are ignored by serde, preserving unrelated settings and dropping obsolete keys on save. Configure/patch APIs reject `chromium`, `obscura` and `browser_url`. The only browser configuration is `browser_allow_private_network`, an explicit **broad** loopback/private-network opt-in, default `false`, not the session-scoped per-host consent envisioned below.

This records implementation, **not successful runtime qualification**. The Python build/package/release scripts and protocol test fixtures have been replaced with Rust. Standard Cargo builds enable the desktop crate's `bundled-browser` feature by default; its `build.rs` invokes the shared Rust browser builder. The builder downloads the versioned V8 static archive with Rust and supplies a local path, bypassing upstream interpreter-based downloads; V8 is still a prebuilt C++ dependency. Offline build-tool tests and Cargo metadata/lock resolution have passed locally on Windows. A completed Obscura build and browser acceptance are separate gates. Platform packaging, rendering, web compatibility, process cleanup and security/confinement acceptance still require runtime evidence; process separation is not a claim of Chromium-equivalent sandboxing.

## Historical proposal and evidence

Everything below is retained as the original assessment, not current setup instructions or an active runtime/configuration contract. In particular its eight-tool count, CDP transport, optional installation, download-preservation requirement and local-network consent proposal are superseded above. Historical source line numbers refer to the pre-cutover tree.

## Recommendation and scope

Integrate Obscura as a **supervised sidecar behind Xylo's existing eight `browser_*` tools**, using CDP initially. After qualification, remove the managed Chrome/Chromium/Edge launch paths entirely. Do not introduce a permanent browser selector or silent fallback.

Here, "default browser" means **Xylo's agent-controlled managed browser**, not the operating system's default URL handler, a personal browser replacement, or an embedded browser UI. Obscura is headless. Xylo's native desktop interface stays native.

This is feasible architecturally, but **Obscura v0.2.6 is not a drop-in replacement for the present implementation**. Downloads and process lifecycle need changes before a full replacement can ship. Do not delete the working backend before the replacement passes the release gates below.

Prefer the render-enabled, non-stealth release variant initially. Rendering is necessary for screenshots. Stealth is not needed for integration and introduces fingerprinting, tracker-blocking and additional build/qualification variables. Do not add Obscura Cloud, proxies, Puppeteer, Playwright or a Node browser bridge.

## Evidence and version boundary

The latest release URL resolved to **v0.2.6**, commit `8534b401898ed17b62f69bdc3d5eb186bffb0d60`. Release behavior below was checked against tagged source where indicated. README and general guides were read on `main`; they can describe behavior beyond this release. Pin a reviewed release plus artifact hashes, not `latest` or a moving Git branch.

Upstream sources:

- [Release v0.2.6](https://github.com/h4ckf0r0day/obscura/releases/tag/v0.2.6)
- [README](https://github.com/h4ckf0r0day/obscura/blob/main/README.md): independent Rust engine, V8, CDP, rendering, Apache-2.0 and stated web-platform limitations. Performance numbers are upstream claims, not Xylo measurements.
- [Browser CDP handler, v0.2.6](https://github.com/h4ckf0r0day/obscura/blob/v0.2.6/crates/obscura-cdp/src/domains/browser.rs): `setDownloadBehavior` and `close` return success without enforcement/process shutdown; `cancelDownload` is not implemented in this handler. `getVersion` reports a Chrome identity, so it cannot authenticate an Obscura installation.
- [Accessibility handler, v0.2.6](https://github.com/h4ckf0r0day/obscura/blob/v0.2.6/crates/obscura-cdp/src/domains/accessibility.rs): implements `getFullAXTree`. Its presence does not prove Chromium-equivalent semantics or compatibility with chromiumoxide's typed deserialization.
- [CDP dispatcher, v0.2.6](https://github.com/h4ckf0r0day/obscura/blob/v0.2.6/crates/obscura-cdp/src/dispatch.rs): CDP method routing, including accessibility, Fetch and IO; some domains deliberately acknowledge calls without implementation.
- [CLI, v0.2.6](https://github.com/h4ckf0r0day/obscura/blob/v0.2.6/crates/obscura-cli/src/main.rs): `serve`, explicit host/port, `--ready-file`, connection limits, storage and file-access flags.
- [Release workflow, v0.2.6](https://github.com/h4ckf0r0day/obscura/blob/v0.2.6/.github/workflows/release.yml): five native target builds, four variants, V8/screenshot smoke tests and archive layout.
- [Environment variables](https://github.com/h4ckf0r0day/obscura/blob/main/docs/Environment-variables.md): CDP bearer authentication, SSRF defaults and deadlines.
- [Playwright guide](https://github.com/h4ckf0r0day/obscura/blob/main/docs/Use-with-Playwright.md): connection and close behavior, rendering, buffered response bodies and web-platform limitations.

## Current Xylo architecture

- `crates/capabilities/src/browser.rs:38`: session owns chromiumoxide browser/page, handler, profile and downloads directory.
- `browser.rs:70`: launches configured `CapabilityConfig.chromium` or detects installed Chrome/Chromium/Edge. This is Chrome-style launching, not Obscura's `serve` command.
- `browser.rs:94`: denies downloads browser-wide before creating the page.
- `browser.rs:154`: timeout/cancellation race; any error tears down the session instead of replaying uncertain actions.
- `browser.rs:185`: navigate, AX snapshot, CSS click/type/wait, viewport PNG, authenticated download and close.
- `browser.rs:303`: downloads require browser download events, cancellation, staging and on-disk byte validation; default 50 MiB, hard ceiling 100 MiB.
- `browser.rs:519`: system-browser autodetection to remove after qualification.
- `crates/capabilities/src/lib.rs:29`: serialized capability configuration; helper owns its persistence and service replacement.
- `crates/engine/src/tools/mod.rs:577`: initial browser advertisement currently checks `config.chromium.is_some()`. Replacing only browser.rs would miss this integration point.
- Current managed sessions have disposable profiles, not persistent personal login profiles. Preserve this default.

## Compatibility assessment

| Area                       | Assessment                                            | Required work or proof                                                                                                                                                                |
|----------------------------|-------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Rust/native application    | Good architectural fit via sidecar                    | Avoid linking V8/rendering into Xylo desktop/helper binaries initially.                                                                                                               |
| License                    | Apache-2.0 is compatible at the project-license level | Review bundled third-party/V8/font notices and redistribution obligations.                                                                                                            |
| CDP                        | Partial compatibility, not full Chrome equivalence    | Test chromiumoxide 0.9.1 connection, target setup, events, enum parsing and every operation. A successful acknowledgement is not proof of enforcement.                                |
| Launch                     | Incompatible with current launcher                    | Spawn `obscura serve`; use CDP connect, not `Browser::launch` with a renamed executable.                                                                                              |
| AX snapshot                | Implemented upstream                                  | Validate node fields/roles/IDs, hidden content, labels, forms, frames and node truncation. Do not substitute MCP readable text without an explicit contract change.                   |
| CSS click/type/wait        | APIs exist upstream                                   | Prove focus, append-vs-fill behavior, input/change events, navigation after click and selector geometry with chromiumoxide.                                                           |
| Screenshots                | Render-enabled build supports PNG                     | Explicitly configure 1280x900 viewport; test fonts, scroll position and artifact output. Window-bound acknowledgements alone are insufficient.                                        |
| Downloads                  | **Blocking mismatch**                                 | `Browser.setDownloadBehavior` is a no-op and `cancelDownload` is unsupported in the inspected handler. Current event-driven implementation cannot be assumed to work.                 |
| Close/cleanup              | **Blocking mismatch without supervision**             | CDP close does not exit the server. Xylo must own, stop and reap the child/process tree.                                                                                              |
| Local development websites | Default behavior differs                              | Obscura blocks private networks by default; Xylo currently allows HTTP(S) loopback/LAN. Introduce explicit session-scoped local-network consent/policy, not a blanket global opt-out. |
| Full web compatibility     | Lower than Chromium                                   | Upstream documents incomplete service workers, media, Web APIs and long-tail CSS; real workflows need qualification.                                                                  |
| Resource use               | Potential benefit, unmeasured here                    | Measure complete process-tree RSS/CPU/startup/latency on representative workflows, not README estimates.                                                                              |
| Untrusted web execution    | Security qualification required                       | Process separation is crash isolation, not an OS sandbox. Audit filesystem/network exposure and establish confinement before claiming Chrome-equivalent security.                     |

### Platform matrix

The v0.2.6 workflow builds Linux x64/ARM64, macOS Intel/Apple Silicon and Windows x64. It does **not** build Windows ARM64, which is a Xylo target. ARM emulation is not native qualification. Initially report managed-browser unavailability on Windows ARM64, or qualify a native source build before claiming support; do not fall back to Edge.

Linux release documentation specifies glibc 2.35+. Check Xylo's actual supported distribution baseline. macOS minimum deployment target, signing/notarization, Windows runtime requirements and fonts still need packaged testing. Upstream uses native Intel macOS builds because V8 startup snapshots are architecture-specific: avoid unqualified cross-built binaries.

## Proposed architecture

```text
Agent -> engine admission/permissions -> capability helper -> BrowserService
                                                         -> owned Obscura child
                                                         -> authenticated loopback CDP
                                                         -> existing ToolObservation/artifacts
```

Retain chromiumoxide as a **CDP client**, subject to the compatibility spike. It is not itself Chromium and does not require bundling Chrome. If its initialization assumptions prevent compatibility, isolate a small CDP adapter behind BrowserService; do not simultaneously build two production browser backends. Remove chromiumoxide only if that decision is supported by tests.

Do not expose Obscura's MCP server directly as the default integration. It has overlapping tool names but different snapshots/results, extra JavaScript evaluation capabilities and no equivalent listed `browser_download` tool. The native adapter preserves Xylo's existing permissions, cancellation and artifact contracts without adding a second public browser tool family.

### Child lifecycle and security

- Resolve only a verified application-owned Obscura asset; do not trust PATH or CDP's Chrome-identifying version response.
- Spawn through Xylo's controlled process infrastructure with a clean environment, no provider secrets, bounded diagnostic output and process-tree ownership (including Windows jobs).
- Run a single worker/session initially; no worker pool or external-server attachment.
- Bind explicitly to `127.0.0.1`, request a dynamically allocated port and use the ready-file mechanism. Prove port-zero behavior and ready-file parsing in the spike; never discover a random pre-existing port-9222 server.
- Generate a random per-session bearer token, pass it through the private child environment and configure authenticated discovery/WebSocket requests. Verify chromiumoxide's header support; patch/replace the connection adapter if necessary. Do not put tokens in logged URLs.
- Keep file access disabled. Start with private-network blocking; grant local access only under the explicit policy described below. Do not inherit arbitrary `OBSCURA_*` variables.
- Preserve disposable storage, idle shutdown and operation deadlines. Remove temporary state on launch failure, normal close, timeout, cancellation and helper termination.
- Own server termination independently of CDP. Killing only a client connection is not cleanup. Never replay a timed-out click, submit or download.
- Assess and implement OS-level restrictions appropriate to arbitrary web content; fail closed where the required confinement cannot be provided. Do not equate the separate process or V8 watchdog with a renderer sandbox.

### Download decision: mandatory gate

Preserve the existing authenticated `browser_download` contract and its 50/100 MiB limits. Do not quietly replace it with a separate unauthenticated reqwest request or copy browser cookies into another transport.

Preferred path: implement/contribute bounded authenticated download support upstream, or carry a minimal reviewed pinned patch. Require no unsolicited filesystem writes, cancellation, unknown-length limits, partial-file cleanup, safe filenames, same-session authentication and correct redirects/source URL reporting.

Evaluate Fetch/IO as an alternative only if it proves those properties end-to-end. Upstream describes IO reads over cached response bodies; that is **not proof of bounded streaming from the network**, and increasing a body buffer to 100 MiB is not an early transfer budget. Host-side counters after full buffering are insufficient.

During a prototype, an explicit unsupported-download error is acceptable. For the full migration it is a release blocker unless the user explicitly accepts a documented feature removal. Never return success from no-op CDP methods.

## Implementation sequence

### 1. Compatibility spike (no removal yet)

- Pin v0.2.6 as the baseline; if fixes require another revision, review and record that exact revision and hashes.
- Build a focused adapter experiment in the capability layer: controlled `serve` child, authenticated CDP connection, ready-file handling and reliable termination.
- Run all eight existing browser contracts against local fixtures plus representative public sites. Local fixtures must use an explicitly authorized local-network test configuration.
- Record actual CDP requests/responses, especially typed AX fields, input events, download behavior and close semantics; redact credentials.
- Produce a support matrix and measured performance baseline. Stop the sole-backend cutover if download or confinement gates cannot be met.

### 2. Managed runtime provisioning

- Add `obscura-runtime` to `crates/capabilities/src/assets/catalog.json`, with reviewed license, exact release URLs, per-platform SHA-256/size and render-enabled artifacts. Never use `/latest/download` in the catalog.
- Add explicit activation in `assets/activation.rs` and active-state mapping in `assets/mod.rs` (unknown IDs currently use a model-oriented default mapping).
- Resolve the runtime from verified asset metadata rather than an arbitrary imported executable path. Use a host-owned runtime selection; persisted legacy settings cannot redirect it.
- Reuse explicit asset installation/activation. "Default backend" must not introduce an unannounced network download during service construction. Offer a clear Install Obscura action when missing, with no Chrome fallback.
- Keep both archive executables under managed ownership; the worker is needed for parallel modes, not necessarily single-worker `serve`. Verify exported files and packaged resolution rather than assuming either layout.
- Add Downloads/settings status, missing/unsupported/corrupt-runtime messages, offline behavior and repair coverage.

### 3. BrowserService implementation

- Replace Chrome launch/session ownership with the supervised Obscura adapter; retain existing public tool names, schemas and observation/artifact shapes.
- Preserve URL credential rejection, node/output limits, 100,000-byte typing limit, viewport-only PNG, idempotent close and no replay after uncertain side effects.
- Implement the qualified download design. Retain browser-wide no-unsolicited-write guarantees through actual enforcement, not CDP acknowledgements.
- Align engine timeouts with Obscura navigation/script/command budgets without disabling watchdogs.
- Add an explicit application-controlled local-network policy. Warn that upstream's broad allow switch includes link-local/cloud metadata and applies to subresources too; do not imply it is a single-host grant. Prefer narrower enforcement if local access is required.
- Preserve web-content provenance and engine admission; do not automatically expose `browser_evaluate` or the rest of upstream MCP tools.

### 4. Configuration and engine migration

- Replace `CapabilityConfig.chromium` with the host-resolved managed runtime contract. Migrate old `capabilities.json` deliberately, preserving unrelated settings. Never reinterpret an old Chrome path as an Obscura path or delete a valid settings file because one field became obsolete.
- Update helper config patch validation/control behavior and configuration replacement tests. Reject obsolete executable-selection patches with an actionable migration message.
- Update `initial_definitions` to use managed-browser configuration/readiness while preserving stable tool advertisement and small/tiny-window discovery behavior.
- Keep helper completion, idle release and configuration-change shutdown paths working.

### 5. Remove obsolete backend code after gates pass

Remove:

- `detect_browser()` and all managed Chrome/Chromium/Edge executable/PATH candidates.
- Chrome-specific `BrowserConfig` launch flags, obsolete session fields and `chromium` configuration consumers.
- The old Chromium-only download implementation only when its qualified replacement is in place; retain generic validation/budget helpers that still apply.
- `XYLO_CHROMIUM` acceptance-test setup, obsolete browser-specific instructions/status strings and managed-Chromium prompt wording.
- chromiumoxide and unused transitive lockfile entries **only if** the final adapter no longer uses that CDP client; regenerate the lockfile using Cargo rather than hand-editing dependency subtrees.

Keep:

- `desktop_*` tools and guidance for controlling the user's existing browser. These also control non-browser applications.
- Sensitive-path protections for Chrome/Edge/Firefox personal profiles.
- `web_fetch`/`web_search`: complementary HTTP/search tools, not alternative managed browser engines.
- PDFium and `pdfium-render`; `chromium/8111` in a PDFium asset URL is not a browser backend.
- Shared `futures-util`, `url`, process, archive, networking and async dependencies while other modules use them.
- Xylo's sandbox-helper Node runtime, which supports executable tools and is unrelated to browser automation.
- User-installed browser applications, personal profiles and user-configured extensions/MCP connections. Do not uninstall or delete them.

### 6. Documentation, packaging and release

- Update `docs/modules/capabilities.md`, `docs/modules/engine-tools.md`, `docs/modules/helper.md`, tool descriptions and `crates/engine/prompts/system.md`. State independent-engine limitations and the difference from personal browsers.
- Review `packager.json`, release scripts and NOTICE/LICENSE handling. With on-demand assets, avoid unnecessarily bundling a second copy; if bundled, qualify nested signing and runtime/resource lookup.
- Gate release per supported OS/architecture. Ship no automatic fallback. Missing or unsupported runtime must report a useful error, not silently switch engines.
- Maintain the option to revert the Xylo release/runtime pin if qualification regresses; this is release rollback, not a permanent dual-browser architecture.

## Validation and acceptance gates

Add deterministic tests in `crates/capabilities/src/browser/tests.rs` and focused adapter tests. Current real-browser coverage is ignored and requires `XYLO_CHROMIUM`; normal workspace tests alone do not qualify a replacement.

Required coverage:

1. Verified runtime selection, missing/corrupt asset, activation, unsupported platform, old-config migration, and rejection of system-browser fallback.
2. Authenticated endpoint discovery, wrong/missing token denial, unique sessions, failed startup cleanup, no secret inheritance and confinement failures.
3. Navigate/title/URL, AX schema and truncation, click, append typing, selector waits, 1280x900 screenshot and no-clobber artifact publication.
4. Login cookies and authenticated downloads; redirects; unknown length; declared and actual oversize; cancellation; safe staging/filenames; no file writes from ordinary links/navigation/scripts; subsequent actions after a download.
5. Explicit close, repeated close without launch, idle expiry, hung scripts, timeout/cancellation, helper exit and process-tree reaping with storage cleanup. No action replay.
6. Private-network denial and authorized-local workflows, including redirects/subresources; file URLs and URL credentials remain refused.
7. Engine permission modes, initial advertisement/discovery, untrusted web provenance, helper configuration locking and native UI title/artifact rendering.
8. Representative static pages, forms, React/Vue-style applications, iframes/shadow content and expected failure reporting for unsupported web APIs. Do not promise arbitrary website compatibility from a fixture pass.
9. Packaged smoke tests on Linux x64/ARM64, macOS Intel/Apple Silicon and Windows x64; explicit Windows ARM64 status until separately qualified.
10. Compare startup, idle RSS, peak process-tree RSS, CPU, screenshot quality and workflow latency against the current browser. Publish measurements, not an assumed 30 MB target.

Suggested narrow-to-broad checks during implementation:

```text
cargo test -p xylo-capabilities browser --locked
cargo test -p xylo-engine --locked
cargo test -p xylo-helper --locked
cargo check -p xylo --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Add a dedicated non-ignored Obscura acceptance target/job using the pinned runtime; exact invocation is to be defined with that test target. Ordinary tests and static checks must not be presented as full real-browser qualification.

## Investigation limits

No implementation or dependency changes were made for this assessment. No Obscura binary was downloaded or executed, and no build, test, benchmark or security qualification ran. Shell commands are unavailable in this Windows environment; Git status failed with OS error 1314. GitHub's repository API returned 403, but raw source and the release page were accessible. Artifact hashes, signing status and exact runtime minimum versions remain implementation-time verification tasks.

Final decision: **proceed with a gated sidecar integration spike; do not immediately remove the existing browser or claim full compatibility.** Full cutover requires bounded authenticated downloads, reliable owned-process shutdown, security/confinement review and platform acceptance. After those pass, Obscura can be the sole managed backend with no Chrome/Edge fallback.
