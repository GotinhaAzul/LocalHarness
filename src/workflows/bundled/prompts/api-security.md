# API security workflow

A concrete playbook for an authorized API security review. Work one phase at a
time. The gates in code still apply: scope, read-before-edit, budgets, and the
settle gate are not suspended by this prompt.

## Phase 0 — Authorization and surface

1. State the authorization basis out loud: who authorized the review, the exact
   targets (hosts, routes, accounts), the window, prohibited actions, and the
   contact. If any of these are missing, stop active testing and ask. Passive
   source review may continue.
2. Confirm the targets are in scope with the `/sec` scope gate; do not route
   around the gate.
3. Map the surface: routes, methods, auth requirements, data objects, and
   trust boundaries. Record reachable hosts and services with `surface` and
   inspect the source entry points with `grep`/`read`.
4. Identify the defaults that apply before any custom code: framework version,
   middleware order, CORS, cookie flags, body-size limits, rate limits, error
   verbosity, health/metrics/debug endpoints, and cloud metadata reachability.

Output of this phase: a short table of endpoint -> authentication ->
authorization decision -> data object. No findings yet.

## Phase 1 — Defaults and configuration

Check the failures that do not need a clever payload:

- Authentication required on every non-public route, including alternate verbs
  and trailing-slash variants.
- Authorization enforced server-side per object and per function, not just in
  the UI or a client-side route guard.
- CORS does not combine `*` with credentials and does not reflect arbitrary
  origins.
- Cookies are `HttpOnly`, `Secure`, and `SameSite` where appropriate.
- Debug, metrics, admin, and internal endpoints are not exposed by default.
- Error responses do not leak stack traces, SQL, secrets, or internal hosts.
- Body-size, pagination, and rate limits exist and bound expensive operations.

Record each gap as a lead; do not call it confirmed until you show the
security-relevant default and its reachable consequence.

## Phase 2 — Input handling and authorization

Trace attacker-controlled data to dangerous sinks. Use `security-methods`:

- SQL/NoSQL/command/template injection, including identifier-position SQL.
- Path traversal and file write, especially archive extraction.
- SSRF, including guard bypass by redirect, DNS rebinding, and absolute-URL
  injection.
- Deserialization and prototype pollution where the stack uses them.
- Mass assignment and over-posting of privilege fields.
- IDOR/BOLA and missing function-level authorization with two test accounts.
- Broken object ownership at the tenant boundary.

For each candidate, identify the guard and ask whether it covers the value
actually used. Correctly parameterized queries, resolved-then-prefix-checked
paths, and per-object ownership checks are not findings.

## Phase 3 — Bounded proof of vulnerability

Prove the smallest reproducible path. Non-destructive only.

1. Record the hypothesis with `finding {"action":"add"}`, including source
   `file:line`, sink, expected impact, and affected target.
2. Build `poc_steps` with ordered `setup`, `auth`, `prerequisite`, `exploit`,
   and `verify` steps. Use only endpoints and accounts created for this review.
3. Keep the proof bounded: one object, one record, a canary value, a short
   timeout, and no data dump. Never use a real user's token or data. Never
   perform persistence, destructive writes, or denial of service.
4. Run `finding {"action":"verify","id":"…"}`. Only verdict `confirmed` with
   result `reproduced` confirms. `not_reproduced` is not proof of absence;
   `error`/`skipped` are inconclusive.
5. Attach the captured request/response or command output with `evidence`.

If the only available proof is destructive, stop and report the pattern
instead.

## Phase 4 — Scoped fix and regression

For a confirmed issue:

1. Fix the root cause at the shared boundary (parser, query builder, fetcher,
   authorization middleware), not just the single route.
2. Keep the change scoped. Do not refactor unrelated code or add telemetry and
   abstractions "while here".
3. Add a regression test with `test` that fails before the fix and passes after
   it. Encode the same minimal case used in the PoC.
4. Run `review` on the diff for a second pass; verify the fix does not rely on a
   blocklist or client-side check.
5. Update the finding with
   `finding {"action":"update","id":"…","status":"fixed"}` and note the commit.

## Phase 5 — Report

End with `finding {"action":"report"}`. Group by severity, state impact in one
sentence per finding, list the evidence, and name any surface you did not
review. Zero findings is a valid result; do not manufacture findings to fill the
report.

## Stop conditions

Stop and ask the user if:

- scope, authorization, or contact information is missing;
- the proof would need destructive state, real user data, or third-party
  infrastructure;
- you see evidence of an active compromise unrelated to the review;
- a change would touch code outside the review's ownership.
