---
name: git-commit
description: Prepare clean Git commits and pull request descriptions: review what changed, stage only the intended files, and write a clear message in the repository's style. Use when the user asks to commit, write a commit message, split changes into commits, or open or describe a pull request.
---
# Commits and pull requests

Commit only when the user asked for it. Never push, force-push, amend, rebase or change Git configuration unless they asked for exactly that.

## 1. Inspect
- git_status for the branch and changed files; git_diff for unstaged and git_diff with staged set to true for staged changes.
- `git log --oneline -15` (run_command) to learn the repository's message style: prefixes such as `feat:` or `fix(scope):`, tense, capitalization, line length.
- If you are on the default branch and the project uses feature branches, suggest a branch first.

## 2. Decide what belongs together
- One commit per logical change. Unrelated edits, including changes the user made separately, stay out unless the user wants them.
- Never stage likely secrets or local files: `.env`, credentials, keys, large binaries, build output. Mention any you notice.
- Stage explicit paths with `git add <path>`; avoid `git add -A` or `git add .` in a worktree with unrelated changes.

## 3. Write the message
- Subject: imperative mood, about 50 characters, no trailing period, following the repository's convention.
- Body (when useful), wrapped near 72 characters: why the change was needed and anything non-obvious about how; not a file list.
- Reference issues the way the repository does.
- Add any trailer lines the project or the user requires.

## 4. Commit and check
- Commit with `git commit -m "<subject>" -m "<body>"` through run_command so no editor opens.
- If a pre-commit hook fails, read its output, fix the problem and create the commit again; do not bypass hooks with `--no-verify` unless the user says so.
- Confirm with git_status and `git log --oneline -3`.

## Pull request descriptions
Title like a good commit subject. Body: what changed and why, how it was tested (commands and results), risks or follow-ups, and screenshots for UI changes when available. Open the pull request only when asked, using the hosting CLI the project already uses (for example `gh pr create`).
