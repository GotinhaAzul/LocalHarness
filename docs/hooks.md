# Lifecycle hooks

Hooks run your own commands at fixed points in a turn. They can add context or
block an action. They cannot change the tool list, the security scope, the
endpoint, or the permission mode.

Hooks are configured per name:

```toml
[hooks]
timeout_secs = 5          # default deadline for a hook that does not set one
max_output_bytes = 16384  # recorded output cap; larger output becomes an artifact

[hooks.guard]
event = "pre_tool"
command = ["/home/me/bin/policy-check"]   # direct exec, no shell
timeout_secs = 2
on_error = "block"                        # the pre_tool default

[hooks.format]
event = "post_tool"
command = ["/home/me/bin/format"]

[hooks.notify]
event = "stop"
command = ["notify-send", "LocalHarness", "turn finished"]
```

`command` is a program plus arguments. It is spawned directly. There is no
shell: `"echo hi | tee out"` is not a command line, and shell syntax is never
interpreted. Use `["sh", "-c", "..."]` only if you want a shell yourself.

## Events

| Event | Runs | Typical use |
|---|---|---|
| `session_start` | once per session, before the first model call | load environment facts, announce a policy |
| `user_prompt` | after the user message is recorded, before the model sees it | classify the request, add a reminder |
| `pre_tool` | after the harness gates and before a tool runs | policy check, block a dangerous command |
| `post_tool` | after a tool ran, before its result is stored | format the edited file, add a note |
| `pre_compact` | before a deterministic compaction | record why context is being rebuilt |
| `stop` | once when a turn ends, for every status | notifications, cleanup |

`session_start` is recorded in the session, so resuming a session does not
repeat it. `stop` is advisory: the turn has already stopped, so its block is
shown but cannot undo work. `pre_compact` runs for the main compaction; the
`thread` split has its own compaction path and does not run it.

## Protocol

One JSON object is written to stdin:

```json
{
  "event": "pre_tool",
  "hook": "guard",
  "session": {
    "id": "20260412-120000-1234",
    "cwd": "/home/me/src/app",
    "model": "qwen3.5-35b-a3b",
    "profile": "coding"
  },
  "tool": {"name": "bash", "arguments": {"command": "git push"}}
}
```

| Event | Extra payload fields |
|---|---|
| `session_start` | none |
| `user_prompt` | `prompt` |
| `pre_tool` | `tool.name`, `tool.arguments` |
| `post_tool` | `tool.*`, `result.content`, `result.is_error`, `duration_ms` |
| `pre_compact` | `compaction.reason`, `compaction.tokens_before` |
| `stop` | `stop.status` (`done`, `cancelled`, `budget`, `stalled`, `context_truncated`, `error`, `blocked`) |

The hook writes one JSON object on stdout:

```json
{"decision": "block", "reason": "pushing is not allowed in this repository"}
```

| Field | Meaning |
|---|---|
| `decision` | `"allow"` (or absent) or `"block"` |
| `reason` | shown to the user and to the model when the hook blocks |
| `context` | a string or an array of strings added to the transcript; `pre_tool`/`post_tool` context joins the tool result, other events join the system context |

Every other field is ignored. That is deliberate: `tools`, `scope`,
`endpoint`, `permission_mode`, and `policy` in a hook's output do nothing. A
hook's `allow` also cannot revive a call that a harness gate already denied.
Hooks run after the read-first, todo, scope, and approval gates.

## Exit codes and failures

| Condition | Behavior |
|---|---|
| exit 0, one valid JSON object | the decision applies |
| exit 2 | block; stderr's first line is the reason |
| other non-zero | failure |
| missing executable, signal, or timeout | failure |
| exit 0, malformed JSON | failure |

A failure is visible in the transcript and recorded as `error` or `timeout`.
`on_error` decides whether it also blocks:

| Event | Default |
|---|---|
| `pre_tool` | `block` |
| everything else | `warn` |

`block` means the action does not run. `warn` means it continues with the
failure named in the result. A guardrail that never executed must never look
like approval, which is why `pre_tool` fails closed by default.

The per-hook deadline kills the process (`kill_on_drop`) and returns
`timeout`. Other turns are not held by a slow hook.

## Output cap and records

`[hooks] max_output_bytes` caps the text copied into the session record
(default 16 KiB). Larger stdout/stderr is written to the session artifact
store and the record keeps the artifact id. `reason` and `context` strings are
capped as well.

Every invocation appends a `hook-invocation` custom session entry with the hook
name, event, outcome (`ok`, `blocked`, `error`, `timeout`), duration in
milliseconds, exit code, reason, and artifact when one exists. `--json` runs
show hook failures as `notice` events; `--json` also shows them in the session.

## Test a hook

```sh
lh hooks list
lh hooks test guard
lh hooks test guard --json
```

`lh hooks test <name>` runs exactly one configured hook with a representative
payload for its event and prints the outcome. It does not call a model and does
not write a session. Exit status is 0 when the hook allowed or succeeded, 1
when it blocked or failed.

## Scope of the mechanism

- Hooks are code you install. Treat them like any other local executable and
  review project hooks before `--trust`.
- A hook cannot add a tool, enable a denied one, widen `/sec` scope, change the
  endpoint, or switch the permission mode. Those values are not read from hook
  output anywhere in the dispatcher.
- Project config, including project hooks, is loaded only with `--trust`.
- Hooks run serially and block the turn while running; they are not a
  concurrency feature.
- There is no embedded scripting runtime. Hooks are external commands.
