# Configuration

Precedence: CLI flags > trusted project config > global config > built-in
defaults. Environment variables fill the gaps.

## Paths

| Purpose | Default |
|---|---|
| Global config | `~/.config/localharness/config.toml` |
| Profiles | `~/.config/localharness/profiles/` |
| Skills | `~/.config/localharness/skills/<name>/SKILL.md` |
| Workflows | `~/.config/localharness/workflows/` |
| External tools | `~/.config/localharness/tools/*.toml` |
| Themes | `~/.config/localharness/themes/*.toml` |
| Data | `~/.local/share/localharness/` |
| Sessions | `~/.local/share/localharness/sessions/<project-id>/` |
| Engagements | `~/.local/share/localharness/engagements/<name>/` |
| Project config | `<cwd>/.harness/` (requires `--trust`) |

`LH_CONFIG_DIR` and `LH_DATA_DIR` override the roots.

## Example

```toml
default_profile = "coding"      # coding | full | security | minimal | strict | shell | custom

[model]
endpoint = "http://127.0.0.1:8080"
# The /v1 suffix is optional. Both forms work, including another machine:
# endpoint = "http://model-host:8088/v1"
model = ""                      # auto-select if /v1/models has one entry
api_key = ""
start_command = ""              # optional; run once if the endpoint is down
context_window = 32768
temperature = 0.2
top_p = 0.95
max_output_tokens = 4096
request_timeout_secs = 900
connect_timeout_secs = 5
first_token_timeout_secs = 120
stream_idle_timeout_secs = 120

[ui]
theme = "default"               # default | catppuccin-mocha | gruvbox-dark | light | high-contrast
color_mode = "auto"             # auto | truecolor | 256 | 16
show_reasoning = false
show_token_counts = true
mouse = true                    # wheel scrolls the conversation; Shift+drag selects text

[session]
dir = ""                        # default data dir's sessions/
no_session = false
redact = true                   # redact secret values before persistence
artifact_retention_days = 30

[budgets]
max_tool_calls = 120
max_model_calls = 40
max_wall_minutes = 30
stall_calls = 12
verify_attempts = 2
wrap_up_at = 0.8

[prompt]
identity = ""                   # replaces the built-in one-line identity
instructions = ""               # extra instructions appended to the prompt
file = ""                       # or a Markdown file, relative to the config dir

[lsp]
gate = "warn"                   # off | warn | block (see below)

# `coding` is the built-in default. `full` adds the security pack; `security`
# and `minimal` narrow it further. `strict` and `shell` are weak-model presets.
[profile.coding]
packs = ["core", "coding"]
model_profile = "auto"

[profile.full]
packs = ["core", "coding", "security"]
model_profile = "auto"

[profile.minimal]
tools = ["read", "bash", "verify"]

[profile.strict]
tools = ["read", "grep", "glob", "edit", "bash", "todo"]
model_profile = "strict"

[profile.shell]
tools = ["bash"]
model_profile = "shell"

[model_profile.default]
context_window = 32768
tool_mode = "repair"
strip_reasoning = true

[model_profile.strict]
tool_mode = "repair"
strip_reasoning = true
parallel_tool_calls = false
tool_choice = "required"
one_call = true
short_tool_schemas = true

[model_profile.shell]
tool_mode = "repair"
strip_reasoning = true
parallel_tool_calls = false
short_tool_schemas = true

[model_profile.qwen3]
match = ["qwen3"]
context_window = 65536
tool_mode = "native"
strip_reasoning = true
reasoning_field = "reasoning_content"
stop = ["<|im_end|>"]
extra_body = { chat_template_kwargs = { enable_thinking = true } }
```

`default_profile` picks the profile used at startup; `--profile` and
`LH_PROFILE` still override it. The built-in default is `coding`.

`model_profile = "auto"` matches by model-name prefix declared in `match`; the
`default` model profile is the fallback. `[model] context_window` is used only
when the model profile does not set one.

## Weak-model profiles

`strict` is the tested small-model configuration: `read`, `grep`, `glob`,
`edit`, `bash`, and `todo`, with flat schemas and short descriptions. Its
model profile sets `parallel_tool_calls = false` and `tool_choice = "required"`
as request hints, and `one_call = true`.

`one_call` is enforced at dispatch, not by prompt text. When a profile sets it,
the first tool call in an assistant message runs and every later call gets a
repairable `tool_call_invalid` result instead of running. After two rejected
calls in one turn the harness appends one compact reminder; after three it ends
the turn with `tool_call_invalid` rather than looping.

`shell` is experimental: only `bash` is exposed, for models that edit files
more reliably with commands than with JSON calls. The output contract is:

- One `bash` call is one command or one `&&` chain. Print what changed.
- Read with `sed -n 'START,ENDp' path` or `cat -n path`; include line ranges
  for a second read instead of dumping a whole large file.
- Edit with `sed -i`, a `printf`/here-document rewrite, or another installed
  tool. Do not rely on an editor tool; none is available in this profile.
- Test with the project's real command (`cargo test`, `pytest`, `npm test`)
  after editing, and report the command plus its exit status. A non-zero exit
  is a failure even when the output looks plausible.

`short_tool_schemas = true` removes property descriptions from the request and
shortens each tool description. It changes only what the model sees; argument
validation still uses the full schema.

## LSP gate

`[lsp] gate` controls what happens when a `write`, `edit`, or `patch` introduces
a new language-server error:

| Value | Behavior |
|---|---|
| `off` | No snapshot or diagnostic check. |
| `warn` (default) | Attach only the new errors to the tool result; keep the change. |
| `block` | Restore the pre-edit file and return `EDIT_REGRESSION` with the diagnostic and a fix hint. |

The gate snapshots the file and its existing diagnostics before the mutation,
then compares only severity-error diagnostics. Errors that existed before the
edit are suppressed, and warnings never block. It only runs when a language
server for that file is already warm and the file belongs to a real project
(`Cargo.toml`, `pyproject.toml`, `package.json`, `go.mod`, or a C/C++ project
marker); cold servers and loose files are never started or blocked. A clean
edit adds no diagnostic text. Warm checks are capped at a 250 ms wait so the
p95 stays under 300 ms; a batch that misses the window is skipped rather than
guessed.

`[prompt] identity` replaces the first line of the system prompt; leave it
empty to keep the built-in one. `instructions` and `file` are appended after
the built-in operating rules and are ordinary text, not code. A relative
`file` resolves against the config directory first, then a trusted `.harness/`
directory, then the path itself; an untrusted repository cannot shadow it.
Per-profile `prompt` files still append after that. None of these can change
the gates, tool selection, scope, or the endpoint; those remain in code.

The TUI does not require `[model]` to be resolved at startup. If the endpoint is
down or `model` is empty, the harness opens with `not logged in`; `/login`
validates `GET /v1/models`, applies the selection, and writes `endpoint`,
`model`, and `api_key` to the global config file (replacing only those keys,
with `0600` permissions). `/model` opens a filterable picker over the endpoint's
models and switches the current session without rewriting the config. Headless `--print` exits with an
error until a model is selected.

`start_command` is optional. When set, it runs through `sh -c` once at startup
if `GET /v1/models` fails. With a model already configured it runs in the
background: the TUI opens immediately, and the first turn waits for the endpoint
instead of racing it. With no model configured the harness waits for the command
before probing so a single exposed model can still be auto-selected. After a
successful command the harness waits up to 30 seconds for the endpoint to
answer; a command that exits non-zero is reported at once without waiting out
the retry window. It is not run when the endpoint is already reachable, and it
is not retried during the session. The command runs as the invoking user.

## Verify steps

Trusted project file `.harness/verify.toml`:

```toml
[[steps]]
name = "test"
command = ["cargo", "test"]
timeout_secs = 600

[[steps]]
name = "lint"
command = ["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]
```

Steps run sequentially without a shell and stop at the first failure.

## Environment

| Variable | Meaning |
|---|---|
| `LH_ENDPOINT` | Endpoint base URL |
| `LH_MODEL` | Model id |
| `LH_API_KEY` | Bearer token |
| `LH_PROFILE` | Tool profile |
| `LH_MODEL_PROFILE` | Force a model profile |
| `LH_THEME` | Theme name |
| `LH_NO_SESSION` | Disable persistence |
| `LH_TRUST` | Trust project config |
| `LH_CONFIG_DIR`, `LH_DATA_DIR` | Override roots |
| `LH_SKIP_TOOL_PROBE` | Skip the one-shot tool-call conformance probe |
| `NO_COLOR` | Disable color |
| `LH_DEBUG_CONTENT` | Allow content in debug logs (warns) |
| `LH_BWRAP` | Override the bubblewrap executable used by `--sandbox` |

## Flags

`--print`, `--json`, `--endpoint`, `--model`, `--model-profile`, `--profile`,
`--theme`, `--api-key`, `--trust`, `--no-session`, `--no-redact`, `--continue`,
`--resume`, `--path-jail`, `--sandbox [read-only|workspace-write]`,
`--no-sandbox`, `--engagement`, `--no-color`, `--keep-artifacts`,
`--clean-artifacts`, `--debug`.

`lh doctor [--json] [--fix]` prints the engine capability manifest without
sending a chat completion. See [model-setup.md](model-setup.md).
