# xylo-evals

Evaluation and trace harness for Xylo's agent engine. It runs tasks through
the real engine (tool broker, action journal, checkpoints, context guard) with
the model replaced by a scripted one, records every trial as a replayable
trace, checks harness invariants, and aggregates trials into reliability and
cost metrics.

Default runs are offline and deterministic, so `cargo test -p xylo-evals`
runs the whole built-in suite in CI without network access or credentials.

## Running

```sh
cargo run -p xylo-evals -- list
cargo run -p xylo-evals -- run --suite builtin --trials 5 --json out.json
cargo run -p xylo-evals -- run --task tool-loop --traces traces/
cargo run -p xylo-evals -- replay traces/scripted/tool-loop-1.json
```

The exit code is 0 when every trial passed, 1 when any failed and 2 on a
usage or setup error.

### Real models (opt-in)

Live runs cost money and send the task fixtures to the provider, so they
need both a matrix file and `XYLO_EVALS_LIVE=1`:

```sh
XYLO_EVALS_LIVE=1 OPENAI_API_KEY=... \
  cargo run -p xylo-evals -- run --matrix models.json --trials 5 --json live.json
```

`models.json` lists `{label, provider, credential_env}` entries, where
`provider` is a `ProviderConfig` (see `src/matrix.rs` for an example).
Credentials come only from the named environment variables and are never
written to traces or reports. Live runs execute only the tasks marked
`offline+live` in `list`: the others inject provider faults that only a
scripted model can produce.

## Reading the report

The table has one row per task and model:

| Column | Meaning |
|---|---|
| `pass` | Successful trials out of trials run. A success passes every task check **and** keeps every harness invariant. |
| `pass^3` | The chance that *all* of 3 fresh trials succeed, estimated without bias from c successes in n trials as C(c,3)/C(n,3) (τ-bench's pass^k; k drops to n when fewer trials ran). It falls fast when a task is flaky, which pass@1 hides. The JSON report has pass^k and pass@k for every k up to n. |
| `tok/succ` | All tokens spent on the task, failed trials included, divided by successes. Offline token counts are a 4-bytes-per-token estimate, useful for comparing harness changes, not for prices. |
| `cache-hit` | Cache-read share of prompt tokens. "not reported" means no reply carried a cache-read field, which is different from a 0% hit rate. |
| `loops` | Loop-detector firings: `HarnessNote` events of kind `stall_notice` or `stall_warning`. The JSON also counts all harness notes per trial (`harness_notes`) and turns the detector stopped (`loop_stops`, an `Error` of kind `stalled`). |
| `compact` | Times a turn entered the compacting phase. |
| `viol` | Trials that passed their task checks but broke a harness invariant. These count as failures. |

Failed trials are listed below the table with the failing checks. The JSON
report (`--json`) adds every trial's checks, invariants and metrics, and
per-task counts of skipped and failed checks. A **skipped** check means the
engine does not yet expose what it needs (for example memory records without
an `origin`); skips never pass or fail a trial, but they are counted.

Harness invariants, checked on every trial (`src/invariants.rs`): one
terminal event, paired tool start and finish events, every tool call answered
exactly once in every request, a byte-identical system prompt and tool block
across the requests of each conversation (the agent's and each child's),
fitted requests leaving the output reserve free, turn usage equal to the sum
of request usage, a settled action journal, denied actions never running, and
a cancelled turn closing its event stream within 10 seconds (otherwise the
trial is abandoned as hung).

## Traces and replay

A trace (`src/trace.rs`, format version 2) holds the provider configuration
the trial ran with (credential reference and headers removed), the scripted
served window if any, every model request with its reply or error, every
engine event, the journal records of the turn's actions and the final
checkpoint. Paths are stored as `{workspace}`, `{outside}` and `{profile}`,
so a trace replays anywhere.

`replay` rebuilds the task under the recorded provider configuration,
answers with the recorded replies (each child agent's by its task), really
runs the tools again and grades the result. A recorded served window is
applied again rather than replaying its rejections. The added
`replay_matches_trace` check names the first request whose messages differ
from the recording; system prompts are ignored, and UUIDs and timestamps are
compared by pattern (numbered placeholders in order of first appearance), so
fresh memory ids and dates do not count as divergence. A trace recorded from
a real model replays offline the same way.

## Adding a task

1. Write a constructor in `src/suite.rs` that starts from `task(id, summary,
   prompt, grade)` and sets what the scenario needs: `files` and
   `outside_files` (fixtures; `{workspace}` and `{outside}` resolve in text,
   prompt and script), `mode`, `approvals`, `context_tokens`, and `script`.
2. Script the model with `ModelReply::text`, `ModelReply::calls(step, [...])`,
   `.truncated()` for an output-limit cut, `Step::Fail(...)` for a provider
   error, or `Script::served_window` for a server that rejects prompts above
   a size.
3. Grade with `Check::expect(name, holds, detail)`; use `Check::skip` when the
   engine cannot support the check yet. Graders read the `Trial`: the final
   answer, workspace files, journal actions, memory records and the trace.
4. Set `live = true` if the task is meaningful for a real model, add the
   constructor to `builtin()`, and run `cargo test -p xylo-evals`.

## Delegation

Child agents are recognised by the helper-agent system prompt and scripted
per task (`Script::children`), so parallel children get the same replies in
any order; the prefix invariant checks each child's conversation on its own.
The built-in `delegation` task delegates two children in the same step.
