# Session Prompt

Read @TEST-TODO.md for the task list. For test architecture, see @TEST-SPEC.md. For macOS design context, see @MACOS-SPEC.md. For Linux context, see @SPEC.md.

Pick the first unchecked task (`- [ ]`) from TEST-TODO.md. Implement, verify, check off, commit, follow the workflow below.

Use agent teams when it would speed things up — for example, to explore existing test files, research patterns, or implement independent pieces in parallel.

## Workflow

* **Read this file again after each context compaction.**
* Code should be simple and clean, well-commented explaining what/how/why.
* Before committing, verify that what you produced is high quality and works.
* If a fix is in Swift (`ember-vz`), rebuild with `cd ember-vz && swift build` to verify.
* Follow the design in TEST-SPEC.md closely — especially the `TestEnv` abstraction and file structure.
* All integration tests must drive the `ember` CLI binary (black-box testing). No internal function calls.
* Platform differences go in `TestEnv` setup or `#[cfg(target_os)]` blocks, not separate files.
* When extracting helpers into `common/`, grep for all copies across test files to make sure nothing is missed.

## After each task

1. Verify the code compiles: `cargo build --tests`.
2. Run `cargo fmt`.
3. Run `./run-integration-tests.sh <suite>` for any modified test file to verify tests still pass.
4. Mark the completed task as done (`- [x]`) in TEST-TODO.md.
5. Create a jj change with a descriptive message (e.g. `tests: extract Linux helpers into common/linux.rs`).
6. Continue with the next unchecked task.
