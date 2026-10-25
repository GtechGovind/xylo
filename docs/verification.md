# Verification

This document separates implemented source, local validation and release acceptance. A passing component test does not establish model quality, account authentication, device permission, operating system support or production performance.

## Recorded local evidence

The current development session used macOS ARM64 with Rust 1.98. Tests have exercised:

- Real task-worker JSONL pipes and OpenAI-compatible streaming against a deterministic local HTTP server, including a file tool and a denied write.
- SQLite recovery, action/turn receipts, history deduplication, scoped retrieval, skill rollback and metadata-only embedding configuration.
- Engine checkpoint failures, child scope, permission handling, context limits and natural-answer finalization.
- Native PTY output, resizing, Unicode, scrollback, search and bracketed paste handling.
- Revisioned text/image drafts, admission and cancellation at a capacity-one worker, restart restoration, and acknowledged Quit/update flush.
- Managed native image import including clipboard pixels, all seven streamed image API families, and a real worker restart with reference-only image history/checkpoints.
- Explicit image generation with real local HTTP fixtures, approval and provider snapshot checks, durable uncertainty, recent-request recovery and native draft attachment.
- Durable guidance with exact draft clearing, newer typing and external-writer conflicts, actual worker interruption and three resumptions under the default 8K context.
- Scoped fact versions, revision-checked connected deletion and vector/index rollback, cancellable export, durable unsaved forms, exact-input restart recovery and native project-switch/save races.
- Native document creation, imported text and DOCX paragraph copies, typed multi-sheet spreadsheets, protected Office structures, dense-cell fallback, independent copy-status reconciliation, document draft conflicts and acknowledged Quit/project switching through real Host/helper fixtures.
- Opt-in native notification settings, durable dedupe and cancellation using a fake adapter; no system notification or permission dialog was triggered.
- Real helper process request correlation, configuration persistence, cancellation, bounded framing and DOCX export/readback.
- Cross-process scheduler claims and concurrent updates.
- A separate actual Chromium CDP navigation, interaction, screenshot and download smoke run.
- GPUI native rendering and interaction tests, plus a live sidebar alignment check.

Individual agent results are recorded in the service/helper documentation. Rerun the full workspace checks after integration; old component counts are not evidence for a later modified tree.

## Performance acceptance

The following are product acceptance targets. Component measurements are recorded separately in [performance](performance.md); they do not establish full application acceptance.

| Measurement | Target |
| --- | --- |
| Cold launch to usable interface | p95 < 2 seconds |
| Input and navigation | p95 < 100 ms |
| Scrolling and streaming | 60 Hz, < 1% dropped frames |
| Empty interface and one warm task worker | < 250 MiB attributable RAM |
| Warm retrieval across 10,000 memories including embedding | p95 < 100 ms |
| Internal tool dispatch | p95 < 25 ms |
| Cancellation acknowledgement | < 100 ms |

Use a release build and a deterministic provider to measure application overhead independently of model/network latency. Record hardware, OS, commit, dependency lock, model revision, context length, budgets and raw samples. Report cold/warm behavior and p50/p95, not the best observation. Count children, browser processes, weights and KV cache in active-memory reports.

The 2026-10-25 release fixture measured 27.192 ms p95 for warm hybrid retrieval over 10,000 memories including BGE query embedding. A separate 100,000-record lexical run measured 113.738 ms p95. Worker control acknowledgement and offscreen native CPU/GPU rendering were also recorded. These uncontrolled development baselines identify their source and binaries, retain raw samples and separate index preparation. They exclude full UI/host routing, display/input timing, complete process-tree memory and live task quality.

Repeat workloads on 8/16/32 GB machines as test hardware, not user-selected application profiles. Include long history, 100,000 memories, large terminal output, concurrent tasks and 100 configured connectors. Evaluate coding with fixed SWE-bench Verified fixtures and memory with fixed LongMemEval configurations; neither benchmark has been run for this preview.

## Release gates still open

- Native build, installation, accessibility, IME, scaling and keyboard acceptance on macOS, Windows and Linux.
- Full Vim/tmux terminal compatibility and native editor/LSP acceptance.
- Real cloud provider accounts, eligible official ChatGPT authentication and expired-account recovery.
- Qualified local inference, ONNX embeddings, speech and PDFium assets, including compatible architecture and redistribution notices.
- Browser isolation and platform capture/input permissions, including supported Wayland paths.
- Document editing fidelity, speech/vision/image providers and remote/container environments with real services.
- Real publisher-signed installers/updates and notarization, full license-text inventory, clean-machine provisioning and repeated reproducibility builds. Local authenticated-update fixtures and release build instructions are implemented.
- Full recorded performance and task-quality evaluations.

Until these gates pass, distribute only a development preview and report its limitations explicitly. The local macOS bundle is ad hoc signed; it is not a Developer ID signed or notarized production installer.

## Native plugin and environment integration

The native `management-interactions` target uses the shipped permission-free echo package, real component host imports, the real application Host/helper, and isolated fake SSH/container executables. It covers review/install/enable/run/revoke/artifacts, restoration, delayed save acknowledgements while typing, scope/Quit guards, cancelled operations, authoritative missing-ID fences and explicit observed-result reconciliation. No live service, account, microphone, display permission or container daemon is used.

```sh
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$PWD/target" cargo test --locked \
  --manifest-path crates/desktop/validation/Cargo.toml --test management-interactions
```

The engine recovery fixtures verify the native command count remains one after recovering a lost outer acknowledgement or resolving an interrupted command. Full observations remain in the action journal for failure and uncertainty; the UI receives valid bounded JSON with artifact references. The resource service separately tests OS-lease accounting and ownership through cancelled native blocking work. On macOS, normal-pressure admission uses physical free plus inactive pages without subtracting compressed pages twice; unavailable or elevated-pressure readings retain the conservative fallback. This is an admission estimate, not a guaranteed allocation or measured application footprint. These fixtures do not establish real SSH/container, installer, cross-platform, or full-product performance acceptance.
