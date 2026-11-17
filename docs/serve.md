# Local HTTP API (`tot serve`)

`tot serve` runs a small HTTP server on your machine so scripts, cron jobs, CI
steps, dashboards, and phone shortcuts can drive Totality sessions without
spawning `tot --print` for every prompt or reimplementing process management.

It is the same harness as the TUI, `--print`, and `tot acp`. Every session uses
the same session store, config, path jail, scope gate, sandbox, and agent loop.
The only new thing is the transport.

```sh
tot --endpoint http://127.0.0.1:8080 --model qwen3 serve
# {"listening":"http://127.0.0.1:43817","token_file":"/home/me/.local/share/totality/serve/token-43817"}
```

stdout carries exactly one JSON line with the address and the **path** of the
token file. Human-readable notes go to stderr. The token itself is never
printed or logged.

## Quick start

```sh
BASE=http://127.0.0.1:43817
TOKEN=$(cat ~/.local/share/totality/serve/token-43817)
auth=(-H "Authorization: Bearer $TOKEN")

# 1. Create a session rooted at the server's working directory.
ID=$(curl -s "${auth[@]}" -X POST $BASE/v1/sessions -d '{}' | jq -r .id)

# 2. Start a turn. The response says where its events start.
AFTER=$(curl -s "${auth[@]}" -X POST $BASE/v1/sessions/$ID/turns \
  -d '{"prompt":"Summarize this repo"}' | jq -r .after)

# 3. Stream the turn until the `done` event.
curl -sN "${auth[@]}" "$BASE/v1/sessions/$ID/events?after=$AFTER"

# Cancel from another shell.
curl -s "${auth[@]}" -X POST $BASE/v1/sessions/$ID/cancel
```

## Flags

| Flag | Default | Meaning |
| --- | --- | --- |
| `--bind IP:PORT` | `127.0.0.1:0` | Listen address. Port `0` picks a free port. `localhost:PORT` means `127.0.0.1`. Host names are refused so the loopback check cannot be fooled by DNS. |
| `--allow-non-loopback` | off | Required for any address that is not loopback (`0.0.0.0`, a LAN IP). Prints a warning: anyone who can reach the port and read the token drives the harness, and plain HTTP carries the token unencrypted. Prefer an SSH tunnel. |
| `--token-file PATH` | `<data dir>/serve/token-<port>` | Where the token is written, created with mode `0600` (its directory `0700`). A stale regular file is replaced; a symlink is refused. The file is removed when the server stops. |
| `--allow-yolo` | off | Lets a session request `"permission_mode": "yolo"`. |

The global flags apply as usual: `--endpoint`, `--model`, `--profile`,
`--sandbox`, `--engagement`, `--trust`, `--no-session`, `--no-redact`, `--debug`.
`--print`, `--json`, `--continue`, and `--resume` are refused with `serve`. To
resume over HTTP, use `{"resume": "<id>"}` (see below).

## Endpoints

Every request needs `Authorization: Bearer <token>`, including `/v1/health`.
The token is checked before routing, so without it you cannot tell whether a
session exists. Request and response bodies are JSON, and request bodies are
capped at 1 MiB.

| Method and path | Success | Body / result |
| --- | --- | --- |
| `GET /v1/health` | 200 | `{"status":"ok","version":"…","sessions":N}` |
| `GET /v1/sessions` | 200 | `{"sessions":[<session>…]}`: the sessions live in this server |
| `POST /v1/sessions` | 201 | Request `{"cwd"?, "permission_mode"?, "resume"?}` → `<session>` plus `"resumed"` |
| `GET /v1/sessions/{id}` | 200 | `<session>` |
| `POST /v1/sessions/{id}/turns` | 202 | Request `{"prompt":"…"}` → `{"session_id","turn","after","events"}` |
| `GET /v1/sessions/{id}/events` | 200 | `text/event-stream`, see [Events](#events) |
| `POST /v1/sessions/{id}/cancel` | 200 | `{"session_id","cancelled":true\|false}` |

`<session>` is:

```json
{
  "id": "20261115-101500-1234",
  "root": "/home/me/project",
  "busy": false,
  "permission_mode": "ask",
  "trusted": false,
  "turns": 2,
  "last_status": "Done",
  "last_event": 57
}
```

### Creating a session

- `cwd`: an absolute, existing directory. It defaults to the directory
  `tot serve` was started in. File tools are jailed to it, as in ACP. Project
  trust (`--trust`) and `verify.toml` apply only when `cwd` is the server's own
  directory. Another root never inherits that trust.
- `permission_mode`: see [Permissions](#permissions). Defaults to `normal`.
- `resume`: a stored session id under that root. The session store reopens
  it, the same way `tot --resume` does. A session already live in this server
  returns `409 session_already_loaded`.

A server with no model selected answers `503 no_model` instead of creating a
session that can never run a turn.

### One turn at a time

A session runs one turn at a time. A second `POST /turns` while a turn is
running gets:

```json
HTTP/1.1 409 Conflict
{"error":{"code":"session_busy","message":"a turn is already running in this session; wait for its `done` event or POST /cancel","session_id":"…"}}
```

Nothing is queued, and the rejected prompt never reaches the session file. The
`done` event is published only after the session is released, so a script can
post its next turn as soon as it sees `done`.

## Events

`GET /v1/sessions/{id}/events` is a standard Server-Sent Events stream:

```
id: 12
event: text
data: {"type":"text","text":"Hello"}

```

- **The names are the `--json` names.** `event:` is the same string as the
  `"type"` field of `tot --print --json`, and `data:` is the same JSON object.
  Both frontends render through one function (`src/events.rs`), so they cannot
  drift. The names are `started`, `state`, `text`, `reasoning`,
  `tool_started`, `tool_finished`, `usage`, `context`, `window_detected`,
  `compacted`, `provenance`, `hunk_review`, `notice`, `guard`, `error`, and
  `done`, plus `cancelled`. As with `--json`, `cancelled` is emitted when a
  cancel is requested, and the turn's own `done` follows with
  `"status":"Cancelled"`.
- **Text is passed through unchanged, as with `--json`.** Strip control
  sequences yourself before painting model text in a terminal.
- **Ids and replay.** `id:` increases within a session. Connect with
  `?after=N` or a `Last-Event-ID: N` header to replay the buffered frames after
  `N` (the last 4096 per session), then follow live. Without a cursor you get
  live frames only. Use the `after` value returned by `POST /turns` so you never
  miss the start of a turn, even if you connect after posting it.
- A reader that falls more than 1024 frames behind is disconnected. Reconnect
  with `Last-Event-ID` to replay what you missed. A `: keepalive` comment is
  sent every 15 seconds while idle.
- `done.status` is the turn status: `Done`, `Cancelled`, `Budget`, `Stalled`,
  `ContextTruncated`, `Blocked`, or `Error`. These are the same statuses behind
  the exit codes of `--print`.

## Permissions

Nobody can answer a permission prompt in the middle of an HTTP turn. Each
session picks its mode when it is created, and that mode is installed on the
session's permission engine before the runtime is built. The engine is the
single pre-dispatch decision point (`authorize::decide()`): config
`[permissions]` rules decide first, and the mode only fills in when no rule
matches. A deny rule always wins. The mode never widens a harness gate
(read-before-edit, scope, path jail, sandbox), which still runs afterward.

| `permission_mode` | Runs | Denied |
| --- | --- | --- |
| `normal` (default; alias `ask`) | built-in read-only tools (`read`, `grep`, `glob`, `repo_map`, `recall`, `review`, `lsp`, `skill`, `todo`, `thread`), plus any call an allow rule names | everything else |
| `accept-edits` | the above plus `write`, `edit`, `patch`, `undo` | `bash`, `bg`, `verify`, `test`, MCP and external tools |
| `deny-all` (alias `dont-ask`) | same as `normal` over HTTP | same as `normal` |
| `yolo` | every call the harness gates allow; needs `tot serve --allow-yolo` | nothing beyond the gates |

A denied call does not run. The model receives a `DENIED` tool result that
names the mode, rule, or next action, and the turn continues. MCP and external
tools always count as needing approval, even when their manifest claims to be
read-only. Add an allow rule under `[permissions]` to run a specific command or
path in `normal` or `deny-all` mode.

**Why not an approval endpoint?** `tot serve` is built for unattended callers.
A round trip that parks a turn until a second client approves it would need its
own event type outside the `--json` schema, and a timeout policy. Picking the
mode at creation is explicit, auditable in the session list, and safe by
default. Approvals over HTTP can be added later without changing these modes.

**One decision point.** The HTTP mode is the same `[permissions]` mode the CLI
uses. Config allow and deny rules apply over HTTP exactly as they do in the
TUI and `--print`; an `ask` that nobody can answer is denied. There is no
separate HTTP approver narrowing the call after the policy already decided.

## Security

- **Loopback by default.** A non-loopback bind needs `--allow-non-loopback` and
  prints a warning.
- **Token.** 256 random bits from `/dev/urandom`, new on every start, written
  to a `0600` file, and compared in constant time. It never appears on stdout,
  stderr, in the log file (even with `--debug`), in the session file, or in an
  error body. A 401 never echoes the value presented.
- **No browsers.** There are no CORS headers. A request carrying an `Origin`
  header is refused with `403 cross_origin_refused`, so a web page cannot drive
  the server even if it learns the port.
- **Same rules as the TUI.** Sessions go through the same agent loop and gates.
  `--sandbox` applies to `bash` and `bg`, the scope gate blocks out-of-scope
  security commands (`SEC_SCOPE`), and the path jail is always on for file
  tools.
- **No telemetry.** Nothing leaves the machine except model requests to the
  endpoint you configured.

## Stopping

SIGINT (Ctrl+C) or SIGTERM stops the server. It refuses new sessions and turns
(`503 shutting_down`), cancels running turns, and waits up to 10 seconds for
each one to write its cancellation to the session file. An open event stream
receives that turn's `done` (`"status":"Cancelled"`) before it closes. The
server then closes the remaining connections, stops MCP servers, and removes
the token file. Sessions stay in the normal session store:

```sh
tot --resume <id>                       # TUI
tot --resume <id> --print "continue"    # headless
curl … -X POST $BASE/v1/sessions -d '{"resume":"<id>"}'   # a new tot serve
```

With `--no-session`, nothing is persisted, ids look like `http-1`, and
`resume` is refused.

## Errors

Every error is `{"error":{"code":"…","message":"…"}}`, with extra fields where
useful (`session_id`).

| Status | `code` |
| --- | --- |
| 400 | `invalid_json`, `invalid_params`, `unsupported` |
| 401 | `unauthorized` |
| 403 | `cross_origin_refused`, `mode_not_permitted` |
| 404 | `unknown_session`, `not_found` |
| 405 | `method_not_allowed` (with an `Allow` header) |
| 409 | `session_busy`, `session_already_loaded` |
| 413 | `body_too_large` |
| 500 | `session_open_failed`, `internal_error` |
| 503 | `no_model`, `shutting_down` |

## Limits

- HTTP/1.1 only, without TLS. Put it behind an SSH tunnel or a reverse proxy
  you control if you need remote access.
- Sessions live until the server stops. There is no delete endpoint yet.
- A turn takes one prompt string. There are no structured content blocks.
- `tests/serve.rs` covers every behavior above against a scripted endpoint.
