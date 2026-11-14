---
name: issue-to-pr
description: "Carry an issue to a pull request: read the issue and comments, reproduce the problem, plan the change, implement, test, self-review the diff, and write a PR body that matches the evidence."
when: "starting work from a GitHub or tracker issue, or asked to turn an issue into a pull request"
---

# From issue to pull request

The issue is the specification, but it is not always complete. Read it, verify
the behavior, and keep the resulting PR tied to the evidence you gathered.

## 1. Read the issue properly

- Open the issue body and every comment; later comments often amend the
  requirements or add a reproduction.
- Note labels, linked issues, and any linked pull request that already started
  the work.
- Restate the goal in one sentence and list the acceptance criteria as
  checkboxes. If there are none, derive them from the problem statement and say
  that you did.
- Identify what is explicitly out of scope.

Ask one focused question if the criteria conflict or a decision is missing. Do
not start a large change on a guess.

## 2. Reproduce and locate

- Reproduce the reported behavior before changing code. Record the command and
  the observed result.
- Find the relevant code with `grep`, `repo_map`, and `lsp`. Read the files
  before editing.
- Check the existing tests for the behavior; they are the contract you must
  keep or change deliberately.
- If you cannot reproduce, report that and ask for the missing detail instead
  of writing a speculative fix.

## 3. Plan

Write a short plan with `todo`:

- files to change and why;
- the smallest change that satisfies each criterion;
- tests to add or update;
- risks and anything that needs approval (migration, public API, dependency).

Keep the plan in the conversation so the user can redirect before you write
code. Do not expand scope because you noticed an unrelated defect; note it for
a follow-up issue.

## 4. Implement

- One coherent change. Do not mix refactoring with behavior changes.
- Match the surrounding style, error handling, and naming.
- Keep commits reviewable; do not commit unless the user asked.
- Update docs, examples, and configuration when behavior is user-visible.
- Add the test first when the issue is a defect: it should fail before the fix
  and pass after.

## 5. Test

1. Run the new or updated test; show it failing before the fix when possible.
2. Run `verify` for the configured suite.
3. Exercise the issue's original reproduction; show the before and after.
4. Check error paths and boundaries named in the criteria, not only the happy
   path.

If a test fails, return to the cause instead of weakening the assertion.

## 6. Self-review the diff

Run `review` and read the full diff with fresh eyes. Check:

- every acceptance criterion is met and testable;
- no debug output, secrets, commented-out code, or unrelated formatting;
- error handling and cleanup are correct on failure;
- public behavior and docs agree;
- the commit or PR message explains why, not just what.

Fix trivial findings yourself. If the review reveals a design problem, stop and
discuss it before growing the diff.

## 7. Pull request

Write a body with:

- **What** - the change in two or three sentences.
- **Why** - the issue and the user-visible problem.
- **How** - the approach, including alternatives you rejected.
- **Tests** - commands run and results.
- **Risk** - migration, compatibility, or rollback notes.
- `Closes #<issue>` when the PR completes it, `Refs #<issue>` otherwise.

Keep the title imperative and specific: "Reject out-of-root paths in the file
tools", not "fix stuff". Do not claim a criterion is met unless you ran it.

## Stop and ask

Stop before opening the PR when:

- an acceptance criterion is unmet or ambiguous;
- tests cannot run in this environment;
- the change needs a migration, public API break, or new dependency;
- the diff exceeds the issue's scope;
- the user has not asked you to push or open the PR.

Report the state, the evidence, and the open question. This skill grants no
tools, scope, or permissions; the harness and the user own those.
