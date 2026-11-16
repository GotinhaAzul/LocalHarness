---
name: code-review
description: "Review a diff, PR, or patch for correctness, security, tests, compatibility, and maintainability; report findings by severity with a verdict. Use for a PR, patch, or your own work."
---

# Review a change

Review the change as it will land, not the author's description of it. Read the
diff first, then the code around it. Do not edit files unless the user asked
for fixes as well as a review.

## 1. Bound the review

- Get the scope: `review` shows working-tree changes; for a branch, use `bash`
  with `git diff <base>...<head>` and `git log`.
- List the files and the purpose of each. A change with no coherent purpose is
  a finding in itself.
- Note generated files, vendored code, and lockfiles; review them for presence
  and consistency, not line by line.
- If the diff is large, review by behavior: entry points, state changes, and
  error paths first.

## 2. Read for correctness

For every changed function, answer:

- What are the inputs, including empty, zero, negative, huge, and malformed?
- What are the outputs and errors on each path?
- What happens on partial failure, retry, cancellation, or concurrent calls?
- Does the change preserve the old contract where callers depend on it?
- Is state initialized, bounded, and released?

Use `read` on the full file, `grep` for callers, and `lsp` for definitions
rather than trusting names. A changed signature used in ten places is ten
review sites.

## 3. Read for security

- Untrusted input reaching a shell, path, query, template, or deserializer.
- Path traversal, symlink following, and writes outside the intended root.
- Authorization checked at the boundary, not only in the UI.
- Secrets in logs, errors, fixtures, or committed files.
- Unsafe blocks, FFI, and integer or buffer arithmetic.
- Dependency changes and new network destinations.

Name the concrete path from input to impact. "Could be unsafe" is not a
finding; "request field `name` becomes the path in `open()` at file:line"
is.

## 4. Check the tests

- Does a test fail without the change and pass with it?
- Is the test asserting behavior, or just the implementation's current shape?
- Are error paths, boundaries, and the bug being fixed covered?
- Do existing tests still express their original intent?
- Is anything skipped, ignored, or made order-dependent?

A behavior change with no test is a finding even when the code looks right.

## 5. Compatibility and operations

- Public API, CLI flags, config keys, file formats, and exit codes.
- Migration and rollback: what happens to existing data and old clients?
- Performance: new work per request, unbounded growth, blocking calls on an
  async path.
- Logging and observability: can an operator tell what happened?
- Documentation and changelog when behavior is user-visible.

## 6. Severity

| Severity | Use for |
| --- | --- |
| blocker | Data loss, security hole, crash, broken build, or a wrong result on a normal path |
| major | Wrong result on a plausible edge case, missing validation, contract break |
| minor | Maintainability, naming, duplication, missing test for a small path |
| nit | Style and preference; say so explicitly |

Order findings by severity, then by file and line. Each finding needs: what is
wrong, where, why it matters, and the smallest acceptable fix. Do not pad the
list; three real findings beat twenty observations.

## 7. Verdict

End with one of:

- **Approve** - no blocker or major finding.
- **Approve with comments** - minor findings only; list them.
- **Request changes** - at least one blocker or major finding.

State what you did not review (unfamiliar subsystem, missing context, generated
code) so the verdict is not read as broader than it is.

## Stop and report

Stop reviewing and report when:

- the diff is too large to review responsibly in one pass; propose a split;
- the change depends on a design decision the user has not made;
- you cannot build or run the tests, and the verdict would be guesswork.

This skill grants no tools, scope, or permissions. The harness enforces those.
