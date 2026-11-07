# Secure code review

A vulnerability is a path from attacker-controlled input to a security-relevant
effect without a correct control in between. Review the path, not the keyword.

## Method

1. **Map the entry points**: HTTP handlers, CLI arguments, queue consumers,
   file parsers, webhooks, IPC, environment variables, database rows.
2. **Mark trust boundaries**: where data crosses from untrusted to trusted, and
   which identity/tenant owns it.
3. **Find the sinks**: SQL, shell, template, deserialization, filesystem, HTTP
   client, redirect, HTML, logs, crypto, authorization decisions.
4. **Trace the control**: validation, parameterization, encoding, allowlist,
   signature check, authorization check. Quote the line that enforces it.
5. **Prove impact**: write a failing test or a local reproduction. A code smell
   without a path is a lead, not a finding.
6. **Report** with file:line, preconditions, evidence, impact, and a fix that
   removes the class of bug, not just the example.

Use `lsp` for definitions/references, `read` for the surrounding code, `review`
for the diff, and `test` to prove the behavior. Do not report a vulnerability
from a single grep hit.

## Checklist

### Authorization and identity

- Is every object access checked against the caller's tenant/user, not just
  "logged in"?
- Can an id, slug, or filename be swapped (IDOR/BOLA)?
- Are admin routes protected on the server, not only hidden in the UI?
- Are tokens scoped, expiring, audience-checked, and revocable?
- Is impersonation or "act as" audited and limited?

### Injection

- SQL: parameterized queries; no string concatenation of identifiers without an
  allowlist.
- Shell: argument arrays, no `shell=True`/`exec` with input; quote and allowlist.
- Template: auto-escaping on; no user-controlled template source.
- LDAP/NoSQL/XPath: parameterization or strict escaping.
- Header/CRLF: reject newlines in values written to headers.
- Log injection: encode newlines if logs are parsed downstream.

### Web and browser

- XSS: contextual output encoding; dangerous sinks (`innerHTML`, `dangerouslySetInnerHTML`,
  `document.write`) with a sanitizer, not a regex.
- CSRF: SameSite plus a token for state-changing requests; not GET.
- SSRF: allowlist schemes/hosts, block link-local/metadata, re-resolve DNS,
  disable redirects or re-check each hop.
- Open redirect: allowlist paths/hosts, not `startsWith`.
- File upload: extension and content type, size limits, storage outside the web
  root, random names, no execution.
- Path traversal: canonicalize, then verify the result is inside the intended
  root; reject absolute paths and `..`.
- Deserialization: never deserialize untrusted data with a format that can
  construct objects (`pickle`, Java serialization, YAML unsafe load).
- XXE: disable external entities and DTDs.

### Crypto and secrets

- No homemade crypto; use the platform library with current parameters.
- Password hashing: Argon2id/bcrypt/scrypt with a per-user salt and sane cost.
- Randomness: CSPRNG for tokens; not `Math.random`/`random.random`.
- Comparison: constant-time for secrets and MACs.
- Secrets: no keys in source, logs, URLs, error pages, or client bundles.
- TLS: verify certificates; do not disable verification to "make it work".

### Logic, state, and availability

- Race conditions on balance, quota, coupon, invite, or one-time token paths.
- Mass assignment: bind only allowed fields.
- Rate limits and lockouts on auth, reset, OTP, and expensive endpoints.
- Replay protection on signed requests and webhooks.
- Resource limits on uploads, regexes, JSON depth, XML size, and pagination.
- Error handling that leaks stack traces, queries, or internal hosts.

## Language notes

- **JS/TS**: prototype pollution (`__proto__`, merge functions), `eval`/`Function`,
  `child_process` with shell, path joins, SSRFs in fetch wrappers, JWT `none`
  or missing algorithm pinning.
- **Python**: `pickle`, `yaml.load`, `subprocess` with `shell=True`, Jinja2
  `|safe`, `os.path.join` with absolute input, Django `raw()`/`extra()`.
- **Go**: `text/template` vs `html/template`, `exec.Command` with `sh -c`,
  `filepath.Join` traversal, unchecked `http.Client` redirects, `InsecureSkipVerify`.
- **Rust**: `unsafe`, `Command` argument handling, path normalization, integer
  casts, `serde` untagged enums with attacker-controlled shapes.
- **C/C++**: bounds, integer overflow, format strings, use-after-free, signedness;
  treat compiler warnings as findings until disproven.

## Severity

- **critical**: pre-auth RCE, auth bypass to admin, mass data access.
- **high**: authenticated RCE, cross-tenant data access, stored XSS in an admin
  surface, credential theft.
- **medium**: reflected XSS with interaction, CSRF on a meaningful action,
  limited SSRF, information disclosure with a clear path.
- **low**: hardening gap, verbose error, missing header with no demonstrated
  impact.

State the prerequisites. "Requires an authenticated admin" changes severity.

## Remediation

Prefer the fix that removes the class:

- Parameterize instead of escaping one query.
- Centralize authorization instead of adding one `if`.
- Canonicalize and allowlist instead of blocking `../` strings.
- Use a safe parser instead of filtering payloads.

Add a regression test at the seam that failed. A fix without a test is a hope.
