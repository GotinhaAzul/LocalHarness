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
`rollback.md`, or `rollback`. Bundled `security-red` and `security-blue` embed
theirs in the binary.

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

`/workflow <name>` starts a new branch, records a checkout, applies the profile
(deriving a workflow profile when `tools` is set), appends the workflow prompt as
the first user message, and loads named skills as labeled user entries. The
model follows the playbook; the scope gate, read-before-edit, budgets, and settle
gates still apply.

The prompt path is resolved relative to the workflow file. Missing prompts,
unknown profiles, and untrusted project workflows produce prescriptive errors
and leave the session unchanged.
