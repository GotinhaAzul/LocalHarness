# Skills and workflows

Skills and workflows are instructions and playbooks. They cannot change scope,
the tool list, or the gates; code enforces those.

## Skills

A skill is a directory with `SKILL.md`: YAML frontmatter, then Markdown. The
format follows the [Agent Skills specification](https://agentskills.io/specification),
so a `.claude/skills` library loads without conversion.

```markdown
---
name: rust-review
description: Review Rust changes for correctness and unsafe code. Use for a diff, a PR, or a pre-handback check.
when: the task is a Rust code review
allowed-tools: read grep lsp
metadata:
  upstream: example
---

# Instructions

...
```

- `name` is required by the specification: 1-64 lowercase letters, digits, or
  hyphens; no leading, trailing, or doubled hyphen. The directory should match
  the name.
- `description` is the trigger: one line, at most 1,024 characters, written for
  the model. It is how the skill gets loaded.
- `when` is the native trigger hint. It is shown in the compact index, next to
  the description.
- `allowed-tools` is an **intersection**, never a grant. The harness intersects
  it with the tools the active profile already selected; it can only remove
  tools the profile has, never add one it does not. A skill cannot enable
  `bash` in a read-only profile.
- Every other frontmatter field is preserved as data. Lint rejects fields that
  claim to grant tools, scope, or permissions (`tools`, `scope`, `permissions`,
  `capabilities`, `grants`, and similar) because the harness would never honor
  them.

### Locations and precedence

Lowest to highest:

| Origin | Path | Trust |
| --- | --- | --- |
| Bundled | compiled into the binary | always |
| Claude global | `~/.claude/skills/<name>/SKILL.md` | user-owned |
| Agents global | `~/.agents/skills/<name>/SKILL.md` | user-owned |
| Native global | `~/.config/localharness/skills/<name>/SKILL.md` | user-owned |
| Claude project | `.claude/skills/<name>/SKILL.md` | `--trust` |
| Agents project | `.agents/skills/<name>/SKILL.md` | `--trust` |
| Native project | `.harness/skills/<name>/SKILL.md` | `--trust` |

Native directories beat the ecosystem directories at the same level, and a
project skill beats a global one. Resolution is deterministic; every collision
is recorded as a shadow and `lh skill lint --all` reports it. An intentional
override sets `shadow: true` in the winning skill's frontmatter, which turns the
duplicate-name error into an explicit warning. Project `.claude/skills` and
`.agents/skills` are untrusted repository content and load only with `--trust`,
exactly like `.harness/skills`.

### Progressive disclosure

Discovery reads only `name`, `description`, and `when`. The body is read from
disk when the skill is activated, and a reference only when the model names it.
A 100-skill library adds 100 index lines at startup, not 100 bodies; the index
is what `SkillIndex::index_text` injects into the system prompt.

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

### Bundled scripts

A skill may ship `scripts/` alongside its instructions. The harness copies those
files but **never runs them**: not at discovery, install, update, lint, or eval.
A `bash` or `bg` call whose command names a script inside a skill directory
requires per-execution approval with the exact command shown. A frontend with
an approval channel (ACP) asks; one without it refuses with
`SKILL_SCRIPT_APPROVAL`, names the command, and runs nothing.

### Linting and trigger eval

```sh
lh skill lint rust-review            # one skill
lh skill lint                        # global and trusted project skills
lh skill lint --all                  # bundled, global, and trusted project
lh skill lint --all --json
```

The linter and the loader share one validator. Errors: a name outside the spec,
a description over 1,024 characters, a body over 500 lines, a reference the body
names but the skill does not ship, an unreadable or over-size reference, a
frontmatter field that claims a grant, and a duplicate name without
`shadow: true`. Warnings: a body over 300 lines, more than ten references, a
missing description, an absolute home path, a secret-shaped string, and a name
that does not match its directory. Exit status is 1 when there is any error;
CI runs `lh skill lint --all` on the bundled skills with no model.

```sh
lh skill eval --fixtures ./skill-fixtures
lh skill eval --fixtures ./skill-fixtures --json
```

A fixture is one `.toml` (or `.json`) file:

```toml
prompt = "the parser test fails after my change"
skill = "test-repair"
expect = "load"                         # or "skip"
reference = "references/triage.md"      # optional
```

`skill` may be omitted when the fixture lives in a subdirectory named after the
skill, or when the fixture directory names exactly one installed skill. The
evaluator uses the deterministic fake provider: it scores the prompt against the
skill name, description, and `when`, loads the best match, and picks a reference
by name and content overlap. No model or network is involved, so CI
can run it. It reports precision and recall per skill, average loaded tokens,
and the false-load cost (tokens spent on fixtures that should have skipped). A
fixture that says `skip` and loads is a failure, not a warning; the command
exits 1.

### Installing skills

```sh
lh skill add ./skills/deploy                  # local directory
lh skill add https://example.com/skills.git#main
lh skill update                               # every locked skill
lh skill update deploy --force                # discard local edits, keep a backup
lh skill update --frozen                      # verify pins, change nothing
lh skill remove deploy
lh skill list --json
lh skill list --frozen
```

`add` accepts a local directory or a git URL with an optional `#ref` (branch,
tag, or commit). Git runs through the `git` CLI; there is no runtime dependency.
The skill is validated before install: frontmatter, body, references, and size.
The description and file list are printed, and bundled scripts are copied but
never executed.

Installed copies live in `~/.config/localharness/skills/<name>/`.
`~/.config/localharness/skills.lock` records the source, requested ref, resolved
commit, install time, and a SHA-256 for every installed file:

```toml
version = 1

[[skill]]
name = "deploy"
source = "https://example.com/skills.git#main"
source_kind = "git"
ref = "main"
resolved_commit = "0123456789abcdef..."
installed_at = "2026-11-15T12:00:00Z"

[skill.files]
"SKILL.md" = "…64 hex…"
"references/rollback.md" = "…64 hex…"
```

`list` reports source, pinned ref, combined digest, per-file digests, and whether
the installed copy differs from the lockfile. `--frozen` compares every digest,
fails on a changed, missing, or extra file, and never writes the lockfile; it is
the CI/reproducible-setup mode.

`update` re-reads the source (the git ref resolves again) and refuses when the
installed copy has local edits, naming the changed files. `--force` replaces it
anyway. `add --force`, `update --force`, and `remove` move the old copy to
`~/.config/localharness/skills-trash/<timestamp>-<name>/` and never delete it.
Removing the last copy leaves the session able to start; the name is simply
missing from `/skill` and `lh skill list`.

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
`prompt` text or `prompt_file` relative to the TOML. Prompt files must stay
inside the workflow's directory: absolute paths, `.`/`..`, and symlinks that
leave it are rejected. Steps cannot name a profile, tools, or skills: the
workflow-level fields are the only policy, and the engine has no code
evaluator. An approval step takes a `message` only; `prompt`, `require`, and
`retry` are configuration errors on a gate.

`lh workflow run` loads and validates the workflow's `skills` and prepends
their instructions to the first prompt step. The TUI does the same by adding
labeled user entries when it starts the workflow.

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

**Retries** are bounded: `retry = 2` allows three attempts in one invocation,
and every attempt is recorded with its cumulative count in the journal. A
failed turn or an unmet requirement consumes an attempt; when the last one
fails, the run ends with `workflow_step_failed` and prints the step and the
reason. A crash leaves the step pending and `--resume` continues its remaining
budget; an explicit `--resume` after a failure starts a fresh budget for that
invocation. An approval step is not retried.

**Approval gates** print the question and read one line from stdin; `y` or
`yes` accepts, anything else denies (EOF denies). The decision and timestamp
are written to the journal. A denial ends the run with
`workflow_approval_denied`, names the step, and never runs it.

**Artifacts** can be declared per step or at the top level with
`artifacts = ["out/report.md"]`. Every one must exist as a non-empty file when
its step finishes, and every artifact belonging to a completed step is checked
again before the workflow reports success, so a later step cannot delete an
earlier output. A missing artifact ends the run with
`workflow_artifact_missing`, names it, and reopens the prompt step that owns it
so `--resume` can recreate it. Paths must be relative, cannot
contain `.`/`..`, and must still resolve inside the working directory after
symlinks.

**Resume** is stored in `<data>/sessions/<project>/workflows/<name>.json` with
`0600` permissions in a `0700` directory. Every step transition is written
atomically. Each record carries a hash of the step definition: a step whose
prompt, condition, requirement, retry, or artifacts changed is rerun instead of
being skipped by id. `lh workflow run <name> --resume` skips unchanged steps
already completed or skipped and starts at the first step that is pending or
failed; it also reopens the recorded session so the model keeps its context.
If that session file is gone, the run warns and starts a new session. `--json`
emits one JSON event per line, including each attempt and the terminal failure.
`--no-session` keeps the journal in memory and writes nothing.

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
