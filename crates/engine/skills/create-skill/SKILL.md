---
name: create-skill
description: Create or improve an Agent Skills package (a folder with SKILL.md plus optional references, scripts and assets) that teaches an agent a repeatable workflow. Use when the user wants to turn a procedure, checklist or domain knowledge into a reusable skill, or to fix a skill that triggers poorly or gives weak instructions.
---
# Create a skill

A skill is a folder whose `SKILL.md` starts with frontmatter (`name`, `description`) followed by Markdown instructions. Agents see only the name and description until the skill is used, then read the body, then open bundled files only when the body points to them. Design for that: the description decides whether the skill is ever used, and the body must be worth its context cost. The full format rules are in references/format.md; read it with extension_resource (package `create-skill`, path `references/format.md`) before writing the frontmatter.

## 1. Capture the workflow
- Ask for, or find in the conversation, two or three concrete examples of requests the skill should handle, and what a good result looks like for each.
- Identify the steps that are not obvious to a capable engineer: project conventions, required order, commands, pitfalls, checks. Leave out what any competent agent already does.
- Decide where the folder goes. Ask the user if they have not said; `.agents/skills/<name>/` in the repository is a common cross-tool location.

## 2. Write the description
One or two sentences, at most 1024 characters: what the skill does, then "Use when ..." with the words a user would actually say. Be specific; a vague description either never triggers or triggers everywhere.

## 3. Write the body
- Imperative, standing instructions organized as numbered phases or a checklist; aim for well under 400 lines.
- Name the agent's real tools where it matters (for example file_read, search_project, run_command) and the exact commands to run.
- State how to verify success and what to report at the end.
- Move long reference material (API details, schemas, large examples) into `references/<topic>.md`, templates into `assets/`, and runnable helpers into `scripts/`. Reference each file by its relative path from the body and say when to open it. Keep references one level deep.
- Never put secrets, credentials or machine-specific absolute paths in a skill.

## 4. Check it
- Validate the frontmatter against references/format.md: the name matches the folder, uses only lowercase letters, digits and single hyphens.
- Re-read the body as an agent that knows nothing about this project: is every step actionable? Is anything missing for one of the examples from step 1?
- Walk through one example request against the skill and fix gaps you find.
- Tell the user where the skill was written and that it is used after they install the folder as a skill package in Xylo's extension settings.
