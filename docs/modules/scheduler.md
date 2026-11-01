# Durable scheduler (`xylo-scheduler`)

## Purpose and boundaries

`xylo-scheduler` is "cross-process durable UTC scheduling, independent of native capabilities" (`crates/scheduler/src/lib.rs`). It stores recurring prompts and conversation wakeups in one JSON file and hands each due occurrence to a host. **The library never executes an agent or shell command**: the host claims prompts, renews active leases and acknowledges completion.

Boundaries, from `crates/scheduler/Cargo.toml` and the source:

- Depends on `xylo-core` ([core](core.md)) for `ToolDefinition`, `ToolObservation` and `PermissionMode`, plus `chrono`, `cron`, `fs2`, `serde`, `serde_json`, `tokio` and `uuid`.
- The only consumer is `xylo-runtime`, which re-exports it as `scheduler` ([runtime](runtime.md)). The host builds the store at `<artifacts>/schedules.json` (`host.rs`).
- All times are UTC. There is no time-zone setting; `status()` reports `"timezone":"UTC"`.

## Module map

| Path under `crates/scheduler/src` | Purpose |
| --- | --- |
| `lib.rs` | `Scheduler`, `ScheduledTask`, `ScheduleClaim`, `ConversationWakeup`, `wakeup_mode`, `definitions`, `parse_loop` |
| `tests/mod.rs` | Store, claim, lease and cadence tests |
| `tests/wakeups.rs` | Conversation wakeup tests |

## Key types and public API

- `ScheduledTask`: `wakeup`, `id`, `prompt`, `workspace`, `cron`, `next_run`, `enabled`, `claim`, `interrupted`. `#[serde(deny_unknown_fields)]`.
- `ScheduleClaim { occurrence, due, lease_until }`: one claimed occurrence. `occurrence` is a UUID v5 of the task id and `due.to_rfc3339()` (`ScheduledTask::occurrence_at`), so it is **stable per task and due time**. Hosts use it as the session and turn id.
- `ConversationWakeup { session_id, permissions, reason, interval_seconds }`: a one-shot or repeating wakeup that keeps the original permission limit.
- `Scheduler::new(path, workspace)`: `path` must be absolute; the workspace is canonicalized and must be a directory.
- Operations (all `async`, each runs on `spawn_blocking`): `list`, `save`, `create(prompt, cron)`, `create_wakeup(session, permissions, delay_seconds, prompt, reason, interval)`, `remove`, `claim_due`, `complete`, `renew`, `release`, `pause`, `status`, `execute`.
- `wakeup_mode(original, current)` returns the tighter mode: `Plan`, then `ReadOnly`, then `AskForApproval`, otherwise `ApproveForMe`. A wakeup can never exceed the authority it was created with; `FullAccess` collapses to `ApproveForMe`.
- `definitions()` lists the agent tools `schedule_list` (read), `schedule_create` and `schedule_delete` (both `writes: true`). `execute(name, args)` runs them; an unknown name fails with "Unknown schedule tool". `schedule_create` takes `cron` and `prompt`.
- `parse_loop("/loop [60s|5m|1h] prompt")` returns `(seconds, prompt)`. The default is 300 s; the interval must be 60..=3600 s and the prompt 1..=8192 bytes.

## Occurrences, claims and leases

1. `claim_due` locks the store, then for each enabled task with no claim and `next_run <= now` sets `claim = { occurrence, due: next_run, lease_until: now + 5 min }` and returns it.
2. While the run is active the host calls `renew(id)` (the host loop wakes every **15 s**), which sets `lease_until = now + 5 min`. `renew` on an unclaimed task fails with "Schedule has not been claimed".
3. `complete(id)` takes the claim and advances `next_run` from the **claimed due time**, not from the completion time. It is idempotent: a repeat call, or a task without a claim, changes nothing.
4. `release(id)` clears the claim without advancing, so the same occurrence is offered again.
5. `pause(id)` sets `enabled = false` and clears the claim. It stops retries until the user resumes the schedule.

### Advancing

- **Cron tasks**: `following()` takes the next cron time after `due`. If that is already past, the latest missed occurrence is returned, so **missed occurrences coalesce into one catch-up run**. The scan is bounded by `MAX_MISSED_SCAN` (100,000); beyond it `now` is used.
- **Wakeups with `interval_seconds`**: `next_run = due + (elapsed/interval + 1) * interval`, skipping missed intervals. Wakeups without an interval set `enabled = false` after completion.

## At-most-once dispatch

**A claim whose lease expires is never handed out again.** On the next `claim_due`, an expired claim (`lease_until <= now`) is retired: `interrupted = Some(occurrence)`, `claim = None`, and the task advances from the claimed `due` as for `complete`. The occurrence is recorded, not retried. The host reports each interrupted occurrence once: "A scheduled run of "..." was interrupted and was not repeated" (`report_interrupted_schedules`).

Only an explicit `release` re-offers the same occurrence. The host uses `pause`, not `release`, when a run cannot start or finished without a completed turn, so uncertain external writes are not repeated.

## Persistence format

- One file, a pretty-printed JSON array of `ScheduledTask` (`serde_json::to_vec_pretty`), at the path given to `Scheduler::new`.
- Limits: **128 tasks**, **2 MiB** store, prompt 1..=8192 bytes, cron at most 512 bytes, wakeup reason 1..=1024 bytes, wakeup interval 60..=3600 s, wakeup delay 60..=3600 s.
- Cron is a seven-field UTC expression (`sec min hour day month weekday year`); wakeups store an empty `cron`.
- Writes go to `<path>.<uuid>.tmp` (`create_new`), `sync_all`, `rename` over the store, then a directory sync on Unix. The file is only rewritten if the tasks changed.
- There is **no cached state**: every operation locks and rereads the file.

## Concurrency

Each operation holds an exclusive `fs2` lock on `<path>.lock` (the store path with the extension `lock`). Contention is retried every 10 ms for **5 s**, then fails with "Schedule store is busy; lock deadline exceeded". The lock is shared by every process using the store, which gives cross-process atomic read-modify-write. Clones of `Scheduler` are cheap (`path` and `workspace`).

`save` accepts only user-editable fields (`prompt`, `workspace`, `cron`, `enabled`) for an existing task. `claim` and `interrupted` always come from the store, so a stale copy cannot release a running occurrence. Changing `cron` reschedules from now. A wakeup's `wakeup` field cannot change ("Wakeup conversation and permission limit cannot be changed"). New tasks start with no claim and no interruption.

## Failure behavior

- **Undecodable or invalid store** (bad JSON, unknown fields, over 128 tasks, invalid task, duplicate id): the file is **deleted under the lock and replaced by an empty store**. Schedules are not carried across format changes.
- **Oversized store, I/O errors, lock timeout**: returned as errors; the file is not deleted.
- An invalid cron, missing workspace or oversized prompt fails `save` before anything is written.
- `remove` of an unknown id fails with "Schedule not found"; so do `complete`, `renew`, `release` and `pause`.
- A failing host-side `complete` or `pause` keeps the occurrence in the host's `scheduled` map, so the next loop retries it (`host.rs`).

## Host integration (`crates/runtime/src/host.rs`)

The schedule loop sleeps 15 s between passes. Each pass renews claims for running or queued occurrences. For an occurrence no longer active, it calls `complete` if the store has a completed turn for it and `pause` otherwise. Then it calls `claim_due`. Conversation wakeups go to `dispatch_wakeup`; other tasks become a `WorkerCommand::Run` with `session_id = turn_id = occurrence`. Scheduled turns run unattended, so `FullAccess` becomes `ApproveForMe` ([runtime](runtime.md), `effective_mode`). A wakeup's mode is further limited by `wakeup_mode`.

## Security

- Agent-created schedules need the user's approval (`writes: true`). **Content of a stored prompt is data to the agent loop; it grants no extra authority.**
- Wakeups retain the original permission limit and can only get tighter.
- Workspace paths are canonicalized and must be absolute.
- Telemetry and secrets are not stored here; the file holds prompts and paths.

## Testing

`cargo test --locked -p xylo-scheduler` (not run while writing this page). Tests live in `crates/scheduler/src/tests/`.

## Related

[core](core.md) (tool and permission types), [runtime](runtime.md) (the host loop that dispatches), [services](services.md), [engine](engine.md) (executes the turns), [capabilities](capabilities.md).
