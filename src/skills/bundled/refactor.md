---
name: refactor
description: "Refactor without changing behavior: define the contract, keep a green baseline, move in verified steps, and stop when the diff stops being mechanical. Use for renames, extracts, or moves."
---

# Refactor without changing behavior

A refactor is a sequence of behavior-preserving moves. If behavior changes, it
is a feature or a fix and belongs in its own change. Keep the two apart.

## 1. Write the behavior contract

Before touching code, list what must stay true:

- public functions, CLI flags, config keys, file formats, and exit codes;
- error messages and status codes that callers or tests match on;
- ordering, timing, and side effects that are observable;
- edge cases the current tests do not cover but users rely on.

Put the list in a `todo` plan; the harness requires one after the second edit.
This is the standard the refactor is reviewed against.

## 2. Get a baseline

Run `verify` on the unchanged tree. Record the command and result. If the suite
is already red, stop: fixing the baseline is a separate task, and you cannot
distinguish your breakage from the existing one.

Check for uncommitted work with `bash` (`{"command":"git status"}`) before
editing. Do not mix a refactor into someone else's unfinished change.

## 3. Small, mechanical steps

One move at a time:

- rename a symbol;
- extract a function;
- move a function to another module;
- replace a repeated block;
- delete now-dead code.

For renames, ask `lsp` for `{"action":"references"}` and
`{"action":"definition"}` at the symbol, then `read` and `edit` every site it
names. There is no rename action: enumeration plus explicit edits is what the
harness can verify. Use `grep` to confirm no string-based or dynamic
references remain.

Do not combine a rename with a logic change, and do not reformat untouched
files. A reviewer should be able to see that each step is provably safe.

## 4. Verify every step

After each move:

1. Compile or type-check.
2. Run the nearest tests.
3. Run `verify` at the end of a group of related moves.

If a step breaks a test, do not "fix it forward" inside the refactor. Revert
with `undo` and the file's path; it refuses when the file changed after the
harness wrote it. Only use `bash` with `git diff -- <path>` first when you
need to discard a change the harness did not make, and never discard work you
did not create.

## 5. Across a boundary

When the refactor crosses a crate, module, or process boundary:

- update every caller and re-export in the same step;
- keep a compatibility shim only if the user asked for a staged migration, and
  mark it for removal;
- check serialized names, reflection, and configuration keys that a compiler
  cannot see;
- search docs, examples, and fixtures for the old name.

## 6. Stop and report

Stop and ask before:

- changing a public API, file format, or persisted data;
- a move that alters error messages or ordering;
- deleting code that might be used dynamically;
- a diff that grows beyond the stated scope.

When you finish, report:

- the behavior contract you preserved;
- the baseline result and the final `verify` result;
- the list of moves, in order;
- anything intentionally left alone.

If you cannot keep every step behavior-preserving, stop and propose the feature
change as a separate task. Do not smuggle it into the refactor.
