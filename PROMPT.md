# Session Prompt

Read @MACOS-SPEC.md for the macOS design and @MACOS-TODO.md for the task list. For Linux context, see @SPEC.md and @TODO.md.

Pick the first unchecked task (`- [ ]`) from MACOS-TODO.md. Implement only that single task. Follow the design principles, architecture, and module structure from MACOS-SPEC.md closely.

Use agent teams when it would speed things up — for example, to research crate APIs, explore the codebase, or implement independent pieces in parallel.

## Workflow

* **Read this file again after each context compaction.**
* Code should be simple and clean, well-commented explaining what/how/why.
* Minimal changes — if we iterate and try multiple things, clean up to the minimum required fix at the end.
* Before committing, verify that what you produced is high quality and works.

## After each task

1. Verify the code compiles (`cargo build`). If there are tests, run them (`cargo test`).
2. Mark the completed task as done (`- [x]`) in MACOS-TODO.md.
3. Create a jj change with a descriptive message that explains what was implemented and why.
4. Continue with the next unchecked task.
