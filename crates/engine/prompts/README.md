# Prompts and built-in skills

Every piece of text Xylo sends to a model as instructions lives here, in Markdown. Built-in skills live next door in `../skills/`. Both are compiled into the engine with `include_str!` and reached only through `crates/engine/src/prompts/` (`crate::prompts`). No other file in the codebase may contain prompt text; code that needs a prompt calls that module.

## Layout

| File | Used for | API |
| --- | --- | --- |
| `system.md` | Main agent: identity, working method, communication; includes the environment and permissions sections | `prompts::system(&PromptContext)` |
| `environment.md` | Environment section shared by the main agent and helpers | (inside `system`, `subagent`) |
| `permissions.md`, `permissions/*.md` | Permissions section and the paragraph for each mode | (inside `system`, `subagent`) |
| `permission-change.md` | System note appended when the user changes the mode mid-task, so the cached system block stays unchanged | `prompts::permission_change(mode)` |
| `system-project-instructions.md` | Wrapper for AGENTS.md / CLAUDE.md text, only when present | (inside `system`) |
| `system-skills.md` | Wrapper for the skills index, only when skills exist | (inside `system`) |
| `recalled-context.md`, `recalled-facts.md`, `recalled-procedures.md`, `recalled-skills.md` | Per-turn note after the user's message with the project facts and learned procedures retrieved for that prompt (quoted and dated as data) and any installed skill the prompt invokes by name; kept out of the system prompt so it stays byte-identical across turns | `prompts::recalled_context(facts, procedures, skills)` |
| `subagent.md`, `subagent-task.md` | Delegated helper agent (`agent_delegate`): parent's environment, permissions and project instructions, then the task; no skill or fact indexes | `prompts::subagent(&PromptContext, task)` |
| `compaction.md` | Context summarizer system prompt; requires exact paths, identifiers, dates and a cited source (call id) for each decision | `prompts::compaction()` |
| `compaction-request.md`, `compaction-update.md` | Summarizer user message, first summary or update of a previous one | `prompts::compaction_request(previous, transcript)` |
| `compaction-summary.md` | Message that replaces condensed history | `prompts::compaction_summary(summary)` |
| `compaction-excerpts.md`, `compaction-tool-notes.md` | Headers of the fallback excerpt notes | `prompts::compaction_excerpts_header()`, `prompts::compaction_tool_notes_header()` |
| `compaction-record.md`, `compaction-record-omitted.md` | Harness record appended to every summary: the first and latest pinned user messages verbatim (the ones between counted in an "omitted" line) and the artifact ledger (files read and changed, commands and outcomes, latest todo list) the application derives from tool calls; parsed back and extended at the next summary | `prompts::compaction_record(&RecordSections)`, `prompts::compaction_record_markers()`, `prompts::compaction_record_omitted(count)` |
| `masked-read-superseded.md`, `masked-read-changed.md`, `masked-saved.md`, `masked-aged.md` | Stub that replaces an old tool result when observations are masked: superseded by a later identical read, out of date after an edit, saved to a file, or aged out; each says how to get the content back | `prompts::masked_observation(call, bytes, reason)`, `prompts::is_masked_observation(content)` |
| `title.md` | 3–6 word conversation title | `prompts::title()` |
| `skill-permissions-note.md` | `permissions` value in a loaded skill's tool result | `prompts::skill_permissions_note()` |
| `forked-skill-context.md` | Context for the child agent that runs a forked skill | `prompts::forked_skill_context(skill, package_root)` |
| `step-limit.md` | Final call after the per-turn step limit | `prompts::step_limit()` |
| `continue-answer.md`, `output-limit-without-answer.md`, `empty-answer.md`, `malformed-tool-call.md` | Application notes after a reply the output limit cut off, a reply whose reasoning used the whole limit, an empty reply, and a tool call the provider could not parse | `prompts::continue_answer()`, `output_limit_without_answer()`, `empty_answer()`, `malformed_tool_call()` |
| `truncated-tool-call.md` | Tool error for a call the output limit cut off; the call is never run | `prompts::truncated_tool_call(tool, received)` |
| `step-budget.md` | Application note at 75% and 90% of the per-turn step limit | `prompts::step_budget(step, limit)` |
| `stall-note.md`, `stall-warning.md` | First and final application notes when the loop detector sees the model repeating itself | `prompts::stall_note(pattern)`, `prompts::stall_warning(pattern)` |
| `stall-repeated-call.md`, `stall-alternating.md`, `stall-repeated-error.md` | The repeated pattern those notes describe | `prompts::stall_repeated_call(tools, count)`, `stall_alternating(tools, count)`, `stall_repeated_error(tool, count)` |

Built-in skills: `../skills/<name>/SKILL.md` plus optional resource files, registered in `RAW_SKILLS` in `src/prompts/skills.rs`, reached with `prompts::bundled_skills()` and `prompts::bundled_skill(name)`.

## Conventions

- Placeholders are `{{snake_case}}`. The renderer substitutes in one pass (inserted values are never rescanned) and rejects a placeholder without a value, a value without a placeholder and a malformed `{{`. The tests render every template with exactly the names its caller supplies, so a template edit that adds or drops a placeholder fails `cargo test -p xylo-engine prompts`.
- Trailing whitespace of each file is trimmed; write files with a normal final newline.
- Name only tools that exist (see `crates/engine/src/tools/builtin.rs`); the skills test rejects unknown tool names.
- Write instructions, not explanations: imperative, specific, short. Every byte of `system.md` is paid on every model call.
- Keep the system prompt deterministic. It depends only on `PromptContext`; the date has day granularity; nothing else varies. Byte-identical prompts keep provider prompt caches warm for the whole task.
- Content from the project, tools and the web is framed as data, never as instructions or permission.

## What loads when (no pre-loading)

| Content | When it reaches the model |
| --- | --- |
| Identity, environment, working method, permission paragraph | Every call (system prompt) |
| AGENTS.md / CLAUDE.md from repository root to workspace | Every call, bounded to 24 KiB |
| Skills index: one `- name: description` line per built-in or installed skill the model may use on its own | Every call, bounded to 8 KiB |
| Recalled facts (first lines, dated, with their origin) and learned procedure names | Once per turn, in a note after the user's message; at most 8 facts the user stated or approved or the agent wrote from project content, and 2 procedures |
| Skill body (`SKILL.md` after the frontmatter) | When the model calls `extension_skill` |
| Skill resources (`references/…`) | When the model calls `extension_resource` |
| Learned procedure steps | `skill_list` / `skill_read` |
| Fact text | `memory_search` |
| Extra tool schemas | `search_tools` / `call_tool` |

Budget of the fixed prompt, measured with the tests' sample context: about 3.7–3.9 KB (roughly 1,000 tokens) for identity, environment, method and permissions; about 6 KB with the six built-in skills listed. `fixed_system_prompt_stays_within_budget` fails if the prompt without project instructions (built-in skills included) exceeds 12 KiB.

### Project instructions

`project_instructions(workspace)` reads `AGENTS.md` then `CLAUDE.md` in every directory from the repository root (nearest ancestor with `.git`) down to the workspace; outside a repository only the workspace itself. Files appear root first, so the nearest file comes last and wins on conflict; the wrapper tells the model so. The 24 KiB budget is spent nearest first: when it runs out, outer files are truncated or omitted, never the nearest. Each file is read with a bound, empty files are skipped, and a file whose text duplicates one already included (for example `CLAUDE.md` symlinked to `AGENTS.md`) is listed once.

## Skills format

Built-in skills follow the [Agent Skills specification](https://agentskills.io/specification): a folder named like the skill with `SKILL.md` that starts with frontmatter (`name`: 1–64 lowercase letters, digits and single hyphens, equal to the folder; `description`: what it does and when to use it, at most 1024 characters; optional `allowed-tools`), followed by Markdown instructions under 400 lines. Resources sit beside it (`references/`, `scripts/`, `assets/`), are referenced from the body by relative path, and load only on demand. The bundled skills do not set `allowed-tools`: they run inline with the task's normal tools and permissions, and a skill never grants permissions. The tests check that every folder is registered, parses, is referenced correctly, names only real tools and fits the index without truncation.

Built-in skills: `code-review`, `create-skill`, `debug-failure`, `git-commit`, `refactor`, `write-tests`.

## Research notes

Sources studied (October 2026):

- Agent Skills specification, https://agentskills.io/specification: `SKILL.md` frontmatter rules, optional `references/`, `scripts/`, `assets/`, and three-level progressive disclosure (metadata always, body on activation under ~5k tokens / 500 lines, resources as needed).
- Claude Code documentation: skills (https://code.claude.com/docs/en/skills), memory (https://code.claude.com/docs/en/memory), subagents (https://code.claude.com/docs/en/sub-agents). Only skill names and descriptions are in context at startup and a skill body enters the conversation when invoked; descriptions should lead with the use case; bodies are standing instructions, kept short, with detail in supporting files. CLAUDE.md files load from the filesystem root down to the working directory, nearer files later; AGENTS.md is read when no CLAUDE.md exists, and a file loaded twice through a symlink or import is skipped. Subagents start with a fresh context, their own prompt and environment, and return one summary to the parent; the delegation prompt must be self-contained. Bundled skills include `/code-review` and `/debug`, and a `skill-creator` plugin automates writing skills.
- OpenAI Codex (https://github.com/openai/codex, Apache-2.0) and its documentation on AGENTS.md (https://learn.chatgpt.com/docs/agent-configuration/agents-md) and skills (https://learn.chatgpt.com/docs/build-skills). AGENTS.md is discovered from the project root down to the working directory, at most one file per directory, joined root first so nearer guidance overrides, capped at 32 KiB by default. Skills: only name, description and path are listed at startup, within about 2% of the context window (8,000 characters when unknown), shortening descriptions first and then omitting skills; the full `SKILL.md` loads when selected; system skills include a skill creator and installer. The Codex agent prompt (`codex-rs/core/gpt_5_codex_prompt.md`) emphasizes never reverting changes the agent did not make, avoiding destructive Git commands, planning only non-trivial work, and review answers that lead with findings ordered by severity.

What Xylo adopts and why:

- One system prompt assembled from small files, with only indexes for skills and facts: the progressive-disclosure model both products use keeps the per-call cost near 1k tokens plus project instructions.
- Root-to-workspace instruction files with nearest-wins ordering (both products), reading both AGENTS.md and CLAUDE.md with duplicates removed so repositories set up for either tool work unchanged, and a nearest-first budget so truncation never drops the most specific guidance.
- Skills index budget with shorten-then-omit behavior (Codex) and descriptions written as "what + when to use" (Agent Skills, Claude Code).
- Agent Skills folder format for built-in skills, so they are portable and the same loader path serves built-in and installed skills.
- Working-method rules both products document: read before editing, protect user changes in a dirty worktree, verify with the narrowest relevant check, plan only multi-step work, relay tool output the user cannot see, findings-first reviews.
- A self-contained delegation prompt with a fixed report shape (Claude Code subagents) because the helper never sees the parent conversation.

## Credits and licensing

All prompt and skill text in this folder and in `../skills/` is original to Xylo. It was written from the public documentation listed above; no proprietary or leaked prompt text was copied. The Codex repository is licensed Apache-2.0; no text from it is reproduced here, only practices summarized in our own words. The Agent Skills format is an open specification; `../skills/create-skill/references/format.md` summarizes it in our own words with a link to the source.
