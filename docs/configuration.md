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
| MCP servers | `[mcp.<name>]` tables in `config.toml` ([docs/mcp.md](mcp.md)) |
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
# Checkpoints, not hard caps. When one is reached and the run produced a new
# result, a file change, or a passing verify within the last `stall_calls`
# calls, the allowance renews and the turn keeps going. A checkpoint with no
# recent progress, the loop guard, and the stall guard stop the turn.
max_tool_calls = 120
max_model_calls = 40
max_wall_minutes = 30
stall_calls = 12
verify_attempts = 2

[prompt]
identity = ""                   # replaces the built-in one-line identity
instructions = ""               # extra instructions appended to the prompt
file = ""                       # or a Markdown file, relative to the config dir
agents_md = true                # load the hierarchical AGENTS.md chain

[lsp]
gate = "warn"                   # off | warn | block (see below)

[git]
commit = "ask"                  # ask: /commit confirm approves; allow: commit at once
pr = "ask"                      # ask: /pr confirm approves drafts and comments
subject_max = 72                # longest accepted commit subject

[permissions]
mode = "ask"                    # ask | accept-edits | plan | dont-ask (yolo: --yolo only)

[[permissions.rules]]           # ordered; see "Permissions" below
id = "no-push"
tool = "bash"
command = '\bgit\s+push\b'
action = "deny"

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

`vision = true` in a model profile sends images to that model as content parts;
`[vision]` configures the size cap, EXIF handling, remote endpoints, and an
optional model that describes images for text-only models. See
[images.md](images.md).

```toml
[vision]
model = ""            # describer for text-only models; empty disables
endpoint = ""         # empty uses [model] endpoint
allow_remote = false  # send images beyond loopback/private networks
max_bytes = 5242880
keep_exif = false
```

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

## Git approval

The `git` tool's `commit` and the `pr` tool's `draft` and `comment` change
history or publish. With the default `ask`, the tool records a preview in the
session and stops; the user reviews it in the TUI and approves with
`/commit confirm` or `/pr confirm` (`/commit edit <message>` replaces the
message; `\n` starts a new line; `cancel` drops either). A commit is made only
while the staged diff still matches the preview. `allow` runs the action at
once and suits unattended runs; headless `--print` has no way to approve, so
`ask` leaves the preview unapplied there.

## Permissions

Every tool call is checked against `[permissions]` before it runs. Policy is
read from the global config and, only when the project is trusted, from
`.harness/config.toml`; rules from both apply, global first. Skills, workflows,
and the model cannot change it.

**Modes** decide calls that no rule matches. Read-only calls (read-only tools,
allowlisted shell commands such as `ls`, `rg`, `git status`/`log`/`diff` with
no redirection or substitution, and read-only `git` tool actions) and harness
bookkeeping (`todo`, `thread`, `finding`) always run.

| Mode | File edits (`write`, `edit`, `patch`, `undo`) | Commands, network, MCP/external tools |
|---|---|---|
| `ask` (default) | ask | ask |
| `accept-edits` | run | ask |
| `plan` | refused (`plan_mode`) | refused unless read-only |
| `dont-ask` | denied | denied |
| `yolo` | run | run |

`yolo` is never the default and cannot be set in a config file; only `--yolo`
selects it, and the session records it. Deny rules still apply in every mode.

**Rules** are an ordered list. Every field a rule sets must match:

| Field | Matches |
|---|---|
| `tool` | tool name glob: `bash`, `write`, `github__*`, `*` |
| `path` | path glob relative to the working directory: `src/**`, `**/.env` |
| `command` | regex searched in the exact command (`bash`, `bg`); `git <action>` for the `git` tool; `gh pr <action>` for `pr` |
| `network` | `true` for calls classified as network access (`curl`, `ssh`, `git push`/`fetch`, package installs, `pr`) |
| `action` | `allow`, `ask`, or `deny` |
| `id` | name reported in a denial; `rule-<n>` when absent |

The last matching rule wins, with one exception: a `deny` holds against any
later `allow` or `ask` that is not strictly more specific (sets more of tool,
path, command, network). So a later `tool = "bash", command = "^cargo test"`
allow beats an earlier `tool = "bash"` ask, but a later broad `tool = "bash"`
allow does not undo a `git push` deny. The network classifier is a heuristic;
`--sandbox` is the hard network boundary.

A denied call returns `[DENIED]` with the rule id to the model and the turn
continues. In the TUI an `ask` shows the exact command or path, with a diff for
file edits; `y` allows, `n` or `Esc` denies. An approval covers exactly that
command or path for the rest of the session: approving `git push origin main`
does not approve `git push origin main --force`. Headless runs cannot ask, so
an `ask` is denied there; use `--mode accept-edits`, allow rules, or `--yolo`.
ACP forwards the question to the editor.

The session header records the mode and a 16-digit hash of the rules
(`permissions.mode`, `permissions.rules_hash`). Rules are reloaded when the
config is (for example on `/trust confirm`); the change is recorded.

### Plan mode

`/plan` (or `--plan` at launch) enters plan mode: the model may investigate but
not change anything. Tools that can only mutate are removed from the tool set,
`bash` and `git` stay but only their read-only forms run, and `todo` is added.
Any mutating call, including one a workflow or skill asks for, is refused at
dispatch with `plan_mode` and the approve command. A plan-mode turn cannot end
without a `todo` plan; the plan is stored as a `plan` session entry and shown
by `/plan`. `/plan approve` restores the previous mode and exactly the
previous tool set and records the approval; `/plan exit` does the same and
records the plan as discarded. `lh --plan --print "..."` runs one plan-mode
turn, prints the plan, and exits 0.

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

`[prompt] agents_md` (default `true`) loads the `AGENTS.md` chain from the git
repository root to the working directory, nearest last, each under a
source-path header. Each file is capped at 12 KiB and the chain at 32 KiB;
truncation is deterministic and reported as a warning. A symlink that resolves
outside the repository is refused. `lh context --show` prints the manifest
(path, bytes, SHA-256, loaded/skipped/truncated) and `lh context --json` emits
it as JSON. `--no-agents-md` or `LH_AGENTS_MD=0` disables loading for one run.
The files remain untrusted, advisory instructions.

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

## MCP servers

```toml
[mcp.time]
command = "uvx"                 # stdio server
args = ["mcp-server-time"]
profiles = ["coding"]           # unset = every profile

[mcp.docs]
url = "https://mcp.example.com/mcp"   # Streamable HTTP server
[mcp.docs.headers]
Authorization = "Bearer ${DOCS_MCP_TOKEN}"
```

Each remote tool is exposed as `<server>__<tool>`. Profiles can also name
`mcp:<server>` or a single `<server>__<tool>` in `tools`. Keys, lifecycle, and
result handling are in [docs/mcp.md](mcp.md).

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
| `LH_AGENTS_MD` | Set to `0` to skip the AGENTS.md chain |
| `LH_CONFIG_DIR`, `LH_DATA_DIR` | Override roots |
| `LH_SKIP_TOOL_PROBE` | Skip the one-shot tool-call conformance probe |
| `NO_COLOR` | Disable color |
| `LH_DEBUG_CONTENT` | Allow content in debug logs (warns) |
| `LH_BWRAP` | Override the bubblewrap executable used by `--sandbox` |

## Flags

`--print`, `--json`, `--endpoint`, `--model`, `--model-profile`, `--profile`,
`--theme`, `--api-key`, `--trust`, `--no-session`, `--no-redact`,
`--no-agents-md`, `--continue`, `--resume`, `--path-jail`,
`--sandbox [read-only|workspace-write]`, `--no-sandbox`, `--engagement`,
`--no-color`, `--keep-artifacts`, `--clean-artifacts`, `--allow-remote-images`,
`--keep-exif`, `--debug`, `--mode <ask|accept-edits|plan|dont-ask>`, `--yolo`,
`--plan`.

`lh doctor [--json] [--fix]` prints the engine capability manifest without
sending a chat completion. See [model-setup.md](model-setup.md).

`lh context --show` prints the AGENTS.md chain and its manifest without
opening a session or contacting a model; `--json` emits one stable object.
With no AGENTS.md files in the chain, it reports an empty chain. With
`--no-agents-md` or `LH_AGENTS_MD=0`, it reports that loading is disabled.
