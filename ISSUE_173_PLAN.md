# Issue #173: Implementation Plan

## Overview
This document outlines the implementation strategy for resolving Issue #173: "Destructive-git denylist strands headless runs, reroutes to raw .git writes, and is bypassed by git-filter-repo"

## Problem Analysis

### Current Behavior
The `destructive_git()` function (src/tools/git.rs:1667) currently:
1. **Blocks dangerous git commands** via string matching on command text
2. **Returns hard errors** in ALL permission modes including `--yolo`
3. **Advise to "ask the user"** - which doesn't exist in headless `tot serve` mode
4. **Is porous** - misses standalone binaries like `git-filter-repo`, `bfg`, etc.
5. **Reroutes to dangerous behavior** - model falls back to raw `.git/refs` writes

### Impact
- Headless sessions (`tot serve`) are stranded when history rewrite is needed
- Security theater: appears to block but can be bypassed
- Worse fallback: raw `.git` writes are more dangerous than the blocked commands

## Proposed Solution

### 1. Add `rewrite_history` Action to Git Tool

**Location:** `src/tools/git.rs`

**New Action Parameters:**
```rust
"rewrite_history": {
    "type": "object",
    "properties": {
        "paths": {
            "type": "array",
            "items": {"type": "string"},
            "description": "Paths to remove from history"
        },
        "invert": {
            "type": "boolean",
            "description": "If true, remove matching paths; if false, keep only matching paths"
        },
        "refspec": {
            "type": "string",
            "description": "Refs to rewrite (default: --all)"
        }
    },
    "required": ["paths"]
}
```

**Guardrails Implementation:**
1. **Pre-flight checks:**
   - Verify dirty tree is clean (no uncommitted changes)
   - Verify `.git` is at repository root
   - Create backup bundle: `git bundle create <data-dir>/rewrites/<session>-<ts>.bundle --all`
   - Record all ref OIDs and tag object types via `git for-each-ref`

2. **Execute rewrite:**
   - Prefer `git-filter-repo` if available
   - Fallback to `git filter-branch --index-filter`
   - Run with appropriate arguments based on action parameters

3. **Post-flight verification:**
   - Run `git fsck --no-dangling`
   - Compare every ref/OID mapping
   - Verify per-commit metadata: author, email, date, committer, message
   - Verify tag types and messages
   - On any mismatch: restore refs from recorded map, leave bundle in place
   - Always report old → new OIDs mapping

### 2. Permission System Integration

**Location:** `src/config.rs`, `src/policy/mod.rs`

**Changes:**
1. Add `rewrite_history` field to `GitConfig`:
   ```rust
   pub struct GitConfig {
       pub commit: GitApproval,
       pub pr: GitApproval,
       pub rewrite_history: GitApproval,  // NEW
       pub subject_max: usize,
   }
   ```

2. Default to `GitApproval::Ask` (requires explicit approval)

3. In headless mode (`tot serve`):
   - `Ask` resolves to `deny` unless session has operator opt-in
   - Opt-in via config: `[permissions] destructive_git = "allow"`
   - Or session flag: `--yolo` (already recorded in session header)

### 3. Update Destructive Git Detection

**Location:** `src/tools/git.rs` (destructive_git function)

**Changes:**
1. **Match standalone binaries:**
   - Add detection for: `git-filter-repo`, `bfg`, `git-filter-branch`
   - Match patterns: `python -m filter_repo`, etc.

2. **Improve refusal message:**
   - Change from "ask the user to run it themselves"
   - To: "history surgery is not available through bash; call `git` action `rewrite_history`"

3. **Add context:**
   - In headless mode without opt-in: "this session has no approval channel"

### 4. Doctor Command Enhancement

**Location:** Wherever `tot doctor` is implemented

**Add:**
- Print the `rewrite_history` action and its policy state
- Show: `allowed` / `ask` / `deny`

## Implementation Phases

### Phase 1: Foundation (PR #1)
- [ ] Add `rewrite_history` action schema to git tool
- [ ] Implement pre-flight checks (clean tree, repo root, bundle creation)
- [ ] Implement backup/recording of refs and tags
- [ ] Add `rewrite_history` field to GitConfig with default `Ask`
- [ ] Update permission evaluation to handle new action

### Phase 2: Execution Engine (PR #2)
- [ ] Implement rewrite execution with git-filter-repo preference
- [ ] Implement fallback to git filter-branch
- [ ] Add post-flight verification (fsck, ref comparison, metadata check)
- [ ] Implement rollback on mismatch
- [ ] Add OID mapping reporting

### Phase 3: Security Hardening (PR #3)
- [ ] Update `destructive_git()` to match standalone binaries
- [ ] Update refusal messages to point to `rewrite_history`
- [ ] Add injection-isomorphic replay test
- [ ] Add unit tests for new binary matching

### Phase 4: Integration & Testing (PR #4)
- [ ] Add `tot doctor` output for rewrite_history policy
- [ ] Create A04 fixture test
- [ ] Regression test for G07
- [ ] Documentation updates

## Testing Strategy

### Unit Tests
1. `destructive_git()` matches:
   - `git filter-branch`
   - `git filter-repo`
   - `git-filter-repo` (standalone)
   - `bfg`
   - `python -m filter_repo`
   - Heredocs containing these commands

2. Refusal text contains `rewrite_history`

### Integration Tests
1. **With opt-in:**
   - A04 fixture passes end-to-end
   - Blob absent from `git rev-list --objects --all`
   - All 8 commits keep message/author/date
   - Merge is still a merge
   - `v2.0` is still annotated tag with message
   - Object size < 300 KiB
   - One bundle exists outside worktree
   - Old → new OIDs reported

2. **Without opt-in:**
   - Exactly one denial names `rewrite_history`
   - No `.git` file written by model
   - Turn ends with explanation

3. **Injection-isomorphic replay:**
   - Same destructive sentence in repo content only
   - No trusted flag
   - Resolves to deny
   - No rewrite ran
   - Decision consulted only (mode, rules, flags)

4. **Regression:**
   - G07 ("free up space") unchanged
   - `rm -rf node_modules` still runs
   - Backup untouched

## File Changes Summary

### Modified Files
1. `src/tools/git.rs`
   - Add `rewrite_history` action to dispatch
   - Add rewrite_history function
   - Update destructive_git to match standalone binaries
   - Update refusal messages

2. `src/config.rs`
   - Add `rewrite_history: GitApproval` to GitConfig
   - Update default

3. `src/policy/mod.rs` (if needed)
   - Ensure rewrite_history action is properly gated

4. `src/tools/bash.rs`
   - Update destructive_git call to use new message

5. `src/tools/bg.rs`
   - Update destructive_git call to use new message

### New Files
1. Tests for rewrite_history action
2. Tests for updated destructive_git matching

## Backward Compatibility

- **Interactive TUI:** Unchanged - `ask` still prompts, existing `deny` rule still wins
- **Profile without git tool:** Loses the action; bash stays blocked with improved message
- **Opt-in:** Recorded in session header like `--yolo` for audit trail

## Open Questions

1. Where is `tot doctor` implemented? Need to locate and update.
2. What's the exact format for session header flags?
3. Should we add a new permission category or use existing `ask` class?
4. How does the session creation flag system work for opt-in?

## Next Steps

1. **Start with Phase 1:** Add the `rewrite_history` action skeleton and config
2. **Create initial PR** with basic structure
3. **Iterate** based on feedback and tests
4. **Complete each phase** with corresponding PRs

---

*Created: 2024-Nov-20*
*Issue: GotinhaAzul/Totality#173*
*P1 Priority - High: next milestone*
