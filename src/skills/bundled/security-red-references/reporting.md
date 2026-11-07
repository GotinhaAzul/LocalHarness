# Reporting

The report is the deliverable. A finding that cannot be reproduced from the
report is not a finding.

## Structure

1. **Executive summary** — what was tested, the overall risk, and the three
   actions that matter most. No jargon without a definition.
2. **Scope and rules of engagement** — targets, window, exclusions, permitted
   and prohibited techniques.
3. **Methodology** — phases, tools, accounts, rate limits, what was not tested.
4. **Findings** — ordered by severity, each with evidence and remediation.
5. **Attack-path summary** — how confirmed findings chain, with each link marked
   confirmed or unverified.
6. **Detection opportunities** — telemetry, ATT&CK technique, a concrete
   detection idea for the blue team.
7. **Evidence index** — file names, hashes, and where the originals are stored.
8. **Retest plan** — the exact checks that prove each fix.

## Finding template

```markdown
### F1 — Title

- Severity: High (CVSS 8.1: AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:N)
- Target: https://api.staging.example.com/v1/orders/{id}
- Status: open
- Affected component: orders API, version 2.14.0

**Description**
One paragraph: the security boundary that is broken and the attacker's
preconditions.

**Evidence**
UTC: 2026-11-10T09:14:00Z
Request: GET /v1/orders/1234 with account B's bearer token
Response: 200, body contained account A's order (redacted)
Repeat: 2 consecutive requests, same result

**Impact**
Account B can read other tenants' order data. One object was accessed; no bulk
enumeration was performed.

**Reproduction**
1. Create test accounts A and B.
2. Create order 1234 under A.
3. Request it with B's token.
4. Observe 200 instead of 403.

**Remediation**
Authorize the order against the caller's tenant inside the repository layer,
not only in the controller. Add a regression test for cross-tenant access.

**Detection**
ATT&CK T1078 (valid accounts). Alert on object access where the resource tenant
does not match the token tenant.
```

## CVSS

Use CVSS v3.1 when the client uses it. State the vector and the assumptions:

- `AV` network vs local.
- `PR` none/low/high — the role required.
- `UI` none/required.
- `C/I/A` impact.

Do not inflate a score with a hypothetical chain. If the chain is unconfirmed,
report the confirmed link at its real severity and list the chain separately.

## Good vs bad wording

- Bad: "The application is vulnerable to IDOR."
- Good: "Authenticated tenant B can read tenant A's order by changing the id in
  `GET /v1/orders/{id}`; the API returns 200 and the full order body."
- Bad: "Critical RCE."
- Good: "An unauthenticated user can run `id` through the `q` parameter of
  `/search`; the command output is returned in the response."

## False positives and unconfirmed

List them explicitly. A false positive that was investigated and ruled out is
useful coverage evidence. An unconfirmed hypothesis belongs in an appendix, not
in the findings list.

## Data handling

- Redact tokens, passwords, personal data, and customer identifiers.
- Keep raw evidence encrypted and access-controlled; state the retention and
  deletion date.
- Do not include a working exploit payload that is more powerful than the proof
  requires.

## Retest

For each finding, specify:

- The request/command to repeat.
- The expected fixed response.
- Any regression test that should exist in the codebase.

A fix without a retest is an assumption.
