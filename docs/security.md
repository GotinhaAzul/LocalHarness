# Security posture and engagements

LocalHarness includes a first-class security pack for authorized testing,
labs, CTFs, and defensive work. It does not provide exploit payloads,
persistence, credential dumping, evasion, or denial-of-service tooling.

## Authorization

Scope is set only by the user:

```text
/sec scope add 10.0.0.0/24
/sec scope add *.example.test
/sec scope add app.example.test
/sec scope remove <target>
/sec scope clear
```

Project files cannot grant scope. There is no model tool for scope.

The scope gate checks `bash`, `bg`, and external tools marked
`scope_gate = true` against a list of active-testing tools (nmap, masscan,
sqlmap, nikto, nuclei, ffuf, gobuster, feroxbuster, dirb, dirsearch, hydra,
medusa, msfconsole, msfvenom, meterpreter, responder, impacket-*, crackmapexec,
netexec, bloodhound, certipy, rubeus, chisel, ligolo, pangolin). Targets are
extracted best-effort from the command; `--help` and `--version` are allowed.

**The gate is a speed bump, not a sandbox.** Unlisted tools, scripts, and
obfuscation can bypass it. The rules of engagement are the real control. If you
need isolation, run the harness in a container.

## Findings

Findings are engagement-global and stored in
`~/.local/share/localharness/engagements/<engagement>/findings.jsonl`
(directory `0700`, file `0600`). Each finding records id, title, severity,
status, target, evidence, impact, remediation, timestamps, and the session and
entry that produced it.

```text
finding {action:"add", title:"...", severity:"high", target:"https://app.test", evidence:"..."}
finding {action:"update", id:"f1", status:"fixed"}
finding {action:"list"}
finding {action:"report"}
/sec report
```

Severities: critical, high, medium, low, info. Statuses: open, fixed, accepted,
false-positive. Caps: 50 findings, 2000 characters of evidence, 1000 of
impact/remediation. `add` auto-attaches the last tool result and its engagement
evidence entry when none is given; `evidence_id:"e3"` links a specific entry.

## PoC verification

A finding can carry a deterministic proof-of-concept step graph. `finding
verify` replays it locally and records a verdict; a finding is `confirmed` only
when every declared assertion passes against a reproduced run.

```text
finding {
  action:"add",
  title:"Reflected marker at /search",
  severity:"high",
  target:"https://app.test/search",
  evidence:"q=ZERO-INJ-OK is reflected unencoded",
  poc_steps:[
    {"id":"probe","kind":"verify",
     "action":{"type":"http","method":"GET","url":"https://app.test/search?q=ZERO-INJ-OK"},
     "expect":{"type":"body-contains","text":"ZERO-INJ-OK"}}
  ]
}
finding {action:"verify", id:"f1"}
finding {action:"verify", id:"f1", minimize:true, max_oracle_calls:20}
```

Step actions:

| Action | Fields | Runs |
|---|---|---|
| `shell` | `cmd`, optional relative `cwd` | `bash -c` in an isolated run directory |
| `http` | `method`, `url`, optional `headers`, `body` | one request, redirects disabled |
| `note` | `text` | nothing; informational only |

Assertions (`expect`): `exit-zero`, `http-status` (number or array),
`body-contains`, `body-matches` (regex), `file-exists` (relative path inside the
run directory). At least one step must carry an assertion, or `verify` refuses
to run.

Verdicts:

| Status | Verdict | Meaning |
|---|---|---|
| `reproduced` | `confirmed` | every assertion passed |
| `not_reproduced` | `rejected` | the run completed but an assertion failed |
| `error` | `inconclusive` | the runner, scope, or a command failed |
| `skipped` | `inconclusive` | no assertion was declared |

`not_reproduced` does not change the finding's status and is not an automatic
false positive. `error` and `skipped` never suppress a finding; only an explicit
`finding update status=false-positive` does. Confidence is the model's reported
value raised to `0.6` when `poc_steps` exist and `0.8` when a verifiable
assertion exists.

Shell steps run in a temporary directory under the engagement with a cleared
environment (only `PATH`, `HOME`, `TMPDIR`, locale, and shell identity are
inherited), a per-step timeout, and a process-group kill on timeout or cancel.
HTTP steps require the URL host to be in engagement scope; out-of-scope targets
are refused before a request is sent. Each executed step appends an engagement
evidence entry with the command, timing, exit status, output, and SHA-256.
Minimization (`minimize:true`) removes steps while the graph still reproduces,
bounded by `max_oracle_calls` (default 20, maximum 50). Exploit/verify steps and
steps with a non-`exit-zero` assertion are anchors and are never removed;
minimization trims setup and noise steps around them.

Only the `local` runner is implemented. Container and QEMU replay are not part
of this build.

## Evidence chain

When an engagement is active, every `bash`, `bg`, and external tool run appends
an immutable entry to
`engagements/<engagement>/evidence.jsonl` and writes its output to
`engagements/<engagement>/evidence/eN.txt` (`0600`). Each entry records the
tool, target, command, start/finish time, duration, exit code, timeout/cancel
flags, artifact path, byte count, and SHA-256.

```text
evidence {action:"list"}
evidence {action:"show", id:"e3"}
evidence {action:"verify", id:"e3"}
```

`verify` recomputes the digest and reports a mismatch. Findings render the
linked evidence id, artifact, session, and entry. Evidence writes never turn a
successful command into a failed one.

## Attack surface

Command output is parsed after each evidence-worthy run. Supported inputs are
nmap XML, grepable, and normal output, URLs, HTTP `Server`/`X-Powered-By`/
`X-Generator` headers, and TLS fingerprints. The result is one `surface.json`
per engagement: hosts, aliases, ports, protocols, services, products, versions,
TLS flags, URLs, technologies, identities, hashed credential references, and
evidence ids.

```text
surface {action:"list"}
surface {action:"show", host:"app.example.test"}
```

With a configured scope, out-of-scope assets are dropped. Without scope,
observations are kept but marked out of scope. Credentials are stored as
`cred:<sha256 prefix>`; URL userinfo is stripped before the URL is stored.

## Detection as code

`detect` validates and runs a deterministic Sigma subset against local JSONL
logs or plain-text samples:

```yaml
title: Suspicious curl
id: shell-curl
level: high
detection:
  selection:
    message|contains: "curl "
  condition: selection
```

```text
detect {action:"validate", rules:"rules/"}
detect {action:"run", rules:"rules/", target:"sample.log"}
detect {action:"run", rules:"rules/", target:"sample.log", finding:false}
```

Supported selections are mappings (all fields match), lists of mappings (any
mapping matches), keyword lists, and strings. Modifiers: `contains`,
`startswith`, `endswith`, `re`, `exists`, `all`, `cased`. Conditions support
`and`, `or`, `not`, parentheses, `1 of name*`, `all of name*`, and `them`.
Unsupported syntax is reported by `validate`, not guessed. Plain text lines are
matched as `{"message": line}` plus the raw line for keyword rules.

Hits create findings by default (title `Detection: <rule>`, severity from
`level`, target = source file) and always append a detection evidence entry.
Findings dedupe by title and source. Use `finding:false` for a dry run.

## Attack-path graph

The graph is derived from the surface, findings, and evidence:

- Nodes: assets, services, technologies, identities, hashed credentials.
- Edges: identity/credential -> asset, asset -> service, service -> technology.
- Findings add risk to the matching asset and, when a port is present or implied
  by an HTTP(S) URL, the service.

```text
attack_path {action:"summary"}
attack_path {action:"paths", from:"identity:alice", to:"service:app.test:443/tcp"}
attack_path {action:"choke"}
attack_path {action:"asset", name:"app.test"}
```

Paths are ranked by summed node risk, then hop count. Choke points are ranked
by betweenness centrality, computed in code. The tool returns summaries and
query results, not a raw graph dump.

## Trust

Global config under `~/.config/localharness/` is trusted because the user wrote
it. Project `.harness/` config, profiles, skills, workflows, tools, and verify
steps are ignored unless `--trust` is passed or the TUI trust confirmation is
accepted. Trust is per session and shown in the header/footer.

`AGENTS.md` is always read as untrusted project instructions. It cannot change
scope, tools, gates, or the endpoint. A trusted project still cannot grant
security scope.

Repository content and tool output are delimited and labeled as untrusted data
in the system prompt. This reduces, but does not eliminate, prompt injection.

## Data handling

- No telemetry, analytics, or phone-home.
- Endpoint defaults to `http://127.0.0.1:8080`; non-local endpoints warn.
- Session logs and artifacts are plaintext. `--no-session` writes neither. An
  explicitly created engagement file is separate, user-owned data. Engagement
  evidence artifacts, findings, and the surface inventory remain on disk after
  the session.
- Evidence artifacts are `0600`; credentials extracted into the surface map are
  stored as SHA-256 references, never plaintext. URL userinfo is stripped from
  stored URLs.
- Diagnostic logs are metadata only: timings, tool names, token counts, error
  codes. Prompt and tool content is not logged unless `LH_DEBUG_CONTENT=1`,
  which prints a warning.
- API keys come from config or `LH_API_KEY`, are never logged, and are redacted
  from errors.
- Artifact retention defaults to 30 days (`--keep-artifacts`,
  `--clean-artifacts`).
- `--path-jail` restricts file tools to the working directory. It does not
  restrict `bash`.
