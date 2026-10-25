# Xylo

A native desktop workspace for conversations, coding, tools, memory, and local or hosted models. The interface uses GPUI Kit 0.8.0; the application host and execution services use Rust. Built-in execution does not start a Python or Node runtime.

**Status: development preview.** The macOS interface and native service tests run locally. This is not a qualified production release: cross-platform native acceptance, packaged model assets, provider accounts, installation, signed updates, and the performance targets still require verification.

![Xylo](packaging/branding/xylo-lockup.png)

## Run the preview

Install the Rust toolchain selected by `rust-toolchain.toml` and the native compiler tools for your operating system. On macOS, install the Xcode command line tools and use a logged-in graphical session.

```sh
cargo build --locked --workspace
cargo run --locked -p agent-workbench
```

On macOS, `bash scripts/preview-macos.sh` creates a complete development app bundle, including its task and capability processes, under `target/debug/Xylo.app`. Its local ad hoc signature is for development; it is not a notarized release. Quit an older preview before opening the refreshed bundle.

Xylo uses `com.qurkos.xylo` for production bundles and `com.qurkos.xylo.preview` for development previews. Native executable names remain `agent-workbench`, `workbench-worker` and `workbench-helper`.

Build the entire workspace first: the desktop executable starts its `workbench-worker` and `workbench-helper` siblings on demand. Launching the interface alone before building those components will leave execution unavailable.

Open a project, go to **Settings → Model**, choose a provider and model, and save it. API keys are stored using the operating system credential store. Existing Ollama, LM Studio, vLLM, llama.cpp and compatible servers can be connected by their API endpoint. No provider is selected as a paid fallback.

Resource scheduling uses current available memory, CPU activity, and task demand. There are no 8/16/32 GB profiles. Model selection stays explicit; the app does not automatically choose the largest model. Native browser, model, document, speech and desktop services start when requested. Some capabilities require separately provisioned binaries, model files or operating system permission; see [native assets](docs/capabilities.md).

## Workspace structure

| Crate | Responsibility |
| --- | --- |
| `desktop` | Native product views, navigation, editor, terminal and tool approvals. |
| `core` | Application-owned contracts and versioned worker messages. |
| `runtime` | Nonblocking UI-to-host commands, supervision, resource scheduling and process execution. |
| `resources` | Automatic local and shared process admission, with reservations retained through native cleanup. |
| `worker` | Isolated Hermes task process and private-pipe protocol. |
| `engine` | Hermes adapter, provider routing, tool policy and durable agent execution. |
| `services` | SQLite journal, memory, skills, credentials and connectors. |
| `helper-client` | Small client for the native capability process. |
| `helper` | Persistent native capability process and configuration control. |
| `capabilities` | Browser, documents, local inference, speech, computer use, plugins and remote execution. |
| `scheduler` | Durable schedules with cross-process claims and leases. |
| `distribution` | Authenticated updates, staged artifacts and rollback protection. |
| `notifications` | Opt-in native notification adapters, durable dedupe and validated task activation. |

Hermes Ultra is vendored at `c002f8c4efe475c89c9f14d3995d44e6a5df5b27`. Its types remain inside the engine adapter. Local changes are documented in [the patch queue](vendor/hermes-ultra/patches/README.md).

## Design and operation

The UI sends bounded commands to a background host. Database, parsing, networking and tool execution do not run on the UI event thread. Each active root task uses a supervised worker. Large capability dependencies stay in the helper graph instead of the UI graph.

The journal records tool intent, approval, execution and results. Known completed actions are replayed from their receipt, and uncertain external writes require reconciliation. A final response and its completion receipt are committed together. Closing a window preserves background work; explicit Quit requests checkpointing and waits for supervised processes to stop.

Memory keeps execution checkpoints, searchable history, scoped facts and versioned skills separate. Lexical search works without model assets. Semantic search loads explicitly configured local embedding assets on first use; startup never downloads an embedding model. See [services](docs/services.md).

## Validate changes

```sh
cargo fmt --all -- --check
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --no-deps -- -D warnings
```

Native GPUI rendering/input tests use the isolated manifest in `crates/desktop/validation` because GPUI's test-support dependency pins differ from its production graph. Run those in an actual native graphical session:

```sh
CARGO_TARGET_DIR="$PWD/target" cargo test --locked --manifest-path crates/desktop/validation/Cargo.toml
```

The real Chromium smoke test is separate from offline tests and requires an installed browser. Tests with HTTP fixtures establish protocol and tool behavior, not real provider quality or model speed. See [verification](docs/verification.md) for evidence and release gates.

## Further documentation

- [Xylo identity, artwork and existing-profile compatibility](docs/branding.md)
- [Architecture and invariants](docs/architecture.md)
- [Storage, memory, credentials and connectors](docs/services.md)
- [Native service routes, pagination and failure behavior](docs/native-connectors.md)
- [Interrupted actions, retained evidence and outcome review](docs/action-review.md)
- [Native action review and saved notes](docs/action-review-ui.md)
- [Evidence-based procedural learning](docs/learning.md)
- [Conversation search and message navigation](docs/history-search.md)
- [Durable composer drafts](docs/drafts.md)
- [Acknowledged guidance during running tasks](docs/steering.md)
- [Project fact management and export](docs/memory-management.md)
- [Native image attachments and vision](docs/vision.md)
- [Image generation and saved request history](docs/image-generation.md)
- [Desktop notifications](docs/notifications.md)
- [Native capabilities and asset formats](docs/capabilities.md)
- [Native document editing and copy recovery](docs/native-documents.md)
- [Installed plugins and permission review](docs/native-plugins.md)
- [Rust plugin SDK and compiled text-report example](sdk/plugins/README.md)
- [Native execution environments](docs/native-environments.md)
- [Reviewed native asset installation](docs/assets.md)
- [Helper API and lifecycle](docs/helpers.md)
- [Native language servers](docs/language-servers.md)
- [Provider protocols and ChatGPT accounts](docs/providers.md)
- [Repeatable overhead measurements](docs/performance.md)
- [Full product completion audit](docs/completion-audit.md)
- [Verification and performance acceptance](docs/verification.md)
- [Native distribution and authenticated updates](docs/distribution.md)
- [First native preview release](docs/preview-release.md)
- [Verified dependency license supplements](docs/license-supplements.md)
- [Release checklist](docs/release-checklist.md)

## License

Application-owned code is Apache-2.0. Vendored dependencies retain their original licenses and notices. See [LICENSE](LICENSE), [NOTICE](NOTICE) and the notices under `vendor/`.
