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
impact/remediation. `add` auto-attaches the last tool result as evidence when
none is given.

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
  explicitly created engagement file is separate, user-owned data.
- Diagnostic logs are metadata only: timings, tool names, token counts, error
  codes. Prompt and tool content is not logged unless `LH_DEBUG_CONTENT=1`,
  which prints a warning.
- API keys come from config or `LH_API_KEY`, are never logged, and are redacted
  from errors.
- Artifact retention defaults to 30 days (`--keep-artifacts`,
  `--clean-artifacts`).
- `--path-jail` restricts file tools to the working directory. It does not
  restrict `bash`.
