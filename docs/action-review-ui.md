# Review interrupted actions

The chat shows **Review interrupted action** when its durable action journal contains an uncertain result. The review panel uses friendly action names and saved times, opens the actual target and retained output files, and shows the retained observation. It does not display journal identifiers or a raw payload dump. The action list renders a bounded page of at most 50 rows using a native virtual list; **Earlier actions** and **Back to newest actions** query the authoritative snapshot cursor.

A user can choose **Completed** or **Did not complete**, write an evidence note after inspecting the target, and confirm **Record outcome** in a native dialog. This records an operator observation. It never executes the original action, undoes partial changes, or verifies the original execution output. Partial observations and artifact references remain in the durable journal. An action leaves the current list only after a receipt exactly matching its saved operation, scope, action, original revision, outcome, and evidence arrives.

## Qualified actions

Recording is available only for current-protocol interrupted in-process file writes, memory saves, and skill proposals after their execution lease has ended. Actual running work is shown separately from work whose ownership cannot be verified. Legacy actions, native programs, shell commands, environments, browser operations, connectors, and other unqualified actions remain visible with **Verify this action in the service where it ran**. A new local lock cannot prove an old or external process has stopped. The interface does not offer a generic retry or execution button for reconciliation.

The host separately refuses a resolution while actual work is pending or running in the chat. The native UI also disables recording while its current turn is active. Model tools cannot invoke these human control methods.

## Saved notes and uncertain acknowledgments

Review selection, note, observed outcome, and the complete pending resolution request have their own scoped SQLite form store, separate from composer and document drafts. Notes save with revision comparison; a conflicting saved review requires an explicit choice. Notes can still be edited while an earlier acknowledgment is delayed. A later note is never cleared by a receipt for an older note.

The exact resolution request is saved before it is sent. A lost resolution acknowledgment retains its original operation identity across controller and application restart. Reopening checks its authoritative receipt without resubmitting the request. **Stop waiting for this request** cancels waiting where possible; it does not claim the record was prevented or effects were undone. An absent receipt is not cancellation proof. **Check recorded outcome** reads the original operation. **Save this observed outcome** explicitly submits the same captured operator record, with the same operation identity and evidence, without running the action.

A bounded acknowledgment timeout also retains an exact unconfirmed form save for revision-safe reconciliation. **Save review note** checks that same saved version before persisting later typing. Conflicting notes or pending writes guard chat/project switching and closing. Quit and update installation wait for durable form acknowledgments; further typing cancels automatic close intent. In-flight resolution must finish or be left as a durable pending request using **Stop waiting** before switching or closing.

## Verification scope

`crates/desktop/validation/recovery_interactions.rs` uses production native controls and actual Host/Store boundaries without a model or account. Its seeded interrupted file write first obtains the current protocol-qualified execution lease, writes a real partial file and retained artifact, records an uncertain result, then drops the lease. Delivery interception exercises lost acknowledgments without inventing a successful receipt. A real SQLite writer lock exercises cancellation before commit.

Focused cases cover partial-effect/artifact retention, exact request restoration after restart, newer typing during missing acknowledgments, cancellation and explicit same-record submission, malformed/wrong-scope/stale responses, 55-action pagination and native virtualization, actual execution-lease contention, unqualified native action refusal, conflicting saved notes, lost form-save acknowledgment, and a separately labeled active-turn UI-state guard. Actual host/worker admission and continuation are qualified by backend integration tests, not inferred from the UI-state fixture.

`recovery-rendering` is a separate offscreen native renderer using the same actual Host/Store partial-write fixture. It honors `WORKBENCH_RENDER_DIR`, writes `recovery.png` and `recovery.json`, and checks visible action/target/artifact controls. Its metadata records image bytes and compiled recovery-source hashes. It neither captures the physical desktop nor establishes WindowServer, physical input, external service, or full release acceptance. Existing home/settings/plugins/environments capture behavior is unchanged.
