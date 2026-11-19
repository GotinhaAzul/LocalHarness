---
name: test-repair
description: "Repair a failing test or a small cluster: reproduce, classify, form one hypothesis, make the smallest fix, then prove the test and the suite. Use when a test fails or the suite is red."
---

# Repair a failing test

Work from the failure, not from a guess about the code. The goal is a fix that
makes the failing behavior correct, not a change that silences the assertion.

## 1. Reproduce

- Run the exact failing test with `verify` (`{"path":"tests/foo.rs","test":"name"}`).
  If you do not know the file, use `bash` with the project's test command.
- Copy the first error verbatim into your notes: test name, file and line,
  expected value, actual value, panic or assertion text.
- Confirm the failure is deterministic by running it once more. An
  intermittent failure is a different task; report that instead of guessing.
- If the test does not fail, stop and say so. A "fix" for a green test is a
  change with no evidence.

## 2. Classify before editing

Read the test and the code it exercises with `read`; use `grep` and `lsp` to
find the real definition instead of assuming a name.

Choose one class and write it down:

| Class | Evidence | Typical fix |
| --- | --- | --- |
| Product bug | The test's expected behavior matches the spec and the code disagrees | Change the code |
| Stale test | The test asserts old behavior that the issue or docs replaced | Change the test, with the source that justifies it |
| Environment | Missing tool, port, fixture, or dependency; the test passes elsewhere | Fix setup, not assertions |
| Flaky | Timing, ordering, shared state; passes on rerun | Make the test deterministic |

Never change an assertion only because it is inconvenient. If the expected
behavior is genuinely unclear, stop and ask which behavior is intended.

## 3. One hypothesis

State one sentence: "The failure happens because X at file:line." If you cannot
state it, gather one more fact (a log line, a value, a call path) before
editing. Do not apply two unrelated fixes at once; you will not know which one
worked.

## 4. The smallest fix

- Read the file before `edit`; the harness refuses edits to unread
  files for a reason.
- Change the fewest lines that make the diagnosed cause impossible.
- Keep the test's intent. If the test is wrong, fix the test and cite the
  behavior it should assert.
- Do not delete, skip, or loosen the test to get green. That is a failure to
  repair, and you should report it as one.

## 5. Prove it

1. Run the exact failing test again. It must pass.
2. Run the neighboring tests for the same module or file.
3. Run `verify` for the configured suite. If it fails on a different test,
   return to step 1 with the new failure; do not widen the edit blindly.
4. If `verify` is not configured, run the project's documented suite with
   `bash` and show the command and result.

## 6. Report

Give the user:

- the original failure and its class;
- the cause in one sentence, with file:line;
- the change, in one or two sentences;
- the exact commands run and their results, including the full suite;
- anything you did not fix, and why.

## Stop and ask

Stop before editing when:

- the expected behavior is ambiguous or contradicts the issue;
- the fix would require a schema, API, or data migration;
- the failure is intermittent and you have no reproduction;
- the test depends on a service or credential you do not have.

A short report of a blocked reproduction is a good outcome. A green test with
no explanation is not.
