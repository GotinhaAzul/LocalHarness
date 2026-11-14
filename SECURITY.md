# Security policy

LocalHarness runs shell commands, edits files, and holds model credentials on
your machine, and it is used on security engagements. It is held to the same bar
as the tools it drives.

## Report privately

Do not open a public issue, discussion, or PR for a vulnerability.

Use **[Report a vulnerability](https://github.com/GotinhaAzul/LocalHarness/security/advisories/new)**
on the Security tab. Private vulnerability reporting is enabled on this
repository; the report, the discussion, and the fix stay private until an
advisory is published.

A useful report has:

- the version (`lh --version`) and platform, plus the commit if you built from source;
- a minimal reproduction — config, command, and the input that triggers it;
- the impact you can demonstrate, and the boundary you believe is crossed.

Do not paste live credentials, client data, or third-party system details into a
report. Redact first; a maintainer can ask for more.

## What to expect

| Stage | Target |
|---|---|
| Acknowledgement | 5 business days |
| Triage and severity | 10 business days |
| Fix or mitigation plan | 30 days for high and critical |
| Coordinated disclosure | 90 days, or sooner by agreement |

You get credit in the advisory unless you ask otherwise. If a report is out of
scope, you get a reason, not silence.

## In scope

Classes that break a boundary the harness claims to enforce:

- Sandbox escape from `--sandbox` (bubblewrap or Landlock).
- `--path-jail` escape through the file tools.
- Secret redaction bypass in sessions, artifacts, evidence, or reports.
- Command injection or argument smuggling through config, skill, workflow,
  external tool manifest, or MCP server definitions.
- Untrusted project content (`.harness/`, `AGENTS.md`, repository files) causing
  execution or configuration changes without `--trust`.
- Credential leakage: bearer tokens or endpoint keys in logs, sessions, error
  output, or process arguments.
- Unbounded disk or memory use caused by a local endpoint's response.
- Supply-chain or build-integrity problems in this repository.

## Out of scope

- Prompt injection as a phenomenon. A model following instructions found in
  repository content is a documented risk; a bypass of a guard the harness
  claims to enforce is in scope, "the model did what the file said" is not.
- Model output that is wrong, unhelpful, or offensive.
- Bugs in your inference engine, model weights, GPU driver, or OS.
- Reports that require the user to trust an untrusted manifest with `--trust`.
  Trust is an explicit, recorded decision.
- Use of LocalHarness against systems you are not authorized to test.
- Automated scanner output with no demonstrated impact.

## Supported versions

The project is pre-1.0 and ships from `main`. Fixes land on `main`; there are no
maintenance branches and no backports.

## For users of the security pack

`docs/security.md` covers authorization, scope, evidence, and the rules of
engagement. Read it before using the harness on client work. The harness records
what it ran; it cannot grant you permission to run it.
