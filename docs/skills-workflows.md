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
each skill. `/skill <name>` appends the full text to the conversation. Skills are
data: they cannot grant security scope, load tools, or bypass a guard. Bundled
`security-red` and `security-blue` include their reference documents inline.

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
