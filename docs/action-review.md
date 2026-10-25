# Reviewing uncertain actions

Action review records what a person observed after an interrupted action. It
never executes, retries, cancels or undoes the original operation. Its controls
are Host APIs and are absent from the agent tool catalogue.

The current supported path covers lease-qualified in-process `file_write`,
`memory_remember` and `skill_propose` operations. The executor takes an OS lock
before creating the durable intent and holds it through actual synchronous work
and result persistence. Late blocking jobs retain the same file by ownership,
even after the enclosing async task or its handle is cancelled. Resolution must
acquire that authority and keep it through the SQLite compare-and-set commit.
The Host additionally refuses new resolutions while that chat has queued or
active worker work. A previously committed exact request can still return its
receipt while later work is active.

Native commands, SSH/container operations, browsers, MCP subprocesses, external
image services and pre-protocol journal rows remain unavailable in this generic
interface. Their UI says **Verify this action in the service where it ran**.
An absent lock, PID, timestamp, worker exit or inherited descriptor does not prove
that arbitrary native work stopped. OpenSSH and other clients can close extra
descriptors; detached children may outlive their worker. Qualifying those paths
requires an independent supervisor/guardian and real process-lifetime tests.
This implementation introduces no native descriptor inheritance or `pre_exec`
fork overhead. Existing environment-specific reconciliation remains separate;
generic action review does not certify or extend its lifetime guarantees.

The database initializes the additive review schema at `Store::open`. Exact
action IDs and protocol versions stay in SQLite. OS leases use 4096 stable SHA256
stripes, with private owned directories, regular single-link files, `O_NOFOLLOW`
and `O_CLOEXEC` on Unix. Lockfiles are never unlinked. A hash collision can
conservatively block an unrelated eligible action; it cannot permit concurrent
resolution of live work. Windows retains the in-process OS-file-lock path, while
native execution remains unqualified.

`ActionReviewScope` binds a chat to its persisted canonical workspace. Pages have
at most 50 summaries, a typed keyset cursor and a sequence snapshot. New or changed
rows require a refresh. List liveness reads small name/protocol metadata and
never loads retained results. Details bound JSON previews to 16 KiB, show at most
64 artifact references and explicitly mark truncation. Full original arguments,
observations and artifact references remain in the database.

`ActionResolutionRequest` carries a stable operation UUID, original action UUID,
opaque revision, explicit `completed` or `did_not_complete` outcome and a nonempty
evidence note of at most 4096 UTF-8 bytes. One immediate SQLite transaction checks
scope and revision, records the operator observation, updates the outer journal
and inserts an immutable audit containing the complete original record and its
SHA256. Update/delete triggers protect that audit. Exact operation replay returns
the same receipt; changed identities, evidence or outcomes are rejected. Receipt
lookup returning `None` is not an acknowledgement and does not discard an intent.

User-confirmed completion uses status `user_confirmed`, carries
`native_execution_verified: false`, and preserves the original result under
`original_observation`. A did-not-complete result has status `failed` and warns
that partial effects may remain. Neither wrapper fabricates native output, grants
permission or makes the original tool call executable again. Procedural learning
does not classify `user_confirmed` as a verified native success.

When resuming the current checkpoint turn, the engine replaces an existing exact
tool observation with its recorded operator outcome, or supplies that observation
for an unfinished tool batch. It preserves unrelated messages and does not append
a second result for the same call. Older-turn historical context is not rewritten;
the durable review record and audit remain authoritative for those actions.

Recovery forms have their own durable namespace and revision CAS. They preserve
the selected action, current note, outcome and exact pending request. At most 512
forms of 16 KiB each are stored; capacity errors preserve existing entries. A
pending request cannot be cleared or substituted until its exact immutable
receipt exists. New pending intents validate the action revision, and a competing
resolution for that action is refused while an exact pending intent is saved.
Newer typing survives receipt reconciliation. No recovery form is a transcript
message or an engine checkpoint.

Public runtime exports are `ActionReview*`, `BoundedActionPayload`, and
`ActionResolution*`. `HostClient` exposes `action_reviews`, `action_review`,
`resolve_action`, `action_resolution`, `action_review_form`, and
`save_action_review_form`. Correlated Host events are `ActionReviews`,
`ActionReview`, `ActionResolved`, `ActionResolution`, `ActionReviewForm`, and
`ActionReviewFormSaved`. Read/review cancellation uses `cancel_capability`;
accepted form saves and a committed resolution still receive their durable
acknowledgement.

Focused qualification uses offline, locked Cargo builds. Store tests exercise
immutable originals, restart/idempotence, stale scope/revisions, bounded paging,
pending-intent CAS, real kernel contention and a separately spawned owner that
is killed and reaped. Engine tests run the actual resume entry with a loopback
provider, preserve manual provenance for both outcomes and retain lease ownership
through late blocking work. Host tests verify correlated receipts and reject
cross-project admission without altering history or checkpoints. The separate
`cargo test -p workbench-worker --test action_review_host` target runs the current worker behind
the real Host, proves denial during an open model stream, cancels that stream and
then records the observed outcome. These checks do not qualify live external
services or arbitrary native process-tree lifetimes, and make no latency claim.
