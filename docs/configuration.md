# Configuration

Precedence: CLI flags > trusted project config > global config > built-in
defaults. Environment variables fill the gaps.

## Paths

Defaults use the XDG bases: `$XDG_CONFIG_HOME` (default `~/.config`) and
`$XDG_DATA_HOME` (default `~/.local/share`). Each holds a `totality/`
directory, or an existing pre-0.3 `localharness/` one — see
[Data and config directories](#data-and-config-directories).

| Purpose | Default |
|---|---|
| Global config | `<config dir>/config.toml` |
| Profiles | `<config dir>/profiles/` |
| Skills | `<config dir>/skills/<name>/SKILL.md` |
| Skills lockfile | `<config dir>/skills.lock` |
| Skills trash | `<config dir>/skills-trash/` |
| Workflows | `<config dir>/workflows/` |
| External tools | `<config dir>/tools/*.toml` |
| MCP servers | `[mcp.<name>]` tables in `config.toml` ([docs/mcp.md](mcp.md)) |
| Themes | `<config dir>/themes/*.toml` |
| Data | `<data dir>/` |
| Sessions | `<data dir>/sessions/<project-id>/` |
| Engagements | `<data dir>/engagements/<name>/` |
| Project config | `<cwd>/.harness/` (requires `--trust`) |

The full paths, with no override and no legacy directory present, are
`~/.config/totality/config.toml` and `~/.local/share/totality/`.

### Data and config directories

`TOT_CONFIG_DIR` and `TOT_DATA_DIR` set the roots outright; when unset, each
root is resolved on its own in this order:

1. `<XDG base>/totality`, unless it does not exist and the legacy directory
   does.
2. `<XDG base>/localharness`, when it exists as a directory and `totality`
   does not.

So a fresh install uses `~/.config/totality` and `~/.local/share/totality`,
while an install upgraded from a pre-0.3 release keeps reading and writing
`~/.config/localharness` and `~/.local/share/localharness`, with its config
and sessions intact. If both exist, `totality` wins; if neither exists, it is
created.

To migrate, stop every `tot` process, then either move the directories or point
the variables at the old paths:

```sh
# Stop `tot` first, then rename while the new name does not exist yet:
mv ~/.local/share/localharness ~/.local/share/totality
mv ~/.config/localharness ~/.config/totality

# Or leave them in place and pin the roots:
export TOT_DATA_DIR=~/.local/share/localharness
export TOT_CONFIG_DIR=~/.config/localharness
```

If a `totality` directory already exists, merge the two by hand; `mv` would put
the old directory inside the existing one.

The pre-0.3 `LH_DATA_DIR` and `LH_CONFIG_DIR` names are still read when the
`TOT_` ones are unset. Scripts that need a fixed path should not assume either
directory name: `tot serve` prints its token file's path on the startup line,
and `TOT_DATA_DIR` pins the root outright.

## Example

```toml
default_profile = "coding"      # coding | full | security | minimal | strict | shell | custom
review = "multi_file"           # always | multi_file | off (see below)

[model]
endpoint = "http://127.0.0.1:8080"
# The /v1 suffix is optional. Both forms work, including another machine:
# endpoint = "http://model-host:8088/v1"
model = ""                      # auto-select if /v1/models has one entry
api_key = ""
start_command = ""              # optional; run once if the endpoint is down
context_window = 32768
engine = "auto"                 # auto | llama.cpp | vllm | sglang | ollama | lmstudio | koboldcpp | tabbyapi | mlx | generic
temperature = 0.2
top_p = 0.95
# Optional global sampler defaults; a model profile overrides each field.
# min_p = 0.05
# repeat_penalty = 1.05
# dry_multiplier = 0.8
# dry_base = 1.75
# xtc_probability = 0.0
# seed = 0
# response_schema = ""          # path to a JSON Schema for the final answer
max_output_tokens = 4096
request_timeout_secs = 900
connect_timeout_secs = 5
first_token_timeout_secs = 120
stream_idle_timeout_secs = 120

[ui]
theme = "default"               # default | catppuccin-mocha | gruvbox-dark | light | high-contrast
color_mode = "auto"             # auto | truecolor | 256 | 16
show_reasoning = false          # Ctrl+R toggles and rewrites this key
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
# calls, the allowance renews and the turn keeps going. The first no-progress
# stop in a turn (a checkpoint or the stall guard) asks the model to switch
# approach and grants one more allowance; the second, or the loop guard, ends
# the turn.
max_tool_calls = 120
max_model_calls = 40
max_wall_minutes = 30
stall_calls = 12
# Repeated or failing calls of one tool family allowed in a turn before it
# stops as stalled. A family is `bash`'s program and subcommand, or another
# tool's name plus its key arguments (a body argument such as `content` does
# not create a new family). Temporary-path and number normalization apply to
# bash families only: `tesseract /tmp/a/r.png --psm 6` and
# `tesseract /tmp/b/r.png --psm 3` are one family, while two `read` calls for
# different paths are two. Interleaving, an exit-0 call of the family, and a
# success that changes nothing do not reset the count; a workspace change or a
# passing verify does. Read-only calls are not counted as repeats (and a
# failing read-only call still counts as a failure); a read repeated with the
# same output is caught by `stall_calls` instead. 0 disables the guard.
max_same_tool_family = 8
# Successful calls of one family that changed no workspace file and passed no
# verify, allowed in a turn. A same-family success does not reset it, so a
# model that reruns OCR or conversion probes forever is stopped even when every
# probe exits 0. Inspection is exempt: read-only tools (read, grep, glob,
# recall, repo_map, lsp, evidence, fetch, skill, review, iocs, surface,
# attack_path); the reading actions of mixed tools (bg logs/status, todo list,
# thread status, finding list, detect validate, pr checks/review,
# git status/diff/log/blame/commit_message, git branch|worktree op = "list");
# and bash commands that only read (cat, ls, rg, git show, git ls-files,
# curl/wget without a writing flag, gh pr view|list|diff|checks,
# gh issue view|list, and a gh api GET).
# A long exploratory analysis turn with many distinct `python3 -c` probes and
# no file writes can reach this limit, because each probe exits 0 and changed
# nothing. Raise `max_same_tool_family_success` for that kind of work, or set it
# to 0 to disable the guard. Writes outside the session directory, including
# temporary paths, do not count as progress.
max_same_tool_family_success = 24
verify_attempts = 2

[prompt]
identity = ""                   # replaces the built-in one-line identity
instructions = ""               # extra instructions appended to the prompt
file = ""                       # or a Markdown file, relative to the config dir
agents_md = true                # load the hierarchical AGENTS.md chain
attach_named_files = true       # attach files a prompt names without @ (up to 3)

[lsp]
gate = "warn"                   # off | warn | block (see below)

[git]
commit = "ask"                  # ask: /commit confirm approves; allow: commit at once
pr = "ask"                      # ask: /pr confirm approves drafts and comments
subject_max = 72                # longest accepted commit subject

[permissions]
mode = "normal"                 # normal | accept-edits | plan | deny-all (yolo: --yolo only)

[[permissions.rules]]           # ordered; see "Permissions" below
id = "no-push"
tool = "bash"
command = '\bgit\s+push\b'
action = "deny"

[hooks]
timeout_secs = 5                # default deadline for one hook
max_output_bytes = 16384        # recorded output cap; larger output is archived

# One table per hook. `event` is required; see docs/hooks.md.
[hooks.guard]
event = "pre_tool"
command = ["/home/me/bin/policy-check"]   # direct exec, no shell
on_error = "block"                        # pre_tool default; warn elsewhere

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

[model_profile.hash]
edit_mode = "hash"            # text (default) | hash; see below
tool_mode = "repair"
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
`TOT_PROFILE` still override it. The built-in default is `coding`. A successful
`/profile` switch writes this key.

`/theme <name>`, `Ctrl+R`, and a successful `/profile` switch update the global
config in place: the write is atomic, every unrelated key is preserved, an
existing file keeps its mode, and a new file is created `0600`. A failed write
leaves the change active for the session and shows a warning. These commands
never write permissions; `/permissions` remains session-only.

`model_profile = "auto"` matches by model-name prefix declared in `match`; the
`default` model profile is the fallback. `[model] context_window` is used only
when the model profile does not set one.

`vision = true` in a model profile sends images to that model as content parts;
`[vision]` configures the size cap, EXIF handling, remote endpoints, and an
optional model that describes images for text-only models. See
[images.md](images.md).

### Sampler profiles

Sampling is per model. Every field below is optional and overrides the same
global `[model]` value when set:

```toml
[model_profile.qwen-coding]
match = ["qwen3-coder", "qwen3.5-coder", "qwen2.5-coder", "qwen3"]
temperature = 0.2
top_p = 0.95
min_p = 0.05
repeat_penalty = 1.05
dry_multiplier = 0.8
dry_base = 1.75
xtc_probability = 0.0
seed = 0
```

Bundled presets are `qwen-coding`, `gemma-chat`, and `glm-agent`. With
`model_profile = "auto"` (the default), selecting a matching model from
`/model` applies its preset; `/model <name> <preset>` pins one explicitly, and
`--model-profile qwen-coding` or a profile's `model_profile = "qwen-coding"`
does the same for the next start. A user-defined profile always beats a
bundled preset for the same model.

The harness sends only fields the detected engine accepts, under the name that
engine uses: `repeat_penalty` for llama.cpp/Ollama/LM Studio/KoboldCpp and
`repetition_penalty` for vLLM/SGLang/TabbyAPI. DRY and XTC go only to llama.cpp
and KoboldCpp; `min_p` is omitted for Ollama and the generic profile. `tot doctor`
warns about every omitted field and names it. Pin `[model] engine` when a
gateway hides the real server, otherwise detection is best-effort.
`extra_body` still merges last, so an engine-only sampler can be passed there.

The streaming repetition guard remains the last line of defense, not the first.

`--debug` records the exact request body in the diagnostic log
(`totality::provider::request`), with the configured bearer token redacted.

### Structured output

`response_schema` (or `--json-schema FILE`) points at a JSON Schema that
constrains the final answer:

```toml
[model]
response_schema = "schemas/reply.json"
```

The schemas must stay inside a small, lowerable subset. `oneOf`, `anyOf`,
`allOf`, `patternProperties`, remote `$ref`s, recursive `$ref`s, `not`,
conditionals, and unknown keywords are rejected before any request is sent, and
the error names the keyword. Local `$ref`s under `$defs`/`definitions` are
allowed when they are not recursive. Supported keywords: `type`, `properties`,
`required`, `additionalProperties`, `items`, `enum`, `const`, `minItems`,
`maxItems`, `minLength`, `maxLength`, `minimum`, `maximum`,
`exclusiveMinimum`, `exclusiveMaximum`, `multipleOf`, and `format` (annotation
only).

The same schema is lowered per engine:

| Engine | Wire shape |
|---|---|
| llama.cpp | GBNF `grammar` |
| vLLM | `structured_outputs: { json: <schema> }` |
| SGLang | `response_format` with `json_schema` |
| Ollama / LM Studio | `response_format` with `json_schema` |
| KoboldCpp | `/api/extra/json_to_grammar`, then `grammar` |
| MLX / TabbyAPI / unknown | no grammar backend: repair fallback |

A strict response format and `tools[]` cannot ride together on llama.cpp,
Ollama, LM Studio, TabbyAPI, MLX, or an unidentified engine. The harness
refuses that combination with an actionable error instead of entering the
reasoning loop documented in llama.cpp #30377. vLLM, SGLang, and KoboldCpp are
treated as supporting both.

When the engine has no grammar backend (MLX), the schema is not sent; the
harness adds a visible warning, still writes the schema into the prompt, and
validates every reply. A reply that is not valid JSON or does not match the
schema is repaired; after three failed repairs the turn ends with
`structured_output` and no invalid JSON is accepted as the final answer. The
same validation runs on every engine, including those with decode-time
grammars.

## Hunk review

`review` decides whether `edit`, `patch`, and `write` land immediately or wait
for the TUI's hunk list:

- `multi_file` (default) opens the review when a turn is about to touch a
  second file. The first file still lands immediately.
- `always` reviews every mutation, including a single-file edit.
- `off` writes immediately, exactly as before the feature existed.

In the review modal, `a` accepts the selected hunk, `r` rejects it, `e` opens
its replacement text in the prompt (Enter saves, Esc cancels), `A`/`R` accept
or reject all, Enter applies, and Esc rejects the whole batch. Accepted hunks
are written atomically: a failed write rolls back every file in the batch. A
rejected hunk is reported to the model with its file and line range, so it can
explain or try a different change instead of silently retrying. The decision
is recorded as a `hunk-review` session entry; `undo` still steps back over an
applied hunk.

Headless (`--print`), ACP, and workflow sessions have no interactive reviewer,
so review auto-accepts: the mutation lands immediately and no review entry is
recorded. That keeps scripts and CI non-blocking.

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

## Hash-anchored edits

`edit_mode = "hash"` is opt-in and defaults to `"text"`. It changes `read` and
`patch` only: the exact-text `edit` tool and text-mode `patch` are unchanged.
The bundled `hash` model profile sets it, or add it to your own:

```toml
[model_profile.mine]
edit_mode = "hash"
```

Select it with `--model-profile mine`, or name it in a profile's
`model_profile`. In hash mode, `read` prints a snapshot tag and an 8-hex
content hash before every line:

```
[a.txt#0123456789ab]
1  85ed202b  alpha
2  954620a7  beta
```

`patch` takes the path with that tag (`PATH#SNAPSHOT` or the bracketed header
`[PATH#SNAPSHOT]` both work) and an `ops` array. Line numbers are 1-based
coordinates from the read, not coordinates after earlier operations.

```json
{
  "path": "a.txt#0123456789ab",
  "ops": [
    {"op": "PUT", "line": 2, "hash": "954620a7", "text": "BETA"},
    {"op": "CUT", "line": 4, "hash": "8a2f0c11", "count": 2},
    {"op": "REM", "line": 7, "hash": "c30a9d42"},
    {"op": "MV", "line": 9, "hash": "7e55b801", "to": 3, "to_hash": "4dc1e6aa"}
  ]
}
```

| Op | Meaning |
|---|---|
| `PUT` | Replace `count` lines (default 1) starting at `line` with `text`. An empty `text` deletes the range. |
| `CUT` | Delete `count` lines (default 1) starting at `line`. |
| `REM` | Delete exactly the one anchored line. |
| `MV` | Move the anchored block (`count`, default 1) to immediately after the `to` line, whose hash is `to_hash`. |

The snapshot covers the exact bytes, so a changed line ending is stale. Every
operation is checked against the snapshot and every referenced line hash before
anything is written. A stale snapshot, stale line hash, missing line, or
overlapping operation writes nothing: the result is `STALE_ANCHOR` (or
`invalid_args` for malformed operations) with fresh `[path#snapshot]` anchors
for the targeted lines. A successful patch returns fresh anchors for the
changed lines, so a follow-up patch can chain without a full re-read.

Line hashes are content hashes, not security hashes: duplicate lines share a
hash and are targeted by line number plus hash; whitespace-only lines hash
their spaces; CRLF files hash the text without `\r` and are rewritten with
their original endings. The same `read`-before-`write` requirement applies, and
hash patches pass through the LSP gate below.

## Git approval

The `git` tool's `commit` and the `pr` tool's `draft` and `comment` change
history or publish. With the default `normal`, the tool records a preview in the
session and stops; the user reviews it in the TUI and approves with
`/commit confirm` or `/pr confirm` (`/commit edit <message>` replaces the
message; `\n` starts a new line; `cancel` drops either). A commit is made only
while the staged diff still matches the preview. `allow` runs the action at
once and suits unattended runs; headless `--print` has no way to approve, so
`normal` leaves the preview unapplied there. When the session root sits inside
a larger git work tree instead of at its root, `/commit` and the `git` tool's
mutating actions refuse and change nothing.

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
| `normal` (default; alias `ask`) | ask | ask |
| `accept-edits` | run | ask |
| `plan` | refused (`plan_mode`) | refused unless read-only |
| `deny-all` (alias `dont-ask`) | denied | denied |
| `yolo` | run | run |

`ask` and `dont-ask` are the old spellings of `normal` and `deny-all`; they
still load from a config file and still work on `--mode`, but new files should
use the new names.

In the TUI, `/permissions` opens a picker over the four levels (`normal`,
`accept-edits`, `deny-all`, `yolo`) with the current one marked, and
`/permissions <level>` sets one directly. The change lasts for the session and
never edits the config file. `yolo` asks for a one-key confirmation, and the
session records it just like `--yolo`. Plan mode keeps its own command,
`/plan`.

`yolo` is never the default and cannot be set in a config file; `--yolo` or
`/permissions yolo` selects it for one session, and the session records it.
Deny rules still apply in every mode.

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
records the plan as discarded. `tot --plan --print "..."` runs one plan-mode
turn, prints the plan, and exits 0.

## Lifecycle hooks

`[hooks.<name>]` runs a local command at `session_start`, `user_prompt`,
`pre_tool`, `post_tool`, `pre_compact`, or `stop`. The command is spawned
directly with one JSON object on stdin and one on stdout; it may add context or
block. `timeout_secs` is per hook (0 inherits `[hooks] timeout_secs`), and
`on_error` is `block` or `warn` — `block` by default for `pre_tool`, `warn`
elsewhere. A missing binary, non-zero exit, timeout, or malformed JSON is
visible in the transcript and recorded in the session; it never looks like
approval. Hook output cannot change tools, scope, the endpoint, or the
permission mode. See [hooks.md](hooks.md) for the payload, output schema, exit
codes, and `tot hooks test <name>`.

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
outside the repository is refused. `tot context --show` prints the manifest
(path, bytes, SHA-256, loaded/skipped/truncated) and `tot context --json` emits
it as JSON. `--no-agents-md` or `TOT_AGENTS_MD=0` disables loading for one run.
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
| `TOT_ENDPOINT` | Endpoint base URL |
| `TOT_MODEL` | Model id |
| `TOT_API_KEY` | Bearer token |
| `TOT_PROFILE` | Tool profile |
| `TOT_MODEL_PROFILE` | Force a model profile |
| `TOT_THEME` | Theme name |
| `TOT_NO_SESSION` | Disable persistence |
| `TOT_TRUST` | Trust project config |
| `TOT_AGENTS_MD` | Set to `0` to skip the AGENTS.md chain |
| `TOT_CONFIG_DIR`, `TOT_DATA_DIR` | Override roots (pre-0.3 `LH_CONFIG_DIR`, `LH_DATA_DIR` still work) |
| `TOT_HOME` | Home used for `~/.claude/skills` and `~/.agents/skills` discovery |
| `TOT_SKIP_TOOL_PROBE` | Skip the one-shot tool-call conformance probe |
| `NO_COLOR` | Disable color |
| `TOT_DEBUG_CONTENT` | Allow content in debug logs (warns) |
| `TOT_BWRAP` | Override the bubblewrap executable used by `--sandbox` |

## Flags

`--print`, `--json`, `--endpoint`, `--model`, `--model-profile`, `--profile`,
`--json-schema <file>`, `--theme`, `--api-key`, `--trust`, `--no-session`,
`--no-redact`, `--no-agents-md`, `--continue`, `--resume`, `--path-jail`,
`--sandbox [read-only|workspace-write]`, `--no-sandbox`, `--engagement`,
`--no-color`, `--keep-artifacts`, `--clean-artifacts`, `--allow-remote-images`,
`--keep-exif`, `--debug`, `--mode <normal|accept-edits|plan|deny-all>`, `--yolo`,
`--plan`.

`tot doctor [--json] [--fix]` prints the engine capability manifest without
sending a chat completion. See [model-setup.md](model-setup.md).

`tot context --show` prints the AGENTS.md chain and its manifest without
opening a session or contacting a model; `--json` emits one stable object.
With no AGENTS.md files in the chain, it reports an empty chain. With
`--no-agents-md` or `TOT_AGENTS_MD=0`, it reports that loading is disabled.
