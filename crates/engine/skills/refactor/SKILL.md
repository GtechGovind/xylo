---
name: refactor
description: Restructure code without changing its behavior, such as renaming, extracting or moving functions, splitting modules or removing duplication, in small verified steps. Use when the user asks to refactor, clean up, rename, reorganize or simplify code while keeping it working.
---
# Safe refactoring

Goal: better structure, identical behavior, every step verified.

## 1. Pin down current behavior
- Identify the code to change and every use of it: search_project for names (including string, reflection or configuration references), find_files for related modules.
- Run the existing tests that cover it and note the result. If coverage is thin and the change is risky, add characterization tests first that record current behavior, including odd cases.

## 2. Plan small steps
- Break the work into steps that each leave the code compiling and tests passing: rename, then move, then simplify; not all at once.
- For more than a few steps, track them with todo_write.
- Keep public interfaces stable unless the user agreed to change them; if they must change, update every caller in the same step.

## 3. Execute each step
- Make the mechanical change with file_edit; use replace_all only after confirming every occurrence should change.
- Prefer the language's own tools when available (compiler errors after a rename show missed call sites).
- Do not mix behavior changes or bug fixes into a refactoring step. If you find a bug, note it and ask, or fix it as a separate, clearly reported change.
- Run the build and the relevant tests after each step; fix or revert the step before continuing.

## 4. Finish
- Remove code made dead by the refactor, and stale comments.
- Run the formatter, linter and the full relevant test suite.
- Report what moved or was renamed, confirm behavior is unchanged with the commands you ran, and list any follow-ups.
