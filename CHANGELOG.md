# Changelog

All notable changes to LocalHarness are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor
release may change defaults when the change is called out below.

## [Unreleased]

### Fixed

- A compaction in the middle of one long agentic turn no longer sends a
  conversation without a user message. The kept tail started at an assistant
  entry, so the request was `system, assistant, tool, ...`, and Qwen-family
  chat templates failed it with HTTP 500 ("No user query found in
  messages."), ending the turn. Context assembly now always opens the
  conversation with a user turn, which covers automatic, forced, and manual
  compaction, thread splits without a kickoff, and resumed sessions.
- The compaction summary's "Last instruction (verbatim)" section keeps the
  instruction whole, line breaks included, when it fits the summary budget.
  It was clipped to 600 characters and flattened to one line, so a long brief
  lost its final constraints. It is now clipped only after every other
  optional section has been dropped.

## [0.2.0] - 2026-11-15

A large feature release covering the engine, permissions, context, skills,
packaging, and a local HTTP API. It includes all work merged in PRs
[#101](https://github.com/GotinhaAzul/LocalHarness/pull/101) through
[#113](https://github.com/GotinhaAzul/LocalHarness/pull/113).

### Added

**Engine and provider**

- One streaming tool-call accumulator with explicit open/closed slots, so an
  engine that reuses a stream index or call id can no longer splice two calls
  together. Call ids are stable across engines, including the nine-character
  ids Mistral-format templates require
  ([#24](https://github.com/GotinhaAzul/LocalHarness/issues/24),
  [#27](https://github.com/GotinhaAzul/LocalHarness/issues/27),
  [PR #101](https://github.com/GotinhaAzul/LocalHarness/pull/101)).
- Reasoning normalization and a thinking policy: `strip_reasoning` controls
  persistence, `reasoning_content`/`reasoning`/`thinking` are all read, and
  compaction keeps the recent exchange and converges on one assembly per step
  ([#5](https://github.com/GotinhaAzul/LocalHarness/issues/5),
  [#7](https://github.com/GotinhaAzul/LocalHarness/issues/7),
  [#25](https://github.com/GotinhaAzul/LocalHarness/issues/25),
  [PR #104](https://github.com/GotinhaAzul/LocalHarness/pull/104)).
- A cache-stable prompt prefix: volatile session facts move to a tail, the
  stable prefix and tool schemas are hashed into a `prompt-prefix` entry, and
  two consecutive low cache-hit rates produce a notice naming the likely
  invalidator ([#43](https://github.com/GotinhaAzul/LocalHarness/issues/43),
  [PR #102](https://github.com/GotinhaAzul/LocalHarness/pull/102)).
- Engine capabilities per model: tool-conformance probing with fixture-backed
  classifications, one structured-output schema lowered to each engine's
  dialect and validated on every reply, and per-model sampler profiles with
  bundled `qwen-coding`, `gemma-chat`, and `glm-agent` presets
  ([#23](https://github.com/GotinhaAzul/LocalHarness/issues/23),
  [#26](https://github.com/GotinhaAzul/LocalHarness/issues/26),
  [#28](https://github.com/GotinhaAzul/LocalHarness/issues/28),
  [PR #110](https://github.com/GotinhaAzul/LocalHarness/pull/110)).

**Security, permissions, and writes**

- Permission modes (`ask`, `accept-edits`, `plan`, `dont-ask`, `yolo`), ordered
  allow/ask/deny rules by tool, path, command, and network, and an enforced plan
  mode. `src/agent/authorize.rs::decide()` is the single pre-dispatch seam, and
  plan-mode denial happens before any hunk buffering
  ([#14](https://github.com/GotinhaAzul/LocalHarness/issues/14),
  [#15](https://github.com/GotinhaAzul/LocalHarness/issues/15),
  [PR #105](https://github.com/GotinhaAzul/LocalHarness/pull/105)).
- Fail-closed lifecycle hooks (`session_start`, `user_prompt`, `pre_tool`,
  `post_tool`, `pre_compact`, `stop`) and instruction provenance. Hooks and
  provenance can deny or add context, never widen policy, and both run through
  the same `authorize::decide()` seam
  ([#16](https://github.com/GotinhaAzul/LocalHarness/issues/16),
  [#32](https://github.com/GotinhaAzul/LocalHarness/issues/32),
  [PR #107](https://github.com/GotinhaAzul/LocalHarness/pull/107)).
- `/rewind [turns]` plus `lh --rewind <session>:<entry>`, and interactive hunk
  review before mutations land. Accepted hunks write atomically and roll back
  as a batch on failure
  ([#19](https://github.com/GotinhaAzul/LocalHarness/issues/19),
  [#56](https://github.com/GotinhaAzul/LocalHarness/issues/56),
  [PR #111](https://github.com/GotinhaAzul/LocalHarness/pull/111)).
- Opt-in `edit_mode = "hash"` for hash-anchored `edit`/`patch`, with stale
  anchors rejected atomically and the same write guard and hunk review as the
  default text mode ([#10](https://github.com/GotinhaAzul/LocalHarness/issues/10),
  [PR #106](https://github.com/GotinhaAzul/LocalHarness/pull/106)).
- Redaction now happens only on persistence, so the model reads raw file text
  and a secret-shaped string in source code cannot be replaced with a
  placeholder that later gets written back. A write guard rejects the
  placeholders themselves
  ([#31](https://github.com/GotinhaAzul/LocalHarness/issues/31),
  [PR #112](https://github.com/GotinhaAzul/LocalHarness/pull/112)).

**Context and skills**

- The `AGENTS.md` chain is loaded hierarchically from the git root to the
  working directory, nearest last, with per-file headers, byte caps,
  truncation warnings, symlink refusal, and `lh context --show`. The same
  release proves persisted secrets stay redacted end to end
  ([#33](https://github.com/GotinhaAzul/LocalHarness/issues/33),
  [#31](https://github.com/GotinhaAzul/LocalHarness/issues/31),
  [PR #103](https://github.com/GotinhaAzul/LocalHarness/pull/103)).
- Skills are spec-compatible, installable from a directory or git ref with a
  pinned lockfile, lintable, and testable with prompt fixtures. Bundled
  scripts always need per-execution approval, including scripts at a skill
  root ([#34](https://github.com/GotinhaAzul/LocalHarness/issues/34),
  [#47](https://github.com/GotinhaAzul/LocalHarness/issues/47),
  [#48](https://github.com/GotinhaAzul/LocalHarness/issues/48),
  [PR #109](https://github.com/GotinhaAzul/LocalHarness/pull/109)).

**Interfaces and distribution**

- `lh serve`: a token-authenticated local HTTP/1.1 API with persistent
  sessions, one turn at a time, SSE events, replay cursors, and cancellation.
  Event names and payloads come from the same `src/events.rs` schema as
  `--print --json` ([#78](https://github.com/GotinhaAzul/LocalHarness/issues/78),
  [PR #113](https://github.com/GotinhaAzul/LocalHarness/pull/113)).
- Static `x86_64`/`aarch64` musl release archives with completions and a man
  page, a checksum-verifying installer, and AUR, Homebrew, Docker, and Nix
  packaging ([#71](https://github.com/GotinhaAzul/LocalHarness/issues/71),
  [PR #108](https://github.com/GotinhaAzul/LocalHarness/pull/108)).

### Changed

- **Default permission mode is `ask`.** Headless runs (`--print`, ACP,
  workflows) refuse edits and commands that would ask, because nobody can
  answer. Use `--mode accept-edits`, add an allow rule under `[permissions]`,
  or use `--yolo` explicitly ([#105](https://github.com/GotinhaAzul/LocalHarness/pull/105)).
- **`strip_reasoning` now drops reasoning from session files by default.**
  Reasoning still streams live; set `strip_reasoning = false` to persist it for
  Ctrl+R replay ([#104](https://github.com/GotinhaAzul/LocalHarness/pull/104)).
- **Redaction is persistence-only.** Raw secrets can reach the configured
  endpoint during a session; session files, artifacts, evidence, and reports
  are still redacted ([#112](https://github.com/GotinhaAzul/LocalHarness/pull/112)).
- **Hunk review defaults to `multi_file` in the TUI.** The first file in a
  turn lands immediately; a second file opens the review list. `always` and
  `off` remain available ([#111](https://github.com/GotinhaAzul/LocalHarness/pull/111)).
- `lh serve` permission modes now install on the session's policy engine, so
  config allow/deny rules apply over HTTP and an unanswerable `ask` is denied.
  There is no separate HTTP approver ([#105](https://github.com/GotinhaAzul/LocalHarness/pull/105),
  [PR #113](https://github.com/GotinhaAzul/LocalHarness/pull/113)).
- Structured output validates every reply and repairs up to three times; a
  strict response format combined with `tools[]` is refused on engines that
  cannot support both ([#26](https://github.com/GotinhaAzul/LocalHarness/pull/26),
  [PR #110](https://github.com/GotinhaAzul/LocalHarness/pull/110)).
- The hierarchical `AGENTS.md` chain loads by default; `--no-agents-md` or
  `LH_AGENTS_MD=0` disables it ([PR #103](https://github.com/GotinhaAzul/LocalHarness/pull/103)).
- `lh doctor` now judges sampler support by a configured `[model] engine` pin
  before the detected engine, so gateway deployments get accurate warnings
  ([PR #110](https://github.com/GotinhaAzul/LocalHarness/pull/110)).
- The Docker image starts in `/workspace`, matching the documented bind mount
  ([PR #108](https://github.com/GotinhaAzul/LocalHarness/pull/108)).

### Fixed

- Redaction no longer corrupts source code or other file text read by the
  model, and placeholder text can no longer be written back into a file
  ([#31](https://github.com/GotinhaAzul/LocalHarness/issues/31),
  [PR #112](https://github.com/GotinhaAzul/LocalHarness/pull/112)).
- `undo` restores a raw in-process snapshot even when the file legitimately
  contains a redaction-shaped string; a redacted snapshot read after a restart
  is still refused ([PR #112](https://github.com/GotinhaAzul/LocalHarness/pull/112)).
- Streamed tool calls survive index/id reuse, and truncated calls are never
  executed ([PR #101](https://github.com/GotinhaAzul/LocalHarness/pull/101)).
- A delayed cache-cold notice keeps the compaction that started the streak as
  its likely invalidator instead of blaming server eviction
  ([PR #102](https://github.com/GotinhaAzul/LocalHarness/pull/102)).
- The `AGENTS.md` per-file cap is applied while reading, so a very large
  ancestor file cannot allocate its full size before truncation
  ([PR #103](https://github.com/GotinhaAzul/LocalHarness/pull/103)).
- Plan mode rejects attached `sort -oFILE` output flags and mutating
  `git reflog expire|delete` forms, and a path-scoped allow rule requires every
  path in a call to match ([PR #105](https://github.com/GotinhaAzul/LocalHarness/pull/105)).
- Session start retries a blocked hook even after another startup hook
  succeeded, `stop` hooks run for a blocked startup, resumed sessions restore a
  pending provenance gate, pre-compaction hook context survives a tail-less
  compaction, and `lh hooks test` exits 1 when a hook invocation fails in warn
  mode ([PR #107](https://github.com/GotinhaAzul/LocalHarness/pull/107)).
- Hunk review preflight routes tool names and path aliases before deciding,
  `/rewind` ignores synthetic continuation prompts when counting turns, and a
  failed multi-file rewind rolls back files it already restored
  ([PR #111](https://github.com/GotinhaAzul/LocalHarness/pull/111)).
- Structured-output validation now runs after text tool-call recovery, rejects
  undeclared keys when `properties` is absent, and enforces exclusive numeric
  bounds and `multipleOf` ([PR #110](https://github.com/GotinhaAzul/LocalHarness/pull/110)).
- Hash-anchored patches reject an invalid `count` instead of applying the
  operation to one line ([PR #106](https://github.com/GotinhaAzul/LocalHarness/pull/106)).
- Skill-root scripts are included in the approval check, relative install
  sources are persisted as resolved absolute paths, and a skip fixture passes
  when a different skill loads ([PR #109](https://github.com/GotinhaAzul/LocalHarness/pull/109)).
- `lh serve` publishes `cancelled` before the terminal `done` frame under the
  event-log lock, and refuses a cross-root session-id collision instead of
  replacing the live entry ([PR #113](https://github.com/GotinhaAzul/LocalHarness/pull/113)).

[0.2.0]: https://github.com/GotinhaAzul/LocalHarness/compare/v0.1.0...v0.2.0
