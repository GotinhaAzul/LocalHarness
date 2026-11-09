# LocalHarness

<p align="center">
  <a href="https://github.com/GotinhaAzul/LocalHarness/actions/workflows/ci.yml"><img src="https://github.com/GotinhaAzul/LocalHarness/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue" alt="License: MIT OR Apache-2.0">
  <img src="https://img.shields.io/badge/rust-1.88%2B-orange" alt="Rust 1.88+">
  <img src="https://img.shields.io/badge/platform-linux%20x86__64%20%7C%20aarch64-lightgrey" alt="Linux x86_64 and aarch64">
</p>

**Local-first agent harness for coding and security work.** One static Rust
binary, one OpenAI-compatible endpoint, a polished TUI, and a headless mode.
No Node, no cloud, no telemetry.

LocalHarness owns its agent loop, tools, context management, session tree, and
interface. The product stance is simple: **anything the model is unreliable at
is enforced by code, not requested by prompt.** The harness supplies facts,
measures progress, and stops loops; the model does the work.

```
+ LocalHarness  ~/src/app (main*)  full  qwen3.5-35b-a3b  12.4k/64k [###---] 19%  +
|                                                                                |
|  You  20:14                                                                    |
|  Fix the parser bug                                                            |
|  Assistant                                                                     |
|  The bug is in the tokenizer. I will patch it.                                 |
|  + bash - cargo test --------------------------------------------------------+  |
|  | PASS cargo 42 passed (3.1s)                                               |  |
|  +--------------------------------------------------------------------------+  |
+--------------------------------------------------------------------------------+
| > type a message...                                             Enter send     |
+--------------------------------------------------------------------------------+
```

## Why LocalHarness

- **Local by default.** No telemetry, analytics, or phone-home. The endpoint is
  localhost unless you point it elsewhere.
- **Model-agnostic.** Any engine speaking the OpenAI chat-completions API:
  llama.cpp, Ollama, vLLM, and anything compatible.
- **Enforced, not prompted.** Read-before-edit, scope, budgets, loop detection,
  stall detection, todo planning, and verification are code paths, not requests
  in a system prompt.
- **Code-aware tools.** Language-server diagnostics after edits, a cached repo
  map, test/verify loops, and undo that refuses to clobber your own work.
- **Security engagements.** Evidence capture, attack-surface inventory, Sigma
  detection, findings with deterministic PoC replay, and an attack-path graph,
  scoped to an authorized target.
- **One binary.** `lh` is a single static Rust binary with a TUI and a JSONL
  headless mode. Nothing to install at runtime.

## Build

```sh
cargo build --release
./target/release/lh --help
```

Requires Rust 1.88 or newer, Linux x86_64 or aarch64. Dependencies are pinned in
`Cargo.lock`; release builds use `--locked`.

## Quick start

### Point it at a model

Start an OpenAI-compatible server, then run `lh`. If the endpoint exposes
exactly one model it is selected automatically.

llama.cpp (tool calling must be enabled):

```sh
llama-server -m Qwen3.5-35B-A3B-Q4_K_M.gguf --ctx-size 65536 --jinja --port 8080
lh
```

Ollama:

```sh
ollama serve
ollama pull qwen3
lh --endpoint http://127.0.0.1:11434 --model qwen3
```

vLLM:

```sh
vllm serve Qwen/Qwen3-35B-A3B --enable-auto-tool-choice --tool-call-parser hermes
lh --endpoint http://127.0.0.1:8000 --model Qwen/Qwen3-35B-A3B
```

`lh` also starts with no reachable endpoint or model. The TUI opens with
`not logged in` and turns stay disabled until you sign in:

```text
/login http://127.0.0.1:8080             auto-select the endpoint's only model
/login http://127.0.0.1:8080 qwen3       choose a model
/login http://host:8000 model sk-...     remote endpoint with a bearer token
```

`/login` validates `GET /v1/models` and saves `endpoint`, `model`, and `api_key`
to the global config with `0600` permissions. `/model` opens a picker over the
models the endpoint exposes (every model behind a gateway such as LiteLLM or
OpenRouter); type to filter, Enter to switch. `/model <name>` switches directly.
Headless `--print` still requires a selected model.

Point at a different endpoint with `--endpoint` or `LH_ENDPOINT` (default
`http://127.0.0.1:8080`). Both `http://host:8080` and `http://host:8080/v1` are
accepted, so you can paste the base URL an engine prints. A non-local endpoint
is allowed but warned about; see [docs/model-setup.md](docs/model-setup.md) for a
tailnet example.

Facts are collected from optional endpoints (`/v1/models`, `/props`,
`/tokenize`, `/api/show`). Missing probes only degrade estimates; they never
fail a turn. See [docs/model-setup.md](docs/model-setup.md) for model profiles,
thinking/reasoning handling, and troubleshooting.

The harness does not launch servers on its own. If an endpoint is down and you
want it started automatically, set the optional `[model] start_command`; it runs
once at startup, only when the endpoint is unreachable. See
[docs/configuration.md](docs/configuration.md).

## Headless

```sh
lh --print "Fix the failing parser test"
lh --print "Summarize this repo" --json
lh --continue                  # most recent session
lh --resume 20261107-201500-1234
```

`--print` runs one turn, streams text to stdout, diagnostics to stderr, and exits
with the turn status: `0` done, `1` error/blocked, `2` budget/stalled, `130`
cancelled. `--json` emits newline-delimited events.

## Sessions

One JSONL file per session tree, append-only, under
`~/.local/share/localharness/sessions/<project-id>/`. `/tree` shows branches;
checkout and fork are non-destructive. `/new` starts a fresh session;
`/resume` picks a previous one (`/resume last`, or `/resume <id>` by id prefix
or suffix). `/sessions` lists every session for the project.
`--no-session` writes nothing: no session, no artifacts.

## Keys

| Key | Action |
|---|---|
| `Enter` | send |
| `Ctrl+J` | newline |
| `Esc` | cancel the current stream |
| `Ctrl+C` | cancel; again to quit |
| `Ctrl+D` | quit when the input is empty |
| `Up`/`Down` | history when input is empty (kept per project across restarts), otherwise cursor |
| `PgUp`/`PgDn`, `Home`/`End` | scroll transcript |
| `Ctrl+R` | toggle reasoning |
| `/` | slash-command completion popup |
| `Tab` / `Shift+Tab` | complete commands, arguments, and paths; move back through choices |

Tab completes whatever sits at the cursor: command names (`/the` → `/theme `),
their arguments (themes, profiles, skills, workflows, session ids, models,
`/sec scope add|remove|clear`), and file paths (`@src/ui/in`, `./`, `~/`, or
any token with a `/`). One match is accepted, several first extend to their
shared prefix and then open a popup, and Enter on a partial command or argument
runs the highlighted choice.

### Mentioning files

Write `@path` in a prompt to attach that file: `fix the panic in
@src/parser.rs`. The file is read through the same path as the `read` tool
(path jail, binary and size checks, line numbers) and appended to your message
for the model, so it starts with the code and may `edit` it without another
`read`. Up to eight files per prompt, within a quarter of the usable context;
anything skipped is reported in the footer. A mention that names no file, like
`@someone`, is left as plain text. The transcript and session keep your prompt
as typed, with the attached snapshots stored alongside it.

Slash commands: `/help /quit /login /model /profile /theme /tree /fork /new
/resume /sessions /compact /tools /skill /workflow /sec /trust /export /clear
/debug`.

## Tools

Twenty-four built-ins in three packs. `full` (core + coding + security) is the
default; `coding`, `security`, and `minimal` narrow it.

| Pack | Tools |
|---|---|
| Core | `read` `write` `edit` `patch` `undo` `bash` `grep` `glob` `todo` `skill` `verify` `recall` `thread` `bg` |
| Coding | `repo_map` `lsp` `test` `review` |
| Security | `surface` `evidence` `detect` `attack_path` `finding` `iocs` |

`repo_map` returns a cached, budget-capped outline of files and top-level
symbols; a compact version is included in the system prompt. `write`, `edit`,
and `patch` append language-server diagnostics for the changed file when a
supported server is installed and the file belongs to a real project. A
missing or cold server never fails an edit.

`undo` reverts the last `edit`, `patch`, or `write` to a file, one step per
call, from snapshots kept in the session (no git involved). It refuses when the
file changed after the harness wrote it, so it never discards your own edits.
Changes made through `bash` are not tracked.

Security tools operate inside the active engagement. `bash`, `bg`, and
external tool runs append an evidence entry with the target, command,
timestamps, exit status, output artifact, and SHA-256 digest. `surface` parses
nmap/HTTP/TLS output into one inventory; `detect` validates and runs a Sigma
subset against local logs; `attack_path` queries the graph derived from the
surface, findings, and evidence. `finding add` auto-links the last tool's
evidence entry, or accepts `evidence_id` explicitly.

A finding can carry `poc_steps` — bounded shell or HTTP actions with
assertions. `finding verify` replays them locally, records one evidence entry
per step, and stores a verdict: `reproduced`/`confirmed`,
`not_reproduced`/`rejected`, or `error|skipped`/`inconclusive`. Only a
reproduced run confirms; a failed replay never auto-marks the finding as a
false positive. `minimize:true` reduces a reproducing step graph with ddmin.

Tool results are capped and archived; `recall` reads a range of an artifact.
Guards enforce read-before-edit, scope, loop detection, budgets, stall
detection, todo planning, and verification. Every error names the next action.

## Security

Security work is for authorized labs, CTFs, and engagements only. Scope is set
by the user with `/sec scope add|remove|clear`; there is no model tool for it.
The scope gate blocks a known set of active-testing tools when no scope is set
or when an extracted target is outside it. It is a speed bump, not a sandbox.

- No telemetry, analytics, or phone-home.
- Localhost endpoint by default.
- Project `.harness/` config, skills, workflows, and tools are ignored unless
  `--trust` is passed.
- Session logs and artifacts are plaintext; `--no-session` disables both.
- `--path-jail` restricts file tools to the working directory. It does not
  restrict `bash`; use a container for isolation.

Read [docs/security.md](docs/security.md) before using it on client work.

## Extend it

- **Profiles** select packs/tools, a model profile, and optional prompt text.
- **Skills** are Markdown instructions; the index is always visible. The model
  loads a matching skill itself with the `skill` tool, or you load one with
  `/skill <name>`.
- **Workflows** are playbooks plus a profile and tool allowlist.
- **External tools** are any executable speaking one JSON object in, one JSON
  object out. See [docs/external-tools.md](docs/external-tools.md).
- **Themes** are TOML files of semantic color tokens. See
  [docs/themes.md](docs/themes.md).

Loop behavior itself is not scriptable. Fork the crate.

## Configuration

Global config: `~/.config/localharness/config.toml`. A project `.harness/`
overrides it when trusted. Precedence: CLI flags > trusted project config >
global config > built-in defaults. Environment: `LH_ENDPOINT`, `LH_MODEL`,
`LH_API_KEY`, `LH_PROFILE`, `LH_MODEL_PROFILE`, `LH_THEME`, `LH_NO_SESSION`,
`LH_TRUST`, `LH_CONFIG_DIR`, `LH_DATA_DIR`.

See [docs/configuration.md](docs/configuration.md) for every key.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo audit && cargo deny check   # advisories
```

No test requires a real model or network. [ARCHITECTURE.md](ARCHITECTURE.md) is
the implementation specification.

## License

MIT OR Apache-2.0. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
