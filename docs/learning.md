# Evidence-grounded procedural learning

Procedural learning is a local, background pipeline: durable action/result evidence → typed candidate procedure → historical evaluation → versioned promotion → relevant retrieval → measured matching reuse. It stores reusable guidance; it does not train model weights, execute learned steps, or grant permissions.

## Lifecycle

`Store::complete_turn` commits a learning job in the same transaction as its completion receipt, checkpoint and assistant history. Replaying a completed turn does not create another job or skill version. An engine cleanup guard queues failed/cancelled terminal evidence on error returns and dropped futures. A process kill is covered by `recover_interrupted` / `recover_session_interrupted`: their recovery transaction queues unfinished affected turn IDs before the host admits a retry. Session recovery does not change other live sessions.

The host runs one `LearningService::run(cancel)` maintenance task. Jobs are durably claimed with a 60-second lease, so another cooperating process can recover a dead owner's work. A job commits its case, candidates, evaluation, promotions/reuse results and completed job state atomically. Failed processing is visible and can be retried through `queue_turn`. Cancelled maintenance leaves unclaimed jobs available for a later run. Startup backfill scans old completion receipts in bounded pages; it never loads the whole conversation database into memory.

Foreground work adds only the durable completion-job insert and bounded lookup of promoted procedures. Candidate extraction and evaluation run on a blocking maintenance worker. No provider API request, embedding model, shell command or tool execution occurs during learning. The service admits at most four jobs per maintenance pass; each job considers at most 64 action records, 16 procedural steps, 16 matching candidate versions and 16 recent evaluation cases. An indexed negative-evidence check retains failures outside that recent positive-case window. Job I/O budgets reject oversized evidence rather than reading unbounded tool results.

## What becomes a candidate

An automatic candidate needs a persisted user turn, a completion receipt and actual successful journaled action results. A final answer claiming success is insufficient. A useful multi-step workflow, or a recognized verification command, creates a candidate. Each source retains session/turn IDs, action IDs, result SHA-256 digests and completion append order. External result text is not copied into instructions.

Steps preserve typed tool calls and safe bounded arguments, including workspace-relative file paths. Unknown values, account IDs, credential fields, outside/private absolute paths and unsupported content become explicit task-input slots. Relative paths cannot traverse parents; hidden credential paths are omitted. Conservative sensitive-content checks reject correction text containing recognized credentials, account identities or private identity paths. This is deliberately narrow extraction, not a claim that arbitrary text can be perfectly classified or sanitized.

External writes and arbitrary commands cannot automatically qualify from success alone. Known `cargo test`, `cargo check`, `cargo clippy` and `cargo fmt --check` argument forms can qualify as verification evidence after independent successful runs. These remain approval-requiring execution: Cargo build scripts/tests can execute project code, and successful historical evidence is not an OS sandbox or preauthorization. Shell scripts, `sh -c`, arbitrary programs, unrecognized flags and file/service mutations are excluded from automatic promotion.

Two different completed turns with the exact scoped typed procedure are required. At least one must be later than the candidate's source receipt. Failed, denied, uncertain, missing, oversized or unsupported evidence cannot supply a passing case. A later matching failure/uncertain result suspends a promoted version and removes it from retrieval. This is conservative: an unrelated transient failure can require review/reconciliation before reuse is enabled again.

## Corrections and review

An explicit product action calls `record_scoped_correction(scope, &CorrectionInput)` with a persisted **user** message ID and a verbatim guidance excerpt. The source conversation must belong to that scope before any candidate or suspension is written. Assistant messages, provider/tool content and unquoted guidance are rejected. An optional `replaces` reference creates a new immutable skill version and suspends the former active version while the correction is evaluated.

A correction containing an explicit supported command, for example `Use ` followed by a backtick-quoted `cargo test --offline`, binds only to matching typed command arguments. It requires two successful later completed cases. A different `cargo check` result cannot verify a `cargo test` correction.

General prose is not mechanically proven by observing a matching tool sequence. Such a correction remains a candidate until the user explicitly reviews the guidance and its referenced successful cases through `review(scope, name, version, evidence)`. Review cannot replace missing successful evidence or turn uncertain/external-write cases into passing evidence. This review API is exposed only to the host's explicit product action, never to an agent tool.

Evaluation identifies itself as `historical-outcome-template/v1`. It reports the actual cases, independent-source check, counterexamples, correction check and current durable action states. Its score is a transparent pass/fail value. It measures outcome/template consistency; it does not prove model-quality improvement, causal learning benefit or benchmark accuracy. Task-quality evaluation on a held-out external task set remains separate work.

Promotion appends to the existing skill promotion history. Corrections create later numbered versions; rollback appends a promotion of an earlier version only after rechecking its current source evidence. Failed evidence still blocks rollback. No version or provenance is deleted to make a score pass.

## Retrieval and actual reuse

The engine queries promoted skills by the exact workspace scope and relevant terms through FTS5. It injects at most two references within a 2,400-byte advisory context budget. Long procedures retain their complete stored steps and can be read through `skill_read`; the initial context tells the agent when further steps need reading. Suspended, superseded, unevaluated, legacy ungrounded or other-workspace skills are excluded.

Retrieved guidance is explicitly subordinate to current task inputs, result verification and ToolBroker permissions. The engine does not interpret a skill document as executable code. A retrieved Cargo verification step still triggers the current write approval and is rejected in read-only mode. `skill_list` and `skill_read` are scoped. Agent `skill_propose` now queues a real completed turn for background evaluation; arbitrary model-authored instructions do not enter the promotion pipeline.

Retrieval and matching reuse are stored separately. A retrieved skill initially records `retrieved`; after a completed matching procedure with verified results it records `verified_reuse`. Nonmatching behavior records `retrieved_not_followed`; failed/uncertain matching evidence records `failed_or_unknown`. Matching reuse means guidance was available and the observed procedure matched. It does not claim the guidance caused a better outcome or saved tokens.

## Host API

All synchronous methods use the bounded dedicated SQLite executor and must be called off the UI thread. Construction is cheap and does not start maintenance automatically.

```rust
let learning = LearningService::new(store.clone());
tokio::spawn({
    let learning = learning.clone();
    let cancel = shutdown.child_token();
    async move { learning.run(cancel).await }
});
```

- `process_pending(limit, &cancel) -> LearningBatch` is an explicit bounded asynchronous drain for tests or maintenance.
- `queue_terminal_turn(session, turn)` idempotently records a terminal hook. `queue_turn(session, turn)` additionally retries already processed evidence after explicit reconciliation. Call only after the turn is terminal or the worker is confirmed reaped. Successful completion queues itself.
- `status(scope) -> LearningStatus` reports pending/failed jobs, candidate/promoted counts and matching reuse. Queue counts are host-wide; skill/reuse counts are scoped.
- `skills(scope, limit)`, `versions(scope, name, limit)`, `active_skill(scope, name)` and `reuses(scope, limit)` expose bounded review/provenance data. List limits clamp to 1..200.
- `record_scoped_correction(scope, &CorrectionInput { user_message_id, guidance, expected_tools, replaces }) -> LearnedSkill` validates source scope before recording a sourced candidate. The internal `record_correction(input)` variant derives scope directly from the persisted source. `expected_tools` and `replaces` are optional/defaulted.
- `review(scope, name, version, evidence)` and `rollback(scope, name, version)` are explicit user product actions and return the resulting version/evaluation.
- `relevant_skills(scope, query, limit)` is bounded to four. The engine uses two with the smaller context budget. `record_retrieval` verifies scope/current promotion before recording the reference.

`LearnedSkill` includes the existing `SkillVersion`, scope/title/state, triggers, typed `ProcedureStep` values, provenance, correction source/constraint/review and `LearningEvaluation`. Each step marks `requires_approval`; this is informational and cannot override actual broker classification. The schema-3 migration is additive and preserves old conversations, receipts, action journals and skill history. Existing raw Store skill primitives remain storage tools; they do not bypass the scoped learning retrieval checks.

## Validation

Integration tests exercise independent-source promotion, typed Cargo arguments, relevant retrieval and recorded matching reuse; rejection of arbitrary commands/external writes/forged verifier arguments; explicit user source binding; wrong-command correction rejection; human-reviewed correction versions and rollback; secret/private-path redaction; durable lease recovery; idempotent receipt processing; reaped-session isolation and automatic counterevidence queueing; startup recovery after a crash before enqueue; cancellation; and actual ToolBroker rejection of a retrieved verification command in read-only mode.

These are local deterministic application tests. Live model task quality, long-term workflow benefit and external benchmark performance are not inferred from them.
