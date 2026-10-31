---
name: debug-failure
description: Systematically find and fix the cause of a failing build, test, crash or wrong behavior by reproducing it, forming hypotheses and confirming the root cause before changing code. Use when a command fails, a test is red, the user reports a bug or error message, or behavior differs from what is expected.
---
# Debug a failure

Goal: fix the root cause with the smallest correct change, proven by a run that failed before and passes after.

## 1. Reproduce
- Run the exact failing command with run_command or shell_command; prefer the narrowest form (one test, one package, one file).
- If the user gave only an error message, search_project for its text to find where it is raised.
- Record the exact error, exit code and the first failure in the output; later errors are often consequences.
- If it does not reproduce, compare environment: working directory, versions, environment variables, generated files, test order.

## 2. Read the failure
- Read the code at each frame or file:line in the error with file_read, from the innermost project frame outward.
- For build errors, fix the first error first; many later ones disappear with it.
- Check recent changes with git_diff and git_status; a new failure usually lives in what changed.

## 3. Hypothesize and test
- Write down the most likely causes, ranked. For each, find the cheapest check that would rule it in or out: read a value, add a temporary assertion or log line, run a smaller case.
- Change one thing at a time, and remove temporary diagnostics before finishing.
- If a hypothesis is disproved, say so to yourself and move to the next; do not stack speculative fixes.

## 4. Fix the cause
- Fix where the wrong value or state originates, not where it is noticed. Do not silence the error, loosen the assertion, skip the test or catch-and-ignore unless that is truly the correct behavior.
- Search for the same mistake elsewhere with search_project.
- Add or update a test that fails without the fix when the project has tests for that area.

## 5. Verify
Rerun the original failing command, then the broader suite for the affected package. Report the cause, the fix (path:line) and the commands that now pass. If you could not fully fix it, report what you established and the next step.
