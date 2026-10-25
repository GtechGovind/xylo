# Acknowledged task guidance

While a task runs, sending composer text creates an identified guidance request. This is direct user input, not a tool result or an instruction from external content. It changes no tool permissions and grants no approvals. Images stay in drafts for a subsequent turn; guidance accepts 1–16 KiB of UTF-8 text.

`Queued` means the host saved the captured input and reserved worker dispatch. It leaves the text visible. `Applied` means the engine committed that exact identified user message to its resumable context. It does not establish that the model followed it or completed any requested action. `Cancelled` means the task ended before that context commit; the draft remains available for explicit retry. Product status says “Guidance added to the task” only for the durable application receipt.

## Commit and ordering

The host validates the active chat/turn, reserves a bounded command slot and calls `Store::queue_steering(request, submission)`. The saved draft revision, text, workspace and chat must match. Requests retain canonical UUIDs; the same captured revision deduplicates even if a reconnecting client supplies another transport UUID. IDs cannot be reused for different input. The correlated Queued outcome is sent before dispatch, establishing canonical identity before a fast Applied receipt can arrive.

The native worker checks the durable identity and the exact active turn. It notifies the application callback through an atomic pending hint. The real Hermes loop invokes that callback before its next model request and at final-answer completion. Guidance received during a final answer therefore causes another loop iteration. Tool batches finish first; user guidance is never inserted between assistant tool calls and their tool results.

`Store::apply_steering` commits four changes in one FULL-synchronous SQLite transaction: an identified user context message, its checkpoint, the deterministic transcript message and the Applied receipt. The same transaction conditionally clears the captured draft revision. Newer text or image attachments win that comparison. A receipt records the original clear revision even if a later draft already exists. The native composer uses that fence to distinguish its own known clear from an unrelated writer when a newer autosave conflicts. It retains newer native typing in either receipt/save-ack order. A repeated receipt cannot clear another chat or later typing, and cannot reset a completed task to Running.

Application uses at most eight pending entries per safe boundary. The pending hint avoids repeatedly serializing context when no guidance arrived. Limits are 64 guidance records and 128 KiB of guidance text per turn, and 16,384 retained records overall. Limits return explicit errors; there is no automatic deletion of receipts. Compaction is future explicit maintenance work.

## Interruption and restart

The engine commits context before publishing Applied. If a worker dies before its pipe receipt arrives, the host reaps it, rereads the journal and replays Applied. Remaining Queued entries become Cancelled; their draft text is retained. Whole-application startup calls `recover_steering` once before workers start, also cancelling only Queued entries. An Applied message is already in the checkpoint and is not injected again. A standalone worker resumed without host recovery can apply a previously Queued entry once; the desktop deliberately requires explicit retry after startup or a reaped task.

When restoring a checkpoint, the adapter removes only its leading system-policy prefix, which Hermes rebuilds from current application policy. It preserves later system notes, user messages, tool results and provider-native/attachment content. Repeated worker restarts therefore do not accumulate copies of the initial policy or exhaust the default context solely through policy duplication.

Failure to reserve dispatch, save a draft, queue guidance or authenticate its target never clears the input. Cancellation during application can race a committed context change: the immutable receipt reports the actual transaction outcome. Applied is a context receipt even when the task is subsequently cancelled; it is not evidence of model execution. Quit/update flush current drafts first; pending guidance remains separately durable and is settled during host shutdown/recovery. No guidance text is preauthorized for later tool execution.

## Interfaces

Core defines `SteeringRequest`, `SteeringReceipt`, `SteeringState` and the additive `WorkerCommand::SteerDurable` / `EngineEvent::Steering` protocol messages. The bundled host and worker ship together. An older worker that rejects the new command cannot cause a false Applied receipt or clear the draft.

Services define `SteeringOutcome { request, receipt, draft, cleared_revision }` and `SteeringApplication { messages, applied, has_more }`. Store methods are `queue_steering`, `steering`, `steering_for_turn`, `apply_steering`, `finish_pending_steering` and startup-only `recover_steering`. Tables initialize lazily without changing the base Store migration. Runtime wrappers in `runtime::steering` move journal work off the UI thread. `HostClient::steer_draft` emits correlated `HostEvent::SteeringResult`; the host validates worker receipts against its own database instead of trusting receipt text from the pipe.

The vendor patch adds an optional `AgentCallbacks::on_application_guidance` callback in streaming and nonstreaming loops. It appends no alternative loop and is disabled unless supplied. The application callback appends direct role-safe user messages; it does not reuse the upstream single-slot interrupt marker. Legacy unacknowledged steering never claims delivery.

## Evidence and boundaries

Service tests cover transaction rollback, reopen, alias deduplication, immutable Applied receipts, newer typing, forged identity, wrong checkpoint and startup cancellation. Real subprocess tests use local gated HTTP streaming responses: guidance during final streaming forces another request; process kills before/after application preserve one context message through three resumptions; cancellation retains the unsent draft. The restart fixture keeps the default 8,192-token context and checks stable system-message counts. Native tests use the actual composer, Store and Host, with durable outcomes injected at controlled points to verify both save/receipt orders, duplicate receipts, exact clearing and cancellation/error preservation. Host fixtures cover reserve-before-queue ordering, lost transport and lost Applied-pipe recovery.

These tests qualify local state, protocol and UI behavior. They do not measure instruction-following quality, remote account acceptance, physical power-loss behavior or every operating system.
