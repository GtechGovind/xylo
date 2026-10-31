---
name: write-tests
description: Add or improve automated tests that follow the project's existing test framework and conventions, cover real behavior and edge cases, and are run to prove they pass. Use when the user asks for tests, when a fix needs a regression test, or when new code lacks coverage.
---
# Write tests

Goal: tests that would fail if the behavior broke, written the way this project already writes tests, and seen passing.

## 1. Learn the project's conventions
- find_files for existing tests near the code (for example `**/*test*`, `**/tests/**`, `**/*_spec*`) and read two or three with file_read.
- Note the framework, file placement, naming, fixtures and helpers, and how tests are run (package scripts, Makefile, Cargo, CI configuration). Reuse existing helpers instead of inventing new ones.

## 2. Decide what to test
- Read the code under test and list its observable behaviors: normal results, boundaries (empty, one, many, maximum), invalid input and errors, and any state or ordering it promises.
- For a bug fix, first write the test that reproduces the bug and see it fail.
- Test through the public interface. Avoid asserting on private details that a harmless refactor would change.

## 3. Write them
- One behavior per test, with a name that says what is expected.
- Arrange, act, assert, with literal expected values rather than values recomputed by the code under test.
- Keep tests deterministic: no real network, wall-clock time or random seeds without control; use temporary directories for files.
- Mock only true external boundaries.

## 4. Run and check
- Run the new tests with run_command or shell_command, then the surrounding suite.
- Check that each new test can fail: if practical, temporarily break the behavior or recall the failing run from step 2.
- Report what is covered, the command used and its result, and any behavior you chose not to test and why.
