---
name: incident-triage
description: "Triage an incident: UTC timeline, separate symptom from cause, check recent changes, mitigate before root cause, keep evidence. Use when an alert fires or a service is down."
---

# Incident triage

An incident is a race between understanding and impact. Stabilize first, explain
second. Preserve evidence while you do it.

## 1. State the incident

Record, in UTC:

- what is broken, for whom, and since when;
- the user-visible symptom and its blast radius;
- the current severity and who is incident lead;
- what has already been tried.

If the report is vague, ask one closed question (which endpoint, which customer,
which host) instead of starting a broad hunt. Do not guess severity upward or
downward without evidence.

## 2. Timeline before theory

Build a timeline from logs, metrics, deploys, and messages. Every entry needs a
timestamp, a source, and a fact marked observed or inferred. Start with:

- the first failing signal;
- the last known good signal;
- deployments, config changes, migrations, certificate or credential changes;
- dependency and infrastructure events;
- the mitigation attempts and their effect.

Use `bash` for read-only inspection and `grep` to narrow logs. Preserve raw
output before rotating or deleting anything; hash or copy volatile evidence.

## 3. Symptom vs cause

Write both explicitly:

- **Symptom**: the error users see, e.g. "checkout returns 503 since 14:02Z".
- **Hypothesis**: the mechanism you think causes it, e.g. "connection pool
  exhausted after the 14:00 deploy raised per-request fanout".

Do not report a hypothesis as the cause. Look for a change that lines up in
time, and for a mechanism that explains the full symptom, not part of it.

## 4. Recent changes first

Check what changed before the first bad signal:

- deploy and release history (`git log`, CI history);
- config, feature flags, and secrets rotations;
- database migrations and schema changes;
- traffic shape and capacity (new client, retry storm, batch job);
- upstream and downstream dependency status.

A recent change is a lead, not a verdict. Roll it back only if the rollback is
safe and reversible.

## 5. Mitigate before root cause

Choose the smallest reversible action that reduces impact:

- roll back the last deploy;
- disable the feature flag;
- shift traffic or scale capacity;
- fail over to a healthy replica;
- rate-limit or shed non-critical load.

Before any mitigation, write down what it changes and how to undo it. Never
delete data, rotate credentials, or change network policy without the incident
lead's explicit approval. After acting, re-measure the symptom and timestamp
the change.

## 6. Confirm recovery

Consider the incident mitigated only when:

- the original symptom is gone, measured the same way it was observed;
- error rate, latency, and saturation are back to baseline;
- the fix has held for an agreed observation window;
- no new symptom appeared after the mitigation.

Record the exact commands and their output. "Looks better" is not confirmation.

## 7. Report and hand off

Give the timeline, impact, current state, mitigation, hypothesis and confidence,
open questions, and the next owner. Separate facts from inferences. List every
evidence path you preserved.

## Stop and ask

Stop and escalate when:

- the action is destructive or hard to reverse;
- the incident involves data loss, a security breach, or personal data;
- the fix requires a schema migration or credential rotation;
- you lack access to the system that is failing.

Report what you know, what you tried, and what you need. This skill grants no
tools, scope, or permissions.
