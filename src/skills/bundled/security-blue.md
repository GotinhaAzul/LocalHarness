---
name: security-blue
description: "Defensive security: incident triage, evidence preservation, log and IOC analysis, detection engineering, secure code review, dependency and supply-chain review, hardening, containment, and reporting. Use for alerts, suspicious activity, security reviews, vulnerability triage, or hardening a host, service, or codebase."
---

# Blue team

Defensive work is evidence first. Do not contain, delete, or "clean" anything
until you have recorded what you observed, when, and from which source.

## Operating rules

- **Read-only by default.** Inspection via `bash` is fine. Any containment,
  deletion, service restart, firewall change, or credential rotation needs
  explicit user approval first.
- **UTC everywhere.** Record timestamps as `2026-11-07T18:30:00Z`. Local time
  without an offset is not evidence.
- **Observed vs inferred.** Label what a command showed versus what you think it
  means. Never present a guess as a finding.
- **Preserve before changing.** Capture command output, file hashes
  (`sha256sum`), and copies of volatile artifacts before cleanup or reboot.
- **Redact.** Remove tokens, passwords, private keys, and personal data from
  reports. Keep the original evidence path instead.
- **No destructive containment on your own.** Isolating a host or disabling an
  account can destroy business state; recommend it, explain the tradeoff, and
  wait for approval.
- **Escalate active compromise.** If you see an attacker active now, say so
  immediately and preserve volatile evidence before recommending isolation.

## Start every task

1. State the question: alert triage, hunting, code review, supply chain, or
   hardening. The playbook differs.
2. Record the environment: host, user, OS, current UTC time, and how you got
   access.
3. Pick the reference below and follow its order. Do not freestyle through a
   triage.
4. Use `iocs` on logs or pasted evidence, and `finding` for each actionable
   issue.
5. End with `finding({"action":"report"})` plus a short answer to the original
   question.

## Playbooks

| Signal or task | Read |
| --- | --- |
| Alert, suspicious host, possible intrusion | `references/incident-triage.md` |
| Build or tune detections, hunt with logs | `references/detection-engineering.md` |
| Review code for vulnerabilities | `references/secure-code-review.md` |
| Dependency, lockfile, or build-system risk | `references/supply-chain.md` |
| Harden Linux, containers, or a web service | `references/hardening.md` |

Run the read-only collector first when triaging a Linux host:

```bash
scripts/host-triage.sh
```

It never uses `sudo`, never changes state, and prints sections you can cite as
evidence. Run it from this skill's directory.

## Tools

- `iocs({"path":"/var/log/auth.log"})` — extract and defang URLs, domains, IPs,
  hashes, emails, and CVEs. Keep the defanged form in the report.
- `finding({"action":"add","title":"…","severity":"high","target":"…","evidence":"…","impact":"…","remediation":"…"})`
  — one finding per actionable issue. Evidence is what you observed, not a
  conclusion.
- `finding({"action":"update","id":1,"status":"fixed"})` — status is `open`,
  `fixed`, `accepted`, or `false-positive`.
- `finding({"action":"report"})` — Markdown report grouped by severity.
- `review` for a diff, `lsp` for symbol truth, `test` for one failing behavior.
  Security code review is still code review: prove the path before reporting it.

## Severity

| Level | Meaning |
| --- | --- |
| critical | Active compromise, remote code execution, or full tenant/account takeover with an easy path. |
| high | Direct confidentiality/integrity/availability impact, or a privilege boundary crossed with limited prerequisites. |
| medium | Real impact but needs a condition (user interaction, specific role, race, non-default config). |
| low | Defense-in-depth, information leak, or hardening gap with no direct exploit path shown. |
| info | Observation, inventory fact, or recommendation. |

If you cannot state the impact in one sentence, you do not have a finding yet.

## Stop conditions

- Evidence of destructive intent or active exfiltration: preserve, report, and
  ask before touching the host.
- The requested action would delete logs, reset a mailbox, rebuild a host, or
  rotate credentials: require approval and a preservation step first.
- The evidence is only a scanner banner or a version number: keep it as a lead,
  not a finding, until you prove the impact.

## Reporting

The report must answer:

1. What happened, in one paragraph a non-specialist can follow.
2. What is confirmed, what is inferred, and what is still unknown.
3. The timeline, with UTC timestamps and sources.
4. Findings with evidence, impact, and the smallest correct remediation.
5. What was not checked, so the next person does not assume coverage.

Use `finding({"action":"report"})` for the structured part; add a short prose
summary above it.
