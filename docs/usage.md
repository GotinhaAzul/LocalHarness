# Using Totality

The [README](../README.md) is the tour; this is the reference for day-to-day
use. Every key and flag is in [configuration](configuration.md).

- [Connecting a model](#connecting-a-model)
- [The loop is enforced, not requested](#the-loop-is-enforced-not-requested)
- [Tools that know the codebase](#tools-that-know-the-codebase)
- [Sessions are a tree, not a log](#sessions-are-a-tree-not-a-log)
- [One binary, two front ends](#one-binary-two-front-ends)
- [Security, when you ask for it](#security-when-you-ask-for-it)
- [Shape it without forking](#shape-it-without-forking)
- [Configuration at a glance](#configuration-at-a-glance)

## Connecting a model

Start any OpenAI-compatible server, then run `tot`. If the endpoint exposes exactly one model, it is selected automatically.

```sh
# llama.cpp (tool calling requires --jinja)
llama-server -m Qwen3.5-35B-A3B-Q4_K_M.gguf --ctx-size 65536 --jinja --port 8080
tot

# Ollama
ollama serve
ollama pull qwen3
tot --endpoint http://127.0.0.1:11434 --model qwen3

# vLLM
vllm serve Qwen/Qwen3-35B-A3B --enable-auto-tool-choice --tool-call-parser hermes
tot --endpoint http://127.0.0.1:8000 --model Qwen/Qwen3-35B-A3B
```

`tot` also starts with no reachable endpoint or model. The TUI opens with `not logged in` and turns stay disabled until you sign in:

```text
/login http://127.0.0.1:8080             auto-select the endpoint's only model
/login http://127.0.0.1:8080 qwen3       choose a model
/login http://host:8000 model sk-...     remote endpoint with a bearer token
```

`/login` validates `GET /v1/models` and saves `endpoint`, `model`, and `api_key` to the global config with `0600` permissions. `/model` opens a filterable picker over every model the endpoint exposes, including gateways such as LiteLLM or OpenRouter. Headless `--print` requires a selected model.

Point at another endpoint with `--endpoint` or `TOT_ENDPOINT` (default `http://127.0.0.1:8080`). Both `http://host:8080` and `http://host:8080/v1` work, so you can paste the base URL an engine prints. A non-local endpoint is allowed but warned about; see [model-setup.md](model-setup.md) for a tailnet example.

Facts come from optional endpoints (`/v1/models`, `/props`, `/tokenize`, `/api/show`). Missing probes only degrade estimates; they never fail a turn. The harness does not launch servers on its own, but `[model] start_command` can start one once when the endpoint is unreachable.

## The loop is enforced, not requested

A model can forget to read a file, claim a test passed, or repeat the same failing call until the context is gone. Totality makes those states visible and stoppable.

```text
[tool] edit src/parser.rs
[error] read src/parser.rs first

[tool] verify cargo test
PASS  cargo  42 passed (3.1 s)

✓ read-before-edit   ✓ plan gate   ✓ verify gate   ✓ review gate
```

- **Read before edit.** `edit`, `write`, and `patch` require a prior read of the same file. A changed file must be read again.
- **Plan before the third file.** `todo` is free for a run that touches one or two files; repeated edits to the same file are one change. From the third distinct file, an edit needs a plan. A finished turn is never reopened for one.
- **Verify before settle.** After changes, configured steps run; a real failure blocks the turn.
- **Review after verify.** The `review` pass checks the diff for secrets, debug statements, lockfiles, deletion-heavy changes, and source changes whose module has no test.
- **Stop the loop, not the work.** Loop and stall guards end a repeating run with a status, not a silent bill. A guard or budget stop gives the model one tool-less step to summarize what it changed and what is left. Budget checkpoints renew on the same progress the stall tracker counts: a file change or a passing verification. The first no-progress stop in a turn (a checkpoint or the stall guard) asks the model to change approach and gives it one more allowance; only the second ends the turn. Without a thinking control, the summary request gets 6144 output tokens instead of 1536 so a thinking model has room to reason and still answer.
- **Name the next action.** Every error tells the model what to do instead of leaving it to guess.

## Tools that know the codebase

Twenty-six built-ins in three packs. `coding` (core + coding) is the default; `full` adds the security pack. `security` and `minimal` narrow it further; `strict` and `shell` are weak-model presets (see [configuration](configuration.md#weak-model-profiles)). `patch` is the hash-anchored editor and is selected only when the model profile sets `edit_mode = "hash"`; text mode exposes `edit` alone.

| Pack | Tools |
|---|---|
| Core | `read` `write` `edit` `patch` (hash mode) `undo` `bash` `grep` `glob` `todo` `skill` `verify` `recall` `thread` `bg` |
| Coding | `repo_map` `lsp` `review` `git` `pr` |
| Security | `surface` `evidence` `detect` `attack_path` `finding` `iocs` |

`git` keeps the actions with a gate: `stage`, `unstage`, `commit_message`, `commit`, `branch`, `worktree`, and `undo`. `status`, `diff`, `log`, and `blame` are plain reads: call them through `bash` (`git status --short`, `git diff`, `git log --oneline -n 20`, `git blame -L a,b path`). A call to one of them through `git` returns that command in the error.

- **`grep`** searches text, gitignore-aware, and `path=artifact:out-3` searches a stored tool result such as a fetched page.
- **`repo_map`** returns a cached, budget-capped outline of files and top-level symbols (2200 chars by default, 8000 at most). A short index of at most 1200 chars ships in the system prompt; it is not exhaustive, and the model is told to call `repo_map` for more. Files are ranked: README, manifests, entry points (`main`, `lib`, `app`, `cmd/`), source, tests, then other docs and config. Code files without room for an outline are counted per directory, and up to 40% of the budget is held for data and config files so a large source tree cannot push them out. Directories with up to 12 such files are listed by name; larger ones get extension counts and name patterns (`inbox/ (26 files: 20 .txt, 5 .html, 1 .eml; names: CN-77.eml, INV-* ×20, RESENT-* ×5)`). A closing line counts anything the budget still left out. `vendor/`, `dist/`, `build/`, and `target/` are only skipped when `.gitignore` excludes them.
- **`lsp`** adds language-server diagnostics after edits when a supported server is installed and the file belongs to a real project. A missing or cold server never fails an edit.
- **`verify`** runs the configured steps. With `path` or `test` it runs the detected runner for that one file or test instead (vitest, jest, go, cargo, pytest, Python unittest, or an npm test script) and returns only the first failure. A targeted run never counts as a passing project verify, so it does not clear the verify gate.
- **`review`** reads the working tree and checks the diff before the session settles. It is scoped to the session root: a workspace inside a larger repository reports only its own changes, and `git` mutations refuse to touch the enclosing repository.
- **`undo`** reverts the last `edit`, `patch`, or `write` from session snapshots, and refuses when the file changed after the harness wrote it. Before a destructive `bash` command (`rm`, `mv`, `find -delete`, `truncate`, or `>` over a file), the harness snapshots the named files, so `undo` and `/rewind` can bring them back; each such result names the snapshot. `git clean` is not parsed.
- **Hunk review.** `review = "multi_file"` (the default) pauses before a turn's second file mutation, shows the proposed hunks in the TUI, and writes only what you accept. Rejected hunks are reported back to the model with their ranges, accepted hunks land atomically, and `undo` still steps back over them. `always` reviews every mutation; `off` restores immediate writes. Headless `--print` has no reviewer, so it auto-accepts and never blocks.
- **`read`/`grep`/`glob`** respect ignore files and keep results small; oversized output is archived and available through `recall`.
- **Hash edits (opt-in)** — `edit_mode = "hash"` makes `read` print content hashes and adds `patch`, which applies `PUT`/`CUT`/`REM`/`MV` line anchors from `path#snapshot`; stale anchors write nothing and return fresh ones. `edit` remains the exact-text editor, and `occurrence` replaces one named match. See [configuration](configuration.md#hash-anchored-edits).

Tool results are capped, archived, and retrievable. The model never has to guess where an excerpt came from.

## Sessions are a tree, not a log

One append-only JSONL file per session tree under the data directory,
`sessions/<project-id>/` ([configuration](configuration.md#paths)). Branches,
checkouts, and forks are non-destructive.

```text
/tree       show branches and switch
/rewind     restore files and the conversation to an earlier turn
/fork       branch from the current leaf
/new        start a fresh session
/resume     pick a previous session by prefix or suffix
/sessions   list every session for this project
/export     write a Markdown transcript
```

`/rewind [turns]` restores every file the harness wrote after the target to its
snapshot there, then moves the leaf; the old branch stays in `/tree`. An edit
made outside the harness refuses the rewind and is named; `/rewind 1 force`
backs such files up before restoring them. `tot --rewind <session>:<entry>`
does the same headlessly.

```sh
tot --rewind last:e42          # latest session, before entry e42
tot --rewind 20261107-…:e9 --force
```

Compaction is deterministic, not model-written. Tool output is archived with an id, and `recall` reads an exact range. The request prefix is cache-stable: tool order and schema key order do not drift, volatile session facts sit at the end of the system block, and a compaction logs how much of the prefix the engine can keep. `--no-session` writes nothing: no session file, no artifacts. Session files, artifacts, evidence, and reports are redacted on disk by default; `--no-redact` is the explicit opt-out and is recorded in the session header. Redaction applies only to what is written to disk: in a live session the model sees the real tool output, so it has no placeholder to copy into a file. A resumed session replays the saved, redacted form with a note, and `write`/`edit`/`patch`/`undo` refuse content that adds a `[REDACTED:...]` placeholder the file does not already hold.

`AGENTS.md` is read hierarchically: the chain from the git repository root to the working directory, nearest last, each with a source-path header. Files are capped at 12 KiB each and 32 KiB total, with deterministic truncation and a warning; a symlink that resolves outside the repository is refused. `tot context --show` prints the manifest (path, bytes, SHA-256, loaded/skipped/truncated), and `tot context --json` emits the same data for scripts. `--no-agents-md` or `TOT_AGENTS_MD=0` skips the chain. The files are advisory: they cannot add a tool, relax a gate, or change the profile.

## One binary, two front ends

### The TUI

The terminal interface is the default. It speaks in two voices: the model in Skoll's amber, the harness in Hati's silver-blue (the `sun` and `moon` theme tokens). Tool calls render as compact rows grouped with the thoughts between them, diffs and streamed Markdown are styled, and the context gauge stays visible. When something needs you (a permission question, a hunk review, a commit or PR to confirm, an injection warning, a finished plan, a missing model), the bottom of the screen says so in one place: a reversed `Needs you: Approve command  $ cargo test  (y allow · n deny)` bar above the input, a `WAITING FOR YOU` badge in the footer next to the keys that answer it, the terminal title, and one bell (`[ui] bell = false` silences it). Otherwise the footer shows `working` during a turn and `✓ your turn` after it. When the engine reports a prefix-cache hit, the header shows `cache 94%`, in the warning color under 70%; `--json` usage events carry the raw counts.

| Key | Action |
|---|---|
| `Enter` | send |
| `Ctrl+J` | newline |
| `Esc` | cancel the current stream |
| `Ctrl+C` | cancel; again to quit |
| `Ctrl+D` | quit when the input is empty |
| `Up`/`Down` | history when input is empty; otherwise cursor |
| `PgUp`/`PgDn`, `Home`/`End` | scroll transcript |
| `Ctrl+R` | toggle reasoning (`[ui] show_reasoning` is saved) |
| `Ctrl+L` | redraw the whole screen |
| `/` | slash-command completion |
| `Tab` / `Shift+Tab` | complete commands, arguments, and paths; move back through choices |
| Review: `a` / `r` / `e` | accept, reject, or edit the selected hunk (`A`/`R` all, `Enter` apply, `Esc` reject all) |

Tab completes the token at the cursor: commands, arguments (themes, profiles, skills, workflows, session ids, models, `/login` endpoints, `/help` topics, `/sec` subcommands), and file paths. Ambiguous matches first extend to their shared prefix, then open a picker. Every command that chooses from a list — `/model`, `/profile`, `/theme` (with live preview), `/permissions`, `/skill`, `/workflow`, `/sec`, `/resume` — opens a filterable menu when run without an argument: type to filter, arrows to move, Enter to pick. Model and profile switches load in the background, so the TUI stays responsive while the server swaps models. `/theme <name>` and a confirmed theme picker selection apply the theme and write `[ui] theme` to the global config at once; moving through the preview writes nothing. `/theme save` re-writes the current theme and reports the path, and `Ctrl+R` saves `[ui] show_reasoning` the same way.

Write `@path` in a prompt to attach a file: `fix the panic in @src/parser.rs`. Typing `@` opens a live picker over the project's files (gitignored files excluded): it matches names fuzzily, shows each file's folder, and ranks paths under a typed directory first; Tab or Enter inserts the path. The file is read through the same path as the `read` tool and appended to your message, so the model can edit it without another read. Up to eight files per prompt, within a quarter of the usable context. You can also just name a file: `the panic at src/parser.rs:88` or a pasted stack trace attaches up to three existing text files in the project the same way, with `:line` suffixes and trace formats understood; a word that only looks like a path is ignored. `[prompt] attach_named_files = false` turns that off. `@shot.png` attaches a screenshot: a model with vision input sees the image, and a text-only model can get a description from a local vision model; the `read` tool opens images too. Images are checked by content, stripped of EXIF, and sent only to local endpoints unless you opt in. See [images.md](images.md).

Slash commands: `/help /quit /login /model /profile /theme /tree /rewind /fork /new /resume /sessions /compact /tools /mcp /skill /workflow /sec /trust /commit /pr /plan /permissions /export /clear /debug`.

Every tool call passes a permission policy first. The default level `normal` runs read-only calls and asks before edits and commands, showing the exact command or path and a diff; `y` approves exactly that call for the session. `/permissions` opens a picker with one line per level — `normal`, `accept-edits` (edits run, commands ask), `deny-all` (anything that would ask is denied), and `yolo` (no approvals; deny rules still apply) — and `/permissions <level>` sets one directly. The footer spells out a non-default level (`deny-all: anything that would ask is denied`), `yolo` needs a one-key confirmation and is never written to the config, and `/plan` keeps its own command: mutating tools are removed and refused at dispatch until `/plan approve`. Modes, rules, and `--yolo` are in [configuration.md](configuration.md#permissions).

### Headless

```sh
tot --mode accept-edits --print "Fix the failing parser test"
tot --mode deny-all --print "Summarize this repo"   # never prompts; would-ask calls are denied
tot --print "Summarize this repo" --json
tot --plan --print "Plan the parser refactor"   # read-only; prints the plan
tot --continue                  # most recent session
tot --resume 20261107-201500-1234
```

`--print` streams text to stdout, diagnostics to stderr, and exits with the turn status: `0` done, `1` error/blocked, `2` budget/stalled, `130` cancelled. `--json` emits newline-delimited events for scripts and CI. A headless run cannot answer a permission prompt, so calls that would ask are denied: pass `--mode accept-edits` or `--mode deny-all`, add allow rules, or use `--yolo` (explicit, recorded in the session).

### Local HTTP API

`tot serve` exposes sessions, turns, a Server-Sent Events stream, and cancel on loopback, for cron jobs, CI, dashboards, and scripts:

```sh
tot serve                       # prints {"listening":"http://127.0.0.1:PORT","token_file":"…"}
curl -H "Authorization: Bearer $(cat <token_file>)" -X POST http://127.0.0.1:PORT/v1/sessions -d '{}'
```

The bearer token is written to a `0600` file. Only its path is printed. SSE event names are the `--json` event names. A session runs one turn at a time, and a second turn gets a typed `409`. A non-loopback bind needs `--allow-non-loopback`. HTTP sessions cannot answer a permission prompt, so commands and edits are denied unless the session is created with `"permission_mode": "accept-edits"`, or with `"yolo"` on a server started with `--allow-yolo`. Sessions stay resumable after the server stops. See [serve.md](serve.md).

### Editors

`tot acp` speaks the Agent Client Protocol v1 over stdio, so Zed (and other ACP editors) can host a Totality thread:

```json
{
  "agent_servers": {
    "totality": {
      "type": "custom",
      "command": "tot",
      "args": ["--endpoint", "http://127.0.0.1:8080", "--model", "qwen3", "acp"],
      "env": {}
    }
  }
}
```

The session root is the `cwd` the editor declares, and the path jail is always on for file tools. Every tool that can change files or run processes asks the editor first; a denial leaves no side effect, and an approval never widens a harness gate. `bash` and `bg` are processes, so their boundary is the permission prompt plus `--sandbox`. Terminal access is not advertised and a terminal request gets a typed refusal. See [acp.md](acp.md).

## Security, when you ask for it

Security is opt-in. The default `coding` profile has no security tools or posture. `--profile security` adds core plus security; `--profile full` adds coding plus security. `/profile` switches during a session and saves the pick as `default_profile` for the next run; `--profile` and `TOT_PROFILE` still win this run, whenever they set it.

Security work is for authorized labs, CTFs, and engagements only. Scope is set by the user with `/sec scope add|remove|clear`; there is no model tool for it. The scope gate blocks a known set of active-testing tools when no scope is set or when an extracted target is outside it. It is a speed bump; `--sandbox` is the process boundary.

Repository files and tool output are tagged with their source. A cheap detector warns when untrusted content contains instruction-shaped phrases, and the next sensitive action needs `/approve`. Untrusted content cannot enable a tool, change scope, change the endpoint, or change permissions; that is enforced at dispatch, not in the prompt.

- **Evidence.** `bash`, `bg`, and external tool runs append an evidence entry with target, command, timestamps, exit status, output artifact, and SHA-256. Secret values are redacted before session, artifact, evidence, finding, and report writes (never in what the model reads); `--no-redact` is the explicit, recorded opt-out.
- **Surface.** `surface` parses nmap/HTTP/TLS output into one inventory; `attack_path` queries the graph derived from surface, findings, and evidence.
- **Findings.** `finding add` auto-links the last evidence entry. A finding can carry bounded `poc_steps`; `finding verify` replays them locally and stores `reproduced`/`confirmed`, `not_reproduced`/`rejected`, or `error|skipped`/`inconclusive`.
- **Detection.** `detect` validates and runs a Sigma subset against local JSONL logs or plain-text samples.
- **Boundaries.** `--sandbox` runs `bash` and `bg` in an OS sandbox with bubblewrap or Landlock. `--path-jail` restricts file tools to the working directory. `--no-sandbox` is the explicit opt-out. `[permissions]` rules allow, ask, or deny calls by tool, path, command, or network before they run.

Read [security.md](security.md) before using it on client work.

## Shape it without forking

Everything at the edge is configurable; the loop itself stays small and auditable.

- **Profiles** select packs and tools. Built-ins are `coding` (default), `full`, `security`, `minimal`, `strict` (six tools, one call per message for weak models), and experimental `shell` (bash only); add your own in `profiles/*.toml` or `.harness/profiles/`.
- **Prompt** customization lives in `[prompt]`: replace the one-line identity, append inline instructions, point at a Markdown file, or disable the hierarchical `AGENTS.md` chain with `agents_md = false`. Per-profile prompts still apply on top.
- **Skills** are Markdown instructions in the Agent Skills format (`SKILL.md`). The model loads a matching skill with the `skill` tool, or you load one with `/skill <name>`. Bundled security skills advertise only when the security pack is active and remain loadable by name; eight bundled coding skills advertise with the coding pack. `.claude/skills`, `.agents/skills`, and `~/.claude/skills` load alongside the native directories, with native precedence. Install and pin a library with `tot skill add|update|remove|list`, validate it with `tot skill lint`, and test its triggers with `tot skill eval --fixtures`.
- **Workflows** are playbooks plus a profile and tool allowlist. Optional `[[step]]` tables add conditions, bounded retries, approval gates, declared artifacts, and resume; `tot workflow run <name>` executes them. The bundled set covers coding, review, general assistance, and security.
- **External tools** are any executable speaking one JSON object in, one JSON object out. No plugin ABI, no second runtime.
- **Lifecycle hooks** run your command at `session_start`, `user_prompt`, `pre_tool`, `post_tool`, `pre_compact`, and `stop`. They can add context or block; a missing hook is visible and `pre_tool` fails closed. `tot hooks test <name>` exercises one.
- **MCP servers** plug in over stdio or Streamable HTTP with one `[mcp.<name>]` table. Their tools join the profile as `<server>__<tool>`; `tot mcp list` and `/mcp` show what connected.
- **Themes** are TOML files of semantic color tokens. `default` (dusk) is bundled with `hati`, `skoll`, `classic`, and four more; `omarchy` is an alias for existing configs. `/theme <name>` applies one and saves it as `[ui] theme`.

```toml
default_profile = "coding"

[ui]
theme = "default"

[prompt]
identity = ""                 # replace the built-in identity
instructions = ""             # append your own rules
file = ""                     # or point at a Markdown file
```

Loop behavior is not scriptable. Fork the crate.

## Configuration at a glance

Global config: `~/.config/totality/config.toml`. A trusted project `.harness/` overrides it. Precedence: CLI flags > trusted project config > global config > built-in defaults.

| Environment | Meaning |
|---|---|
| `TOT_ENDPOINT`, `TOT_MODEL` | Endpoint base URL and model id |
| `TOT_API_KEY` | Bearer token |
| `TOT_PROFILE`, `TOT_MODEL_PROFILE` | Tool profile and model profile |
| `TOT_THEME` | Theme name |
| `TOT_NO_SESSION`, `TOT_TRUST` | Disable persistence; trust project config |
| `TOT_AGENTS_MD` | Set to `0` to skip the AGENTS.md chain |
| `TOT_CONFIG_DIR`, `TOT_DATA_DIR` | Override config and data roots |
| `TOT_SKIP_TOOL_PROBE` | Skip the one-shot tool-call conformance probe |

`model_profile = "auto"` matches by model-name prefix; the `default` model profile is the fallback. `[model] context_window` is used only when the model profile does not set one. See [configuration.md](configuration.md) for every key and [model-setup.md](model-setup.md) for engine-specific tuning.
