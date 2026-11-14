# Contributing

The backlog is public and mostly claimed: [issue #45](https://github.com/GotinhaAzul/LocalHarness/issues/45)
maps it, and most issues name the area and the intended shape of the fix. Read
it before starting something new.

**Security reports do not go here.** Follow [SECURITY.md](SECURITY.md).

## Before you write code

1. Search [issues](https://github.com/GotinhaAzul/LocalHarness/issues) and
   [discussions](https://github.com/GotinhaAzul/LocalHarness/discussions). If the
   item exists, say so on it and ask whether it is taken.
2. For anything larger than a typo, open an issue or comment on the existing one
   and get a yes before the PR. An unsolicited 2,000-line PR against a moving
   `main` gets closed, not reviewed.
3. Read [ARCHITECTURE.md](ARCHITECTURE.md). It is the implementation
   specification for the loop, the gates, and the tool protocol. A change that
   contradicts it needs to say why in the PR.

## Setup

Rust 1.88 or newer. Linux x86_64 or aarch64; the harness is not supported on
macOS or Windows.

```sh
git clone https://github.com/GotinhaAzul/LocalHarness
cd LocalHarness
cargo build --release
./target/release/lh --help
```

Build what the users install:

```sh
cargo install --path . --locked
```

The binary is `lh`; the crate is `localharness`. You do not need a model server
to develop or to run the test suite.

## Checks

CI runs these and they are all required:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

Advisories and licenses run alongside CI and are advisory today:

```sh
cargo audit
cargo deny check
```

`deny.toml` bans wildcard versions, unknown registries, and unknown git sources.
A new dependency is a design decision: justify it in the PR, or expect the
question.

Integration tests live in `tests/` (`agent_loop`, `headless`, `mcp`,
`provider_http`) with fixtures under `tests/fixtures/`. No test needs a real
model or network access; keep it that way.

## What a good change looks like

- **One concern per PR.** Small diffs get reviewed in an hour; mixed diffs wait.
- **Guards are code, not prompt text.** The project's stance is that a rule the
  model can ignore is not a rule. A fix that adds a sentence to the system prompt
  where a code path belongs will be sent back.
- **Tests for behavior.** A change to the loop, a gate, the context budget, or
  the tool protocol needs a test that fails without it. Wording, formatting, and
  snapshot-of-source tests are not wanted.
- **Docs move with the contract.** CLI flags, config keys, tool schemas, and
  session formats are documented in `README.md`, `docs/`, and `ARCHITECTURE.md`.
  Change the code and the doc in the same PR.
- **No secrets anywhere.** Not in tests, fixtures, example configs, or the PR
  body. Use `127.0.0.1` endpoints and placeholder keys.
- **Errors name the next action.** Tool errors are read by a model that has to
  recover from them. `failed` is not an error message.

## Commits and PR titles

Write the PR title as a sentence that reads well in `git log --oneline`:
"Keep the recent exchange through compaction", not "fix stuff". The PR body
explains what changed and how you verified it; the template asks for both.
History is merge commits on `main`; do not force-push a branch someone is
reviewing.

## Review

- A maintainer reviews; CI green is necessary, not sufficient.
- Changes to `src/security/`, the sandbox, redaction, or the gates get a second
  read.
- If a PR goes quiet, comment on it. A PR with no activity for 60 days may be
  closed by hand; reopen it with a comment and it comes back.

## What gets closed

- Drive-by rewrites, mass reformatting, and dependency bumps without a reason.
- Prompt-only fixes for enforcement problems.
- Feature PRs with no issue and no prior agreement.
- Anything that adds telemetry, a hosted service, or a Node runtime. The
  product stance is local-first, one binary, no phone-home.
- Issues with no reproduction that stop responding to `needs-info`.

## License

Contributions are dual-licensed under MIT OR Apache-2.0, matching the project.
By opening a PR you agree to license your contribution under those terms.
