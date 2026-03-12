# Session Prompt

Read @REVIEW-TODO.md for the review issue list. For macOS design context, see @MACOS-SPEC.md. For Linux context, see @SPEC.md.

Pick the first unchecked task (`- [ ]`) from REVIEW-TODO.md. Fix only that single issue. Follow the existing architecture and coding conventions.

Use agent teams when it would speed things up — for example, to research crate APIs, explore the codebase, or implement independent pieces in parallel.

## Workflow

* **Read this file again after each context compaction.**
* Code should be simple and clean, well-commented explaining what/how/why.
* Before committing, verify that what you produced is high quality and works.
* If a fix touches shared types (e.g. renaming `zvol_path`), update all call sites in the same pass.
* If a fix is in Swift (`ember-vz`), rebuild with `cd ember-vz && swift build` to verify.

## Bug fixes: red/green TDD

When fixing an actual bug (not a refactor or cosmetic change), use red/green TDD:

1. **Red**: Write a test first that reproduces the bug. This can be a unit test or an integration test — don't shy away from adding new integration tests. Run the test and confirm it fails (red).
2. **Green**: Implement the fix. Run the test again and confirm it passes (green).
3. Run the full test suite (`cargo test` and/or `./run-integration-tests.sh`) to make sure nothing else broke.

## After each task

1. Verify the code compiles (`cargo build`). For Swift changes, also `cd ember-vz && swift build`.
2. Run `cargo fmt`.
3. Mark the completed task as done (`- [x]`) in REVIEW-TODO.md.
4. Create a jj change with a descriptive message that explains what was fixed and why.
5. Continue with the next unchecked task.
