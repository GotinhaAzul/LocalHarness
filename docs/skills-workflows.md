# Skills and workflows

Skills and workflows are instructions and playbooks. They cannot change scope,
the tool list, or the gates; code enforces those.

## Skills

A skill is Markdown with optional YAML frontmatter:

```markdown
---
name: rust-review
description: Review Rust changes for correctness and unsafe code
when: the task is a Rust code review
---

# Instructions

...
```

Locations:

- Global: `~/.config/localharness/skills/<name>/SKILL.md`
- Project: `.harness/skills/<name>/SKILL.md` (requires trust)

The system prompt carries a compact index of name, description, and when to use
each skill. Write the `description` and `when` for the model: they are how it
decides to load the skill. Bundled security skills are advertised only while
the `security` pack is active, so the `coding` default stays focused; they are
still loadable by name and are preloaded by the security workflows.

**The model loads skills itself** with the `skill` tool (core pack, read-only):

| Call | Result |
|---|---|
| `{}` | the skill index |
| `{"name": "security-blue"}` | the instructions plus a list of its references |
| `{"name": "security-blue", "reference": "hardening"}` | one reference document |

References load on demand, so a skill with large supporting documents costs
little until they are needed. A skill already loaded since the last compaction
is not sent again. Skill results are kept inline up to 24k characters instead
of being archived like other tool output.

**You load skills** with `/skill <name>`, which appends the instructions and
every reference to the conversation at once.

References are the other `.md` and `.txt` files in the skill directory (up to
three levels deep, symlinks ignored), named by relative path, e.g.
`references/rollback.md`. The model can ask for `references/rollback.md`,
`rollback.md`, or `rollback`. Bundled `security-red`, `security-blue`, and
`security-methods` embed theirs in the binary.

`security-methods` is the class-specific method library. It has exactly six
references, chosen by bug class:

| Reference | Classes |
| --- | --- |
| `native-memory` | C/C++, kernel, binary parser memory safety |
| `web-injection` | Command injection, SQLi (including identifier injection), path traversal, crypto misuse, races |
| `auth-and-access` | JWT, AD/Entra attack paths, broken-assumption mining |
| `ssrf-and-request-smuggling` | SSRF guard bypass, HTTP request smuggling |
| `deserialization-and-prototype-pollution` | Deserialization chains, prototype pollution, HTTP/spec differentials |
| `llm-application-security` | Prompt injection, RAG poisoning, excessive agency, insecure output handling |

It does not grant scope: authorization comes from `security-red` and the `/sec`
scope gate, and a finding is confirmed only by a reproduced
`finding {"action":"verify"}` result.

Skills are data: they cannot grant security scope, load tools, or bypass a
guard. Project skills load only in trusted sessions, for the tool and for
`/skill` alike.

### Bundled coding skill pack

Eight coding skills ship in the binary. They advertise while the `coding` pack
is active, so the coding profile's index carries them and a security-only
profile stays focused; any of them is still loadable by name. Each is a single
document under 120 lines, names only core/coding tools, and ends with the
condition that means "stop and ask".

| Skill | Triggers |
| --- | --- |
| `test-repair` | A failing test or a red suite: reproduce, classify, one hypothesis, smallest fix, exact test then suite |
| `code-review` | A diff, pull request, or pre-handback self-review: scope, correctness, security, tests, compatibility, severity, verdict |
| `refactor` | A behavior-preserving restructure: behavior contract, green baseline, small moves, `lsp` reference enumeration, verify each step |
| `dependency-upgrade` | A version bump or CVE fix: changelog, breaking changes, one package, adapt, test, rollback |
| `perf-profile` | A slowness report: one metric, baseline, profile before optimizing, one change, re-measure |
| `incident-triage` | An alert or outage: UTC timeline, symptom vs cause, recent changes, mitigate before root cause |
| `release-notes` | A release or changelog: commits since the tag, classify, link issues, upgrade notes, invent nothing |
| `issue-to-pr` | An issue to implement: read the comments, reproduce, plan, implement, test, self-review, PR body |

The pack is loaded the same way as any other skill: the model calls
`skill {"name":"test-repair"}`, or you run `/skill test-repair`. Loading one
adds instructions only; it does not add tools, change the profile, or relax a
gate. The index filter is a prompt-budget aid, not a permission boundary.

## Workflows

A workflow is a TOML playbook plus a profile and tool allowlist:

```toml
name = "recon"
description = "Passive and active reconnaissance with scope checks"
profile = "security"
prompt = "prompts/recon.md"
skills = ["security-red"]
tools = ["read", "grep", "glob", "bash", "bg", "finding", "iocs"]
```

Paths:

- Global: `~/.config/localharness/workflows/<name>.toml`
- Project: `.harness/workflows/<name>.toml` (requires trust)
- Bundled: compiled into the binary

Resolution order is global, then trusted project, then bundled. A global file
shadows a project file of the same name; a project file shadows the bundled
workflow. Project files still require a trusted session — an untrusted project
workflow produces the trust error instead of silently falling back to a bundled
workflow. `/workflow` with no argument lists every name from all three sources,
deduplicated and sorted.

## Bundled workflows

| Name | Purpose |
| --- | --- |
| `api-security` | Authorization-aware API review: surface and defaults, input handling and authorization, bounded PoV verification, scoped fix and regression |
| `coding` | Changing an existing repository: orient, plan, small edits, narrow verification, diff review, and a short report |
| `general-assistance` | Read-only assistance: answer questions, orient in a repository, and summarize with citations, with no write or shell tools |
| `native-code-review` | Native C/C++ and kernel memory-safety review: parser surface, length/free triage, bounded sanitizer proof, scoped fix and regression |
| `security-blue` | Defensive work: evidence-first triage, validated Sigma detections, hardening, secure code review, and supply-chain checks |
| `security-red` | Authorized offensive testing: scope, reconnaissance into a surface map, hypotheses, bounded proof, evidence-backed findings, and a report |

The two security deep-dives preload `security-red` and `security-methods` when
the workflow starts. The new security workflows load their skill on demand with
the `skill` tool — the body in Phase 0, the method reference only when Phase 2
names a bug class — so a 27B–35B model does not start with every reference in
context. `coding` and `general-assistance` ship without a skill and rely on the
repository's own instructions.

Every workflow carries an explicit tool allowlist. The list is exact: starting a
workflow derives a profile that drops the base profile's packs, so a tool not in
the list stays unavailable even when its pack is part of the base profile. The
security workflows include `finding`, `verify`, `test`, `review`, `surface`,
`evidence`, `bash`, and `bg`; their prompts require bounded, non-destructive
validation: the reproduction is the proof, and destructive or third-party
testing stops and asks. `general-assistance` has no write, edit, shell, or
project-check tools by design.

`/workflow <name>` starts a new branch, records a checkout, applies the profile
(deriving a workflow profile when `tools` is set), appends the workflow prompt as
the first user message, and loads named skills as labeled user entries. The
model follows the playbook; the scope gate, read-before-edit, budgets, and settle
gates still apply.

The prompt path is resolved relative to the workflow file. Missing prompts,
unknown profiles, and untrusted project workflows produce prescriptive errors
and leave the session unchanged.

### Deterministic steps

A workflow can also declare steps. `lh workflow run <name>` executes them in
order; `/workflow` refuses a step workflow and points at the command, because
the TUI owns one turn at a time.

```toml
name = "tested-change"
description = "Reproduce, verify, and gate the push"
profile = "coding"

[[step]]
id = "reproduce"
prompt = "Reproduce the reported failure. Run it once and quote the first error."
require = { tool = "bash", exit = 0 }
retry = 1

[[step]]
id = "test"
prompt = "Make the smallest fix and run the configured verification."
when = "changed_files > 0"
require = { verify = "pass" }
retry = 1

[[step]]
id = "push"
run = "approval"
message = "Push the branch and open a draft PR?"
```

Each `[[step]]` has an `id`, `run` (`prompt` or `approval`), and either inline
`prompt` text or `prompt_file` relative to the TOML. Steps cannot name a
profile, tools, or skills: the workflow-level fields are the only policy, and
the engine has no code evaluator.

**Conditions** read a fixed fact set: `changed_files`, `open_tasks`,
`last_exit`, `steps_completed`, and `last_verify` (`pass`, `fail`, `none`).
The grammar is one or more `<fact> <op> <value>` clauses joined by ` and `;
operators are `==`, `!=`, `>`, `>=`, `<`, `<=`. Anything else is a
configuration error, so a typo fails the run instead of silently skipping the
step. Facts are read from session entries: file-change records, `verify`
results, todo state, and the last `bash` exit code.

**Requirements** decide whether an attempt succeeded:

| Form | Passes when |
| --- | --- |
| `require = { tool = "bash", exit = 0 }` | the step called `bash` and its last result exited 0 |
| `require = { tool = "bash" }` | the step called `bash` and it did not report an error |
| `require = { verify = "pass" }` | the step's own `verify` result passed (`fail` also accepted) |
| `require = { artifact = "out/report.md" }` | that file exists and is non-empty |

**Retries** are bounded: `retry = 2` allows three attempts total, and every
attempt is recorded. A failed turn or an unmet requirement consumes an attempt;
when the last one fails, the run ends with `workflow_step_failed` and names the
step and the reason. An approval step is not retried.

**Approval gates** print the question and read one line from stdin; `y` or
`yes` accepts, anything else denies (EOF denies). The decision and timestamp
are written to the journal. A denial ends the run with
`workflow_approval_denied`, names the step, and never runs it.

**Artifacts** can be declared per step or at the top level with
`artifacts = ["out/report.md"]`. Every one must exist as a non-empty file when
its step finishes (or when the workflow finishes, for the top-level list),
otherwise the run ends with `workflow_artifact_missing` before reporting
success. Paths must be relative and cannot contain `..`, so a workflow cannot
declare a file outside the working directory.

**Resume** is stored in `<data>/sessions/<project>/workflows/<name>.json`. Every
step transition is written atomically. `lh workflow run <name> --resume` skips
steps already completed or skipped and starts at the first step that is
pending or failed; it also reopens the recorded session so the model keeps its
context. `--json` emits one JSON event per line, including each attempt.

Success means: every executed step met its requirement and every declared
artifact exists. The harness still applies the same gates inside each step's
turn; a step cannot disable `verify`, the read-before-edit rule, the scope gate,
or the sandbox.

## Installable copies

Every bundled workflow also ships as a valid on-disk copy under
`src/workflows/bundled/`, as `<name>.toml` plus `prompts/<name>.md`:

```text
src/workflows/bundled/api-security.toml
src/workflows/bundled/coding.toml
src/workflows/bundled/general-assistance.toml
src/workflows/bundled/native-code-review.toml
src/workflows/bundled/security-blue.toml
src/workflows/bundled/security-red.toml
src/workflows/bundled/prompts/api-security.md
src/workflows/bundled/prompts/coding.md
src/workflows/bundled/prompts/general-assistance.md
src/workflows/bundled/prompts/native-code-review.md
src/workflows/bundled/prompts/security-blue.md
src/workflows/bundled/prompts/security-red.md
```

The TOML files are ordinary installable workflows: `prompt` is a path relative
to the TOML, and copying the TOML plus its prompt directory into
`~/.config/localharness/workflows/` (or `.harness/workflows/`) installs it.
Editing the installed copy does not change the compiled-in workflow unless you
rebuild with an updated source file. Workflow tests check that every TOML
parses, every prompt exists and is nonempty, and every skill and tool name
resolves in the crate registries.
