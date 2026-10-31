# Agent Skills format reference

Summary of the open Agent Skills specification (https://agentskills.io/specification) as Xylo uses it.

## Layout
```
<name>/
  SKILL.md        required: frontmatter + instructions
  references/     optional: documentation read on demand
  scripts/        optional: code the agent runs instead of reading
  assets/         optional: templates and static files
```

## Frontmatter
The file starts with a line `---`, then `key: value` lines, then a closing `---`.

| Field | Required | Rules |
| --- | --- | --- |
| `name` | yes | 1-64 characters: lowercase letters, digits and hyphens; no leading, trailing or doubled hyphen; equals the folder name. |
| `description` | yes | 1-1024 characters. What the skill does and when to use it, with the keywords users would say. |
| `license` | no | A license name or the name of a bundled license file. |
| `compatibility` | no | Up to 500 characters of environment requirements (tools, network, product). Only when needed. |
| `metadata` | no | A map of string keys to string values for client-specific data. |
| `allowed-tools` | no | Space-separated tools the skill expects to use. Experimental; support varies between agents, and in Xylo a skill never gains permissions from it. |

Example:
```
---
name: release-notes
description: Draft release notes from merged changes since the last tag, grouped by user impact. Use when preparing a release or when asked what changed since a version.
---
```

## Progressive disclosure
1. Metadata (about 100 tokens per skill) is listed to the agent at all times.
2. The body (keep under about 5000 tokens and 500 lines) loads when the skill is used.
3. Files under references/, scripts/ and assets/ load only when the body directs the agent to them.

## File references
Use paths relative to the skill folder, for example `references/api.md` or `scripts/check.py`, and say when each should be opened or run. Avoid chains of references that point to further references.
