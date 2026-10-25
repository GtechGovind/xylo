# Native document editing and copy preservation

The native document tools open UTF-8 text, DOCX paragraphs and XLSX typed cells,
and save explicit edits to a new copy. They preserve the source file. This is a
conservative editing surface rather than full Office layout, rendering, formula
calculation or object editing. Unsupported structures are protected explicitly;
the backend does not flatten them into a replacement document.

No Python, Node, Office installation, cloud model, macro engine or automatic
asset download is involved. Existing PDF extraction/preview and read/extract
support for other spreadsheet formats remain separate. XLSM, XLSB, XLS, ODS and
PDF are not editable through these copy operations.

## Contracts

`workbench_core` exports app-owned document types. Native helper capabilities
return typed JSON inside `ToolObservation.summary`:

- `document_open`: `DocumentOpenRequest` → `DocumentSnapshot {source,content}`.
  `source` contains the canonical path, SHA-256 `revision`, byte length and kind.
- `document_save_copy`: `DocumentSaveRequest {operation_id,source,name,edits}` →
  `DocumentSaveReceipt {operation_id,state,result,message}`. The portable filename `name`
  (Unicode/spaces and an optional matching extension) creates a new artifact; it never overwrites a source or existing artifact.
- `document_save_status`: `{operation_id}` → the same durable receipt, without
  dispatching another copy write.

Open requests use optional `expected_revision` to reject paging a changed source.
DOCX uses zero-based `offset` and `limit` (default 100, maximum 200). XLSX uses
`sheet`, zero-based `row_start` / `column_start`, `row_count` (default 64, maximum
200) and `column_count` (default 16, maximum 64), with at most 4,096 page cells.
Every coordinate in the requested rectangle is returned, including typed blank
cells and protected cells. `total_rows` / `total_columns` describe used extents;
requests beyond used extents can append data up to Excel's hard coordinates.

An editable page either fits the configured response budget completely or fails
with an explicit instruction to request a smaller page. It is never silently
truncated or resized. A focused 1×1 request can retrieve an entire 32,767-character
cell. The UI must track actual returned coverage and retain dirty patches outside
its bounded page cache.

The helper keeps one revision-keyed metadata/shared-string cache. Each page still
checks the source content hash, but materializes only the selected worksheet and
the requested cell values. Unrelated large sheets are not reparsed into cells on
each scroll. Selected-sheet scanning remains bounded; this is not a claim of
constant-time paging for arbitrary workbook sizes.

`DocumentEdit` is tagged by `kind`:

```json
[
  {"kind":"text","text":"Complete UTF-8 document"},
  {"kind":"paragraph","index":2,"text":"Revised paragraph"},
  {"kind":"cell","sheet":"Sheet1","address":"B3",
   "value":{"type":"number","value":"9007199254740993"}},
  {"kind":"cell","sheet":"Sheet1","address":"C3",
   "value":{"type":"formula","expression":"=SUM(A1:B3)","cached":null}}
]
```

Only edits matching the source kind are accepted. Coalesce repeated text,
paragraph or sheet-coordinate edits before saving. Cell types are `blank`,
`text`, `number`, `boolean`, `formula`, `error` and imported `date_time`. Numbers
retain their decimal lexical value, avoiding conversion through an imprecise
floating-point representation when writing. Validation still requires a finite
spreadsheet number. Formula expressions include the leading `=`. Source
formula caches are last-saved values; the app never evaluates them.

## DOCX preservation

The editor scans `word/document.xml`, including paragraphs inside tables. A text
change uses the smallest character difference and must fit one existing text
run. Paragraph properties, run properties, surrounding runs and hyperlink
relationships stay intact. Empty ordinary paragraphs can receive a new text run.
Unchanged ZIP entries are raw-copied, preserving their compressed bytes rather
than rebuilding the document from paragraphs. Styles, images, links, headers,
footers, custom package parts and other unaffected entries remain as supplied.

Replacing text across differently styled runs or hyperlink boundaries is rejected
with a preservation explanation. Paragraphs containing or belonging to an image,
embedded object, field, tracked change, content control, special symbol, note
reference, nested object, tab or line break are protected. Existing unrelated
objects remain in the copy; this surface does not remove them to make an edit
possible. Digitally signed Office packages are rejected because an edit would
invalidate their signatures. Use a signature-aware Office editor for those files.

## XLSX preservation and formula semantics

The backend reads workbook relationships and ordinary worksheet XML. Multiple
worksheets are listed without flattening them into one grid. Patches target a
specific sheet and A1 coordinate, preserving original cell style attributes and
all unrelated cells, rows, worksheets, relationships, shared strings, styles and
other package parts. Missing cells/rows can be inserted in coordinate order, and
used dimensions extend to include new data.

Merged ranges, shared/array/data-table formulas, rich-text cells, linked/dynamic
cell metadata, protected sheets and structured-table worksheets are conservative
read-only boundaries. Formula edits in workbooks with a calculation chain are
also rejected. ISO date cells are displayed with their original type and protected
from implicit conversion; numeric date serials remain typed numbers with their
existing styles. Chart sheets and arbitrary Office object manipulation are not
part of this editor.

Explicit normal formula edits write a formula only when `value.type` is
`formula`; cached results are discarded for that edit. Text beginning `=` remains
an inline string. Workbook recalculation-on-open is requested after spreadsheet
patching, while unrelated formula caches/package content are preserved. The app
runs no formulas or macros; displayed caches can therefore be stale after edits
until an external spreadsheet engine recalculates the copy. Existing
`document_export_xlsx` also keeps formula-looking strings as text by default.

## Bounds, cancellation and publication

| Boundary | Limit |
| --- | --- |
| Source file | 64 MiB |
| Office package | 10,000 entries; 128 MiB total decompressed; 32 MiB per part |
| XML structure | 250,000 elements per part; depth below 128; no DTD/entities |
| Editable worksheet | 100,000 materialized cells per worksheet; 256 ordinary sheets; 100,000 shared strings |
| Sheet coordinates | 1,048,576 rows × 16,384 columns |
| Worksheet page | 4,096 cells; 200 rows; 64 columns, also bounded by response bytes |
| Edits per copy | 4,096 coalesced edits and 1 MiB JSON; DOCX at most 512 paragraphs |
| Text / paragraph / cell | 512 KiB whole UTF-8 document; 64 KiB paragraph; 32,767 cell characters |
| Output artifact / receipt | 128 MiB artifact; 32 KiB receipt |

Encrypted, duplicate, unsafe or symbolic-link ZIP parts, excessive compression
ratios, malformed XML and unsupported package structures fail explicitly. The
parser uses bounded parts and never expands archives onto arbitrary paths.

The host reserves 384 MiB for native document operations. Blocking parse/I/O runs
on helper blocking workers. A cross-process `.document-admission.lock` serializes
native document parsing and publication; waiting is cancellable and bounded to
30 seconds. The blocking worker retains this lock even if its client cancels its
request and releases the host's resource reservation. Native codec/OS calls are
not forcibly interrupted mid-call; cancellation is checked between bounded
read, parse, edit and publication stages.

A completed copy is staged on the same artifact filesystem, synced, and published
with atomic no-clobber persistence. Legacy DOCX/XLSX exports now use this same
staging boundary. The source's exact content hash is checked again immediately
before publication. External source changes cause a revision conflict and leave
the original untouched. This is a just-before-commit hash check, not a claim that
uncooperative external writers cannot change their file after the check.

## Interrupted-write reconciliation

Every copy request has a durable UUID and request digest. Receipts are stored in
`artifacts/.document-operations/UUID.json`; they contain identity, hashes, paths
and outcomes, not raw edit text. An exclusive operation lease protects a live
writer. Reusing a UUID with changed edits, source or name is rejected. Reusing the
same UUID returns/reconciles its previous receipt instead of writing again.

Before publication, the receipt records the exact expected output path/hash.
After publication it becomes `committed`. If the response or process is lost,
`document_save_status` checks the live lease and actual output: a matching copy
is reconciled as committed; definitely absent output is cancelled; conflicting
output remains `outcome_unknown`. For a missing receipt, a live operation lease reports `prepared`. If no writer
owns the ID, status durably cancels/tombstones it before reporting no copy, so a
delayed queued request cannot publish later. Preserve the pending UUID and reconcile it rather than
inventing another name or operation automatically.

Cancellation may race a completed publication. The backend waits for its blocking
operation and returns the actual committed/cancelled outcome; it does not claim
cancellation while its own future can later publish. A client process can still
lose that response, which is why the durable status operation remains required.

## Backend qualification

Offline Rust fixtures exercise real DOCX/XLSX ZIP packages, raw preservation of
unaffected compressed entries, table paragraphs, styled runs, hyperlinks/media,
multi-sheet typed/formula patches, exact large decimal lexical values, merged
protection, formula-looking text, source conflicts, operation reuse/recovery,
response budgets and cancelled/no-clobber staging. These tests do not establish
Microsoft Office/LibreOffice layout fidelity, signing workflows or physical
platform acceptance. Unsupported edits remain explicit product boundaries.

```sh
CARGO_INCREMENTAL=0 cargo +stable test -p workbench-capabilities documents
CARGO_INCREMENTAL=0 cargo +stable clippy -p workbench-capabilities --all-targets -- -D warnings
```

## Native document editor

Artifacts now includes **Open document**, **New Word document**, and **New
spreadsheet**. Creation makes a uniquely named artifact and opens the same
structured editor used for imported files. PDF preview stays separate. File and
artifact lists route DOCX/XLSX into the editor; text documents can be imported
with Open document. The editor never modifies the imported source.

The spreadsheet uses GPUI Kit's native table with virtual rows and columns,
worksheet tabs, row/column headings, native arrow-key navigation, double-click
editing, and an address field such as `C70`. Values can be automatic, text,
number, boolean, formula, or blank. ISO date/time cells remain protected with
an explanation. The value field retains the whole
cell while grid labels show at most 256 characters. The editor exposes the used
extent plus 64 rows / 16 columns for adding data, within Excel's bounds. Protected
cells remain selectable with an explanation, but cannot generate edits.

Pages normally request 64×16 cells. An oversized response automatically retries
4×4, then 1×1; DOCX paragraph pages likewise shrink to fit. The sparse cache
tracks actual returned coordinates, never treats an unloaded cell as blank,
retains at most eight blocks and approximately 16 MiB of cell payload, and keeps
dirty patches separately. Requests are correlated to the document generation,
source revision and worksheet. Cancelling loading pauses automatic requests;
Resume loading retries them. Revision changes reject subsequent paging and
copying instead of silently rebasing edits.

Word paragraphs are paged and selectable, with a native multiline editor for
the selected paragraph. The helper decides whether its existing runs and
objects permit the edit. This surface does not render Word pagination or
provide a complete Office layout editor.

Recovery is separate from conversation drafts and source files. Each document
has a scoped CAS recovery row containing its original revision, coalesced typed
edits, copy name, worksheet, cell selection and any pending copy operation.
Autosave debounces for 300 ms; continued typing stays enabled while a previous
write is pending. Conflicts require **Use saved edits** or **Keep these edits**.
A project/document switch waits for the recovery acknowledgement. Quit and
update use the same acknowledged flush path; new typing cancels a delayed
close intent. Local recovery is bounded to 6 MiB per document and 64 MiB in
aggregate, and size/write errors keep the editor open with a retry action.

**Save a copy** (also Cmd/Ctrl+S on Artifacts) first persists the exact copy
intent, then dispatches its stable operation ID. A lost response or restored
pending operation exposes **Check saved copy**; this reads its durable receipt
without issuing another write. Cancellation retains the operation until its
result is known. Newer typing stays in the proposal against the imported
original, including after a successful copy. On source-revision conflict the
complete recovery state and receipt controls remain available. Users can copy
recovered edits, or explicitly confirm discarding only those recovered edits
before reopening the latest source; a pending save must be reconciled first.

The native `document-interactions` target exercises the actual host, helper,
SQLite recovery and native controls with temporary artifacts. It covers typed
XLSX copy/reopen, multi-sheet paging and protected merges, native cell keyboard
navigation, Word/spreadsheet creation, canonical alias imports, dense-cell
response fallback, receipt reconciliation after controller restart and source
replacement, and typing/project-switch/Quit races during a delayed DB write.
Headless native tests do not establish visual, IME or accessibility acceptance
on physical macOS, Windows or Linux desktops. No account or OS permission
prompt is used by these fixtures.

## Missing or moved originals

**Recent documents and recovered edits** opens the acknowledged draft independently
of its original file. If that file is missing, unavailable or changed, Workbench
keeps the saved proposal visible instead of waiting for a successful Office parse.
The native view lists every retained change in a virtual list. Selecting one shows
its full text, paragraph location, or worksheet/cell/value; formula previews retain
the expression and distinguish any old cached value from a fresh calculation.
**Copy this recovered change** copies those recovery notes. It does not reconstruct
an Office document or claim to include unchanged content that was not saved.

**Check saved copy** uses the original operation receipt even if the original file
has disappeared. A pending copy must be reconciled before its source can be
relinked. Existing copy operation IDs, arguments, retained edits and no-clobber
publication rules remain unchanged.

**Find moved original** chooses a file inside the current project or application
artifacts. Native `document_open` first checks the saved source revision. The UI
also requires the same document kind and byte length and asks for native
confirmation. A file with different bytes, format, or revision is refused while
all saved edits remain in place. Put an original moved outside those authorized
roots back inside the project before locating it. A changed or absent original
cannot be replaced by a silently synthesized DOCX/XLSX package.

The complete `DocumentDraftRelinkRequest` is persisted in the old draft before it
is sent. `document_draft_relink(request_id, request)` then independently verifies
the candidate and atomically tombstones the old draft, creates the proposal at
the new key, and records a compact immutable receipt. The transaction checks the
exact original revision/request, disallows an unresolved copy and refuses a
destination with its own saved proposal. It preserves edits, copy name, worksheet
and selection. Replaying the same operation returns its receipt without replacing
newer destination edits; changing its arguments is rejected. A clean cached
destination is invalidated and reloaded after acknowledgment; dirty or in-flight
cache state is retained for explicit review.

`document_draft_relink_status(request_id, original_key, operation_id)` reads that
same receipt. A missing acknowledgment retains the exact request; **Check saved
location** does not invent a replacement operation. After application restart,
Recent documents points to the authoritative new row when a move committed, or
to the original pending proposal when it did not. **Stop waiting** and **Keep
previous location** do not move a file or undo an already committed relocation.
Withdrawing a pending proposal uses the normal draft CAS; a concurrent committed
move produces a conflict and preserves the new record. Choosing its saved state
returns the user to Recent documents rather than recreating the old proposal over
it. Conversation, memory and other document drafts are not changed.

Verification reads stay off the UI thread, use the profile-wide native resource
pool, hash the opened regular-file descriptor, and enforce a 64 MiB source bound.
Unix traversal uses anchored no-follow directory handles; application management
files are excluded as for normal document inputs. Cancellation and a ten-second
soft deadline are checked between 64 KiB chunks and before the atomic write.
These checks cannot interrupt an individual blocked filesystem syscall. The
reservation stays owned by the real blocking task until it stops. Normal helper
open and save-copy recheck the source revision after relocation, so a later file
change remains a conflict rather than permission to copy different content.

The focused `document-recovery-interactions` target uses real Host/Store/helper
flows for missing-source inspection, a committed copy whose original was deleted,
wrong-source refusal, moved DOCX/XLSX copies with unrelated package parts and
formulas preserved, lost acknowledgment/restart, cached destination reload, and a
withdrawal racing an already committed move. `document-recovery-rendering` is an
offscreen capture of an actual missing-original XLSX draft. It writes
`document-recovery.png` and source/image hash metadata in `WORKBENCH_RENDER_DIR`
(defaulting to the repository's `docs/native-preview`). It establishes a scoped
native layout fixture, not physical input, platform-wide Office fidelity, or full
release acceptance.
