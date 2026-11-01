# Evaluation harness (`xylo-evals`)

## Purpose and boundaries

`xylo-evals` runs tasks through the real [engine](engine.md) (tool broker, action journal, checkpoints, context guard) with the model replaced by a scripted one. Each trial yields a replayable trace, task checks, harness invariants and metrics; a `Report` aggregates trials into pass^k, tokens per success, cache-hit rate, loop-detector firings and compactions.

Dependencies (`crates/evals/Cargo.toml`): `xylo-core`, `xylo-engine` (feature `test-support`), `xylo-services`, plus `tokio`, `serde`, `regex`, `sha2`, `tempfile` and similar. The crate ships the `xylo-evals` binary (`src/main.rs`).

Guarantees stated in `src/lib.rs`:

- **Default runs are offline and deterministic**: scripted replies, scripted token counts, a fresh temporary profile and workspace per trial, no network.
- **Live runs require `XYLO_EVALS_LIVE=1`** (`matrix::LIVE_ENV`) and a matrix file.
- Nothing here changes engine behavior; the engine sees the recorder only as a provider layer beneath its own guards.
- A check the engine cannot support yet is reported as **skipped, never as passed**.

## Module map

| Path under `crates/evals/src` | Purpose |
| --- | --- |
| `lib.rs` | Re-exports, `RunOptions`, `run_suite` |
| `main.rs` | CLI: `list`, `run`, `replay` |
| `task.rs` | `Task`, `ApprovalPolicy`, `Check`, `CheckStatus`; placeholders `{workspace}`, `{outside}`, `{profile}` |
| `scripted.rs` | `Script`, `Step`, `ChildScript`: deterministic model |
| `recorder.rs` | `RecordingProvider` and `ExchangeLog` |
| `runner.rs` | `ModelSource`, `Trial`, `TrialResult`, `run_trial`, `CANCEL_GRACE` |
| `trace.rs` | `Trace`, `TRACE_VERSION`, `sanitized`, request/reply/error records |
| `replay.rs` | `script(trace)`, `compare` (`replay_matches_trace`) |
| `invariants.rs` | `check(trial)`: harness invariants |
| `metrics.rs` | `TrialMetrics`, `TaskSummary`, `Report`, `pass_hat_k`, `pass_at_k` |
| `matrix.rs` | `LiveModel`, `load`, `LIVE_ENV` |
| `suite.rs`, `suite/project_discovery.rs` | `builtin()`, `find(id)` and task constructors |
| `web_fixture.rs` | Loopback HTTP page server used only by offline tasks |

## Scripted provider (`scripted.rs`)

- `Script { steps, children, summaries, titles, served_window }`. Root agent-loop requests consume `steps` in order; summary and title requests have their own queues, so a script does not depend on when compaction happens. `DEFAULT_SUMMARY` and `DEFAULT_TITLE` are returned once exhausted.
- `Step` is `Reply(ModelReply)` or `Fail(RecordedError)`.
- `ChildScript { task, steps }` routes a delegated child by its task message (an exact match beats a partial one) with its own cursor, so parallel children get the same replies in any order.
- **The provider never touches the network and never sleeps.** A request beyond the last step fails with a permanent error naming the step, so a looping harness fails loudly instead of hanging.
- With `served_window` set, an agent request estimated above it fails with `ContextOverflow` **without consuming a step**.
- Token usage is estimated from request and reply sizes (about 4 bytes per token), so offline costs are stable but not prices.

## Tasks and the built-in suite

A `Task` has `id`, `summary`, `prompt`, `mode`, `files`, `outside_files`, `context_tokens` (default 32 768), `output_tokens` (2 048), `script`, `approvals`, `live`, optional `web_fixture` and a `grade: fn(&Trial) -> Vec<Check>`. `ApprovalPolicy` is `AllowAll`, `DenyAll` or `AllowOnly(&[tool])`.

`suite::builtin()` returns 17 tasks: `tool_loop`, `plan_interaction`, project-discovery scoped instructions, `code_tool_safety`, `shell_working_directory`, `web_fetch`, `worktree_approval`, `length_continuation`, `truncated_call`, `empty_answer`, `context_overflow`, `tiny_window`, `repeated_call`, `delegation`, `prompt_injection`, `memory_from_user` and `memory_from_tool_output`. Use `cargo run -p xylo-evals -- list` for ids (not run here) and for which are `offline+live`. Tasks injecting provider faults are offline only.

To add a task: write a constructor from `task(id, summary, prompt, grade)` in `suite.rs`, script the model (`ModelReply::text`, `ModelReply::calls`, `.truncated()`, `Step::Fail`, `Script::served_window`), grade with `Check::expect` or `Check::skip`, set `live = true` when meaningful for real models, and add it to `builtin()`.

## Traces, invariants and metrics

### Traces (`trace.rs`)

`TRACE_VERSION = 2`; **`Trace::load` rejects any other format version**. A trace holds the sanitized provider configuration, the scripted served window, every model request with its reply or error, every engine event, the journal records of the turn and the final checkpoint. `sanitized()` **removes `credential_ref` and all headers**. Paths are stored as `{workspace}`, `{outside}` and `{profile}`, so a trace replays anywhere. `RecordingProvider` sits beneath the engine's context guard, so it records exactly what a provider receives after fitting and compaction.

### Replay (`replay.rs`)

`xylo-evals replay TRACE.json` rebuilds the task under the recorded provider configuration, answers with the recorded replies (each child's by its task), **really runs the tools again** and grades the result. It adds the invariant `replay_matches_trace`, naming the first request whose messages differ; system prompts are ignored and UUIDs and timestamps are compared by pattern. The task is found by `suite::find(trace.task)`, so replay works only for built-in task ids.

### Invariants (`invariants.rs`)

Checked on every trial: one terminal event; paired tool start and finish events; every tool call answered exactly once per request; byte-identical system prompt and tool block across the requests of each conversation (agent and each child); fitted requests leaving the output reserve free; turn usage equal to the sum of request usage; a settled action journal; denied actions never running; a cancelled turn closing its stream within `CANCEL_GRACE` (10 s), otherwise the trial is abandoned as hung. **A trial passes only if all task checks pass and every invariant holds**; a task-passing trial with a broken invariant counts as `viol`.

### Metrics (`metrics.rs`)

`TrialMetrics` per trial; `TaskSummary` and `Report` per run. `pass_hat_k` is the unbiased C(c,k)/C(n,k) estimate (pass^k) and `pass_at_k` the pass@k estimate; both return `None` when undefined. Table columns: `pass`, `pass^3`, `tok/succ` (failed-trial tokens included), `cache-hit` ("not reported" differs from 0%), `loops` (`stall_notice`/`stall_warning` notes), `compact`, `viol`. `--json` adds per-trial checks, invariants, metrics and skipped/failed check counts.

## Running suites

Commands (**not run while writing this page**):

```sh
cargo run -p xylo-evals -- list
cargo run -p xylo-evals -- run --suite builtin --trials 5 --json out.json
cargo run -p xylo-evals -- run --task tool-loop --traces traces/
cargo run -p xylo-evals -- replay traces/scripted/tool-loop-1.json
```

Options: `--suite` (only `builtin` exists), repeatable `--task ID` (unknown ids are errors), `--trials N` (default 5, at least 1), `--json PATH`, `--traces DIR` (writes `DIR/<label>/<task>-<trial>.json`), `--timeout SECONDS` (per-trial deadline, default 300), `--matrix FILE`.

Exit codes: **0** all trials passed, **1** any failed, **2** usage or setup error.

Library entry: `run_suite(suite, tasks, sources, &RunOptions { trials, deadline, traces })`. `run_trial` creates a temporary profile and workspace per trial.

## Opt-in real providers (`matrix.rs`)

`--matrix models.json` plus `XYLO_EVALS_LIVE=1`; without the variable `matrix::load` fails with an opt-in error. The file is a non-empty JSON array of `{label, provider, credential_env?}` (`deny_unknown_fields`); `provider` is a `ProviderConfig` and must pass `validate()`. The secret is read from the environment variable named by `credential_env` and a missing variable is an error. `LiveModel`'s `Debug` prints `[REDACTED]`. **Credentials are never written to traces or reports.** Live sources run only tasks marked `live`; live runs cost money and send fixtures to the provider. See [engine-providers.md](engine-providers.md).

## Security

- Offline web tasks use an exact loopback fixture server (`web_fixture.rs`, bound to `127.0.0.1:0`), not a production exemption.
- Traces drop credential references and headers; the harness uses fresh temp directories per trial.

## Testing

`cargo test -p xylo-evals` runs the built-in suite offline (unit tests in `*/tests.rs`). Not run here.

## Related docs

[engine.md](engine.md), [engine-tools.md](engine-tools.md), [core.md](core.md), [services.md](services.md), `crates/evals/README.md`.
