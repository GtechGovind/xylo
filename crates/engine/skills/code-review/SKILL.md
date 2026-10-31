---
name: code-review
description: Review a diff, branch or set of files for correctness bugs, security problems, regressions and missing tests, and report findings by severity with path:line references. Use when the user asks for a review, asks what could break, or wants changes checked before committing or merging.
---
# Code review

Goal: find the problems that matter in the change, prove each one from the code, and report them so the author can act. A review changes no files unless the user asks for fixes.

## 1. Establish the change
- Uncommitted work: git_status, then git_diff (and git_diff with staged set to true for the index).
- A branch or commit range: run `git diff <base>...HEAD` or `git log --oneline <base>..HEAD` with run_command.
- Specific files: read them with file_read.
Note what the change is meant to do, from the user's request, commit messages or nearby comments.

## 2. Read in context
For every changed hunk, read enough of the surrounding file with file_read to understand it, and use search_project to find callers, implementations and tests of each changed function, type or setting. A change is only correct relative to how it is used.

## 3. Look for real defects
Check, in order of importance:
1. Correctness: wrong conditions, off-by-one errors, unhandled `None`/null/error cases, broken invariants, changed behavior for existing callers, races and ordering assumptions, resource leaks.
2. Security: injection (shell, SQL, paths), missing authorization or validation, secrets in code or logs, unsafe deserialization, paths escaping their root. Security checks must fail closed.
3. Data and compatibility: migrations, serialized formats, public APIs and configuration that other code or users depend on.
4. Tests: is the new behavior tested, and would the tests fail if the change were wrong?
5. Maintainability, only when it will cause bugs: duplicated logic that will drift, misleading names or comments.
Do not report style preferences the project's formatter or linter already decides.

## 4. Confirm before reporting
For each candidate, trace the actual code path. If a test or quick command can demonstrate it, run it with run_command or shell_command. Drop anything you cannot support; mark what is plausible but unproven as a question.

## 5. Report
Findings first, most severe first. For each:
- Severity (high, medium or low) and a one-line title.
- `path:line` and what goes wrong, with the concrete input or sequence that triggers it.
- The fix, in a sentence or a short snippet.
Then open questions, then a one-paragraph overall assessment. If there are no findings, say so plainly and name what you checked.
