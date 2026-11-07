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
[model]
endpoint = "http://127.0.0.1:8080"
model = ""                      # auto-select if /v1/models has one entry
api_key = ""
context_window = 32768
temperature = 0.2
top_p = 0.95
max_output_tokens = 4096
request_timeout_secs = 900
connect_timeout_secs = 5
first_token_timeout_secs = 120
stream_idle_timeout_secs = 120

[ui]
theme = "omarchy"
color_mode = "auto"             # auto | truecolor | 256 | 16
show_reasoning = false
show_token_counts = true

[session]
no_session = false
artifact_retention_days = 30

[budgets]
max_tool_calls = 120
max_model_calls = 40
max_wall_minutes = 30
stall_calls = 12
verify_attempts = 2
wrap_up_at = 0.8

[profile.full]
packs = ["core", "coding", "security"]
model_profile = "auto"

[profile.minimal]
tools = ["read", "bash", "verify"]

[model_profile.default]
context_window = 32768
tool_mode = "repair"
strip_reasoning = true

[model_profile.qwen3]
match = ["qwen3"]
context_window = 65536
tool_mode = "native"
strip_reasoning = true
reasoning_field = "reasoning_content"
stop = ["<|im_end|>"]
extra_body = { chat_template_kwargs = { enable_thinking = true } }
```

`model_profile = "auto"` matches by model-name prefix declared in `match`; the
`default` profile is the fallback. `[model] context_window` is used only when
the model profile does not set one.

## Verify steps

Trusted project file `.harness/verify.toml` (or `[verify]` in config):

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
| `NO_COLOR` | Disable color |
| `LH_DEBUG_CONTENT` | Allow content in debug logs (warns) |

## Flags

`--print`, `--json`, `--endpoint`, `--model`, `--model-profile`, `--profile`,
`--theme`, `--api-key`, `--trust`, `--no-session`, `--continue`, `--resume`,
`--path-jail`, `--engagement`, `--no-color`, `--keep-artifacts`,
`--clean-artifacts`, `--debug`.
