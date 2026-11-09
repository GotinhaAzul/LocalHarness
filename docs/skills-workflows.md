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
decides to load the skill.

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
| `general-assistance` | Read-only assistance: answer questions, orient in a repository, read logs and configuration, and summarize with citations |
| `native-code-review` | Native C/C++ and kernel memory-safety review: parser surface, length/free triage, bounded sanitizer proof, scoped fix and regression |
| `security-blue` | Defensive work: evidence-first triage, validated Sigma detections, hardening, secure code review, and supply-chain checks |
| `security-red` | Authorized offensive testing: scope, reconnaissance into a surface map, hypotheses, bounded proof, evidence-backed findings, and a report |

The two security deep-dives preload `security-red` and `security-methods` when
the workflow starts. The new security workflows load their skill on demand with
the `skill` tool — the body in Phase 0, the method reference only when Phase 2
names a bug class — so a 27B–35B model does not start with every reference in
context. `coding` and `general-assistance` ship without a skill and rely on the
repository's own instructions.

Every workflow carries an explicit tool allowlist. The security workflows
include `finding`, `verify`, `test`, `review`, `surface`, `evidence`, `bash`,
and `bg`; their prompts require bounded, non-destructive validation: the
reproduction is the proof, and destructive or third-party testing stops and
asks. `general-assistance` has no write, edit, or project-check tools by
design.

`/workflow <name>` starts a new branch, records a checkout, applies the profile
(deriving a workflow profile when `tools` is set), appends the workflow prompt as
the first user message, and loads named skills as labeled user entries. The
model follows the playbook; the scope gate, read-before-edit, budgets, and settle
gates still apply.

The prompt path is resolved relative to the workflow file. Missing prompts,
unknown profiles, and untrusted project workflows produce prescriptive errors
and leave the session unchanged.

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
