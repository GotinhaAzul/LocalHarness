# Rules of engagement

The rules of engagement are the contract. If they are incomplete, the test is
not ready to start.

## Required fields

| Field | What to record |
| --- | --- |
| Engagement | Name, reference/ticket, authorizer, date |
| Targets | Exact domains, IPs, CIDRs, applications, accounts, environments |
| Exclusions | Third-party hosts, shared infrastructure, production data, specific paths |
| Window | Start/end in UTC, blackout periods |
| Permitted techniques | Network scanning, web testing, authenticated testing, code review |
| Prohibited techniques | DoS, destructive payloads, social engineering, physical, persistence |
| Data handling | What may be accessed/copied, encryption, deletion date |
| Contacts | Technical owner, emergency contact, escalation path |
| Stop conditions | What ends the test immediately |
| Reporting | Recipient, deadline, severity scale, disclosure policy |

## Scope examples

Good:

```text
Targets:
  10.20.0.0/24 (staging only; 10.20.0.10 is the database and is excluded)
  api.staging.example.com
  app.staging.example.com (test tenant only)
Window: 2026-11-10T09:00Z to 2026-11-12T17:00Z
Prohibited: DoS, file upload to production buckets, modifying IAM, persistence
Contact: #sec-eng, +1-555-0100
```

Bad:

```text
Test example.com and anything related. Be careful.
```

The second is not authorization for the third-party services example.com uses,
for customer data, or for destructive tests.

## Restate before acting

Write back to the user:

- "I will test X, Y, Z between times A and B."
- "I will not touch N, M."
- "If I find real user data or a real compromise, I stop and report."

This catches misunderstandings before packets are sent.

## Prohibited by default

Even when the client says "be thorough", these need explicit, separate approval:

- Denial of service, load tests, or resource exhaustion.
- Destructive payloads, file deletion, database writes outside test records.
- Persistence, new accounts, SSH keys, scheduled tasks, or implants.
- Credential dumping, password cracking beyond a lab sample, or token theft.
- Detection evasion, log modification, or anti-forensics.
- Social engineering, phishing, or physical access.
- Testing third-party cloud metadata, package registries, or SaaS providers.

If approval is ambiguous, treat it as denied.

## Stop conditions

Stop immediately and notify the contact when:

- A target becomes unavailable or unstable.
- You access real personal, financial, or health data.
- You find evidence of an active compromise by someone else.
- You accidentally exceed scope.
- A test would require a prohibited technique to prove impact.

Preserve the minimum evidence needed, do not copy the data, and wait for
instructions.

## End of engagement

- Remove any test accounts, files, callbacks, and local listeners you created.
- Return or delete collected data per the agreement.
- Deliver the report and the evidence index.
- State what was not tested, so coverage is not overstated.
