# Issue #173: Detailed Task Breakdown

## Overview
This document provides a granular breakdown of all tasks needed to resolve Issue #173.

---

## 📋 Task Categories

### Category 1: Configuration & Permissions (Phase 1)
**Goal:** Add rewrite_history permission configuration

- [ ] **Task 1.1:** Add `rewrite_history: GitApproval` field to `GitConfig` struct in `src/config.rs`
  - Location: Line ~1257 in `src/config.rs`
  - Default: `GitApproval::Ask`
  - Add to default implementation

- [ ] **Task 1.2:** Update `GitConfig` documentation to include `rewrite_history`

- [ ] **Task 1.3:** Add config parsing test for new field

---

### Category 2: Git Tool - Schema & Dispatch (Phase 1)
**Goal:** Add rewrite_history action to git tool schema and dispatch

- [ ] **Task 2.1:** Add `"rewrite_history"` to action enum in git tool schema
  - Location: `src/tools/git.rs` line ~70-77
  - Add to the enum list

- [ ] **Task 2.2:** Add schema definition for `rewrite_history` action
  - Properties: `paths` (array), `invert` (boolean), `refspec` (string)
  - Required: `paths`

- [ ] **Task 2.3:** Add `rewrite_history` to dispatch match statement
  - Location: `src/tools/git.rs` line ~127-137
  - Add case for `"rewrite_history"`

- [ ] **Task 2.4:** Create `rewrite_history` function
  - Location: New section in `src/tools/git.rs`
  - Signature: `fn rewrite_history(ctx: &ToolContext, config: &GitConfig, args: &Value) -> ToolResult`

---

### Category 3: Rewrite History Implementation (Phase 2)
**Goal:** Implement the rewrite_history action with full guardrails

#### Pre-flight Checks
- [ ] **Task 3.1:** Implement dirty tree check
  - Use `git status --porcelain` to verify no uncommitted changes
  - Return error if dirty

- [ ] **Task 3.2:** Implement repository root check
  - Verify `.git` is at the repository root
  - Use existing `enclosing_repo` or similar logic

- [ ] **Task 3.3:** Implement bundle backup creation
  - Create directory: `<data-dir>/rewrites/`
  - Generate filename: `<session>-<ts>.bundle`
  - Run: `git bundle create <path> --all`
  - Handle errors gracefully

- [ ] **Task 3.4:** Implement ref recording
  - Run: `git for-each-ref --format='%(objectname) %(refname) %(objecttype)'`
  - Parse and store all refs with their OIDs and types
  - Special handling for tags (annotated vs lightweight)

#### Execution
- [ ] **Task 3.5:** Check for git-filter-repo availability
  - Run: `which git-filter-repo` or `command -v git-filter-repo`
  - Prefer git-filter-repo over filter-branch

- [ ] **Task 3.6:** Implement git-filter-repo execution
  - Build command based on parameters:
    - `--path <path>` for each path in `paths`
    - `--invert-paths` if `invert` is true
    - `--force` flag
    - `--refspec` if provided, else `--all`
  - Capture stdout and stderr

- [ ] **Task 3.7:** Implement git filter-branch fallback
  - Build index-filter command
  - Handle path removal via `--index-filter 'git rm --cached --ignore-unmatch <path>'`
  - Use `--prune-empty` to remove empty commits
  - Target all refs or specified refspec

#### Post-flight Verification
- [ ] **Task 3.8:** Run git fsck
  - Run: `git fsck --no-dangling`
  - Parse output for errors
  - Return error if any issues found

- [ ] **Task 3.9:** Implement ref comparison
  - Get new ref state via `git for-each-ref`
  - Compare with pre-recording:
    - All original refs still exist
    - OIDs match expected (rewritten)
    - No unexpected refs added

- [ ] **Task 3.10:** Implement metadata verification
  - For each commit, verify:
    - Author name, email, date
    - Committer name, email, date
    - Commit message
  - Use: `git log --format='%an %ae %ad %cn %ce %cd %s' --all`

- [ ] **Task 3.11:** Implement tag verification
  - Verify annotated tags:
    - Type is still "tag"
    - Message is preserved
  - Verify lightweight tags still point to correct commits

- [ ] **Task 3.12:** Implement rollback on mismatch
  - Restore refs from recorded map
  - Use: `git update-ref <ref> <old-oid>`
  - Leave bundle in place for debugging
  - Return detailed error about what failed

- [ ] **Task 3.13:** Generate OID mapping report
  - Compare old and new OIDs for all refs
  - Format: `old-oid -> new-oid` for each ref
  - Include in ToolResult output

---

### Category 4: Destructive Git Updates (Phase 3)
**Goal:** Improve destructive_git function to match standalone binaries and provide better messages

- [ ] **Task 4.1:** Update command parsing to detect standalone binaries
  - Add detection for: `git-filter-repo`, `bfg`, `git-filter-branch`
  - Match at start of command or after whitespace
  - Handle variations: `python -m filter_repo`, `python3 -m filter_repo`

- [ ] **Task 4.2:** Update refusal message
  - Change from: "ask the user to run it themselves"
  - To: "history surgery is not available through bash; call `git` action `rewrite_history`"

- [ ] **Task 4.3:** Add headless mode detection
  - Check if running in `tot serve` mode
  - If headless and no opt-in: append "this session has no approval channel"

- [ ] **Task 4.4:** Update bash.rs and bg.rs calls
  - Ensure they use the updated message
  - Verify error code is appropriate

- [ ] **Task 4.5:** Add unit tests for new binary matching
  - Test commands:
    - `git-filter-repo --path file --invert-paths --force`
    - `bfg --delete-files file`
    - `python -m filter_repo --path file`
    - `python3 -m filter_repo --path file`
    - `git filter-branch --index-filter 'git rm --cached file'`

- [ ] **Task 4.6:** Verify refusal text contains `rewrite_history`

---

### Category 5: Permission Integration (Phase 3)
**Goal:** Ensure rewrite_history respects permission system

- [ ] **Task 5.1:** Check permission before executing rewrite_history
  - Use existing permission evaluation
  - Map to `ask` class

- [ ] **Task 5.2:** Implement headless mode handling
  - In `tot serve` without opt-in: resolve `ask` to `deny`
  - Check for session flags (--yolo or config opt-in)

- [ ] **Task 5.3:** Record opt-in in session header
  - Ensure audit trail for destructive operations

---

### Category 6: Doctor Command (Phase 4)
**Goal:** Add rewrite_history policy state to tot doctor

- [ ] **Task 6.1:** Locate tot doctor implementation
  - Search for "doctor" in codebase

- [ ] **Task 6.2:** Add rewrite_history action to doctor output
  - Show: action name, policy state (allowed/ask/deny)
  - Format consistent with other actions

---

### Category 7: Testing (All Phases)
**Goal:** Comprehensive test coverage

#### Unit Tests
- [ ] **Task 7.1:** Test destructive_git matches new binaries
- [ ] **Task 7.2:** Test refusal message contains rewrite_history
- [ ] **Task 7.3:** Test rewrite_history schema validation
- [ ] **Task 7.4:** Test pre-flight checks (dirty tree, repo root)
- [ ] **Task 7.5:** Test bundle creation
- [ ] **Task 7.6:** Test ref recording and comparison

#### Integration Tests
- [ ] **Task 7.7:** Test A04 fixture with opt-in (end-to-end)
- [ ] **Task 7.8:** Test A04 fixture without opt-in (denial)
- [ ] **Task 7.9:** Test injection-isomorphic replay
- [ ] **Task 7.10:** Test G07 regression (rm -rf node_modules)

#### Acceptance Tests (from Issue)
- [ ] **Task 7.11:** Blob absent from git rev-list --objects --all
- [ ] **Task 7.12:** All 8 commits keep message/author/date
- [ ] **Task 7.13:** Merge is still a merge
- [ ] **Task 7.14:** v2.0 is still annotated tag with message
- [ ] **Task 7.15:** Object size < 300 KiB
- [ ] **Task 7.16:** One bundle exists outside worktree
- [ ] **Task 7.17:** Old → new OIDs reported

---

### Category 8: Documentation (Phase 4)
**Goal:** Update documentation for new feature

- [ ] **Task 8.1:** Update git tool description in code
- [ ] **Task 8.2:** Add documentation for rewrite_history action
- [ ] **Task 8.3:** Update README or user docs if applicable
- [ ] **Task 8.4:** Add examples of usage

---

## 🎯 Milestones

### Milestone 1: Foundation (PR #1)
- Tasks: 1.1, 1.2, 1.3, 2.1, 2.2, 2.3, 2.4
- Deliverable: Basic schema and dispatch infrastructure

### Milestone 2: Core Implementation (PR #2)
- Tasks: 3.1-3.13
- Deliverable: Working rewrite_history action with guardrails

### Milestone 3: Security Hardening (PR #3)
- Tasks: 4.1-4.6, 5.1-5.3
- Deliverable: Improved destructive_git, permission integration

### Milestone 4: Integration & Polish (PR #4)
- Tasks: 6.1-6.2, 7.1-7.17, 8.1-8.4
- Deliverable: Full test coverage, documentation, doctor command

---

## ⏱️ Estimates

| Category | Tasks | Estimated Time |
|----------|-------|----------------|
| Config & Permissions | 3 | 2-4 hours |
| Schema & Dispatch | 4 | 4-6 hours |
| Rewrite Implementation | 13 | 10-15 hours |
| Destructive Git Updates | 6 | 4-6 hours |
| Permission Integration | 3 | 2-4 hours |
| Doctor Command | 2 | 2-3 hours |
| Testing | 17 | 8-12 hours |
| Documentation | 4 | 2-3 hours |
| **Total** | **52** | **40-53 hours** |

---

## 🚀 Getting Started

### For Contributors
1. Pick a task from the list
2. Claim it by adding your name/date
3. Create a feature branch: `git checkout -b feature/issue-173-<task>`
4. Implement and test
5. Open PR referencing Issue #173

### For Reviewers
- Verify tasks are completed as described
- Check for backward compatibility
- Ensure tests pass
- Verify documentation is updated

---

*Created: 2024-Nov-20*
*Last Updated: 2024-Nov-20*
*Issue: GotinhaAzul/Totality#173*
