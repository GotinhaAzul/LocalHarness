# LocalHarness

Local-first agent harness for coding and security work. One static Rust binary,
one OpenAI-compatible endpoint, a polished TUI and a headless mode. No Node, no
cloud, no telemetry.

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

## Build

```sh
cargo build --release
./target/release/lh --help
```

Requires Rust 1.88 or newer, Linux x86_64 or aarch64. Dependencies are pinned in
`Cargo.lock`; release builds use `--locked`.

## Point it at a local model

Any engine that implements the OpenAI chat-completions API works. Start one,
then run `lh`. If the endpoint exposes exactly one model it is selected
automatically.

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

Facts are collected from optional endpoints (`/v1/models`,
`/props`, `/tokenize`, `/api/show`). Missing probes only degrade estimates; they
never fail a turn. See [docs/model-setup.md](docs/model-setup.md) for model
profiles, thinking/reasoning handling, and troubleshooting.

Point at a different endpoint with `--endpoint` or `LH_ENDPOINT` (default
`http://127.0.0.1:8080`). A non-local endpoint is allowed but warned about.

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
checkout and fork are non-destructive. `/sessions` switches sessions.
`--no-session` writes nothing: no session, no artifacts.

## Keys

| Key | Action |
|---|---|
| `Enter` | send |
| `Ctrl+J` | newline |
| `Esc` | cancel the current stream |
| `Ctrl+C` | cancel; again to quit |
| `Ctrl+D` | quit when the input is empty |
| `Up`/`Down` | history when input is empty, otherwise cursor |
| `PgUp`/`PgDn`, `Home`/`End` | scroll transcript |
| `Ctrl+R` | toggle reasoning |
| `/` | slash-command completion |

Slash commands: `/help /quit /model /profile /theme /tree /fork /sessions
/compact /tools /skill /workflow /sec /trust /export /clear /debug`.

## Tools

Seventeen built-ins in three packs. `full` (core + coding + security) is the
default; `coding`, `security`, and `minimal` narrow it.

| Pack | Tools |
|---|---|
| Core | `read` `write` `edit` `patch` `bash` `grep` `glob` `todo` `verify` `recall` `thread` `bg` |
| Coding | `lsp` `test` `review` |
| Security | `finding` `iocs` |

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
- **Skills** are Markdown instructions; the index is always visible, full text
  loads with `/skill <name>`.
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
the implementation specification. License: MIT OR Apache-2.0.
