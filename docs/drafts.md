# Durable composer drafts

Composer state is stored separately from messages, checkpoints and action journals. A `DraftKey` combines chat UUID and optional workspace. Rows have no conversation foreign key: an unsent new chat remains recoverable before its first admitted turn. Text and up to four typed image references are saved together; image bytes, native paths and base64 data are not embedded in draft rows. Attachment-only drafts count as nonempty.

## Ordering and admission

`Store::draft` returns a `DraftSnapshot`; revision zero denotes an absent row. `save_draft(DraftWrite)` runs an immediate SQLite transaction and compares `expected_revision`. A committed mutation increments that key's revision; a conflict returns the current snapshot without changing it. Retrying the current mutation ID with the same payload returns its receipt; changing that payload is an error. Empty rows retain a revision tombstone so a delayed initial save or clear cannot resurrect sent text. Two Store connections cannot both win the same revision.

The native composer coalesces changes with a 350 ms debounce and permits one outstanding save per key. It retains new typing while earlier saves are acknowledged. Requests are correlated by ID and key. Load/save/admission callbacks capture the actual current native input before applying a receipt, covering delayed `InputEvent::Change` delivery. An asynchronous load never replaces text typed into another chat. If a nonempty saved version arrives after different local typing, both versions remain available until the user explicitly chooses Keep current text or Use saved draft.

Sending first awaits the captured draft's save. The host validates the provider and image references, starts the native worker and reserves dispatch capacity. `Store::admit_draft_turn` then atomically inserts the canonical idempotent user message and conditionally clears the exact draft revision. A later draft revision wins the clear comparison. Failed validation, worker admission, queue reservation or database commit leaves the draft intact. The host dispatches through the reserved slot after commit and returns `DraftAdmitted`; cancellation during the commit cannot abandon its durable result. Actual worker events own running/completed state, so a fast completed event arriving before admission acknowledgement cannot be reset to Started.

Draft text remains visible until authoritative admission. Later user typing remains visible and is saved against the returned revision. Steering an already running task retains the editable text while Queued. Only an Applied receipt, committed with the exact user context message and checkpoint, can clear the captured revision. Later typing wins; cancellation or admission failure preserves it. See [acknowledged task guidance](steering.md) for receipt, restart and permission semantics.

## Shutdown, recovery and bounds

Quit and explicit update installation first flush the latest drafts and wait for their acknowledgements. A failed save leaves the window, text and retry action available. Keep working cancels the close intent while permitting the pending save to finish. The host drains accepted operation handlers before teardown. No SQLite/filesystem work runs on the UI thread, and typing does not produce a transaction for every character/token.

The cache contains at most 128 draft entries. Only clean, persisted, idle entries can be evicted from memory; their database contents remain available through Saved drafts. Oversized native input stays visible and blocks switching/closing until shortened or copied to a file. Service caps are 256 KiB of UTF-8 text plus bounded reference metadata per draft, 64 MiB of aggregate draft payload and 16,384 rows including empty tombstones. Reaching a cap returns an explicit error and does not delete a nonempty draft. Tombstone compaction is not currently implemented; reaching the row limit requires a future explicit maintenance flow rather than unsafe revision reuse.

Saved drafts is a bounded, virtualized, keyset-paged picker for the current project. It reads summaries and previews, not every full draft body. Drafts before choosing a project is an explicit recovery route for the original workspace=None scope. Opening it restores that original key without silently moving or deleting it. Automatic project adoption is not claimed.

## Typed APIs

Services export `DraftKey`, `DraftSnapshot`, `DraftWrite`, `DraftSave`, `DraftCursor`, `DraftSummary`, `DraftPage`, `DraftSubmission` and `DraftAdmission`. `Store::draft`, `save_draft`, `drafts_page` and `admit_draft_turn` run on its dedicated bounded SQLite executor. Derived draft tables initialize lazily and do not alter the application's migration version. The existing FULL synchronous durability policy covers these transactions.

`runtime::drafts::{load, save, list, admit_turn}` move synchronous operations to blocking host work. Load/save/list check cancellation before dispatch. Once a write is dispatched, the runtime awaits its receipt instead of racing and abandoning the commit. Admission is intentionally not cancellation-raced. The host exposes correlated DraftLoaded/DraftSaved/Drafts/DraftAdmitted events. A loaded snapshot has the source key; a page echoes its workspace; an admission includes the durable turn UUID and whether its clear comparison succeeded.

## Validation scope

Deterministic service tests cover database reopen without a conversation row, text/reference scope, attachment-only recovery, stale saves and clears, mutation retry validation, simultaneous writers, capacity errors, equal-timestamp pagination, atomic admission rollback and preservation of newer typing. Host tests cover provider failure, pre-admission cancellation, full worker queues, completed-turn retry, postcommit metadata failure and shutdown operation draining. A capacity-one worker fixture covers cancellation arriving while the SQLite transaction is waiting for a write lock.

Native tests use a real Store and Host for saved-draft picker restoration, chat switching, Unicode text, coalesced in-flight saves, late-response isolation, latest-value Quit flushing, cancelled close intents and a real SQLite failure trigger that keeps unsaved text visible. These establish local protocol and UI behavior. Crash/power-loss filesystem guarantees, every desktop platform, long-lived storage-limit maintenance and user task quality remain separate acceptance work.
