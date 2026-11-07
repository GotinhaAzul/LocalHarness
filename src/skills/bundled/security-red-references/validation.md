# Minimal-impact validation

The goal is the smallest reproducible proof that the security boundary is
broken. Stop at proof. Do not turn a read primitive into a data dump, and never
turn a proof into persistence.

## Rules

- Use accounts and records created for the engagement.
- One request or one small sequence per hypothesis.
- No destructive writes, no deletes, no mass enumeration.
- No credential theft, no session hijacking beyond your own test session.
- No webshell, implant, new admin user, or scheduled task.
- If the only proof is destructive, report the pattern and ask for a safer
  validation.

## Access control

### IDOR / BOLA / tenant isolation

1. Create two test accounts in different tenants/roles.
2. Capture an object ID owned by A.
3. Request it as B.
4. Record: request, response code, and whether B received A's object.

Proof is one object, not a list. If the response contains personal data, stop,
do not copy it, and report the exposure.

### Missing function-level authorization

- Call an admin endpoint with a normal user's session.
- Record the request and the response.
- Do not perform the admin action beyond the minimum that demonstrates access.

### Forced browsing

- Request a page or API route that the UI hides.
- A 200 with sensitive content is the proof; do not crawl the whole admin area.

## Injection

### SQL injection

Use a benign boolean/error pair on a test parameter:

```text
id=1' AND '1'='1
id=1' AND '1'='2
```

- A consistent true/false difference or a database error is a lead; confirm with
  a controlled, non-extracting payload.
- Do not use `UNION SELECT` to dump tables, read files, or write files.
- Record the exact parameter, encoding, and response difference.

### Command injection

- Prefer a harmless arithmetic or sleep marker in a test environment.
- Do not read credentials, spawn a shell, or establish a callback beyond the
  minimum needed to prove execution.
- If the target is production, do not run the payload without explicit approval.

### Template injection

- Use an arithmetic expression that renders a unique result in a test template.
- Do not attempt filesystem or process access.

### Header/CRLF and log injection

- Use a marker that would create a visible extra header or log line.
- Do not inject into downstream systems or forge a real alert.

## Cross-site scripting

- Use `alert(1)` or a DOM marker in your own browser session and a test record.
- Stored XSS: create one test record, confirm it executes for the same test
  account, then stop.
- Do not steal cookies, tokens, or another user's session.
- Record the exact input, context, and execution point.

## SSRF

- Point the server at a listener you control (a local test service or a
  collaborator you own).
- Record the callback: source IP, path, headers, time.
- Do not query cloud metadata, internal admin APIs, or arbitrary internal hosts.
- If the callback proves the request, stop; do not use the SSRF to read data.

## Path traversal and file access

- Create a harmless marker file in a location the application should not reach,
  or read a known non-sensitive file such as the application's own test fixture.
- Do not read `/etc/shadow`, private keys, or user data.
- Record the parameter and the returned content; stop after one file.

## File upload

- Upload a harmless text file or an image with a marker.
- Check where it is stored, whether it is executable, and whether it is served
  with a dangerous content type.
- Do not upload a webshell or a polyglot payload.

## Authentication and session

- Test default credentials only on lab/test accounts and only if the rules
  allow it.
- Do not brute-force production accounts. If lockout behavior must be tested,
  use a dedicated test account and a tiny number of attempts.
- For session issues, use your own session: fixation, missing rotation, weak
  cookie flags. Do not hijack another user's session.

## Business logic and races

- Use test carts, coupons, balances, or invites created for the engagement.
- For races, use a small number of parallel requests on a test record.
- Stop as soon as the invariant is broken; do not repeat for volume.

## Cryptography and TLS

- Use a configuration scanner (`testssl.sh` or equivalent) in safe mode.
- Do not downgrade, intercept, or attack other users' traffic.
- Report the configuration and the affected endpoint; a missing header or old
  protocol is not automatically high severity.

## Evidence capture

For each validation, keep:

- UTC start/end.
- Exact request or command, including headers and body.
- Response status, relevant headers, and the minimal body excerpt.
- A hash of any file you created or downloaded.
- The expected behavior vs the observed behavior.

Redact secrets and personal data before putting evidence in the report. Keep the
original in the evidence store with restricted access.

## Severity

State prerequisites explicitly:

- Pre-auth or post-auth?
- Which role/tenant?
- User interaction?
- Configuration-dependent?
- Does it require a test-only feature?

A post-auth admin-only issue is not critical merely because the impact text says
"full access".
