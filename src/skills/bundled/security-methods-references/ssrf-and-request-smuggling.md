# SSRF and HTTP request smuggling

## SSRF and filter bypass

The high-value finding is rarely "the app fetches a user URL with no checks". It
is "the app has an SSRF guard and the guard is bypassable". Show both the guard
and the bypass.

### Find the sinks and the guard

Locate server-side requests to a URL influenced by input: webhooks, avatar/image
fetch, link preview, import-from-URL, `baseURL` overrides, OAuth/OIDC discovery,
PDF/screenshot renderers. Then find the guard: allowlist, private-IP check,
scheme filter, host regex.

### Bypass patterns

- **TOCTOU via redirect.** The guard validates the initial URL, then the client
  follows a redirect to an internal target. Check whether each hop is
  re-validated and whether `maxRedirects`/`follow` is enabled.
- **DNS rebinding / resolution gap.** The guard resolves and checks the host,
  but the HTTP client re-resolves at connect time; flip the A record to
  `169.254.169.254` between check and use. The fix is to resolve once and
  connect to that IP.
- **Absolute-URL injection.** A method that takes a path also accepts an
  absolute URL, escaping the configured base to an attacker host, and the client
  sends its Authorization/bearer header along.
- **Parser confusion.** `new URL()` and the HTTP client disagree on the host:
  `http://allowed@evil/`, `http://evil#@allowed/`, backslashes, fragments,
  embedded credentials, IPv6 brackets, decimal/octal/hex IPs
  (`0x7f000001`, `2130706433`), trailing dots.
- **Scheme smuggling.** `file://`, `gopher://`, `dict://`, or `http://[::]` when
  only `https?` was assumed.

### Impact

Reaching cloud metadata (`169.254.169.254` for IAM credentials) or internal
services; leaking the client's own bearer/cookie to the attacker host; blind
internal port scanning with a timing oracle.

### Confirm and bound

- Show the guard code and the exact bypass (`file:line` for both). "There is a
  fetch" is not a finding; the bypass is.
- Demonstrate one request leaving to a canary host you control, or one internal
  URL fetch that proves reachability. Do not dump metadata credentials; prove
  access to one non-sensitive field and stop.
- Encode the request and the canary observation as `poc_steps` and run
  `finding verify`. A captured callback is the proof.

### Reject

The redirect target is re-validated per hop, resolution is pinned, and
cross-origin redirects strip credentials. Or the URL is fixed configuration, not
attacker-controlled.

## HTTP request smuggling

Smuggling exploits a disagreement between a front-end proxy and the back-end
about where one request ends and the next begins. The smuggled suffix becomes
the start of the next request.

### Timing probes first (safe)

**CL.TE** (front-end trusts Content-Length, back-end trusts Transfer-Encoding):

```http
POST / HTTP/1.1
Host: target
Content-Length: 4
Transfer-Encoding: chunked

1
A
X
```

If the back-end hangs waiting for the next chunk, CL.TE is likely.

**TE.CL** (front-end trusts TE, back-end trusts CL):

```http
POST / HTTP/1.1
Host: target
Content-Length: 6
Transfer-Encoding: chunked

0

X
```

If the back-end consumes `X` while the front-end has ended the request, TE.CL is
likely.

### Exploitation shapes

- CL.TE: smuggle a prefix that the back-end treats as the next request.
- TE.CL: smuggle a chunked request with a forged `GPOST`/`GET` line.
- H2.CL: an HTTP/2 front-end forwards a `Content-Length` that disagrees with the
  actual body; surplus bytes become the next request.
- TE.TE: obfuscate one `Transfer-Encoding` so one server ignores it —
  `xchunked`, a space before the colon, a vertical tab, duplicate headers, case
  variation, or `identity` after `chunked`.

### Confirming exploitability

- Smuggle a request for a known 404 path; if the next legitimate request gets
  that 404, the boundary is confused.
- Smuggle a request that reflects a header, or one for `/admin`, and show the
  response leaking to a subsequent request on the same connection.
- Use keep-alive and send the two requests back-to-back on one socket. `curl`
  may normalize headers; use a small Python socket script for raw bytes.
- Bound the test: one socket, one canary, no cache-poisoning payloads against
  shared production caches.

### Impact escalation

Auth bypass to internal/admin endpoints, cache poisoning, credential theft from
a following user's request body, and cross-virtual-host routing. State which one
you demonstrated.

### Reject

A properly patched front-end rejects requests with both CL and TE, or normalizes
them consistently. Many CDNs patched classic smuggling; focus on custom proxies
and older versions. If both servers agree on the boundary, there is no finding.

### Confirm

Record the raw request bytes, the timing observation, and the downstream effect
(a 404 for the next request, leaked admin content, a canary callback). Encode
them in `poc_steps` and run `finding verify`. Do not poison a shared cache to
prove the issue.

---

_Adapted from 0 (0sec-labs/0), MIT OR Apache-2.0, commit 63e057d._
_Source: skills/vulnerabilities/ssrf-bypass.yaml, request-smuggling.yaml._
