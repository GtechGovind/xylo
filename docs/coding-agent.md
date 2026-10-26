# Coding agent

The task worker runs the agent loop with application-owned tools. Every tool call is journaled before it runs; approvals and permission modes are enforced by the broker, not by the model.

## Tools offered on every turn

| Tool | Purpose | Approval |
| --- | --- | --- |
| `file_read` | Numbered lines from a 1-based `offset`, up to 2000 lines or 40 KiB per call, with `next_offset` for continuation. Also reads a saved `full_result`. | No |
| `file_edit` | Exact `old_string` → `new_string` replacement. It must match once unless `replace_all` is set. CRLF files keep their line endings. | Yes |
| `file_write` | Creates or replaces a whole file (2 MiB limit), keeping existing file permissions. | Yes |
| `files_list`, `find_files` | One directory, or glob discovery (`**/*.rs`) that respects `.gitignore`, newest first. | No |
| `search_project` | In-process ripgrep engine: regex or `literal`, `glob`, `path`, `context`, `case_insensitive`, `files_only`. No external `rg` is needed. | No |
| `git_status`, `git_diff` | Branch and short status; working tree or `staged` diff, optionally for one `path`. | No |
| `run_command` | Program and argument array, no shell. Windows `PATHEXT` launchers such as `npm.cmd` resolve. | Yes |
| `shell_command` | A command line through PowerShell on Windows, or bash/sh elsewhere. | Yes |
| `todo_write` | A visible checklist for multi-step work. | No |
| `agent_delegate` | A supervised child agent for self-contained subtasks. | No |

Specialised capabilities (documents, browser, environments, images, connectors) remain available through `search_tools` and `call_tool`.

## Outcomes

- A command that runs to completion is a definitive observation. A non-zero exit returns `exit_code`, `stdout` and `stderr` to the model and is journaled as failed, not as uncertain. Timeouts, cancellation and lost results remain `outcome_unknown`.
- A missing program, an invalid path or an edit whose text does not match is rejected before any intent or approval exists.
- The model receives up to 64 KiB of each result. Larger results keep both ends of every long field and name a `full_result` text file that `file_read` can page through. UI events stay at 8 KiB.

## Permission modes

| Mode | Asks before |
| --- | --- |
| Ask before changes (default) | Every file change, command and external write |
| Auto-accept edits | Commands and external writes; project file edits apply directly |
| Edits and commands | External writes only (connectors, plugins, environments, memory) |
| Read only | Nothing runs that writes; writing tools are refused |

The mode is chosen in the composer and applies to the next message. An approval card shows the actual diff or command line. **Always allow** remembers a rule for the rest of the chat session: file edits, a command prefix such as `cargo test`, one exact shell command, or one connector tool. Remembered rules never widen Read only.

## Context

- The system prompt states the workspace, OS, shell, branch and date, and includes `AGENTS.md` and `CLAUDE.md` files from the repository root down to the workspace (24 KiB in total; deeper files take precedence).
- When a request nears 80% of the configured context, older history is replaced by a model-written summary placed in the system message. The summary is reused while it fits, so the transmitted prefix stays stable between steps; bounded excerpts remain the fallback.
- Token estimates use the model's exact encoding where known, otherwise o200k counts with a 1.3 safety factor.
- Anthropic requests carry prompt-cache breakpoints on tools, system policy and the newest message.
- One turn may make up to 200 model calls; delegated children may run for 15 minutes.

## Keyboard

| Shortcut | Action |
| --- | --- |
| Ctrl/⌘ + Shift + Enter | Allow the oldest pending action |
| Esc or Ctrl/⌘ + . | Deny the oldest pending action, otherwise stop the task |
| Ctrl/⌘ + L | Focus the message box |

Text inputs keep plain Escape for their own use, so Ctrl/⌘ + . is the reliable way to stop from the message box.
