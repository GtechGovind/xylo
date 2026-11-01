# Remaining parity work after the working-build milestone

The user requested the fastest workable consolidated default-branch build, followed by this remaining-work list. This milestone does not claim completion of the original 17-package plan.

- C — File snapshots with the 50 MB conversation cap; code/conversation/both rewind and conflict preview; conversation forks; edit-and-resend; double-Escape rewind picker. Forks must copy G's durable instruction provenance.
- K (partially implemented) — Native commit message/model drafting, Push, PR creation/status/checks and failing-job-log prompts. Connect the existing standalone VS Code extension and private discovery scanner to Xylo's MCP client and attach selections/diagnostics in the composer. Hunk actions, line comments, and review templates are the implemented slice.
- L — Fuzzy @ file/folder/range and MCP-resource mentions; ! shell output attachment; # scoped memory entry; prompt history/search; editable keybindings; optional Vim controls; filtered slash palette.
- M (partially implemented) — Bind statusLine.command to layered settings files. The existing app-saved command remains available. Background subagent accounting awaits N. Native search metering, worktree style preservation and process cleanup are included in this milestone.
- O — Next-prompt suggestions, expanded copy actions, resend/model retry/continue, transcript navigation/collapse, answer quotations, queued-prompt editing/reordering.
- N — Named agent discovery, per-agent model/effort/tool controls, background follow-up messaging and Stop, parent-history forks, depth/concurrency/token limits, worktree isolation and full child transcripts.
- P — Unified built-in slash registry, including compaction, init/review, usage/model/permissions, rewind/fork/resume/export, memory/hooks/agents/MCP, doctor/help. E already supplies direct /loop entrypoints; K supplies review prompt text.
- Q — Side-by-side chats, cross-session messaging, and small declarative fan-out/review/merge workflows.

Qualification still needed: live provider/account flows, real VS Code host attachment, live Docker/OAuth refresh, PDFium-backed PDF reads on an installed runtime, and release signing/feed ownership. Fixture and native transport checks are not claims of live-account verification.

Known dependency disposition: sandbox-helper's locked dependency graph has the previously documented Zod advisory; the pinned upstream runtime requires the older major. Cargo audit and the standalone IDE extension's npm audit are separate checks.

Process cleanup follow-up: positively acknowledge exit for host-owned local MCP inspection processes before releasing their managed checkout. Local MCP inspections and shell, hook, or status commands whose cleanup cannot be confirmed retain durable checkout markers across worker or app restarts. Commands with confirmed cleanup, including cancellation or a nonzero exit, release only their own marker. Retained checkouts require explicit Git removal after stopping the processes.

## Implementation decisions

These decisions preserve the working-build scope and record their tradeoffs:

1. Use all four available agent slots: one coordinator and three package agents. Work beyond those slots proceeds in batches.
2. Implement independent packages in isolated worktrees, as requested. Shared interfaces need explicit reconciliation and integration tests; a bad merge could otherwise lose behavior.
3. Run focused package tests and workspace checks during implementation, then the combined suite for the consolidated build. Cross-package failures may therefore surface during integration.
4. Serialize builds against the shared target directory and refresh local crate inputs when switching worktrees. This avoids incorrect cached metadata at the cost of rebuild time.
5. Use a transparent compiler workspace wrapper to distinguish agent artifacts from concurrent IDE checks while retaining shared dependency caching. It adds one workspace rebuild and changes no application compiler flags or IDE settings.
6. Follow the updated request for a working default-branch build and remaining-work list. Merge the implemented K/M slices without waiting for unstarted C and later packages; the original full parity plan remains incomplete.
7. Persist checkout ownership before command dispatch and retain it whenever cleanup is unconfirmed, including across app restarts. Local MCP inspection retains ownership until its exit can be positively confirmed. This prevents unsafe automatic deletion but requires explicit Git cleanup after stopping processes whose ownership remains retained.

## Smaller follow-ups

- Bound completed shell-job history in the desktop UI.
- Improve search paging/context grouping and keep notebook inserted-cell IDs stable between preview and application.
- Reconcile live retry-thinking fragments with the final stored reasoning; validate SDK turn identity/reuse.
- Add native LSP permission-grant inspection and revocation controls.
- Disambiguate colliding MCP prompt command names and add a real concurrent OAuth-refresh fixture.
- Retire inspection markers only after positive process cleanup; a manually removed and recreated checkout with the same name remains conservatively retained today.
