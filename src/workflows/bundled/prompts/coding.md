# Coding workflow

A playbook for changing an existing repository. Work one phase at a time. The
gates in code still apply: read-before-edit, budgets, loop detection, and the
settle gates are not suspended by this prompt.

## Phase 0 — Orient

1. Read the repository map, then `AGENTS.md` and `README` when present. Find
   the build, test, lint, and format commands before touching code.
2. Check the working tree with `bash`: current branch, `git status`, and a
   short `git log`/`git diff --stat`. Know which changes are already there and
   which will be yours.
3. Locate the smallest set of files that owns the behavior with `repo_map`,
   `grep`, and `read`. Do not scan the tree file by file.

Output: one short paragraph naming the files, the commands, and the main risk.

## Phase 1 — Plan

For anything beyond a one-file change, call `todo action="plan"` with three to
six steps, each with an observable outcome ("test X passes", "endpoint returns
Y"). Keep the plan in the tool, not only in prose. Re-plan when a fact
invalidates it.

## Phase 2 — Implement in small increments

1. Read a file immediately before editing it, every time it changed.
2. Make the smallest change that satisfies the current step. No drive-by
   refactors, no new dependencies without a stated reason, no formatting-only
   churn in untouched code.
3. After each meaningful edit, run the narrowest check that covers it: the
   single test file, the package's type check, the formatter for that language.
4. If a check fails, read the first failure and fix the cause. Never weaken,
   skip, or delete a test to make it pass.

## Phase 3 — Verify

Run `verify`, or the project's configured check, on the final state. If no check
is configured, run the commands found in Phase 0. A change is not done until the
check passes on the files you touched. Report pre-existing failures separately;
do not hide them inside your change.

## Phase 4 — Review

Run `review` on the diff and read it: no secrets, no debug prints, no commented
out code, no TODO left behind, no unrelated files. Confirm `git diff --check` is
clean. If the change touches auth, parsing, shell, paths, or SQL, say so in the
report and justify the safe construction.

## Phase 5 — Report

Report changed files, the exact verification command and its result, what the
tests do not cover, and the next action. Do not commit, push, tag, or open a PR
unless the user asked.

## Stop conditions

Stop and ask when:

- requirements conflict, or two reasonable designs lead to different code;
- the fix requires deleting or migrating data;
- the check fails for reasons outside the change's scope;
- the task needs credentials, a paid service, or a production host.
