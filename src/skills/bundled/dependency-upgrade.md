---
name: dependency-upgrade
description: "Upgrade a dependency safely: read the changelog, find breaking changes, change one package, adapt the code, test, and keep a rollback path. Use for version bumps or CVE patches."
---

# Upgrade a dependency

Upgrades fail from skipped release notes and from doing several at once. Read
first, change one package, and keep the previous lockfile until the suite is
green.

## 1. Establish the target

- Current version, target version, and why: security fix, needed feature,
  maintenance, or transitive pressure.
- The package manager and lockfile in use (`Cargo.lock`, `package-lock.json`,
  `uv.lock`, `go.sum`, ...).
- Whether the upgrade is direct or forced by another package.

For a security fix, record the advisory id and the affected range. Do not
upgrade unrelated packages to "clean up" in the same change.

## 2. Read the release notes

Read every release between current and target, not just the last one. For each
breaking change, write down the old and new behavior and the code in this repo
that depends on it. Use `grep` to find call sites; use `lsp` to confirm types.

If the changelog is missing, inspect the diff or the exported API, and say in
the report that you did. Never assume a patch bump is safe because it is small.

## 3. Change one package

- Edit the manifest to the target version, then let the package manager update
  the lockfile.
- Do not hand-edit the lockfile.
- If another package pins a conflicting version, report the conflict; do not
  force two versions silently unless the ecosystem expects it.
- Keep the old lockfile content available (a copy or VCS) for rollback.

## 4. Adapt the code

Work through the breaking changes one at a time. After each:

1. Build or type-check.
2. Run the nearest tests.
3. Note the migration in the report.

Prefer the new public API over compatibility shims. Add a shim only when the
user asked for a staged migration, and leave a comment naming its removal
condition.

## 5. Prove it

- Run `verify` for the full suite.
- Exercise the path that uses the package, not just its unit tests: a CLI
  command, an HTTP route, a serialization round-trip.
- Check behavior that a compiler cannot: default values, timeout units, error
  types, log formats, and feature flags.
- For a security fix, confirm the vulnerable version is gone from the resolved
  graph, including transitive copies.

## 6. Rollback

If the upgrade cannot be completed:

- restore the manifest and lockfile, then re-run `verify`;
- report the exact breaking change that blocked it;
- propose the smallest follow-up: pin the old version with a note, patch
  locally, or schedule the migration.

Do not leave the tree half-upgraded. A clean revert with a clear blocker is a
good result.

## Stop and ask

Stop before proceeding when:

- the upgrade requires a data migration or a public API break;
- the new version drops a platform or runtime you must support;
- the license changed;
- the package is unmaintained and the fix needs a fork;
- the build needs network or credentials you do not have.

Report the changelog evidence, the blocked step, and the options. This skill
grants no tools, scope, or permissions.
