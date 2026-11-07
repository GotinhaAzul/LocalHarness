# External tools

Any executable can become a tool. One manifest, one JSON request, one JSON
response. No plugin ABI.

## Manifest

Global manifests live in `~/.config/localharness/tools/*.toml` and are trusted.
Project manifests live in `.harness/tools/` and require `--trust`.

```toml
name = "semgrep"
description = "Run semgrep and return JSON findings"
command = "semgrep"
args = ["--json", "--config", "auto", "{target}"]
input_schema = "semgrep.schema.json"   # relative to this manifest
timeout_secs = 120
shell = false
scope_gate = false

[env]
SEMGREP_SEND_METRICS = "off"
```

- Placeholders `{key}` are substituted from validated arguments. Values must be
  strings, numbers, or booleans.
- `shell = true` is required for shell syntax; the default spawns the program
  directly.
- `scope_gate = true` checks string arguments against the engagement scope.
- The schema file is a JSON Schema object with flat properties and a `required`
  list.

External tools are attached to profiles/workflows; they never auto-join the
default schema.

## Protocol

The harness writes one JSON object to stdin:

```json
{"tool": "semgrep", "args": {"target": "src/"}, "cwd": "/home/user/app", "session_id": "20261107-..."}
```

The tool writes one JSON object to stdout:

```json
{"content": "3 findings", "is_error": false, "details": {"count": 3}}
```

- If stdout is not JSON, it is treated as text and the exit code decides
  `is_error`.
- Stderr is captured for the error message.
- Output is capped; failures and timeouts are prescriptive.
- Streaming is not supported. Long-running tools should be wrapped with `bg`.

## Example tool

```python
#!/usr/bin/env python3
import json, sys
request = json.load(sys.stdin)
target = request["args"]["target"]
print(json.dumps({"content": f"checked {target}", "is_error": False}))
```
