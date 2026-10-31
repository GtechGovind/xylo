# Xylo UI review, 2026-10-31

A live tour of every screen in a debug build. The build used the normal profile and a ChatGPT account and ran real prompts. Screens were compared with the Codex desktop app (`ref-codex-chat.jpg`) and Claude's desktop app. Screenshots in this folder are numbered by screen.

The aim is not to copy either app. Both share a few principles, and Xylo breaks them in many places:

1. **One reading column.** User turns, agent activity and answers share one centred column about 760 px wide. The composer is exactly as wide as that column.
2. **Work is summarised, answers are prominent.** Tool activity collapses into one line ("Worked for 32s ›"). Only the final answer is full-size text.
3. **One place per action.** Each command appears once, where it is expected. Shortcuts are not repeated as buttons.
4. **Quiet empty states.** An empty area says one short sentence, or nothing. Controls that can't do anything yet are hidden, not disabled.
5. **Settings are settings.** Tools such as recorders and transcribers belong in the workspace, not in settings pages.

## Chat screen (the weakest area)

| # | Finding | Screenshot | Decision |
|---|---|---|---|
| C1 | A multi-line command in a tool row spills over the rows above it. | 04 | Tool rows are single-line. A command shows its first line, ellipsised. The full text appears in the row's detail view. |
| C2 | Each tool row has a check mark, a chevron and a "…" menu. | 04 | Remove the per-row "…" menu. The row opens its detail, and copy actions live in the detail. Show the status icon only for failures or running rows. |
| C3 | The same file read twice shows as two identical rows. | 04 | Merge consecutive identical calls into one row with a count. |
| C4 | Activity summary line differs from Codex's clear "Worked for Xs" pattern. | 03, ref | When the turn finishes, the activity header becomes "Worked for 1m 12s ›" and collapses. While running, it shows the live phase and the last step. |
| C5 | Assistant answers carry a logo avatar that wastes a 24 px gutter and misaligns answer text with the tool rows. | 03 | Remove the avatar. Answer text and activity share one left edge. |
| C6 | No action row under a finished answer. | 03 | Show an action row on hover and under the latest answer: Copy, Retry, Read aloud, and the time. |
| C7 | User bubble is saturated indigo. | 03 | Neutral raised surface (Claude and Codex both use grey), max 80% width, with copy and edit on hover. |
| C8 | The composer placeholder reads "How can I help you today?" inside a conversation. | 03 | Use "Reply to Xylo…" in a conversation and keep the greeting only on home. |
| C9 | Content runs into the composer without separation. | 03 | Fade the transcript into the background above the composer. Leave a fixed bottom inset so the last line is never hidden. |
| C10 | The context ring looks like a loading spinner. | 08 | Draw a static progress arc with a visible track. Hide it below 5% usage. Its tooltip shows the percentage. |
| C11 | The model picker says "Reasoning: Default" while the composer says "Medium". | 09 | One source of truth. The chip shows the effective effort, and the picker's effort control shows the same value. |
| C12 | The "+" menu has "Paste image" (⌘V already works) and "Saved drafts" (also in the sidebar). | 06 | The "+" menu offers Attach files or images and Transcribe audio file. Remove the other two. |
| C13 | The header "…" menu repeats "New chat" (sidebar and ⌘N). | 05 | Header menu: Rename, Copy conversation, Export, Archive. |
| C14 | Sending opens the work panel at half the window width. | 02 | Never auto-open the panel when a turn starts. Open it only on request or when the user opens a diff or file. |
| C15 | Terminal toolbar has eight controls, including Copy and Paste buttons and "Opened in xylo" text. | 10 | Keep new tab, split and close. Font size and find move into a "…" menu. Copy and Paste use the system shortcuts. |

## Shell and workspace

| # | Finding | Screenshot | Decision |
|---|---|---|---|
| S1 | "Open project" row and the "+" next to Projects open the same dialog. | 12 | Keep "+" in the Projects header (tooltip "Open project…"). Remove the row. |
| S2 | "Filter chats" field duplicates "Search chats". | 12 | Remove the filter. Search becomes a ⌘K palette that filters titles instantly and searches message text below them. |
| S3 | Search opens as a full page with "First results / Next results" pager buttons and a blank empty state. | 11 | Modal palette over the workspace. Recent chats appear when the query is empty. Scrolling loads more, so the pager buttons go. |
| S4 | The chat context menu has only "Rename". | 13 | Rename, Pin, Archive. A hover "…" opens the same menu. Archive replaces Delete: the action audit is immutable by design, so chats are archived (hidden from the list and search, history kept) with an Undo toast. |
| S5 | The sidebar footer shows the model name, which repeats the composer chip. | 00 | The footer shows the account or plan instead. |
| S6 | The work panel's Subagents tab is always shown, usually empty. | 16 | Show the tab only while the chat has subagents, with a count badge. |
| S7 | Changes and Files show oversized empty-state headings in a narrow pane. | 14, 15 | Small muted single-line hints. |
| S8 | Saved drafts renders loose text lines, a pager and a docked terminal. The header keeps the previous chat's title. | 17 | Drafts list as rows (first line, project, age, attachments), opened from one place (sidebar). The page header reads "Saved drafts". Never dock the terminal on non-chat pages. |
| S9 | The Schedules page is reachable only from a notification. | — | Add a "Scheduled" sidebar entry, shown when any schedule exists. |
| S10 | Pages without a chat (Search, Drafts) still show the stacked terminal at the bottom. | 11, 17 | The work panel belongs to the chat page only. |

## Settings

| # | Finding | Screenshot | Decision |
|---|---|---|---|
| P1 | The Context page only links to Memory, Skills and Connections. | 32 | Remove the page. |
| P2 | The Agent page repeats the model picker and the Environments page. | 21 | Remove the page. Response timeout moves to General under Advanced. |
| P3 | Updates is two rows plus an unrelated "Stop project shells" danger zone. | 34 | Merge it into General as About. Stopping shells moves into the install confirmation. |
| P4 | Theme has no System option. | 20 | System, Light, Dark. |
| P5 | Voice is mostly tools: a recorder, a transcriber and a read-aloud box. | 26 | Keep only model and voice choices. Read aloud becomes a message action, and transcription moves to the composer "+" menu. |
| P6 | Image generation and Connections show a permanently open blank form. | 25, 27 | List plus an "Add" button, the same pattern as Providers. |
| P7 | The Environments empty state offers the same three options twice. | 29 | Keep the list only. |
| P8 | Skills says "No skills installed" although six built-in skills ship. | 30 | List the built-in skills. |
| P9 | Memory shows search, export and "Delete all" with nothing saved. | 31 | Show only the empty state until there are memories. |
| P10 | Providers shows "Needs setup" for a working ChatGPT account. Rows have both a "…" menu and a chevron. | 24 | Fix the status. Keep the chevron and move the actions into the detail page. |
| P11 | Notifications has a manual "Refresh" button for the permission. | 23 | Re-check the permission when the window gains focus. |

## Validation suite

The native validation suite had 14 failing targets during this review. UI id changes explain some failures, which are fixed alongside the redesign. Others point at host behaviour: fixtures that never shut down, and file tree, project search and draft loads that time out. Those are investigated separately.
