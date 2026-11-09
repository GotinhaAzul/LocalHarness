# Authentication, access, and directory attack paths

Three related problems: token verification that can be forged, directory graphs
that hide a short path to Tier-0, and implicit assumptions that one code path
breaks. All are high impact; all need a concrete reproduced path, not a graph
edge or a decoded token alone.

## JWT attacks

### Decode and inventory

Decode both segments without verification (base64url):

```bash
cut -d. -f1 token.txt | tr '_-' '/+' | base64 -d 2>/dev/null
```

Record `alg`, `kid`, `jku`, `x5u`, `typ`, and the claims (`sub`, `role`,
`admin`, `iss`, `exp`, `iat`, `scope`). The algorithm and key-selection headers
are the attack surface.

### Algorithm `none`

Set the header to `{"alg":"none","typ":"JWT"}` and remove the signature:
`base64url(header).base64url(payload).` Variations: `None`, `NONE`, `nOnE`, an
empty segment, or omitting `alg`. Submit the forged token; a library configured
to allow `none` accepts it.

### Algorithm confusion (RS256 -> HS256)

If the server verifies with an asymmetric public key but the library accepts
HS256, sign a forged token with the public key as the HMAC secret. Obtain the
key from `/.well-known/jwks.json`, `/api/keys`, an exposed `public.pem`, or the
TLS certificate. The server's HMAC verification matches because you signed with
the key it uses to verify.

### `kid` injection

`kid` selects a key; if it feeds a path or query:

- path traversal: `"kid":"../../../../dev/null"`, signed with an empty secret;
- SQL injection: `"kid":"1' UNION SELECT 'secret-key' -- -"`, signed with
  `secret-key`;
- command injection: `"kid":"key1|/usr/bin/id"`.

### `jku` / `x5u` injection

Generate your own keypair, host a JWKS with your public key, set
`"jku":"https://attacker.example/jwks.json"`, and sign with your private key. If
the server fetches it without an allowlist, verification succeeds. An internal
`jku` URL is also SSRF.

### Claim manipulation and weak secrets

After a signing bypass, escalate claims (`role`, `admin`, `sub`, `scope`, remove
`exp`). For HS256, try common secrets with a bounded offline wordlist. Check
whether logout invalidates tokens server-side, and whether a token from tenant A
is accepted by tenant B.

### Confirm

Forge one token, submit it to one endpoint, and show the response that only an
authenticated/privileged caller should receive. Keep the proof to your test
account. Encode the forged token, request, and expected status/body as
`poc_steps` and run `finding verify`. Do not use a real user's token.

## Active Directory attack paths (offline graph)

When a BloodHound CE / SharpHound JSON collection is on disk, analyze the export
offline; do not re-collect over the network during a review.

### When this applies

A directory of `*.json` collector output or a single BloodHound export, and a
question about reaching Domain Admin, tiering violations, or dangerous
ACL/permission chains.

### How to analyze it

- Parse the JSON with `bash` and `jq` into principals, group memberships, ACEs,
  delegation flags, sessions, and admin rights. Keep outputs as evidence.
- Build the reachability question explicitly: which concrete edge lets principal
  A reach principal B, and what precondition (membership, ACE, delegation,
  credential) does it require?
- If the engagement graph already models those identities and assets, use
  `attack_path {"action":"summary"}` then
  `attack_path {"action":"paths","from":"…","to":"…"}` to rank paths; otherwise
  keep the analysis in a local report.
- Check whether the export actually collected memberships and ACLs. An empty or
  partial result is not a clean domain; paths that depend on missing collections
  cannot be computed.

### Reading paths

Prioritize short paths to Tier-0: a two-hop path to Domain Admin is materially
different from a six-hop path. For each claimed path, confirm the concrete edge
(group membership, ACE, delegation flag) in the source export and record it in
the finding evidence.

## Entra ID attack paths (offline or read-only Graph)

Use an AzureHound export when one is on disk; otherwise use read-only Graph calls
only if the engagement authorizes it and a token is available.

### What to look for

Paths to Global Administrator, service-principal escalation, consent-grant
abuse, owner chains, guest escalation, privileged role assignments,
conditional-access gaps, app registrations and their credentials, and federated
domain trust.

### How to analyze it

- Parse the export with `bash`/`jq`, or query Graph read-only. Start from
  principals already under test control to make the path question concrete.
- Check `relationshipsCollected`. If membership/ownership was not collected,
  paths that depend on it cannot be computed; recommend a fuller collection
  rather than declaring the tenant safe.
- Keep posture (configuration hygiene) separate from reachability/escalation.
  A misconfiguration is a lead; a path needs the concrete edge.

Attribute escalation only when you can trace principal -> role/ownership/consent
edge -> target privilege. Record the edge and the export path as evidence.

## Assumption mining

Many real bugs are an implicit assumption that holds everywhere except one path.
Hunt for statements such as:

- "this value is never null or empty here";
- "this input was validated or sanitized upstream";
- "the caller already holds this lock or checked permission";
- "this length/size always fits and this index is always in range";
- "these two representations can never disagree".

For each assumption, look for a reachable path where it is not guaranteed: an
entry point that skips the upstream validation, a second caller that does not
hold the lock, an attacker-controlled size, or a state change between check and
use.

Mined assumptions are **leads, not findings**. Verify by tracing the concrete
path from a real entry point to the site and confirming the assumption is
genuinely unenforced there, not enforced by a wrapper or earlier guard. Then
write the PoC and run `finding verify`.

## Failure modes to avoid

- Reporting a decoded token, a graph edge, or a documented assumption as a
  confirmed vulnerability without a reachable path.
- Testing a real user's session or credentials instead of a test account.
- Reporting an empty directory graph as proof the domain is clean.
- Treating a version or configuration mismatch as exploitation.

---

_Adapted from 0 (0sec-labs/0), MIT OR Apache-2.0, commit 63e057d._
_Source: skills/techniques/jwt-attacks.yaml, ad-attack-paths.yaml, entra-attack-paths.yaml, assumption-mining.yaml._
