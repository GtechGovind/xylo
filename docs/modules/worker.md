# Supervised task worker (`xylo-worker`)

## Purpose and boundaries

`xylo-worker` is the "supervised native task worker" binary (`crates/worker/src/main.rs`). **One process executes one root task at a time.** Stdout carries only versioned JSONL events; logs go to stderr. The host in `xylo-runtime` ([runtime](runtime.md)) spawns it, speaks to it over stdin/stdout and supervises its lifetime.

Boundaries, from `crates/worker/Cargo.toml` and the source:

- Depends on `xylo-core` ([core](core.md)) for the protocol types, `xylo-engine` ([engine](engine.md)) for `AgentEngine` and `ToolSession`, `xylo-services` ([services](services.md)) for `Store`, `CredentialVault`, `VaultKey` and `ServedContextCache`, and `xylo-runtime` only for `AppPaths`.
- **The worker never opens the OS credential store.** The host sends the vault key with `Unlock` (`CredentialVault::unlocked`).
- Runs a Tokio multi-thread runtime with **2** worker threads.
- Command line: `--data-dir <path>` (otherwise `AppPaths::discover()`), and `--version`, which prints `Xylo worker <version> (protocol 2)`. The database is opened with `Store::open_existing`, so it must already exist.

## Module map

| Path under `crates/worker` | Purpose |
| --- | --- |
| `src/main.rs` | Command loop, writer task, run lifecycle (`ActiveTask`, `RunEnding`) |
| `src/credentials.rs` | `CredentialBroker`: provider credential renewal requested from the host |
| `src/steering.rs` | Verification and acknowledgement of durable guidance (`SteerDurable`) |
| `src/shell_events.rs` | Coalescing of shell job snapshots into `ShellJobUpdated` events |
| `tests/` | Protocol tests: `worker_protocol`, `cancellation_protocol`, `steering_protocol`, `title_protocol`, `vision_protocol`, `action_review_host` |

## Protocol

- Both directions use `Envelope<T> { version, request_id, session_id, sequence, payload }` (`xylo-core`), one JSON object per line. Commands are `Envelope<WorkerCommand>` (tagged by `command`, snake_case); events are `Envelope<EngineEvent>`.
- `PROTOCOL_VERSION` is **2**. A command with another version is answered with an `Error` ("Unsupported worker protocol version N") and not executed.
- Each line is limited to `MAX_PROTOCOL_BYTES` (**8 MiB**). An oversized command line produces an `Error` ("Command exceeds protocol size limit") and **the worker exits its loop**. An oversized output event makes the writer task fail.
- A command that does not parse yields `Error` "Invalid worker command: ..." with a nil request id, and the loop continues.
- Output goes through one `mpsc` channel of `EVENT_QUEUE_CAPACITY` (256) to a single writer that flushes after each line. `sequence` comes from a shared counter starting at 1 and increases with every event, so events are ordered.
- On start the worker sends `Ready { version }` with a nil request id, and again in reply to `Hello`.

## Commands

| Command | Behavior |
| --- | --- |
| `Hello` | Replies `Ready`; the host uses it for the handshake |
| `Unlock { key }` | Opens the vault with `VaultKey`, builds a `ToolSession` bound to the shared permission cell and installs the shell job sink. A repeated `Unlock` shuts down the previous session first. A bad key yields `Error` "Could not unlock the secret vault: ..." (`ErrorKind::Authentication`, not retryable) |
| `Run { turn }` | Starts one task (see lifecycle). Before `Unlock` it fails with "Worker is locked: the host must send Unlock before Run"; while a turn is active it fails with "Worker is busy; steer or cancel the current turn" (retryable) |
| `RunAuthorized { turn, credential }` | Like `Run`, with a transient provider credential; the engine can request renewals through `CredentialBroker` |
| `ProviderCredentialUpdated` | Delivers a renewal reply; ignored unless it matches the active run's request id and session, and the run is not cancelled |
| `SetPermissions { mode }` | Sets the live `PermissionCell`, which every broker reads at each tool call. **A change applies to the running turn at its next tool call** and to later turns |
| `Cancel` | Cancels the active task's `CancellationToken` and answers `Status` "Cancellation requested; new tool dispatch stopped" |
| `SteerDurable { guidance }` | Verifies the guidance against the journal and replies `Steering { receipt }` (below) |
| `Approve { action_id, allow, remember }` | Decides a pending approval; failure answers `Error` on the Approve's own request id |
| `Answer { action_id, answer }` | Answers a pending interaction; replies `Status` "Answer accepted" or an `Error` |
| `RespondMcpElicitation` | Delivers an MCP form answer to the session (ignored if locked) |
| `KillJob { job_id }` | Stops a shell job owned by the envelope's `session_id` (required) |
| `Shutdown` | Leaves the command loop |

`Unlock` and `SetPermissions` produce no reply on success. The permission mode at admission is `turn.permissions`, set into the cell before the engine starts.

## Lifecycle of a task

1. The host spawns the worker, sends `Unlock` and `Hello`, and waits for `Ready` with the `Hello` request id.
2. `Run`: a previous finished turn's leftover best-effort work (such as title generation) is aborted. The worker registers the user turn with `Store::register_user_turn` on a blocking thread, then runs `AgentEngine::run` with `interactive: true` and the cancellation token.
3. Engine events are forwarded with the run's request id and session. The **first** terminal event (`Completed`, `Cancelled` or `Error`) is held by `RunEnding::end`, which clears `busy`, settles guidance that never reached the turn (`steering::finish`, emitting `Steering` receipts) and then sends the terminal event. Later terminal events are dropped.
4. If the engine ends without a terminal event, or fails to register, the worker sends a retryable `Error` (`ErrorKind::Internal`). An engine error after a terminal event is only logged.
5. Because `busy` clears before the terminal event is sent, the host may start the next `Run` as soon as it sees the result.
6. At stdin EOF, `Shutdown` or an oversized command: the active task is cancelled and given **5 s** to finish, then aborted; the tool session is shut down; the writer drains and the process exits.

## Durable guidance (`steering.rs`)

`SteerDurable` replies with the saved `SteeringReceipt` only when the journal holds the identical request. A missing record or mismatched identity returns a `Cancelled` receipt with a reason. If the matching turn is active and not cancelled the engine is woken with `notify_steering`; otherwise pending guidance is finished with "Task ended before guidance was added". A store failure produces a retryable `Error` and **does not end the worker or the turn**.

## Credential renewal (`credentials.rs`)

`ProviderCredentialRequested { request_id, generation, force }` is emitted on the run's request id. At most **4** renewals are pending (otherwise "Provider renewal is busy"), and the wait times out after **65 s**. Cancellation interrupts it. The reply travels over private stdin and never as a UI event.

## Host supervision (`crates/runtime/src/host/worker_control.rs`)

- **Spawn**: `spawn_worker` runs the sibling `xylo-worker --data-dir <data>` with piped stdin, stdout and stderr, `kill_on_drop(true)` and `RUST_LOG`. The vault key is exported with `export_key`; it goes only through `Unlock` on stdin, never arguments or environment.
- **Handshake**: `Unlock` and `Hello` are queued (command channel capacity **64**), then `handshake` reads stdout until `Ready` with the `Hello` id, within **20 s**. Every line's `version` is checked first; a mismatch fails with "The task runner belongs to a different Xylo version ... Reinstall the complete application." An `Error` event, early EOF, oversized or unreadable line also fails startup. On failure the child is killed.
- **Reader**: `WorkerReader::run` forwards each validated envelope as a host event (stamping the session and a host per-session sequence). An invalid message or an oversized line ends the reader. A closed stdout or `stop` token also ends it, then `reap` runs.
- **Writer**: `write_commands` serializes one command per line, flushing each; it stops after `Shutdown`, a write error or the `stop` token.
- **Diagnostics**: stderr lines (16 KiB cap) are redacted by `redact_secrets` (bearer tokens, `sk-` keys, `api_key=` values) and logged at debug.
- **Warm pool**: idle workers are kept for `IDLE_WORKER_TTL` (5 min), at most `MAX_IDLE_WORKERS` (4), least recently used retired first. Active workers are never retired.
- **Key change**: `retire_workers_for_new_key` retires idle workers at once and busy ones when their turn ends, so the next worker is unlocked with the new key.
- **Permissions**: `forward_permissions` sends `SetPermissions` to every live worker (2 s timeout each). Scheduled turns use `effective_mode`, and wakeups are further limited by `wakeup_mode` ([scheduler](scheduler.md)). If delivery fails to an active worker the host reports "A running task could not receive the new permission mode. Stop it to apply the change."

## Cancellation and forced stop

`stop_active_run` sends `Cancel` once per run request. It also cancels any pending credential renewal. If the run has not reached a terminal event within `STOP_GRACE` (**10 s**; 300 ms in tests), or `Cancel` cannot be delivered, the host sets `forced_stop` and cancels the `stop` token, ending the worker process. The worker itself, on `Cancel`, stops new tool dispatch and the engine emits `Cancelled`.

**Reaping** (`reap`): the child is killed and waited for. Turn guidance is settled, interrupted journal entries for the session are recovered (`recover_session_interrupted`), approvals are cleared and the turn queue is settled. If a run was unfinished the host emits `Cancelled` plus "The task did not stop in time, so Xylo ended it. Actions it had started are listed for review." after a forced stop, or a retryable `Error` "This task stopped unexpectedly. Resume the chat to continue." after a crash.

## Security

- **The vault key travels only over the worker's private stdin**; `WorkerCredential` has a redacted `Debug` and its docs forbid settings, turns, events, arguments and environment variables.
- Provider credential replies are private stdin commands, never UI events.
- Stdout carries protocol data only, so host-side parsing never sees log text. Diagnostics are redacted before logging.
- Approvals and answers are accepted only through the host; a locked worker rejects them.

## Testing

`cargo test --locked -p xylo-worker` (not run while writing this page). Integration tests are in `crates/worker/tests/`; unit tests in `credentials/tests.rs` and `shell_events/tests.rs`.

## Related

[core](core.md) (envelope and command types), [runtime](runtime.md) (host supervision), [engine](engine.md) (`AgentEngine`, `ToolSession`), [services](services.md) (store and vault), [scheduler](scheduler.md), [capabilities.md](capabilities.md).
