# Blue team workflow

Defensive work: triage, detection engineering, hardening, secure code review,
and supply-chain review. Evidence first: do not contain, delete, restart, or
rotate anything without explicit approval.

Load the `security-blue` skill in Phase 0 and follow the reference that matches
the task: `incident-triage`, `detection-engineering`, `hardening`,
`secure-code-review`, or `supply-chain`.

## Phase 0 — Scope and evidence

1. State the question and the environment: host, service, repository, log
   source, current UTC time, and how the data was collected.
2. Preserve before changing: command output, file hashes (`sha256sum`), and
   copies of volatile artifacts. Record the original path instead of pasting
   secrets into the conversation.
3. Say what the task is: alert triage, hunting, code review, supply chain, or
   hardening. The playbook differs.

## Phase 1 — Triage

1. Separate observed from inferred and label every claim.
2. Build a UTC timeline from logs and artifacts. Use `iocs` to extract and
   defang indicators, and record them on the engagement.
3. Name the affected assets, the blast radius, and what you still do not know.
   If there is an active compromise, say so immediately and preserve volatile
   evidence before recommending isolation.

## Phase 2 — Detection

1. Write a Sigma rule for the behavior, not for one sample, and validate it with
   `detect validate`. Unsupported syntax is reported, never guessed.
2. Run it against the available logs with `detect`. Check a hit case and a
   benign case; a rule that fires on everything is not a detection.
3. Record coverage gaps and the data source each rule needs. Absence of a hit is
   not absence of activity; say what the logs do not cover.

## Phase 3 — Hardening, code, and supply chain

1. Prefer configuration and defaults over code changes; prefer one root-cause
   fix over many special cases.
2. For code, read before edit, keep the change scoped, and add a regression
   test that fails before the fix and passes after it.
3. Check the supply chain: lockfiles, dependency freshness, CI token
   permissions, secrets in history, and build steps that execute remote content.
4. Run `verify` or the project's check, then `review`, on any diff.

## Phase 4 — Report

Answer the original question first, then report findings grouped by severity
with evidence, impact, remediation, owner, and residual risk. State what was not
examined. Zero findings is a valid result.

## Stop conditions

Stop and ask before:

- containment, isolation, deletion, service restart, firewall change, or
  credential rotation;
- any write to a production host or a shared system;
- publishing indicators or evidence that may contain personal data.
