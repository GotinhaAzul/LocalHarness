---
name: security-red
description: "Authorized security testing: scoping and rules of engagement, passive and active reconnaissance, enumeration, minimal-impact vulnerability validation, evidence capture, ATT&CK mapping, and reporting. Use for labs, CTFs, and engagements with explicit written authorization. Not for unauthorized access, malware, persistence, credential theft, evasion, or denial of service."
---

# Authorized security testing

This skill is for work the user is allowed to perform: their own lab, a CTF, or
an engagement with explicit written authorization. The methodology is useful;
the authorization is what makes it legal and ethical.

## Authorization gate

Do not run active tests until the user has stated all of the following:

1. **Authorization**: who authorized the test, and the engagement/reference.
2. **Targets**: exact domains, IPs, CIDRs, apps, and accounts. "The company" is
   not a target.
3. **Window**: start and end time, including timezone.
4. **Prohibited actions**: anything off-limits (production data, destructive
   payloads, social engineering, third-party providers).
5. **Contact**: who to call if something breaks or evidence of compromise is
   found.

If any item is missing, stop active testing and ask. Passive review, code
reading, and defensive analysis can continue. A lab or CTF target the user owns
still needs its boundary stated: which host/network, and what is out of scope.

The user records scope with:

```text
/sec scope add <host> [*.domain] [CIDR]
```

The `/sec` bash gate blocks a small set of active-testing tools when no scope is
set or when an extracted target is outside it. The gate is a speed bump, not a
sandbox; do not try to evade it, and do not treat a scope entry as permission for
actions the rules of engagement prohibit.

## Hard limits

Never, even with scope:

- Deploy malware, ransomware, wipers, or a backdoor.
- Establish persistence or modify startup, cron, systemd, SSH keys, or accounts.
- Dump, crack, or exfiltrate credentials or personal data.
- Evade detection, clear logs, disable auditing, or alter timestamps.
- Run denial-of-service, stress, or destructive payloads.
- Test third-party infrastructure (cloud metadata, registries, SaaS providers)
  that is not explicitly in scope.
- Access data beyond the minimum needed to prove the finding; stop at proof.
- Attack a production system without explicit permission for that system and a
  rollback/stop plan.

If you find evidence of a real compromise, stop testing and hand off to the blue
playbook. Preserve what you saw.

## Workflow

1. **Read the rules of engagement** (`references/authorization.md`) and restate
   the scope in your own words.
2. **Passive reconnaissance** (`references/recon.md`): public DNS, certificates,
   published metadata. No packets to the target yet.
3. **Active reconnaissance and enumeration** (`references/recon.md`): only within
   scope, rate-limited, with a recorded start/end.
4. **Validate** one issue at a time (`references/validation.md`). Minimal proof,
   no data destruction, no privilege persistence.
5. **Record evidence** as you go: UTC time, command, target, request/response,
   expected vs observed. Use `iocs` for indicators and `finding` for each issue.
6. **Report** (`references/reporting.md`) and clean up any artifacts you created.

## Evidence standard

A finding needs:

- A reproducible request/command and the exact target.
- The observed response or state change, with UTC timestamps.
- The security consequence, not just the anomaly.
- The smallest remediation that removes the class of issue.

Screenshots alone are weak evidence. Prefer command output, HTTP transcripts, and
hashes of files you created. Keep secrets and personal data out of the report.

## Tool use

- `finding({"action":"add",…})` for each actionable issue; `finding({"action":"report"})`
  at the end.
- `iocs({"text":…})` to extract and defang indicators from evidence.
- `bash` for the commands in `references/recon.md` and `references/validation.md`.
- `lsp`, `test`, and `review` when the target includes source code: read the
  implementation before claiming a runtime behavior.
- `/sec scope` is user-set. Do not ask the model to set it, and do not edit the
  scope entry directly.

## Severity and ATT&CK

Use CVSS v3.1 for the base score when it helps prioritization, and always state
the prerequisites. Map the technique to MITRE ATT&CK for the blue team
(`T1190` public-facing exploitation, `T1078` valid accounts, `T1059` command
interpreter, `T1098` account manipulation, and so on). Mapping is for detection
coverage, not decoration.

## Stop conditions

- The target becomes unstable: stop, report, and wait for the owner.
- You encounter real user data: stop, do not copy it, report the exposure.
- The scope or window changes: stop and reconfirm.
- You find evidence of an active third party: preserve and hand off to blue.
- A test would require persistence, credential theft, evasion, or destruction:
  do not run it; propose a safer validation instead.

## References

| Topic | File |
| --- | --- |
| Rules of engagement and scope template | `references/authorization.md` |
| Phase plan and evidence discipline | `references/methodology.md` |
| Passive/active recon commands | `references/recon.md` |
| Minimal-impact validation per class | `references/validation.md` |
| Finding and report format | `references/reporting.md` |
| Local lab and CTF setup | `references/lab.md` |
