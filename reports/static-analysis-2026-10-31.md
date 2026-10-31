# Static analysis, 2026-10-31

Branch `feat/harness-research`, before merging to `main`. Three independent read-only reviews (backend correctness and security, desktop UI, performance), plus dependency analysis. Every confirmed finding below was fixed and covered by a regression test unless noted.

## Dependencies

- `cargo audit`: three advisories in `zip` 8.6.0 (RUSTSEC-2026-0367, -0368, -0369), pulled in by `docx-rs`. Xylo never extracted archives through it, but the dependency is removed: Word export now uses a small built-in writer (`capabilities/src/documents/docx_export.rs`) on `zip` 9. The audit reports no vulnerabilities. Eight "unmaintained" warnings remain, all transitive through GPUI's font and Linux D-Bus stacks.
- `cargo machete`: removed unused `async-trait` (evals), `hex` (distribution) and `ignore` (desktop). `image` stays in capabilities to enable PNG encoding for PDF previews and screenshots.

## Backend

| Severity | Finding | Fix |
|---|---|---|
| High | A cancelled or timed-out ChatGPT refresh could drop rotated tokens | Signing keys fetched before the exchange; after the token request, validation and save always complete |
| High | A tiny spreadsheet could request billions of cells | Streaming reader with row and column caps; shared-string and ODS pre-scans; legacy `.xls` refused |
| High | Remote MCP over HTTP buffered unbounded bodies | Bounded HTTP client with an 8 MiB cap |
| Medium | A steering database error ended the worker | Error answered to the request; the worker keeps serving |
| Medium | Transient errors permanently failed learning jobs and could stop learning | Owner-guarded failure, bounded retries, loop survives errors |
| Medium | Speech text could inject command-line flags; braces in text failed | One-pass template expansion with `--` before the text |
| Plausible | Input files were checked, then reopened by path | Opened once by handle, one no-follow component at a time; links are followed only while they stay inside the root (Unix). Other platforms keep a documented check-then-open |
| Plausible | Browser downloads stayed enabled after one download; a later navigation hung | Downloads denied by default, run in a throwaway page, byte-capped |
| Plausible | Extension calls lost results when closing failed; hooks could deadlock on large input | Result kept; stdin and stdout handled concurrently |
| Low | Settings snapshots could undo a concurrent project open | Ordered by command receipt |
| Low | Stale managed-endpoint entries blocked local inference after 256 unclean exits | Dead processes pruned |
| Low | Several install paths lacked size caps, locking or fsync | Capped reads, locked atomic writes, staged replacement |
| Found by the evals | Parallel helper agents could fail each other at shutdown | Helpers close only their own sessions |

## Desktop UI

| Finding | Fix |
|---|---|
| A continued answer showed its first part twice, also in copy and export | The answer is settled once on completion |
| The cut-off warning was folded away and lost on reload | Shown as the answer's status; stored as `MessageStatus::Truncated` |
| Archive acted on running chats, half-applied when switching was blocked, and could be undone by a stale page | Disabled while busy; switch first; pending archives filtered |
| Cmd-. could deny an approval that was not on screen | Same guard as the approve shortcut |
| Memory origins were worded for the model | People-facing wording; untrusted origins tagged |

## Performance

| Finding | Fix |
|---|---|
| The engine re-tokenised the whole history about six times per model call | Token costs cached per message; schemas counted once per turn |
| Every checkpoint rewrote the whole conversation with an fsync | One row per message; only the changed tail is written |
| Images were re-read, hashed and encoded on every model call | Verified and encoded once per turn |
| Masking parsed JSON in nested loops; Anthropic streaming was quadratic | Flags computed once; streamed text accumulated linearly |
| Schemas recompiled per call; tool search index rebuilt per call | Cached per tool and per broker |
| Blocking store and embedding work ran on the worker's async threads | Moved to blocking tasks |
| Desktop rows cloned and hashed message text every frame; streaming rebuilt text per delta; background chats and keystrokes redrew the window | Shared text cached; in-place streaming flushed once per frame; redraws only when visible state changes |

Release-build desktop measurements (p50, 10,000-message history): streaming frame 1.00 → 0.75 ms, history scroll frame 2.03 → 1.50 ms, cold start to first frame 411 → 295 ms. The "before" run included work in progress, so treat these as indicative.

Not changed: animation frames still re-render the single root view (GPUI marks every ancestor dirty), so spinners are capped at 30 fps instead; splitting the workspace into separately cached views is a larger restructure.
