---
name: release-notes
description: "Write release notes from the commits and issues actually in a release: find the range since the last tag, classify changes, link issues, add upgrade notes, and invent nothing."
when: "preparing a release, changelog, or summary of what changed since a tag"
---

# Write release notes

Release notes are a factual index of what shipped. Every line must trace to a
commit, merged pull request, or issue. If you cannot prove it, leave it out.

## 1. Bound the release

Find the last released tag and the head being released:

```sh
git tag --sort=-v:refname | head
git log --oneline <last-tag>..HEAD
git diff --stat <last-tag>..HEAD
```

Confirm with the user when the release range is ambiguous. Check whether the
project uses `CHANGELOG.md`, GitHub releases, or both, and which style it
already uses. Match the existing format instead of inventing one.

## 2. Collect the facts

For each commit or merged pull request, record:

- the change in one sentence, from the user's point of view;
- the author or PR number;
- linked issues (`Refs #12`, `Closes #34`);
- whether it changes default behavior, configuration, output, or performance;
- whether it needs a migration or manual step.

Read the diff when the message is unclear. Do not infer a feature from a commit
title alone. Summarize a breaking change only after reading the change.

## 3. Classify

Use the headings the project already uses; otherwise:

- **Added** - new capability.
- **Changed** - behavior or defaults that differ from the previous release.
- **Fixed** - a defect corrected, with the symptom users saw.
- **Security** - a vulnerability or hardening change, with the advisory id when
  one exists.
- **Deprecated / Removed** - what stops working and when.
- **Upgrade notes** - required actions, in order.

Exclude internal refactors, test-only changes, and dependency bumps with no
user-visible effect unless the project's policy includes them. Dependency
upgrades that fix a CVE belong under Security.

## 4. Write for the reader

- Lead with what users can do or must do, not with the commit list.
- One line per user-visible change; link the PR or issue.
- Say who is affected: all users, a platform, a config, a version range.
- For fixes, name the symptom that no longer happens.
- For breaking changes, give old behavior, new behavior, and the migration
  step. Put them first, not buried.
- Do not use marketing adjectives ("blazing", "seamless") and do not promise
  work that is not in the range.

## 5. Check the notes against the tree

Before handing over:

- every entry maps to a commit or merged PR in the range;
- every linked issue number exists and is the right one;
- version numbers, dates, and tag names match the actual release;
- upgrade steps were run or read from the migration itself;
- no entry describes unmerged, reverted, or unreleased work.

Read the final notes once as a user deciding whether to upgrade.

## 6. Report

Deliver the notes in the project's file or as a draft for the user, plus:

- the range and number of commits considered;
- entries you excluded and why;
- anything that needs a decision before publishing.

## Stop and ask

Stop before writing when:

- there is no tag or clear range to diff;
- the version number is not decided;
- a change's user impact cannot be determined from the commit and diff;
- publishing the notes would announce something not yet released.

Never publish or tag without the user's explicit request. This skill grants no
tools, scope, or permissions.
