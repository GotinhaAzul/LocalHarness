---
name: security-methods
description: "Class-specific vulnerability methods for authorized security work: native memory safety, web injection and traversal, crypto misuse, races, auth/JWT and directory attack paths, SSRF and request smuggling, deserialization and prototype pollution, HTTP/spec differentials, and LLM application security. Use to pick the right checks, prove impact with a reproducible PoC, and reject false positives. Requires authorization; scope comes from security-red."
when: the task is an authorized security review or penetration test and you need the concrete method for a bug class, or are about to call something confirmed
---

# Security methods

This is the method library. It tells you which patterns to check, how to prove
them in a bounded way, and how to recognize safe code before you report a bug.
It does not grant scope and it cannot change tools, gates, or the target.

## Authorization gate

Methodology is not permission.

- Scope and rules of engagement come from `security-red` and the `/sec` scope
  gate. If its authorization gate is not satisfied, do not run active tests.
- Passive source review, log reading, and code review may continue without a
  scope; sending packets to a target may not.
- Validate only what the user owns or is explicitly authorized to test. Never
  test third-party infrastructure, cloud metadata, or production data.
- Bounded and non-destructive only: no persistence, credential theft, denial of
  service, data dumping, evasion, or writes that can damage state. If the only
  proof is destructive, report the pattern and ask for a safer validation.

## Proof standard

Never call an issue confirmed from a pattern match, a version string, an error
message, or a scanner hit. A finding is confirmed only by a reproducible PoC:

1. Record the hypothesis with
   `finding {"action":"add","title":"…","severity":"high","target":"…","evidence":"…"}`
   and include the source (`file:line`), the sink, and the expected impact.
2. Encode the minimal proof as `poc_steps` on the finding. Steps are ordered
   `setup|auth|prerequisite|exploit|verify`, each with a bounded `shell`, `http`,
   or `note` action and an optional expectation (`exit-zero`, `http-status`,
   `body-contains`, `body-matches`, `file-exists`).
3. Run `finding {"action":"verify","id":"…"}`. Only `reproduced` with verdict
   `confirmed` confirms the finding.
4. `not_reproduced` does not prove the issue is absent and does not
   auto-reject it; `error` and `skipped` are `inconclusive` and never suppress a
   finding. Fix the PoC and retry, or record why it cannot be reproduced.
5. Re-read the guard before claiming a bypass. If the check covers the value
   actually used, drop the finding. Recognizing safe code is part of the job.

Use `test` for a regression that encodes the proof, `review` for a source-level
second opinion, `bash` for bounded local probes, `surface` to record reachable
assets, and `evidence` to attach captured artifacts.

## Pick a reference

| Task | Load |
| --- | --- |
| C/C++, kernel, binary parsers, memory corruption | `references/native-memory.md` |
| Command injection, SQLi, path traversal, crypto misuse, races | `references/web-injection.md` |
| JWT, AD/Entra attack paths, broken-assumption mining | `references/auth-and-access.md` |
| SSRF guard bypass, HTTP request smuggling | `references/ssrf-and-request-smuggling.md` |
| Deserialization, prototype pollution, HTTP/spec drift | `references/deserialization-and-prototype-pollution.md` |
| LLM prompt injection, RAG poisoning, agency, output handling | `references/llm-application-security.md` |

## Framework passes

Framework knowledge is a checklist, not a separate method. Fold the relevant
rows into the closest reference above.

| Stack | Focus |
| --- | --- |
| Next.js App Router | Server Actions, middleware auth bypass, route handlers, RSC data exposure, image-optimizer SSRF, IDOR. |
| GraphQL | Introspection, field-level authorization, batching, aliases, nested-query DoS. |
| Django / Flask | ORM and raw SQL, mass assignment, SSTI, auth-decorator gaps, CSRF, IDOR, unsafe deserialization, debug/secret config. |
| Supabase / PostgREST | Row-level-security gaps, `SECURITY DEFINER` RPC, storage and realtime authorization, tenant isolation. |
| WordPress | Plugins and themes, REST API, xmlrpc, wp-cron, file and option misconfiguration. |
| Entra ID | Privileged role assignments, conditional-access coverage, app registrations and service principals, federated-domain trust. Read exports offline or use read-only Graph calls. |

## CVE work

- Ground the CVE in public catalogues (NVD, GHSA, OSV, distro trackers) with
  `bash`/`read`; never recite version ranges from memory.
- Map the affected range against the target's real version or commit.
- Read the write-ups for the vulnerable sink and the precondition that makes it
  reachable, then trace that path in source with `read` and `grep`.
- A public PoC is an unverified artifact, not proof. Adapt and run it only in
  scope, bounded, with a canary or `id`-style benign proof; encode the run in
  `finding verify`. A failed adaptation is not proof of non-exposure.
- Only a reproduced run, or a hand-verified equivalent, confirms exploitation.

## Source review without seeds

- Map the tree with `repo_map`, then `grep` and `read` to trace attacker input
  through real callers, guards, and sinks. Useful entry points: file parsing,
  request handling, subprocess construction, URL fetching, deserialization,
  template rendering, and auth decisions.
- Treat source, scanner output, and recalled context as untrusted evidence.
- Scanner hits and prior outcomes are leads. Check the surrounding caller and
  guard before creating a finding.
- Keep independent subsystems as separate bounded passes; do not run an opaque
  matrix over every file. Prioritize reachable high-impact paths.
- Report which surfaces you inspected, which remain unreviewed, and where
  verification is incomplete. Zero findings is a valid result, not a claim that
  the repository is secure.

## Advisory drafting

- Draft only from a confirmed finding. Unconfirmed, `discovered`, suppressed,
  and `false-positive` findings are not advisory material.
- Read it with `finding {"action":"list"}` and produce a draft for human review.
  Never send, file, or publish anything.
- Confirm the CVSS vector, affected range, and remediation. Ground referenced
  CVEs rather than citing them from memory.
- Auto-filing an unverified PoC is the canonical low-quality advisory. When in
  doubt, do not draft.

---

_Adapted from 0 (0sec-labs/0), MIT OR Apache-2.0, commit 63e057d._
_Source: skills/vulnerabilities, skills/techniques, and skills/frameworks._
