## What and why

<!-- One paragraph. What changes, and what was wrong before. Link the issue: Fixes #123 -->

## How it was verified

<!-- The commands you ran and the output you saw. "Tests pass" alone is not verification. -->

```text
$ cargo test --locked
...
```

## Checklist

- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked` pass locally
- [ ] The behavior change has a test that fails without it
- [ ] Docs move with the contract: `README.md`, `docs/`, or `ARCHITECTURE.md` where it changed
- [ ] Guards and gates are enforced in code, not only in a prompt
- [ ] No secrets, tokens, client names, or engagement data in the diff, fixtures, or examples
- [ ] No new dependency without a reason below, and `cargo deny check` still passes

## Breaking changes

<!-- CLI flags, config keys, tool schemas, session format, or on-disk paths. "None" is a fine answer. -->

## Notes for review

<!-- Where the reviewer should look hardest, and what you are unsure about. -->
