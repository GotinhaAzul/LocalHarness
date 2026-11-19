# Editor integration (ACP)

`tot acp` serves the [Agent Client Protocol v1](https://agentclientprotocol.com)
on stdio, so an editor can host a Totality thread instead of a terminal.
Zed, Neovim, and Emacs clients that speak ACP can drive it.

The process speaks only newline-delimited JSON-RPC on stdout. Diagnostics go to
the log file, `totality.log` in the data directory; see
[configuration](configuration.md#paths). They never go to the protocol stream.

## Zed

Add a custom agent in `settings.json` (`agent: open settings` → **External
Agents** → **Add Custom Agent**):

```json
{
  "agent_servers": {
    "totality": {
      "type": "custom",
      "command": "tot",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

`tot` must be on the `PATH` Zed sees. Pass the endpoint and model as ACP does not
carry a model picker yet:

```json
{
  "agent_servers": {
    "totality": {
      "type": "custom",
      "command": "tot",
      "args": [
        "--endpoint", "http://127.0.0.1:8080",
        "--model", "Qwen3.5-35B-A3B-Q4_K_M",
        "acp"
      ],
      "env": {}
    }
  }
}
```

The same flags work from `TOT_ENDPOINT` and `TOT_MODEL`, or from the global config
written by `/login` in the TUI. Start a thread from the Agent Panel after saving
the settings; Zed detects the file without a restart.

## Protocol surface

| Method | Behaviour |
| --- | --- |
| `initialize` | Returns protocol version 1, `loadSession`, and text/embedded-context prompt capabilities. Image and audio prompts are refused. |
| `session/new` | Creates one harness runtime rooted at the absolute `cwd` the client declares. Returns the harness session id. |
| `session/load` | Reopens a session file under that root and replays user and assistant text as `session/update`. |
| `session/prompt` | Runs one turn, streams `session/update`, and returns the stop reason when it settles. |
| `session/cancel` | A notification that cancels the running turn; the in-flight `session/prompt` response is `cancelled`. |

`session/update` carries `agent_message_chunk`, `agent_thought_chunk`,
`tool_call`, and `tool_call_update`. Every string is stripped of ANSI and OSC
control sequences with the same `sanitize::display` used by the TUI. Streamed
deltas are buffered when an escape sequence straddles two chunks, so its
payload never reaches the client as visible text.

Harness diagnostics (the todo and verify gates, provider retries, compaction,
output-limit notices) are sent as `agent_thought_chunk` with the text
`[harness] …` on its own lines, never as `agent_message_chunk`. A client can
therefore show them separately from the model's answer and filter them out of
a chat transcript.

Stop reasons match the CLI's turn status:

| CLI status | ACP `stopReason` |
| --- | --- |
| done | `end_turn` |
| cancelled | `cancelled` |
| budget or context truncated | `max_tokens` |
| stalled | `max_turn_requests` |
| blocked | `refusal` |
| error | a JSON-RPC error (`-32000`) |

An errored turn is a protocol error rather than `refusal`, so a transient
endpoint failure does not tell the client to drop the conversation. The exact
harness status and error code ride in `_meta.totality` (or the error
`data`).

## Roots

The declared root is the session's `cwd`. File tools run with `--path-jail`
equivalent enforcement, so a path outside the root is denied even when the
client asks for it and even after the client approves the tool call.

`additionalDirectories` is refused with
`unsupported_capability` instead of being silently ignored: this version
supports one root per session. Start a second session for the second root.

Project trust and `verify` steps belong to the process working directory. When
a session declares a different root, that session runs untrusted and with no
verify steps, so a client cannot extend the launched project's trust to an
arbitrary directory. Launch `tot acp` in the project root (or pass
`--endpoint`/`--model` explicitly) for project config and verify behavior.

## Permissions

Every tool call that can change files or run processes asks the client with
`session/request_permission`: `write`, `edit`, `patch`, `undo`, `bash`, `bg`,
`verify`, and external tools. Only the built-in read and session tools
(`read`, `grep`, `glob`, `repo_map`, `recall`, `review`, `lsp`, `skill`,
`todo`, `thread`) run without a prompt; a tool that merely declares itself
read-only, such as an MCP server, still asks.

The two options are `allow_once` and `reject_once`. A denial is final for that
call and leaves no filesystem or process side effect: the tool does not run. A
client approval is necessary but never sufficient. The harness gates run first
and cannot be relaxed by the client:

- `READ_FIRST` still blocks an edit to a file that was not read;
- the path jail still blocks a write outside the root;
- the security scope gate still blocks an out-of-scope command;
- the sandbox, tool allowlist, budget, and settle gates still apply.

`bash` and `bg` are processes, not path arguments: the path jail does not
confine them. Their boundary is the permission prompt plus the configured
sandbox (`--sandbox`). Run editors with `--sandbox workspace-write` when the
client should be able to run commands but not write outside the root.

The decision is per call and is not remembered across calls or turns.

## Unsupported in this iteration

These return a typed error with `error.data.code = "unsupported_capability"`
rather than a silent success:

- terminal access: it is not advertised, the agent never calls `terminal/*`,
  and a client that sends one of those reverse-request names to the agent gets
  the typed refusal;
- `fs/read_text_file` and `fs/write_text_file` sent to the agent (the agent
  reads and writes through its own tools and path jail);
- `additionalDirectories`;
- image and audio prompt blocks, and binary embedded resources.
- `session/load` when the server was started with `--no-session`.

Client-supplied `mcpServers` are not connected. Configure MCP servers in the
harness config (`[mcp.<name>]`) instead; `session/new` reports the ignored count
in `_meta.totality` rather than claiming they are available.

## Conformance

`tests/acp.rs` drives the real binary over stdio against a scripted endpoint. It
covers initialization, session creation, a streamed prompt, cancellation,
cancel while a permission is pending, session load and replay, root enforcement
(absolute paths and symlinks), a denied permission with no side effect, a
permission gate that is never reached for a denied path, concurrent prompts,
malformed JSON, sanitization, and the typed terminal refusal.
