# MCP servers

LocalHarness is a [Model Context Protocol](https://modelcontextprotocol.io)
client. Every tool an MCP server advertises becomes a harness tool with the
same contract as the built-ins: prescriptive errors, capped output,
cancellation, and evidence entries during an engagement.

Both standard transports are supported:

- **stdio**: the harness starts the server as a child process and speaks
  newline-delimited JSON-RPC over its stdin/stdout.
- **Streamable HTTP**: each message is one POST to the server URL. The server
  can answer with JSON or an SSE stream. `Mcp-Session-Id` is kept and sent
  back, and the session is ended with `DELETE` on exit.

The client negotiates protocol `2025-06-18` and accepts `2025-03-26` and
`2024-11-05`.

## Configure

Add one `[mcp.<name>]` table per server to `~/.config/localharness/config.toml`.
Project config (`.harness/config.toml`) can declare servers too, but only with
`--trust`, the same as every other project-local setting.

```toml
# stdio: any command that speaks MCP on stdin/stdout
[mcp.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
[mcp.github.env]
GITHUB_PERSONAL_ACCESS_TOKEN = "${GITHUB_TOKEN}"   # read from your environment

[mcp.time]
command = "uvx"
args = ["mcp-server-time"]
profiles = ["coding", "full"]   # attach only to these profiles

# Streamable HTTP
[mcp.docs]
url = "https://mcp.example.com/mcp"
[mcp.docs.headers]
Authorization = "Bearer ${DOCS_MCP_TOKEN}"
```

| Key | Default | Meaning |
|---|---|---|
| `command` | | Executable for a stdio server. Set this or `url`, not both. |
| `args` | `[]` | Arguments for `command`. |
| `env` | `{}` | Extra environment for the server. The harness environment is inherited. |
| `cwd` | project directory | Working directory for a stdio server. A relative path is resolved against the project directory. |
| `url` | | Endpoint for a Streamable HTTP server (`http` or `https`). |
| `headers` | `{}` | HTTP headers sent with every request, for example `Authorization`. |
| `enabled` | `true` | `false` keeps the table but never starts the server. |
| `profiles` | unset | Unset attaches the server to every profile. A list attaches it only to those profiles. `[]` attaches it only where a profile names it (see below). |
| `tools` | `[]` | Allowlist of remote tool names. Empty exposes all of them. |
| `exclude_tools` | `[]` | Remote tool names to hide. |
| `startup_timeout_secs` | `30` | Deadline for the handshake and `tools/list`. |
| `timeout_secs` | `120` | Deadline for one tool call. |

`${VAR}` in `command`, `args`, `env`, `url`, and `headers` is replaced from the
environment when the server starts. If a variable is unset, that server fails
with an error that names it. Keep tokens in the environment, not in the file.

Server names may use letters, digits, `-`, and `_`.

## Tool names and profiles

A remote tool `create_issue` on server `github` is exposed to the model as
`github__create_issue`. Names are sanitized to `[A-Za-z0-9_-]`. A name longer
than 64 characters is shortened and given a stable hash suffix. Built-in and
external tools keep their names: an MCP tool that would clash with one is
skipped, with a warning.

A profile can also attach MCP tools explicitly through its `tools` list:

```toml
[profile.research]
packs = ["core"]
tools = ["mcp:docs", "github__search_issues"]
```

- `mcp:<server>` attaches every exposed tool of that server.
- `<server>__<tool>` attaches one tool.

Naming a server that is not configured is a startup error. When a configured
server fails to start, its tools are left out and the reason is shown as a
session warning. The harness still starts.

Every tool schema costs prompt tokens on every turn. With small context windows,
use `tools`, `exclude_tools`, or `profiles` to keep the surface small. The
static-prompt warning at startup shows what the profile costs.

## Inspect and test

```sh
lh mcp list                 # connect to every enabled server, list exposed tools
lh mcp list --json          # the same as one JSON object
lh mcp call time get_current_time '{"timezone": "UTC"}'
```

`lh mcp list` exits with `1` when any server fails. `lh mcp call` runs one tool
directly without a model, which helps when debugging a server's configuration.

In the TUI, `/mcp` shows each server's state: connected (with server version,
protocol, and tool count), failed (with the error), disabled, or not used by the
active profile. `/tools` lists the active tools, MCP tools included.

## Behavior

- **Lifecycle.** Servers start when a profile that uses them loads. They stay
  running across `/profile`, `/model`, and `/theme` changes, are restarted when
  their `[mcp.*]` table changes, and are stopped when `lh` exits. Each stdio
  server runs in its own process group so launchers such as `npx` and `uvx` are
  cleaned up with it.
- **Crashes.** A request in flight when the server exits fails with the last
  lines of the server's stderr, and it is not retried, because it may already
  have had effects. The next call starts a fresh server process. A request that
  was never delivered (the process was already gone, or an HTTP session
  expired) is retried once on a new connection.
- **Timeouts and cancellation.** When a call times out or the turn is
  interrupted (`Esc`), the harness sends `notifications/cancelled` and returns
  at once.
- **Results.** Text content is passed through. Images and audio are replaced by
  a short placeholder with the MIME type and size, because chat-completions
  tool results are text. Embedded text resources are inlined. `structuredContent`
  is shown when the result has no text, and it is always stored in the result
  details. `isError: true` becomes a tool error prefixed `[MCP_TOOL_ERROR]`.
  Output is capped at 64 KiB per call, before the harness's usual result cap.
- **Arguments.** Required keys and primitive types from the tool's
  `inputSchema` are checked before the call, so a bad call gets an
  `[MCP_ARGS]` error that says what to fix, with no round trip.
- **Server requests.** The client declares no optional capabilities, so it
  answers only `ping`. Other server-initiated requests (sampling, roots,
  elicitation) get "method not found".

Not supported yet: MCP resources and prompts as first-class features, OAuth
authorization flows (use a bearer token in `headers`), refreshing the tool list
on `notifications/tools/list_changed` without a profile reload, and the legacy
HTTP+SSE transport from protocol 2024-11-05.

## Security

An MCP server is code you chose to run, so treat a server entry like an
external tool manifest:

- stdio servers run **outside** `--sandbox` and `--path-jail`, as your user,
  with your environment plus `env`. The sandbox applies to `bash` and `bg`
  only.
- Project-declared servers load only with `--trust`, because starting one runs
  a command from the repository.
- Tool descriptions and results come from the server and are untrusted model
  input. A malicious or compromised server can try prompt injection.
- During an engagement, each MCP call appends an evidence entry recorded as
  `mcp <server>/<tool> <arguments>`. MCP calls are not scope-gated: the harness
  cannot see what a remote tool will touch.
