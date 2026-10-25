# Inspectable project memory

Facts, chat history, execution checkpoints, learned procedures, composer drafts
and unsaved Memory forms are separate application data. This page covers explicit
facts and their unsaved editor state. [History search](history-search.md) and
[procedural learning](learning.md) cover the other product routes.

## Browsing and provenance

Memory opens on a bounded page of current facts. Search uses lexical/vector
ranking and returns at most 100 matches. Ordinary browsing uses 50-record keyset
pages. **Browse all saved versions** also includes inactive previous versions,
so deleting a current version does not hide its remaining older versions.
Selecting a fact opens its complete connected version history with current or
previous status, text, project, source and timestamp. The selected version is
read from the same database snapshot as its history, rather than shown from an
old sidebar cache. Version lists render visible rows and replace bounded pages.

Version cursors include the fact scope revision. A change between pages requires
a refresh, preventing a list assembled from incompatible snapshots. Request IDs,
project scope and anchor IDs fence late native responses; cancelled inspections
cannot populate a newly selected fact or another project. SQLite progress hooks
interrupt long traversals and are removed before the database serves another
operation. A deleted intermediate version retains only its identifiers/linkage,
so the remaining chain stays connected.

**Correct current fact** creates a new immutable version. It requires the
predecessor to remain current in the same project; stale branches are rejected.
Previous versions cannot silently be corrected into a second active branch.
Opening a different correction does not overwrite an existing unsaved entry.
Cancelling a correction keeps the text and saves its changed editing state.

## Explicit deletion scopes

Each destructive action first obtains a scoped preview with counts and a revision,
then presents a native confirmation describing the consequence:

| Action | Data removed | Data retained |
| --- | --- | --- |
| **Delete selected version…** | That version's canonical text and all registered vector/FTS entries | Every other version; no older version becomes current automatically |
| **Forget entire fact…** | Every remaining version connected to the selected fact, with its vector/FTS entries | Other facts in the project |
| **Delete all project facts…** | All committed fact versions in the chosen project and their vector/FTS entries | Facts in other projects |

All three preserve chat history, checkpoints, learned skills, composer drafts and
unsaved Memory entries. Project purge is never a generic reset of application
data. If facts change while confirmation is open, the transaction rejects the
stale preview; the user must review a new preview. Cancellation before commit
rolls back the entire operation. A committed operation returns its durable receipt
even if cancellation arrives too late. The UI does not claim cancellation proves
that nothing was deleted.

Canonical, lexical and vector deletion share one transaction. A failed canonical
delete also rolls back preceding derived-index deletions. Stable operation IDs
make matching retries idempotent; reusing an ID for a different request is an
error. Receipts retain the most recent 4,096 requests. Minimal UUID/scope/deletion
metadata prevents an old remember retry from resurrecting a deleted version and
keeps chains navigable; deleted content and vectors are not retained there.
A destruction generation also rejects a remember operation that began before
that project's deletion and attempted to commit afterward. New explicit entries
created after deletion remain permitted.

This is logical application deletion. It is not a secure wipe of SQLite free
pages, historical WAL copies, exported files, backups or external provider data.

## Durable unsaved Memory entries

The Memory text field autosaves on a 300ms editing debounce and flushes before a
project switch, explicit Quit or application update. This saves an **unsaved
entry**, not a fact, and never promotes it into agent memory automatically.
A cold start restores the project or shared/global entry. The native page shows
whether its recovery copy is saving, saved, conflicted, or needs retry.

`memory_forms` is separate from composer tables. Scope-keyed writes use revision
compare-and-swap and stable mutation IDs. Empty revision tombstones fence old
saves. Conflicting writers preserve both the visible text and the stored snapshot
until the user chooses **Use saved entry** or **Keep this entry**. A failed enqueue
or persistence receipt leaves the input visible and retryable. New typing cancels
a pending close intent; programmatic no-op restores do not. Close waits for the
latest acknowledged form generation rather than an older in-flight save.

An explicit fact save first persists its proposed record ID and exact submitted
input with the form. Only that matching form receipt permits the host fact write.
Fact acknowledgements settle the source project even after switching projects,
without clearing another project's text. Newer typing survives and a continued
correction targets the newly committed version. Restart reconciliation checks
canonical provenance and the exact submitted input: a lost success acknowledgement
does not resurrect already-submitted text, while even new whitespace typing is
retained. An unconfirmed save is never automatically retried on startup.

Decoded input is bounded to 256KiB. Encoded form rows allow 6MiB to account for
JSON escaping plus the submitted input and proposed record. A transactional
migration expands older row CHECK constraints while preserving text, revisions,
mutation IDs and usage accounting. Total form storage is bounded to 64MiB and
4,096 scope rows; nonempty forms are not automatically evicted. The native cache
holds at most 64 scopes and evicts only clean, acknowledged entries. A storage
limit pauses saving and keeps the visible text rather than deleting existing data.
Abrupt termination can still lose edits that have not received an autosave receipt;
ordinary Quit and update installation wait for that receipt.

## Scoped export

**Export project facts…** exports only that project's remaining active and
superseded versions. **Export this fact…** exports only the connected fact chain.
Both include scope, source, timestamps, active status and original supersession
references. Deleted content, embeddings, credentials, chats, checkpoints, learned
procedures and unsaved forms are excluded. References to a deleted predecessor
can remain in a surviving version; export does not restore deleted content.

Schema version 2 records the selection and fact revision. Export uses a separate
read-only connection and a consistent WAL snapshot, streams through a 64KiB
writer, stages in the canonical destination directory, flushes/syncs, and
atomically publishes. The read transaction does not occupy the app's database
executor during file writes. Cancellation or failure before publication preserves
an existing destination. Export cannot replace the database or its WAL/SHM files.
The chosen JSON file contains fact content and remains the user's separate copy.

## Implementation and evidence

Services expose typed `FactPage`, `FactDeletePreview`, `FactDeleteRequest`,
`FactDeletion`, `ExportSelection`, `MemoryFormSnapshot`, `MemoryFormWrite` and
`MemoryFormSave` contracts. The host owns background execution/cancellation and
correlated replies. The native modules perform no database, filesystem or
embedding operations on the rendering thread. No model assets are loaded for
ordinary version inspection, explicit deletion, export or form autosave.

```sh
CARGO_INCREMENTAL=0 cargo test -p workbench-services memory_
CARGO_INCREMENTAL=0 cargo clippy -p workbench-services --all-targets -- -D warnings
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$PWD/target" cargo test \
  --manifest-path crates/desktop/validation/Cargo.toml --locked \
  --test memory-interactions
```

Focused service fixtures cover tied timestamps, scope isolation, stale previews
and cursors, selected and connected version deletion, including a deleted middle version, real sqlite-vec/FTS removal,
transaction rollback, in-flight SQL cancellation followed by an unrelated query,
idempotent retries, exact scoped export, restart recovery, new whitespace,
concurrent form writers, encoded bounds and migration. Native fixtures use the
actual host, SQLite and production GPUI controls for restore/autosave, correction,
rejected writes, switching projects with a queued mutation and newer typing,
confirmation cancellation/stale preview rejection, preservation of unsaved text
during chain deletion, and typing during close flush. Keyboard autosave tests
advance GPUI's virtual timer; they do not substitute a production polling loop.

These tests do not qualify retrieval quality, large-archive latency, physical
platform accessibility, or full installer behavior; those remain separate
benchmark and release gates. No external account, device or model is required by
these fixtures.
