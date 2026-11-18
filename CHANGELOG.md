# Changelog

All notable changes to LocalHarness are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor
release may change defaults when the change is called out below.

## [Unreleased]

### Added

- `/permissions` gathers the permission levels in one TUI command. With no
  argument it opens the filterable picker over `normal`, `accept-edits`,
  `deny-all`, and `yolo`, marking the current level and describing each one;
  `/permissions <level>` sets one directly, and Tab completes the names. The
  footer spells out a non-default level (`yolo: no approvals`,
  `deny-all: anything that would ask is denied`). `yolo` needs a one-key
  confirmation, lasts only for the session, never edits the config, and is
  recorded in the session like `--yolo`. `/plan` is unchanged.
- A welcome screen with Skoll and Hati, two pixel-art wolves closing in on an
  eclipse, drawn with half blocks (the eclipse alone on narrow terminals,
  outlines only with `NO_COLOR`).
- `sun` and `moon` theme tokens for the interface's two voices (the model and
  the harness), and the bundled themes `hati` and `skoll`.
- Live, fuzzy `@` file mentions from a cached, gitignore-aware project index:
  the popup shows each file's name and folder as you type.
- The terminal title shows whether a turn is running, waiting for approval,
  or done, and is restored on exit.

- The skill index is capped at 200 characters per entry and 2400 characters in
  total; the remaining skills are listed by name and stay loadable.
  `[skills] index = "full" | "names" | "off"` chooses the form,
  `lh skill lint` warns when an entry would be clipped, and `lh context`
  reports the size and the clipped names
  ([#121](https://github.com/GotinhaAzul/LocalHarness/issues/121)).
- `short_tool_schemas` accepts `true`, `false`, and `"auto"`. Auto sends the
  short schemas when the system prompt plus the full schemas would exceed 20%
  of the usable prompt, decided once per session and model so the prefix stays
  byte-stable. The default stays the full form, and `lh doctor` prints both
  sizes ([#122](https://github.com/GotinhaAzul/LocalHarness/issues/122)).
- When a successful edit, write, or patch leaves the run one step from the
  todo gate, its tool result says the next edit needs a plan. Once per turn;
  the gate itself is unchanged
  ([#124](https://github.com/GotinhaAzul/LocalHarness/issues/124)).
- `read` takes `paths`: up to six entries, each optionally `path:start-end`.
  The result budget is split between them in whole lines, and each file prints
  under its own header with its own next-start trailer
  ([#125](https://github.com/GotinhaAzul/LocalHarness/issues/125)).
- `read` takes `symbol` and returns a declaration's whole extent, doc comments
  and attributes included. The range comes from a language server that is
  already running, otherwise from the scanner plus brace or indentation
  matching; ambiguous names list candidates and unknown names list the nearest
  declarations ([#126](https://github.com/GotinhaAzul/LocalHarness/issues/126)).
- New opt-in `fetch` tool: `GET`s one public http(s) URL and returns readable
  text, converting HTML and paging long pages by character offset. It is in no
  built-in profile, classifies as a network call whose approval subject is the
  full URL, sends no credentials or cookies, caps the body at 2 MB and the
  request at 20 s, and refuses loopback, private, link-local, CGNAT, and
  metadata addresses on every hop unless `[fetch] allow_private` names the
  host ([#127](https://github.com/GotinhaAzul/LocalHarness/issues/127)).
- `lh verify [--init]` shows or saves verify steps detected from the project's
  manifest files, and `/verify [accept]` does the same in the TUI; nothing
  from the repository runs before the user accepts. Accepted steps live in
  `verify-steps.json` in the user data directory, keyed by project, so they
  load without `--trust`. The header shows `verify: not configured`, and
  `lh doctor` reports what is configured or detected
  ([#128](https://github.com/GotinhaAzul/LocalHarness/issues/128)).

### Fixed

- `review` and the `git` tool stop at the session root. A workspace without a
  `.git` of its own no longer inherits the repository that contains it:
  `review`, `status`, `diff`, `log`, `blame` and `commit_message` report only
  files under the workspace and paths that leave it are refused, while
  `stage`, `unstage`, `commit`, `undo`, `branch`/`worktree` mutations,
  `/commit` and `pr draft` return `GIT_REPO_OUTSIDE_ROOT` and change nothing in the parent
  repository ([#136](https://github.com/GotinhaAzul/LocalHarness/issues/136)).
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
- The token estimate no longer charges the tool schemas twice. The prose
  characters-per-token ratio was learned from the whole request, schemas
  included, and the schemas were then added again at their fixed price, so
  the estimate ran about 5-7% high and compaction started early. The ratio is
  now learned from the request minus the schemas.
- The post-compaction state block rides on the last message of each request
  instead of the leading system message, so a change in changed files, verify
  status, or the budget band costs only the tail. Hook context that fires
  during a session renders in place on the user or tool message it belongs to,
  and the cold-cache notice now says whether the harness rewrote the system
  message or the tool list changed
  ([#117](https://github.com/GotinhaAzul/LocalHarness/issues/117)).
- Tool output is sized to the window: the per-result cap is 4% of the usable
  prompt in characters (clamped to 2500..16000) and the per-batch cap 12%
  (8000..40000), overridable with `[budgets] result_cap_chars` and
  `batch_cap_chars`. `read`, `grep`, and `glob` return whole lines or entries
  and report the real range (`lines a-b of n; next start=c`, `N of M matches
  shown`); a blind cut at a line boundary is only the fallback
  ([#118](https://github.com/GotinhaAzul/LocalHarness/issues/118)).
- A token shortfall no longer ends a turn on arithmetic alone. The engine's
  truncated flag and context-shift count still count as proof, but a ratio or
  shortfall now needs a second witness: `prompt_tokens + cached_tokens`
  covering the prompt, `/tokenize` against the window, or a repeat on the next
  call. A count under 512 tokens marks the usage report unreliable: the reply
  is kept, the session warns once, calibration from usage stops, and
  `lh doctor` reports it
  ([#120](https://github.com/GotinhaAzul/LocalHarness/issues/120)).
- `grep` prints `--` separators only between context groups. With no context
  lines it printed one after every non-adjacent match.

### Changed

- Permission levels are named for what they do: `dont-ask` is now `deny-all`
  and `ask` is now `normal`, in the TUI, the docs, and `--mode`. `dont-ask`
  and `ask` are still accepted as deprecated aliases in config files and on
  `--mode`, so existing setups keep working, and `--mode` also takes the new
  names. `yolo` stays reachable only via `--yolo` or `/permissions yolo`.
- **Renamed to Totality.** The binary is `tot` (installers also add a
  `totality` link), the crate is `totality`, and environment variables use
  the `TOT_` prefix. The `LH_` names still work. Config and data move to
  `~/.config/totality` and `~/.local/share/totality`; the old `localharness`
  directories are used until the new ones exist, so sessions and settings
  carry over. Older sessions' harness messages are still recognized. The
  installer still installs pre-0.3 releases (`lh`) when pinned.
- **Default theme:** `default` is now the warm dusk palette (`totality`); the
  previous default is available as `classic`.
- Tool rows label calls by their main argument (`Read src/cli.rs · first 130
  lines`), fold read and recall output until ctrl+o, and sit on consecutive
  lines with the thoughts between them. Markdown headings drop their `#`
  markers and lists use round bullets. The spinner is an eclipse.
- `/resume`, `/sessions`, `--continue`, and `/resume last` skip the prompt
  history file and sessions without a user message; times read as `5 min ago`.
- A model call that fails for a transient reason is retried inside the turn:
  a connection failure, a dropped or stalled stream, a 5xx except 501/505,
  408/504, or 429 (now its own `rate_limited` class). `[model] retries`
  (default 3) bounds the attempts, with jittered 1x/4x/15x backoff of
  `retry_backoff_ms` (default 1000), honoring `Retry-After` within
  `retry_deadline_secs` (default 120). The partial reply is logged as a
  `model-retry` entry, never replayed; retries are notices and do not count as
  model calls. `retries = 0` restores the old behavior
  ([#119](https://github.com/GotinhaAzul/LocalHarness/issues/119)).
- With `[gates] auto_settle` (default true), the harness runs verify and then
  review itself when the model settles after changing files, through the same
  permission rules and hooks, and records `verify-result` plus
  `review-result`. Clean results settle on the answer already written with a
  `[harness] verify passed, review clean` notice; a failed verify or flagged
  review continues with one user-role message carrying the results.
  `auto_settle = false` restores the model-relayed gate messages
  ([#123](https://github.com/GotinhaAzul/LocalHarness/issues/123)).
- A turn stopped by the loop guard now ends with the `stalled` status instead
  of `done`: `lh -p` exits 2 instead of 0, a workflow step is no longer marked
  succeeded, and ACP reports `max_turn_requests`.

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
