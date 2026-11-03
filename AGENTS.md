# Repository instructions

These instructions apply throughout the Xylo repository.

## Tool execution and interrupted chats

- Do not introduce durable tool-action journals, persisted action reconciliation state, action review forms, or cross-process execution leases backed by an action journal. This is an explicit product decision.
- Do not block starting or continuing a chat because an earlier action is unfinished or has an unknown outcome. Do not ask users to mark interrupted actions as completed or not completed.
- Keep live action tracking in bounded memory. Worker-to-host activity updates are transient, scoped to the owning chat and turn, and cannot grant permission or authorize execution.
- Report actual failures and interruptions to the model and user. Never invent success or automatically replay an interrupted side effect. Retain in-process duplicate-call protection and normal permission checks.
- Preserve conversations, settings, drafts, chat checkpoints, generated artifacts, and redacted rotating diagnostic logs. These are ordinary application data, not tool-action journals. SQLite transaction durability must remain enabled.
- Preserve the version-2-to-version-3 migration that removes action journals without deleting the user's profile. Never restore the old tables; schema_v2.sql exists only to validate migration input.
- Keep regression coverage for migration preservation, continued chat after interrupted calls, bounded live tracking, no replay, worker/host scoping, and absence of manual action-review UI.

## Browser failures

- Keep Obscura's native OS certificate roots and bundled roots enabled, with certificate and hostname verification intact. Never fix TLS failures by accepting invalid certificates.
- Preserve the pinned source patch and build-provenance verification in crates/build-browser.rs and crates/patches/.
- Distinguish failures before dispatch from operations that may have started. Return a useful tool error without imposing a global recovery gate.

## Verification and documentation

- Update README.md and relevant docs/modules files when behavior changes. Do not describe transient tracking as durable or promise exactly-once execution across restarts.
- Run cargo fmt --all -- --check, relevant regression tests, and strict Clippy for code changes. Use --locked. A fast local workspace check can use --no-default-features; bundled browser behavior needs separate real-browser validation.
- Run native UI checks with cargo test --locked --manifest-path crates/desktop/validation/Cargo.toml and the relevant --test target. Use an explicit --target-dir when sharing a build cache.
- Preserve unrelated user work. Do not weaken sandboxing, permission checks, certificate validation, or diagnostic redaction to make tests pass.
