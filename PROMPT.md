# Session Prompt

When working on a spec, create or update a tracking file that records which parts of the spec have been implemented and which remain. Check off items as you complete them. This file is the bridge between the spec and the git history — it should stay accurate.

Before starting work, check `git log` to understand what has already been implemented. The spec may describe features that are already done. Use the git history (commits, diffs) as the source of truth for current implementation state.

Pick the first unimplemented item from the spec/TODO and complete one self-contained unit of work. Do not chain multiple items in one session. Implement, verify, update the tracking file, commit, then stop.

## Workflow

* Code should be simple and clean, well-commented explaining what/how/why.
* Before committing, verify that what you produced is high quality and works.
* Follow the design in the spec closely.
* When exploring for design or debugging, start producing actionable output (plans, hypotheses, code) early. Don't spend the whole session just reading code.
* Deliver complete implementations — do not silently cut scope, leave mocks in place of real logic, or substitute hardcoded data where dynamic generation or fetching is feasible. When a full solution seems impractical, say so explicitly, explain the constraints, and workshop an alternative approach with the user rather than unilaterally downgrading the design.

## After each task

1. If you changed code, verify it compiles: `cargo build`.
2. If you changed Rust code, run `cargo fmt`.
3. If you changed code, run tests for any modified code to verify they still pass.
4. Run `./run-integration-tests.sh <suite>` for any modified test file to verify tests still pass.
5. Mark the completed item as done in the tracking/TODO file.
6. Create a commit with a descriptive message.
7. Stop after this self-contained unit of work. Do not continue to the next task automatically.
