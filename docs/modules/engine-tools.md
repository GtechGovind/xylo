# Tool system (`xylo-engine::tools`)

## Purpose and boundaries

`crates/engine/src/tools` is the **application-owned, durable tool and permission boundary** of [engine](engine.md). Every tool call the model makes (and every call a delegated helper makes) is a journaled action with an idempotency key. Before an intent exists, the broker classifies the call (external metadata cannot supply its own permission flag), reads the **live** permission mode and asks `permissions::decide` whether the call runs, needs the user's decision or is refused.

The loop-facing half lives outside the directory: `agent_loop/dispatch.rs` (argument repair, validation, scheduling) and `broker_tools.rs` (the loop's view of a `ToolBroker`). Persistence (`Store`, action journal) is in [services](services.md); contract types (`PermissionMode`, `EngineEvent`, `ToolDefinition`) are in [core](core.md); the browser, desktop, document, environment, plugin, local-inference and speech tools are implemented by the capability service in [capabilities](capabilities.md). Helper agents that receive child brokers are in [engine-agents-and-skills](engine-agents-and-skills.md); the model side of the window budget is in [engine-providers](engine-providers.md) and [engine](engine.md).

## Module map

| Path under `crates/engine/src/tools` | Purpose |
| --- | --- |
| `mod.rs` | `ToolBroker`: tool lists, `execute` pipeline, journaling, approval wait, `search_tools`, result bounding, child brokers |
| `permissions.rs` | `PermissionCell`, `ToolSession`, `ApprovalGate`, `decide`, settings-file overlay, "always allow" rules |
| `risk.rs` | Deterministic risk assessment (`assess`) and `routine_fetch` classification |
| `builtin.rs` | Built-in definitions, path resolution, `write_target` (protected targets), edit lock, file/git/memory/skill/todo execution |
| `code.rs`, `file_read.rs`, `read_state.rs`, `notebook.rs` | File read/edit/search implementations, limits, stale-write protection |
| `shell_jobs.rs` | `run_command`, `shell_command`, `shell_output`, `shell_kill`, `shell_jobs` |
| `web.rs` | `web_fetch`, `web_search` |
| `catalog.rs`, `lookup.rs` | BM25 tool search, nearest-name suggestions, compiled schemas, connector snapshot |
| `interaction.rs` | `ask_user`, `exit_plan_mode`, `schedule_wakeup` |
| `worktree.rs`, `lsp.rs`, `image_generation.rs`, `portable.rs` | Worktrees, language servers, image generation, `extension_*` skills/components |
| `hooks.rs`, `headless.rs`, `project_discovery.rs` | Lifecycle hooks around calls, headless tool constraints and SDK custom tools, project connector (`.mcp.json`) authority and discovery |

## The broker (`ToolBroker`)

`ToolBroker::new(request, store, capability_config, session, events, cancel)` builds the full definition list: `builtin::definitions()` (which already includes `interaction`, `notebook_edit`, `lsp` and the web tools), `portable::definitions()` (`extension_*`), the image-generation definition and `CapabilityService::tool_definitions()`. A second list, `advertised`, drops `image_generate` unless an image provider is configured and the `connector_*` tools unless a connector exists (a stored connector or a workspace `.mcp.json`).

- **`ToolSession`** is the per-worker shared state: the `ApprovalGate`, interaction gate, the live `PermissionCell`, the credential vault, the web session, the MCP connection pool, read state (`reads`), the language-server service, the shell job registry and the live workspace (worktree) map. Every broker of one worker shares it.
- **Child brokers** (`scoped_child`) are created for `agent_delegate`. They share the journal, the permission cell, the turn's approved rules, the web session and the tool policy; they stamp events with the parent action as `parent_id`; they **cannot delegate further and cannot change the checkout** (`worktree_enter`/`worktree_exit` are refused with a namespace). `BrokerTools::offered` hides `agent_delegate`, `worktree_enter` and `worktree_exit` from children.
- **Tool policy:** `--allowed-tools` fills `allowed_tools`; a call to any other name fails with "excluded by the allowed/disallowed tool policy" (also through `call_tool`) and notifies the refusal observer. `restrict_tools` (helper/skill restrictions) accepts exact tool names plus the aliases `Read` (`file_read`, `extension_resource`), `Write`, `Edit`, `Glob` (`find_files`, `files_list`), `Grep`, `Bash` (`shell_command`, `run_command`); anything else is an error, never widened.
- **SDK custom tools** (`headless.rs`) need an `sdk_` name (letters, digits, `_`), must not duplicate a built-in, are offered directly, and always classify as `Reach::Outside("SDK client")`.
- **Workspace lock:** each call holds a read lock on the live workspace; `worktree_enter`/`worktree_exit` take the write lock, so a checkout switch waits for in-flight calls.

### Which tools the model is offered

`initial_definitions()` depends on the effective window ([engine](engine.md), `provider_guard/window.rs`), **never on the permission mode**, so the provider's cached tool prefix survives a mode change:

| Window | Offered directly |
| --- | --- |
| Normal | `CORE_TOOLS` (**21**): `ask_user`, `exit_plan_mode`, `extension_skill`, `extension_resource`, `file_read`, `file_edit`, `file_write`, `files_list`, `find_files`, `search_project`, `git_status`, `git_diff`, `run_command`, `shell_command`, `todo_write`, `agent_delegate`, `memory_search`, `memory_remember`, `skill_read`, `search_tools`, `call_tool`; plus configured families: `browser_*` (Chromium configured), `local_inference_*` (llama configured), `speech_*` (whisper or sherpa configured), `image_generate` (provider configured), `connector_list`/`connector_tools`/`connector_call` (a connector exists), and SDK custom tools |
| Small (`< 16_384` effective tokens, or tiny) | `SMALL_WINDOW_CORE_TOOLS` (**8**): `file_read`, `file_edit`, `search_project`, `shell_command`, `todo_write`, `memory_search`, `search_tools`, `call_tool` |
| Tiny (`< 5_632` input tokens) | `TINY_WINDOW_CORE_TOOLS` (**5**): `file_read`, `file_edit`, `shell_command`, `search_tools`, `call_tool`, in **compact form** (one-line descriptions from `compact_description`) |

Everything else is reachable through `search_tools` + `call_tool` (below). On Windows `run_command`, `shell_command`, `shell_output`, `shell_kill` and `shell_jobs` are **removed from the definitions** because the OS sandbox is not qualified there.

## Dispatch (`agent_loop/dispatch.rs`, `broker_tools.rs`)

Every call is checked **before dispatch, and so before any permission decision or approval prompt**:

1. **Parse** (`arguments::parse`): strict JSON first; lenient fallbacks (code fences, surrounding prose, trailing commas, single quotes, bare keys, Python literals, double encoding) only for text that does not parse. Empty text or `null` means no arguments.
2. **Resolve the name:** the tool itself, else an entry of the exact alias table (`ALIASES`, 24 entries, e.g. `read`→`file_read`, `bash`/`shell`→`shell_command`, `grep`→`search_project`, `glob`→`find_files`, `ls`→`files_list`, `todo`→`todo_write`, `WebFetch`→`web_fetch`, `askuserquestion`→`ask_user`, `taskoutput`→`shell_output`). An unknown name becomes an error result with "Did you mean …" nearest names and a pointer to `search_tools`/`call_tool` (or to `connector_call` for names containing `__`).
3. **Repair and validate** against the tool's compiled JSON Schema: deterministic, only where the intent is unambiguous (`"5"` for an integer, `"true"` for a boolean, a JSON string where an object/array belongs, enum case, `null` for an optional field, a misspelled field that folds to one declared name, a missing required field with a default, an undeclared top-level `arguments`/`input`/`parameters`/`args` wrapper). Failures list at most 5 problems with the expected signature. A repaired call is **recorded in the assistant message as it will run**, so transcript, journal and resume agree.
4. `call_tool` targets and `connector_call` targets with a discovered schema are checked the same way; `call_tool` naming `call_tool` is rejected.

Malformed calls become **tool error results**, never turn failures.

### Scheduling

`run_batch` runs prepared calls and returns results **in call order**. A call's `Access` is its definition's `writes` flag; `call_tool` takes its target's access (unknown or recursive targets count as writes); `connector_call` is a read only when `connector_call_reads` can vouch for it (native connector, or an MCP server the user marked trusted); **anything unknown is a write**.

- Read-only calls run concurrently, **at most `MAX_PARALLEL_READS = 4`** at a time, each on a blocking worker.
- A writing call **waits for every earlier call and runs alone**, so calls touching the same state apply in the order written.
- A retained hook stop (`pending_stop`) turns later calls into `Not run: <reason>` error results.
- Writes to one file are additionally serialized process-wide by an `EditLock` held from preview to write, so concurrent edits of one file both land.

## Execution pipeline (`ToolBroker::execute`)

`execute(call_id, name, arguments)` returns the model-facing JSON string. In order:

1. **`call_tool` unwrapping.** `call_tool` is transparent: the nested `name`/`arguments` run as if called directly, so journal, policy and approval see the **target**. `call_tool` → `call_tool` is refused (`Recursive tool dispatch is not allowed`).
2. **Checkout lock** and rebinding to the live workspace root (a checkout switch rebuilds root authority).
3. **Policy check** (`allowed_tools`), then **journal replay**: an action with the same key (`journal_key(namespace, call_id)`) and identical arguments returns its recorded result instead of re-running. Child calls are namespaced by the parent `agent_delegate` key; children cannot call `agent_delegate`.
4. **Settings deny and `PreToolUse` hooks** (not in Plan mode): a settings deny rule fails the call (`Denied by settings permission rules`); a hook may block or supply `updatedInput`, which is schema-validated again.
5. **Classification**: effective `writes` (connector and extension calls resolved; a skill with dynamic commands counts as a write), `Reach` (path resolves outside the project, or the command's `cwd` outside), network request (`run_command`/`shell_command` only), and, for `memory_remember`, the conversation's **exposure** (least-trusted content any journaled action showed the model).
6. **Verdict** from `permissions::configured_named(mode, call, alias)`: `Allow`, `Ask(reason)` or `Refuse`. The mode is read **live from the `PermissionCell`**. Hooks can add an `ask`, or an `allow` that never overrides an explicit rule, network, `lsp`, outside reach or untrusted memory input. A `PermissionRequest` hook runs when the verdict is `Ask`.
7. **Intent and approval.** An `ActionIntent` is written (`begin_action`), `ToolStarted` is emitted with a bounded presentation (`PRESENTATION_EVENT_BYTES` = 256 KiB; edits show a diff of at most 128 KiB), then, on `Ask`, `ApprovalRequired` is emitted and the call blocks (polling every 100 ms) until the user decides or the turn is cancelled. **An approval never outranks a mode that now refuses the action** (`Permission mode changed to …; not run`). Denial or cancellation records the action as denied.
8. **Execution** (`mark_action_running`), dispatched by name: SDK custom tool, `search_tools`, `agent_delegate`, `connector_*`, `web_*`, `image_generate`, `extension_*`, `memory_remember` (with the broker-assigned origin), `worktree_*`, shell tools, interaction tools, other built-ins, else the capability service (with a cross-process resource reservation).
9. **Settlement.** The result is recorded **before** artifact/UI work. If a write's result cannot be persisted the action is marked *unknown*, never replayed. A failed write is marked *outcome unknown* unless the error is a definitive rejection (unapplied edit, process not started, interaction tools, …); a command that exited with a code is an observed failure, not an uncertain write. `PostToolUse` hooks run after success; their failure adds `post_tool_hook_error` but does not undo the call.

An approved "always allow" decision (`decide_with(id, allow, remember)`) inserts the rule into the turn's `RuleSet`; the host persists it for later turns. Rules are keyed per tool on selected fields (`path` for file tools, `command` for `shell_command`, `program`+`args` for `run_command`, `connector_id`+`tool` for `connector_call`); keys over 400 bytes are hashed.

## Permissions (`permissions.rs`, `risk.rs`)

`PermissionCell` is an atomic shared by every broker of a worker. A `WorkerCommand::SetPermissions` from the host applies to the **very next call of a running task**. The five `PermissionMode`s:

| Mode | Behavior of `decide` |
| --- | --- |
| **Plan** | Refuses any write, any network request and any hook-origin call ("Plan mode permits only reading and questions"); otherwise behaves like Read only. `ask_user` and `exit_plan_mode` are always allowed; approving a plan via `exit_plan_mode` sets the chosen continuation mode on the cell |
| **ReadOnly** | Refuses every write (network access included, native `lsp`, outside shell commands); reads behave like Ask for approval |
| **AskForApproval** | `run_command`/`shell_command` run offline without asking (the sandbox confines them). Asks for anything outside the project, `desktop_*`, every `connector_call`, network access, and every writing tool except `PROJECT_WRITES` (`shell_kill`, `file_write`, `file_edit`, `notebook_edit`, `memory_remember`, `skill_propose`); those still ask when `risk::assess` flags them |
| **ApproveForMe** | Asks only for what `risk::assess` flags, writes outside the project, connector calls that write or send more than a small secret-free lookup, and network beyond a `routine_fetch` |
| **FullAccess** | Asks for nothing; coding commands keep their sandbox |

Refinements applied in every mode:

- `web_fetch`/`web_search` are always `Allow` at this layer; the web module checks domain and private-network authority per hop.
- `worktree_enter`/`worktree_exit` always ask (refused in Read only): they run native Git outside the sandbox.
- `lsp` is its own authority: refused in Read only/Plan, `Allow` in Full access, otherwise asks ("Runs a native language server outside the sandbox…"). A remembered native grant covers later calls.
- Outside shell commands (`cwd` outside the project) ask "Run in the sandbox with read and write access to …" unless Full access; refused in Read only.
- A command with `network: true` asks (`Network access: <justification>`) in Ask for approval; in Approve for me only a `routine_fetch` (a plain download with allowlisted options and no project config that could redirect it) runs unasked. `allow_local_binding` always asks.
- **Untrusted memory writes:** once the conversation has read content from outside the project (web pages, connectors, plugins, files outside it), `memory_remember` asks in Ask for approval and Approve for me and **no "always allow" rule covers it**. In Full access it runs, and the fact keeps the untrusted origin, keeping it out of auto-recalled context. A user-approved write is stamped `UserApproved`.
- **Settings files** (`SettingsFiles::for_workspace`) are reread before each call. `deny` always wins; `ask` forces approval; `allow` applies only if the default is not a refusal and the call is not an independent authority (`lsp`, network, outside shell, untrusted memory write).

`risk::assess` is a table over tool names and parsed shell words (no model call): destructive/irreversible commands, privilege escalation, downloads piped into interpreters, uploads/publishing, credential and secret access (`Reads …`/`Changes …` for secret-looking paths), project files that make tools run code later, writes outside the project, nested shell code deeper than 4 levels, and the `TOOL_RISKS` table. In that table `desktop_displays`, `browser_snapshot`/`wait`/`screenshot`/`close`, `plugin_list`, `environment_list`/`status`, `schedule_list`, `ask_user` and `exit_plan_mode` are harmless; every other `desktop_*`, `browser_*`, `plugin_*`, `environment_*`, `schedule_*` tool and `image_generate` is flagged, so a tool added to a family asks until it is listed.

## Built-in tools

Definitions are strict JSON Schemas (`additionalProperties: false`). "W" below is the tool's `writes` flag, which drives scheduling and the read-only modes. Paths are resolved to canonical form **before any I/O**; a path that leaves the project (and is not a saved `full_result` artifact, which is readable) is a `Reach::Outside` and needs approval in the modes above. A path being created resolves through its existing parent, so a symlinked parent cannot hide the target.

### File tools

| Tool | W | Arguments | Limits and behavior |
| --- | --- | --- | --- |
| `file_read` | no | `path`; `offset` (≥1), `limit` (1–10000); `pages` (PDF) | Text: numbered lines (`%6d\t`), default line window `tokens/64` clamped 100–2000, bytes bounded by the window's tool-result budget, **2000 chars per line** (longer lines cut with `[line shortened]`), files **≤ 64 MiB**, NUL bytes → "appears to be binary". Result carries `truncated`/`next_offset`. Images (`png jpg jpeg gif webp`) become managed attachments; `.ipynb` returns cells with outputs (limit 1–1000 cells, ≤16 images of ≤12 MiB each); `.pdf` goes through `document_read` (**at most 20 pages**, default `1-20`). Records a fingerprint for stale-write protection |
| `file_edit` | yes | `path`, `edits[1..1000]` of `{old_string, new_string, replace_all?}` | Applied sequentially and **atomically** (all or nothing). `old_string` must be non-empty, differ from `new_string`, and match exactly once unless `replace_all`. CRLF files accept LF `old_string`. File ≤ **8 MiB** before and after. UTF-8 text only. Returns a unified diff (patience, 3 lines of context) and `replacements` |
| `file_write` | yes | `path`, `content` | `content` **≤ 2 MiB** (`Write exceeds 2 MiB`); creates or replaces the whole file; parent directory must exist. Result has `created`, `bytes`, diff |
| `notebook_edit` | yes | `path`, `cell_id`, `new_source`, `edit_mode` (`replace`/`insert`/`delete`), `cell_type?` | Notebook ≤ 8 MiB before and after; insert after `cell_id` (empty = first); replace clears execution outputs |
| `files_list` | no | `path?` | One directory, names sorted, dirs end with `/`, at most **2000** entries (`truncated`) |
| `find_files` | no | `pattern`, `path?`, `max_results?` | Glob (a pattern without `/` matches at any depth), `.gitignore` respected, `.git` skipped, **newest first**, default **300**, max **5000**, walk capped at 200 000 entries |
| `search_project` | no | `query`; `path`, `glob`, `literal`, `case_insensitive`, `output_mode` (`content`/`files`/`count`), `before`/`after` (0–10), `multiline`, `type`, `offset` (0–200000), `max_results` | Ripgrep-style regex (`literal` for exact text), default **200**, max **2000** results, files over **4 MiB** skipped, lines cut to **300 chars**, deterministic path order, walk capped at 200 000 entries; `truncated` + `next_offset` for paging |
| `git_status` | no | none | `git status --short --branch` via native read-only Git, 30 s timeout; "No changes" when empty |
| `git_diff` | no | `staged?`, `path?` | `git diff --no-ext-diff --no-textconv` (`--cached` when staged); `path` must stay inside the project; non-zero exit is an error |

The approval presentation of a write repeats only `path` and options (not content) plus a diff bounded to **128 KiB**; the write itself is never shortened. `file_edit` and `file_write` additionally append language-server diagnostics when a remembered native `lsp` grant exists (`lsp::after_write`).

### Stale-write protection (`read_state.rs`)

`file_read` records a SHA-256 + size + mtime fingerprint of the file per (conversation, canonical project root, canonical path). `file_edit`, `file_write` (over an **existing** file) and `notebook_edit` require a matching fingerprint:

- never read: `Read the existing file with file_read before editing it`;
- changed since the read: `The file changed since your last read; read it again`;
- changed between approval and write (`WriteRevision::check`): `The file changed after the edit was prepared; read it again`;
- changed while being read: `The file changed while being read; read it again`.

A successful own write advances an existing fingerprint; **creating a file does not create one**. Fingerprints are keyed by root, so a read in another project or checkout never authorizes a write. All of these are `Unapplied` errors (definitive rejections, not uncertain writes).

### Protected write targets (`builtin::write_target`)

`file_write`, `file_edit` and `notebook_edit` resolve their target through `write_target` **before an intent exists and again at execution**. A refusal is an `Unapplied` error: **regardless of permission mode, including Full access, and for hook and SDK calls**, the file is never touched. Refused targets:

- **Settings permission files** (any source of `SettingsFiles::for_workspace(root)`): `Settings permission authority cannot be changed by tools`.
- **`.git` at any depth** (a nested repository runs its hooks just the same), and **`.agents`, `.claude`, `.codex`, `.xylo` as the first path component under the project root**. Names compare ignoring ASCII case. For targets outside the project every component counts, so another repository's `.git` is refused even when outside writes are approved. Error: `Tools cannot modify repository metadata or agent configuration`. This mirrors the command sandbox's `denyWrite` list.
- **Xylo's application data directory** (the parent of the artifacts directory): `Tools cannot modify Xylo's application data`.
- **The application execution boundary** (`sandbox::ensure_writable_target`: sandbox helper, host executable directory, trusted Node runtime).

A write outside the project is otherwise possible only when approved (`Reach::Outside`), and is flagged by `risk::assess` ("Changes a file outside the project"). Writes to secret-looking paths and to auto-run configuration (`.envrc`, CI workflows, `.cargo/config.toml`, …) ask in Ask for approval and Approve for me.

### `lsp`

`lsp` (W) takes `operation` (`definition`, `references`, `hover`, `symbols`, `workspace_symbols`, `diagnostics`), `path`, and zero-based UTF-16 `line`/`character`. It starts a **native language server outside the sandbox with account file and network access**, so it is a separate authority (see Permissions). After `file_edit`/`file_write`, diagnostics (1.5 s budget) are added only when a native grant was remembered; otherwise the result carries `diagnostics_unavailable` explaining how to enable them.

### Shell and job tools (`shell_jobs.rs`)

| Tool | W | Arguments |
| --- | --- | --- |
| `run_command` | yes | `program`, `args[]`, `timeout_seconds` (1–1200), `run_in_background`, `cwd`, `network`, `justification`, `allow_local_binding` |
| `shell_command` | yes | `command` (bash; `cd` persists across calls in the conversation), plus the same options |
| `shell_output` | no | `job_id`, `wait_seconds` (0–300), `filter` (keep lines containing the text) |
| `shell_kill` | yes | `job_id` (stops the job and its child processes; only jobs of this conversation) |
| `shell_jobs` | no | none; lists this conversation's jobs, including ones running from earlier turns |

**Execution model.** Commands always run in the OS sandbox helper (`sandbox::prepare_coding_with_scope`), confined to the project root (plus an approved outside `cwd` scope) with Xylo's data directory protected. **Network is off unless `network: true` is granted.** If the sandbox blocks a host, the result (and the job log) carries a `network_hint` telling the model to retry with `network: true` and a justification. A sandbox that cannot start fails closed (`NotStarted`, a definitive rejection). `run_command` resolves `program` like the platform shell would and passes `args` as an array (no shell); `shell_command` runs the line through bash with a trailing marker that captures the final directory, so **`cd` persists** for the conversation.

**Arguments and limits**

- `timeout_seconds`: 1–1200 (clamped). **Foreground default 120 s**; a background job has **no timeout unless one is given**.
- `network`: boolean; **`true` requires a non-empty `justification` of at most 500 characters** (trimmed). A malformed request is rejected before any intent exists. `allow_local_binding: true` requires `network: true` and a justification, and sets the sandbox's local-listen flag; it always asks (Ask for approval and Approve for me).
- `cwd`: relative to the conversation's current directory or absolute; must be an existing directory. A `cwd` outside the project is a fresh, explicit grant for **that directory and descendants** only; shell text can never widen it. An inherited outside `cwd` keeps its earlier reviewed scope. The directory is captured before approval and must still canonicalize identically at start (`Reviewed shell directory changed; request approval again`).
- `command` must be non-empty; `args` entries must all be strings.
- **At most 8 running jobs per conversation** (`This conversation already has 8 running shell jobs…`); at most 8 jobs are retained, finished ones evicted oldest-first.
- Output: each job keeps the **last 4 MiB** in memory (`OUTPUT_BYTES`) and writes the full stream to a `0600` log under the artifacts' `shell-jobs/<conversation>/<job>.log` (`log_path` in every result); the UI receives a 16 KiB tail. Foreground results return `exit_code`, `stdout`, `stderr`, `output_truncated`; a non-zero exit is `status: "failed"` with the output still returned.
- **Background jobs** (`run_in_background`) return `{job_id, running, log_path}` immediately and outlive the turn. `shell_output` returns only output **not yet read** (a per-job cursor); `output_truncated: true` means older bytes fell out of the 4 MiB window (read `log_path` with `file_read`). `wait_seconds` (≤ 300) returns early on new output or exit. `shell_jobs` lists only this conversation's background jobs; `shell_kill` and `shell_output` refuse another conversation's job. Jobs keep their checkout alive (a workspace lease) until all descendants stop.
- Not available on Windows: `start` fails with `Commands are not supported on Windows yet; nothing was run`, and the five tools are not even defined.

**Permission behavior.** `run_command`/`shell_command` are writes. Ask for approval runs them offline without asking; Approve for me asks only when `risk::assess` flags the command line (destructive commands, privilege escalation, `curl | sh`, uploads, secrets, auto-run files, outside-project paths in the parsed words); Full access never asks; Read only/Plan refuse. A network request, an outside `cwd` and `allow_local_binding` are separate authorities (see Permissions). `shell_kill` is a write but in `PROJECT_WRITES`, so Ask for approval does not prompt for it.

## Web tools (`web.rs`)

Both tools are read-only (`writes: false`), always `Allow` at the permission layer (`decide`), and gate network authority themselves **per hop**. Both outputs are stamped `untrusted: true`, `provenance: "web_content"`; neither tool is in `PROJECT_LOCAL_TOOLS`, so **both count as content from outside the project** for later memory-write origins.

| Tool | Arguments | Behavior |
| --- | --- | --- |
| `web_fetch` | `url` (≤ 8192), `prompt?` (non-empty, ≤ 16 KiB), `max_bytes?` (1–10 MiB, default 10 MiB) | Reads a page as Markdown without a browser. HTTP is **upgraded to HTTPS** (port 80 dropped, fragment removed); credentials in the URL or secret-looking query keys (`key`, `token`, `api_key`, `signature`, `client_secret`, …) are rejected. With `prompt`, the page is passed to **this turn's provider** (max 4096 output tokens, lowest reasoning, `TIMEOUT` 30 s) wrapped as untrusted and the answer replaces the content |
| `web_search` | `query` (1–8192), `allowed_domains[≤100]`, `blocked_domains[≤100]` | Uses the provider's **native search** when it supports it, otherwise the configured search service (Brave, Tavily or SearXNG from Settings > Connections; `Disabled` is an error). Domain filters are enforced again on the returned results; grounding redirect URLs are resolved to publisher URLs (≤ 50 sources, ≤ 10 redirects). Domains must be lowercase without schemes, ports or paths |

**Bounds:** no automatic redirects (each of at most **10** hops is re-authorized and re-resolved, `Web request exceeded 10 redirects`); **30 s** total deadline (time spent waiting for the user is excluded); response and converted page **≤ 10 MiB** (`Web response exceeds max_bytes`); only text, JSON or XML content types; UTF-8 only; no proxy, no retries, DNS answers pinned to the request. HTML is converted to Markdown with `script`/`style`/`noscript`/`iframe` dropped.

**Cache:** per worker session (shared with child brokers), keyed by normalized URL, **15 minutes** TTL, at most 16 pages (the cache is cleared when full). A hit is reported as `cached: true` and **still re-runs every domain and private-network approval** the original fetch needed.

**Approvals** (`web_access` pseudo-action):

- **Domain:** each host (including redirect targets) asks "Allow web access to `<host>`?" unless the mode is **Full access** or an "always allow" rule `web-domain:<host>` was granted for the turn/project. Read only and Plan still reach this prompt (no refusal at this layer).
- **Private addresses:** if any resolved address is not ordinary global unicast (private, loopback, link-local, CGNAT, documentation, multicast, translated/transition ranges, IPv6 outside `2000::/3` and special blocks), a **separate approval is required in every mode, Full access included, and it has no "always allow" rule**.

## Tool sets: `search_tools` and `call_tool`

Tools not offered directly stay reachable through two always-resident tools (both `writes: false`; **`call_tool` takes its target's classification**).

**`search_tools`** (`query` text) ranks with **BM25** (`catalog.rs`: `K1 = 1.2`, `B = 0.75`; tool-name terms count 3×, a connector's own name 2×; stop words dropped, camelCase split, plurals folded; deterministic, equal scores keep catalog order, built-ins before connector tools) and returns **at most 8 results** (`SEARCH_RESULTS`). Each result has `name`, a description cut to 200 chars, and either full `parameters` (schema ≤ 1536 bytes) or a compact `arguments` signature (≤ 600 chars).

- **What is searched:** every advertised built-in/capability tool that the **current mode may use** (`usable`: in Read only/Plan, write tools are hidden except `ask_user` and `exit_plan_mode`; child brokers also omit `agent_delegate`), plus the **discovered** tools of configured connectors under `server__tool` names with a `connector_call: {connector_id, tool}` hint. Search never starts a server; connectors not yet discovered are listed as `undiscovered_connectors` with a pointer to `connector_tools`.
- **Untrusted ranking input:** connector descriptions are cut to 48 terms, scores scaled by 0.8, and a connector tool that shares a built-in's name never outranks it.
- **Budget:** results are fitted to the window's tool-result budget minus 640 bytes for the envelope (`fit_search_results`): lower results' schemas become signatures first, then the weakest results are dropped, and only then does the best match lose its full schema.
- The index is cached and rebuilt only when the usable tool names, the write permission or the connector snapshot change.

**`call_tool`** (`name`, `arguments` object) runs a discovered tool "with its documented parameters and normal permissions". At dispatch the target is resolved (aliases apply), its arguments are repaired and validated against the **target's** schema, and the call executes as the target: its own journal entry, permission verdict, approval prompt, hooks, and allowed/disallowed tool policy (`--disallowed-tools` applies **through** `call_tool` and delegation). `call_tool` → `call_tool` is rejected. For a connector result the model should use `connector_call` with the `connector_id`/`tool` that search returned; an unknown tool name gets nearest-name suggestions.

Only tools in the offered set are advertised, but `call_tool` can reach **every defined tool** (`definitions()`), so a model that knows a name can call it directly even in a small window; mode gating still applies at dispatch.

### Interaction, planning and scheduling

| Tool | W | Arguments / behavior |
| --- | --- | --- |
| `ask_user` | yes¹ | `questions` 1–4, each `{question ≤ 4096, header ≤ 80, multi_select, options 2–4 of {label ≤ 160, description ≤ 1024}}`; "Other" free text is always available. Blocks (emitting `WaitingForApproval`) until answered or the turn is cancelled |
| `exit_plan_mode` | yes¹ | `plan` (1–32768 chars). Emits `PlanRequired`; an explicit approval selects the continuation mode (set on the live `PermissionCell`), "keep planning" supplies feedback, rejection grants no authority |
| `schedule_wakeup` | yes | `delay_seconds` 60–3600, `prompt` ≤ 8192, `reason` ≤ 1024. Creates a durable wakeup resuming the same conversation under the **current permission limit**; asks like other `schedule_*` work |

¹ Always allowed by `decide` in every mode (`Interaction authority never grants file, network, or native execution`), and **never counted as an external write** for crash reconciliation. Without an interactive client (headless) they emit the event, notify the refusal observer and return `{"answer":{"kind":"reject"},"reason":"No interactive client is attached"}`.

### Memory, skills, todos, delegation

- `todo_write` (read): `todos[]` of `{content, status: pending|in_progress|completed}`, **at most 100** items, content non-empty; returns the rendered checklist (`[ ]`, `[~]`, `[x]`).
- `memory_search` (read): `query`; recalls up to **12** project facts with source, origin, date and corrected earlier versions (`previously`). Journaled results count toward the conversation's exposure.
- `memory_remember` (write): `content`, `source`, optional `supersedes` (a fact id from `memory_search`) with a required `reason`. **The origin is assigned by the broker, never the model**: the least-trusted content any journaled action of the conversation exposed, or `UserApproved` when the user approved the write. Only facts from the user or approved/pre-exposure writes are auto-recalled (at most 8 per turn). A correction from a less trusted origin is refused by the store.
- `skill_read` (read), `skill_list` (read), `skill_propose` (write, queues the latest verified turn for background evaluation).
- `extension_skill` (`id`, `arguments?`), `extension_resource` (`package`, `path`) are reads; built-in skills resolve before installed ones. A skill that carries dynamic commands is classified as a **write** from one immutable snapshot used for classification, approval and execution. `extension_component` (`id`, `component`, `operation` of `status|verify|tools/list|tools/call|request|event`, `input?`) is a write; plugin tools reject a `capabilities` argument ("Plugins run only with the grants reviewed in Settings").
- `agent_delegate` (`task`, `context?`): runs a supervised child with a child broker (see [engine-agents-and-skills](engine-agents-and-skills.md)); the result is bounded by the delegate to a share of the window and presented to the UI at the model's bound.
- `worktree_enter` (`name?`, `base?` default `HEAD`) and `worktree_exit` (`keep` required): create/leave an isolated Git worktree for the chat using native Git with no network. Always ask; refused in Read only; not available to children. `worktree_exit` with `keep: false` removes the checkout only if it is clean, has no unpushed work, is not shared with another chat and no MCP/LSP process or background job still depends on it; otherwise it is kept.
- `image_generate` (`prompt` ≤ 16 KiB, `size` of `auto|256x256|512x512|1024x1024|1024x1536|1536x1024`): offered only when an image provider is saved; no fallback or retry; may incur a charge, so it asks in both ask modes.
- `connector_list` (read) and `connector_tools|resources|resource_read|prompts|prompt_get|call` (writes by definition; `connector_call` is classified by the target tool when trustworthy). Ask for approval asks for every `connector_call`; Approve for me asks unless it is a trusted read-only call with a small secret-free argument set. Project `.mcp.json` servers need a one-time approval for that configuration.

## Result bounds

Three separate bounds apply to every result; all are enforced after the result is recorded in the journal.

| Surface | Bound | Behavior |
| --- | --- | --- |
| **Model** (`model_result`) | `EffectiveWindow::tool_result_bytes()`: about **12 %** of the effective window, clamped to 1,024–25,000 tokens (× bytes per token); a tiny window gets about 16 %, 256–1,024 tokens | Within budget: returned unchanged. Over budget: long string fields are shortened **in the middle** (limits 16384 → 256 bytes, keeping both ends), and if still too large the result becomes an excerpt. The result gains `truncated: true`, a `note`, and **`full_result`**: the path of a complete readable text rendering saved as `tool-<action>.txt` in the artifacts directory, which `file_read` can page with `offset`/`limit` (artifact paths are readable although outside the project). Image attachments are preserved |
| **UI event** (`bound_result`) | 8192 bytes | Larger results become a small placeholder `{status, summary: "Tool result recorded…", action_id, truncated: true, artifacts: [...]}` with the full JSON saved to `tool-<action>.json`; artifact references that do not fit are counted in `artifact_references_in_full_result`. `agent_delegate` is presented at the model bound instead |
| **Start/approval event** | `PRESENTATION_EVENT_BYTES` = 256 KiB | Long argument strings are shortened (`request_shortened: true`); the journal and the tool always receive the complete arguments |

Persisted artifacts are written once with `create_new` semantics and re-verified byte for byte on replay, so a resumed turn sees identical content. Per-tool bounds (file reads, search, shell output, web pages, `file_write`, `file_edit`) are listed with each tool above; the `file_read` byte budget is the tool-result budget minus 1,024 bytes (at least 2,048; tiny windows subtract 384, at least 512), so a read always leaves room for its own fields.

## Capability tool families

`CapabilityService::tool_definitions()` contributes the remaining definitions. They are implemented in [capabilities](capabilities.md), executed by the broker's fallback branch under a cross-process resource reservation (`xylo_resources::capability_demand`) and a child cancellation token, and carry the same journal, approval and result bounds as built-ins. They are **not** in `CORE_TOOLS`; they are offered directly only when configured (below) and are otherwise found with `search_tools`.

| Family | Tools (from the capability definitions) | Offered directly when | Approval (Ask for approval / Approve for me) |
| --- | --- | --- | --- |
| `browser_*` | `navigate`, `click`, `type`, `snapshot`, `wait`, `screenshot`, `download`, `close` | Chromium is configured | `snapshot`/`wait`/`screenshot`/`close` are harmless; the rest ask ("Opens a web page…", "Clicks in a web page…") |
| `desktop_*` | `displays`, `screenshot`, `click`, `type` | never (search only) | Ask for approval always asks; Approve for me asks except `desktop_displays` |
| `document_*` | `read`, `open`, `pdf_preview`, `save_copy`, `save_status`, `export_docx`, `export_xlsx` | never (`file_read` calls `document_read` for PDFs) | by `writes` flag |
| `environment_*` | `list`, `status`, `execute` | never | `list`/`status` harmless; `execute` asks ("Runs in a remote or container environment") and validates its request before an intent exists |
| `plugin_*` | `list`, `run`, `run_installed`, `run_wasm` | never | `list` harmless; the others ask ("Runs an installed plugin"); `plugin_run_installed` rules are keyed by `id` + `package_sha256` |
| `local_inference_*` | `start`, `status`, `generate`, `stop` | llama configured | by `writes` flag |
| `speech_*` | `transcribe`, `synthesize` | whisper or sherpa configured | by `writes` flag |

Because the offered list never depends on the permission mode, write-capable tools of these families remain visible in Read only but are refused at dispatch.

## Hooks and journaling summary

Enabled plugin hooks run around model-origin calls: `PreToolUse` (not in Plan mode; may block or rewrite input, which is re-validated against the schema), `PermissionRequest` (only when the verdict is `Ask`) and `PostToolUse`. Hook-origin calls are refused in Plan mode and are only `extension_component`. Every call is a durable action (`begin_action` → `mark_action_running` → `finish_action`/`fail_action`/`mark_action_unknown`/`deny_action`) in the [services](services.md) store, keyed by `(conversation, turn, call key)`: a resumed turn replays recorded results instead of re-executing, and an interrupted write is surfaced for review rather than silently retried. See [engine](engine.md) for how the loop consumes these results and [core](core.md) for the `EngineEvent`s (`ToolStarted`, `ApprovalRequired`, `ToolFinished`, `PlanRequired`) the broker emits.
