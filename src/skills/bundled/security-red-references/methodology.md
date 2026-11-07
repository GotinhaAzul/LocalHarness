# Methodology

The engagement is a sequence of questions, each answered with evidence. Do not
jump to exploitation because a tool printed a banner.

## Phase plan

| Phase | Question | Output |
| --- | --- | --- |
| 0. Authorization | Am I allowed, on what, when? | Restated scope and rules of engagement |
| 1. Passive recon | What is publicly known? | Asset list, technologies, exposure hypotheses |
| 2. Active recon | What responds, and how? | Reachable hosts, ports, services, versions |
| 3. Enumeration | What does each service expose? | Endpoints, roles, parameters, auth flows |
| 4. Validation | Is the hypothesis exploitable with minimal impact? | Reproducible proof, one issue per finding |
| 5. Impact | What can an attacker actually do? | Confidentiality/integrity/availability consequence |
| 6. Report | What should the owner fix first? | Findings, evidence, remediation, coverage gaps |

This maps to PTES for infrastructure and OWASP WSTG for web applications. Use
the checklist for coverage, not as a reason to run every test.

## Timeboxing and rate

- Give each phase a timebox. If a hypothesis is not confirmed in the box, record
  it as unconfirmed and move on.
- Rate-limit active requests. A staging app should not see more traffic from you
  than from its normal users.
- Stop when the evidence is sufficient. Repeating a successful proof adds risk,
  not confidence.

## Evidence log

Keep a running log from the first command:

| UTC | Target | Command/request | Result | Hypothesis |
| --- | --- | --- | --- | --- |
| 2026-11-10T09:04:00Z | api.staging.example.com | `curl -i /v1/users/1` | 200 with another tenant's user | IDOR |
| 2026-11-10T09:06:00Z | api.staging.example.com | same with test account B | 403 | hypothesis rejected |

Negative results are valuable: they show what was tested. Do not delete them
from the log.

## Asset and hypothesis tracking

- Maintain a target inventory: host, IP, service, version, auth required, owner.
- For each target, list hypotheses: missing authz, injection, SSRF, file access,
  default credentials, exposed admin, outdated component with a reachable path.
- Rank by likely impact and cost to validate.
- Validate one hypothesis at a time; do not chain unrelated tests into one
  ambiguous result.

## Minimal-impact validation

- Use test accounts and test records created for the engagement.
- Stop at the first proof: one record read, one marker reflected, one callback.
- Never dump a table, list a directory, or download user data to "prove" access.
- Do not upload a webshell, create an admin user, or persist access.
- If proof requires destructive action, report the pattern and ask for a safer
  validation.

## Chaining

A chain is only a finding when each link is evidenced. If you cannot show the
first privilege boundary crossing, the rest is a hypothesis. Report the
confirmed link and list the unverified links separately.

## Handoff to blue

For every confirmed technique, provide:

- The observable telemetry: process, command line, HTTP request, auth event,
  network flow.
- The ATT&CK technique.
- A detection idea and the fields it needs.

This is often more valuable to the client than the exploit itself.

## Coverage statement

End with what was tested and what was not:

- Hosts and ports actually reached.
- Roles and tenants exercised.
- Tests skipped because they were prohibited or unsafe.
- Areas that need a follow-up engagement.

Never imply full coverage from a partial test.
