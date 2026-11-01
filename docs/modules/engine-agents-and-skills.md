# Helper agents, skills and prompts (`xylo-engine` + `xylo-services::extensions`)

## Purpose and boundaries

This page covers the parts of [engine](engine.md) that shape *what the model is told* and *who else works on a task*: delegated helper agents (`crates/engine/src/subagent.rs`), the built-in skills (`crates/engine/skills/*`), prompt rendering (`crates/engine/prompts/*.md` and `crates/engine/src/prompts/`), and how installed skills, plugins, MCP servers, hooks and plugin agent definitions are discovered and invoked. The engine side is `tools/portable.rs`, `tools/hooks.rs` and `tools/project_discovery.rs` (see [engine-tools](engine-tools.md)); the discovery, catalog and hook-runner side is `crates/services/src/extensions/*` and `crates/services/src/hooks.rs` (see [services](services.md)). User-facing summary: [agents](../../README.md). Wire adapters are in [engine-providers](engine-providers.md).

## Module map

| Path | Purpose |
| --- | --- |
| `crates/engine/src/subagent.rs` | `Delegation`: runs one helper loop for an `agent_delegate` call |
| `crates/engine/src/prompts/mod.rs` | `PromptContext`, `PromptBudget`, `system`, `subagent`, note builders |
| `crates/engine/src/prompts/instructions.rs` | AGENTS.md / CLAUDE.md discovery, `@imports`, `.claude/rules`, `ScopedInstructions` |
| `crates/engine/src/prompts/skills.rs` | `RAW_SKILLS`, skill parsing, `skills_index` |
| `crates/engine/src/prompts/output_styles.rs` | Appends the selected output style to the system prompt |
| `crates/engine/prompts/*.md`, `crates/engine/prompts/README.md` | Every prompt text; the README is the layout reference |
| `crates/engine/skills/<name>/SKILL.md` | The six built-in skills |
| `crates/engine/src/tools/portable.rs` | `extension_skill`, `extension_resource`, `extension_component`; skills index entries |
| `crates/engine/src/tools/hooks.rs` | Hook execution around tool calls and lifecycle events |
| `crates/engine/src/tools/project_discovery.rs` | `.mcp.json` connectors; `@server:uri` and `/mcp__…` notes |
| `crates/services/src/extensions/` | `ExtensionService`: catalog, manifest inspection, project discovery, skill loading |
| `crates/services/src/hooks.rs` | Settings hooks: events, matching, sandboxed run, output parsing |

## Helper agents (`subagent.rs`)

`agent_delegate` (arguments `task` required, `context` optional; `tools/builtin.rs`) hands a bounded, self-contained subtask to a supervised child. `Delegation::handler()` is installed on the parent broker with `set_delegate`; the broker calls it on the blocking thread that runs the parent's tool call, which `block_on`s `Delegation::run`.

### Guarantees

- **Depth 1.** `ToolBroker::execute` rejects `agent_delegate` from a child namespace ("Child agents cannot delegate further"), and the child's tool list omits it.
- **Children cannot switch the checkout.** `worktree_enter` / `worktree_exit` fail with "Child agents cannot change the parent chat's checkout".
- **At most `MAX_CONCURRENT_CHILDREN = 2` run at once per turn.** Extra calls wait on the `slots` semaphore (or fail with "Delegation cancelled" if the turn is cancelled while waiting).
- **Each child is bounded by `CHILD_TIMEOUT` = 15 minutes.** On expiry the child's token is cancelled, its in-flight tools settle, and the result is `{"status":"timeout","result":"The helper agent did not finish within 15 minutes."}`.
- **Parent cancellation cancels helpers.** A cancelled child while the parent token is cancelled makes the call fail with "Delegation cancelled"; the child broker is shut down either way (`child.shutdown()`).
- **The answer is bounded.** `bounded()` cuts the final text at a character boundary to `EffectiveWindow::subagent_result_bytes()` (about 6 % of the effective window, between 768 and 8,192 tokens; about 10 % and at most 768 tokens in a tiny window) and appends `[answer truncated at N bytes]`.
- **Results are `{"status":"completed"|"timeout"|"error","result":…}`.** A helper failure that is not a cancellation returns `status: "error"` with the error text, not a tool failure.

### What a child gets

- A fresh conversation: one user message made of `task` plus, after a blank line, `context`. **It never sees the parent conversation.**
- `prompts::subagent(&context, task)` as system prompt (`subagent.md`, `subagent-task.md`): parent's environment, the live permission mode and project instructions budgeted with the same window scaling (`PromptContext::detect(…, window.prompt_budget())`), no skills or facts index. Instruction sources are recorded with `record_instruction_sources`.
- A scoped child broker (`ToolBroker::scoped_child`): shares the journal, permission cell and approvals with the parent; its tool events carry the parent action as `parent_id`; its actions are namespaced under `parent_key` (see [engine-tools](engine-tools.md)). It runs `AgentLoop` with `MAX_MODEL_CALLS`, no turn journal and the parent's provider, budget and `max_tokens`.
- Its report format is fixed by the prompt: answer first, then evidence (paths with line numbers, commands and results), then what could not be determined.

### `allowed_tools`

The model-facing `agent_delegate` schema has no `allowed_tools`. The argument is set only by the engine for skills (below). `ToolBroker::restrict_tools` accepts exact host tool names plus the aliases `Read` (`file_read`, `extension_resource`), `Write`, `Edit`, `Glob` (`find_files`, `files_list`), `Grep` and `Bash` (`shell_command`, `run_command`); anything else fails with "Unsupported child tool constraint" rather than being widened. Excluded tools are added to the child's `policy_excluded`.

## Built-in skills (`crates/engine/skills/*`)

Six skills are compiled in with `include_str!` through the `skill!` macro and listed in `RAW_SKILLS` (`prompts/skills.rs`): `code-review`, `create-skill` (ships `references/format.md`), `debug-failure`, `git-commit`, `refactor`, `write-tests`. Each folder holds `SKILL.md` (frontmatter `name`, `description`, optional `allowed-tools`, then Markdown) plus optional resources.

- `parse` rejects a skill whose `name` is not a valid name or whose description exceeds **1024 characters** (`SkillError::DescriptionTooLong`); an invalid skill is logged with `tracing::error!` and skipped, not fatal.
- `bundled_skills()` / `bundled_skill(name)` expose them; `BundledSkill::resource(path)` returns a bundled resource.
- `allowed_tools` is **informational**: invoking a built-in skill never grants permissions and the bundled skills set none. They run inline with the task's normal tools.
- A new skill needs a folder and a `skill!(…)` line in `RAW_SKILLS`; the prompts tests check every folder is registered, parses, references its resources correctly, names only real tools and fits the index.

## Prompts (`crates/engine/prompts`, `src/prompts`)

All model-facing instruction text lives in Markdown under `crates/engine/prompts/` and is reached only through `crate::prompts`. `render.rs` substitutes `{{snake_case}}` placeholders in one pass (inserted values are never rescanned) and rejects a missing value, an unused value or a malformed `{{`. `prompts/README.md` lists every file with its API.

### System prompt variants

| Variant | Template | When |
| --- | --- | --- |
| Full | `system.md` + `environment.md` + `permissions.md` and `permissions/<mode>.md` | Normal windows |
| Compact (tiny window) | `system-tiny.md` + `permissions-tiny.md` | `PromptBudget::compact`; names only the resident tools and points at `search_tools` / `call_tool` |
| Helper | `subagent.md`, `subagent-task.md` | `prompts::subagent` (above) |
| Output style | `output-style-explanatory.md`, `output-style-learning.md`, or the style's own prompt | `output_styles::apply` appends the explicitly selected style (`default` appends nothing) |

- `PromptContext::detect` fills workspace, OS, arch, shell, Git state, the UTC **day** (`utc_date`, never finer), the permission mode, project instructions and their canonical sources. **Two equal contexts render byte-identical prompts**, which keeps provider prompt caches warm; the permission mode can change mid-task through the `permission-change.md` note instead of a new system block.
- The skills index is set with `PromptContext::set_skills_index`, from `ToolBroker::project_skill_context` (called in `engine.rs` `system_prompt` and after a worktree switch).
- In a compact context, project instructions and the skills index are included only if they fit whole (budgets of 8 % and 4 % of the input budget per `prompts/README.md`); otherwise they are left out, recorded in `PromptContext::omitted` and named by `system-omitted*.md`. Full contexts truncate instead.
- Per-turn extras are not in the system prompt: the recalled-context note (`recalled-*.md`: facts, learned procedures, and installed skills the prompt invoked by name) and MCP notes are appended as one application note after the user's message (`engine.rs`).
- Loop notes (`step-limit.md`, `step-budget.md`, `stall-*.md`, `continue-answer.md`, …) are described in [engine](engine.md); compaction and masking prompts in [engine](engine.md) (`provider_guard`).

### Project instruction discovery (`instructions.rs`)

`project_instruction_sources(workspace, budget)` returns the text and the canonical contributing files.

- **Files per directory:** `AGENTS.md`, `CLAUDE.md`, `AGENTS.local.md`, `CLAUDE.local.md` (`INSTRUCTION_FILES`), in every directory from the repository root (nearest ancestor with `.git`) down to the workspace; outside a repository only the workspace.
- **User files:** `~/.claude/CLAUDE.md` and `~/.xylo/AGENTS.md` are added too (read from `HOME`).
- **Order and budget:** sections are rendered outermost first so the nearest file comes last and wins. The budget is `min(budget, INSTRUCTION_BYTES = 24 KiB)`, **spent nearest first**: outer files are truncated (`[… truncated]`) or omitted (`[Outer instruction files omitted: size limit reached]`), never the nearest. Below `MIN_FILE_BYTES = 256` remaining, further files are omitted.
- **Duplicates:** empty files and files whose expanded text equals one already included (for example `CLAUDE.md` symlinked to `AGENTS.md`) are skipped.
- **`@imports`:** a whole-line `@path` (a line that is a single word starting with `@`) is replaced by the imported file's text. Relative paths resolve from the importing file, `~/` from `HOME`. Imports are skipped inside fenced code blocks, limited to depth 5, each file expanded once, and **targets must canonicalize under the workspace or `HOME`**. A missing or rejected import is dropped silently. Every read is size-bounded.
- **Provenance:** the contributing sources (including imports) are recorded with `record_instruction_sources` before the text reaches a model; the wrapper (`system-project-instructions.md`) tells the model files run root to workspace, the nearer file wins on conflict, and direct user instructions override them.

### Scoped instructions and rules (`ScopedInstructions`)

Held by the broker (`scoped_instructions`), it adds guidance **lazily when a tool touches a path** (`touch_with_sources`), once per source per turn and within the same 24 KiB budget (minus what the system prompt already used):

- instruction files in directories between the workspace root and the touched path (exclusive of the root, already in the prompt);
- rule files `*.md` in `.claude/rules` and `.xylo/rules` (at most 256 per folder, 2 MiB snapshot per turn and root). A rule without frontmatter always applies; with a `paths:` list (quoted list, comma list or YAML list) it applies only when a glob (`literal_separator`) matches the touched path.

Paths outside the workspace root yield nothing, and a root change resets the state.

### Skills index (`skills_index`)

`ToolBroker::project_skill_context` builds entries from the built-in skills, installed portable skills the model may invoke itself (`portable::index`), and one "Not a skill: an installed … component" line per enabled MCP or language-server component. `skills_index(entries, budget)`:

- renders `- name: description`, flattened to one line (names to 80 chars);
- the budget is clamped to `[128, SKILLS_INDEX_BYTES = 8 KiB]` and scaled to the window by the caller (compact contexts get the 8 KiB budget so the index is taken whole or left out);
- descriptions are cut to **400 chars**; if the index would exceed the budget they are cut to **100 chars**; if it still does, the remaining skills are replaced by `- (N more skills not listed)`.

## Skills and plugins: discovery (services side)

`ExtensionService` (`crates/services/src/extensions/`) owns the catalog under `<app data>/extensions`; the engine builds it per call in `tools/portable.rs::service` and scopes it with `for_workspace(discovery_root)`. **Discovery never executes package code**, installed resources are immutable snapshots keyed by a package digest (`package_root` = `packages/<digest>`), and readiness is per component (`Installed`, `NeedsSetup`, `UnsupportedRequirement`, `ExecutionVerified`).

### Sources

- **Installed packages** (`install_local`, marketplace sources in `sources.rs`/`marketplace.rs`): `inspect` (`manifest.rs`) reads an optional manifest (`.codex-plugin/plugin.json`, `.claude-plugin/plugin.json` or `.agent-plugin/plugin.json`, at most 256 KiB), a root `SKILL.md`, and the `skills`, `commands` and `agents` folders (plus manifest-declared paths, which must pass `safe_path`). It also records `mcpServers` (`.mcp.json`), `hooks` (`hooks/hooks.json` or `hooks.json`), `lspServers` (`.lsp.json`) and `apps` (`.app.json`) as components with `NeedsSetup` readiness. A package with no components, or with duplicate skill/command/agent names, is rejected.
- **Project and user folders** (`discovery.rs::discover_project`): for the canonical workspace (scope `project`) and `HOME` (scope `user`), the `.claude` and `.xylo` folders are copied (only `skills/` and `commands/`) into a staging snapshot and installed as `project:<root>/<folder>` / `user:…` packages. Newly discovered or changed packages are **not enabled**; enabled packages whose source disappeared are disabled. Agents are deliberately excluded from this path. `ToolBroker::new` calls `discover_workspace` for every task and emits a status ("Some project packages need attention in Plugins settings.") if it fails; `runtime/src/extensions.rs` also calls it.
- `in_scope` hides `project:` packages that belong to another workspace.

### Skill metadata and automatic invocation

`skill_metadata()` lists enabled, in-scope components of kind skill, command or agent that are not `UnsupportedRequirement`. `automatic` (model may start it, and it appears in the skills index) is `!disable-model-invocation && kind != Agent`; `user_invocable` defaults to true. A Codex sidecar `agents/openai.yaml` is parsed as harness input: `policy.allow_implicit_invocation: false` disables automatic use, a `products` list that omits `xylo`, unknown keys and tool dependencies become `host_requirements` (reported, not executed). Frontmatter read limit is 256 KiB; skill arguments are limited to 64 KiB.

### Plugin agent definitions

An `agents/*.md` file is an `Agent` component. It is **never listed in the index** (not automatic) and runs only as a forked, supervised child:

- a skill or command selects it with `agent: <name>` and `context: fork`; otherwise the component is marked `UnsupportedRequirement` ("Agent selection requires a forked skill or command"). Built-in role names `Explore` and `Plan` (tools `Read, Grep, Glob`, "do not change files") and `general-purpose` are available without a definition; any other name must be an installed agent component whose readiness is `Installed`.
- the role's instructions are prepended to the skill body; role `tools`/`allowed-tools` replace the skill's list, or intersect with it when the skill restricts tools; a role `model` applies when the skill's is unset or `inherit`.
- **unsupported:** nested agent selection from an agent, and agent-scoped `hooks` (`load_skill` fails "Agent-scoped hooks require explicit lifecycle setup").

## Skills and plugins: invocation (engine side)

Three tools in `tools/portable.rs` (all journaled and permission-gated like any tool; see [engine-tools](engine-tools.md)):

- **`extension_skill {id, arguments?}`:** built-in names resolve first (`prompts::bundled_skill`) and return `{skill, description, instructions, resources, resource_tool, permissions}`. Installed skills use `<package>:<name>` and `ExtensionService::load_skill`, with `automatic = false` only when the user's prompt starts with `/name` or `$name` (or `package:name`) for that skill (`explicit_invocation`). A skill with `disable-model-invocation` therefore fails "Skill requires explicit user invocation" unless the user typed it; a skill with `user-invocable: false` cannot be started by the user.
- **`extension_resource {package, path}`:** a built-in skill's bundled resource, or a file of an enabled package (`safe_path`, at most 256 KiB, else "use a scoped file tool").
- **`extension_component {id, component, operation, input?}`:** `status`, `verify`, `tools/list`, `tools/call`, `request`, `event` for configured local MCP, language-server and hooks components; `writes: true`, executions follow the task's permissions and require the component to be configured for the current workspace.

Loading rules for an installed skill:

- **Arguments:** `$ARGUMENTS`, `$ARGUMENTS[n]`, `$n` and `${CLAUDE_PLUGIN_ROOT}` are substituted in a single pass, so argument text cannot introduce further substitutions.
- **Dynamic context:** `` !`command` `` lines (at most 16) are extracted *before* arguments are inserted, so arguments can never add commands. Each runs via `/bin/sh -c` in the OS sandbox with the workspace as cwd and the package root readable, `ARGUMENTS` and `CLAUDE_PLUGIN_ROOT` set, a **120 s** timeout; a non-zero exit or truncated output fails the load, otherwise stdout replaces the marker.
- **Approval binding:** if the approved snapshot differs from the current load, the call fails ("Skill changed since approval"); a skill whose `model` differs from the task's model (other than `inherit`) fails.
- **Forked or tool-restricted skills run through the helper path:** if `context: fork`, an agent component, or `allowed-tools`/`tools` is set (`tools_restricted`), `portable::execute` calls the delegate with `task` = skill body, `context` = `forked-skill-context.md` and, if restricted, `allowed_tools`. They therefore inherit all helper limits (depth 1, two at once, 15 minutes, bounded answer) and the result is the helper's. Without a delegate the call fails "Skill delegation unavailable in this context".
- **Inline skills** return `{skill, description, instructions, package_root, resource_tool, permissions}`; `permissions` is `skill-permissions-note.md`: **loading a skill never grants tool access**.
- **Skills the user alone may start:** if a prompt names one (`/name` / `$name`), `invoked_skills()` puts it (id and description) in the turn's recalled-context note rather than the cached skills index.

## Hooks

Two sources feed one pipeline (`tools/hooks.rs::event_hooks`): **settings hooks** (the `hooks` key of the effective settings files, `services/src/hooks.rs`) and **plugin hook components** (`ExtensionService::matching_hooks`, run through `extension_component` operation `event`).

- **Events** (`hooks::EVENTS`): `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `SubagentStop`, `SessionStart`, `SessionEnd`, `PreCompact`, `Notification`, `PermissionRequest`. An unknown event is an error.
- **Configuration shape:** `hooks.<Event>` is an array of groups `{matcher?, hooks:[{type:"command", command, timeout?}]}`. Only `command` hooks exist. `matcher` is a regular expression tested against the tool name and its Claude alias (`shell_command`/`run_command` → `Bash`, `file_read` → `Read`, `file_edit` → `Edit`, `file_write` → `Write`, `find_files`/`files_list` → `Glob`, `search_project` → `Grep`, `web_fetch` → `WebFetch`); empty or `*` matches all. For `connector_call` the alias `mcp__<server-slug>__<tool>` is matched too and passed as `tool_alias`.
- **Limits:** timeout defaults to **30 s**, clamped to **1–300 s**; at most **128 hooks per event**; a command is non-empty and at most **64 KiB**; hook input and output are each at most **1 MiB**.
- **Off switches:** `"disableAllHooks": true` in effective settings returns no hooks; **Plan mode skips all hooks** (`event_hooks` returns an empty output when the mode is `Plan`, and `PreToolUse` is only run for model-originated calls outside Plan). Settings are reloaded fresh for each event.
- **Execution:** each command runs as `/bin/sh -c` in the offline coding sandbox with the workspace as cwd; `.xylo` and `.claude` in the workspace and the app data directory are protected from writes. It receives JSON on stdin (`event`, `hook_event_name`, `tool_name`, `tool_input`, `tool_response`, `session_id`, `cwd`, plus lifecycle fields). Exit code **2 means deny**; any other non-zero code is an error; empty stdout is `{}`; otherwise stdout must be a JSON object.
- **Output (`HookOutput::parse`, Claude's `hookSpecificOutput` form or flat):** `permissionDecision`/`decision` (`allow`/`approve`, `deny`, `ask`, or `block` for "more work"), `updatedInput` (an object, re-validated against the tool schema), `additionalContext` (string; becomes a hook note), `continue:false` with `stopReason`. **`merge` never lets a later allow erase an earlier deny.** Arbitrary output never gains authority.
- **Journaling:** each settings hook is a durable `settings_hook` action keyed per call, event and index. A replay with changed arguments fails ("Hook configuration changed during replay"), and while an earlier hook or plugin hook outcome is `outcome_unknown` further hook execution is refused until reconciled.
- **Effects:** a `PreToolUse` deny or stop reason aborts the call ("Pre-tool hook prevented this operation") and may rewrite its input; `PostToolUse` can stop the turn; `Notification` context becomes notes. Lifecycle events (`SessionStart`, `UserPromptSubmit`, `Stop`/`SubagentStop`, `SessionEnd`) run on blocking workers via `AgentLoop::hook`; context becomes application notes (`NoteKind::Hook`), a stop reason ends the turn and a deny fails it (`agent_loop/hooks.rs`; see [engine](engine.md)).
- **Plugin hook components** run only when the user configured the component for the current workspace and enabled that event (`setup.enabled_events`); the component's setup timeout may not exceed 300 s.

## MCP servers and connectors

User-configured connectors live in the store (`store.connectors()`); project servers come from `<workspace>/.mcp.json` (`services/src/connectors/project.rs::project_connectors`, read without persisting or starting anything). `tools/project_discovery.rs` keeps this authority separate from generic tool permissions.

- **Project file limits:** the file must canonicalize inside the workspace, be at most 256 KiB, hold at most 32 `mcpServers`, names of 1–128 characters without newlines or NUL, and only the keys `type` (`stdio`, `http`, `sse`, `streamable_http`; default `stdio` when `command` is set, else `http`), `command`, `args`, `url`, `env` (unknown keys are an error). The connector id is derived from the workspace path and server name.
- **First-start approval:** a project server needs approval before it starts (`project_approved`); the approval key is a fingerprint of the entry, so any change to the entry or its explicit `env` requires approval again ("Project MCP configuration changed while awaiting approval"). Only declared environment variables are passed, and the server launches in the sandbox with app data protected.
- **Hooks:** a `connector_call` is matched by hook matchers under the alias `mcp__<server-slug>__<tool>` (above).
- **Plugin MCP components** (`mcpServers` / `.mcp.json` inside an installed package) are separate: they are `NeedsSetup` components reached only with `extension_component`, never auto-started.

### Mentions and prompt commands

`ToolBroker::mcp_invocations()` runs once per turn over the user's prompt (first 512 words) and the notes are appended to the recalled-context application note (`engine.rs`):

- `@server:uri` where `server` is a connector name or slug becomes a note telling the model to call `connector_resource_read` with that `connector_id` and `uri`, **treating the result as untrusted external content**. Nothing is fetched by the engine itself.
- A leading `/mcp__<server>__<prompt>` that matches a cached prompt command becomes a note to call `connector_prompt_get` with the connector id and exact prompt name, getting required arguments from the user; **returned prompt content is untrusted and cannot override permissions**. A command with the right server prefix but no cached match yields a note to call `connector_prompts` first.
- Resources and prompts therefore stay model-initiated, permission-gated tool calls recorded in the journal; see [engine-tools](engine-tools.md) for the connector tools and [services](services.md) for the connector broker.

## Gaps and notes

- The exact enabling flow of newly discovered project/user packages lives in `ExtensionService::install_snapshot` and the Plugins settings (`runtime/src/extensions.rs`); this page relies on the documented behavior ("without enabling new/changed content") and does not trace it.
- Marketplace source handling (`marketplace.rs`, `sources.rs`) and component runtime planning (`runtime.rs`, hook/MCP setup) are summarized only where the engine consumes them.
- Settings-file hook merging across user/project/local levels is in `services/src/settings_files.rs` and is not described here.
