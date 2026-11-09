# Native code review workflow

A concrete playbook for reviewing native C/C++ and kernel code for memory-safety
defects. Work one phase at a time, with bounded, non-destructive validation.
The scope gate and read-before-edit gates still apply.

## Phase 0 — Authorization and surface

1. State the authorization basis and the exact boundary: which repository,
   modules, targets, and test harnesses are in scope; what is prohibited; and
   who to contact. If this is a lab or CTF, state the boundary anyway. If the
   review is source-only, say so and do not run target binaries.
2. Map the native surface: parsers, protocol handlers, file and archive
   decoders, ioctl entry points, socket paths, shared-memory interfaces, and
   anything that consumes attacker-controlled bytes.
3. Use `repo_map`, `grep`, and `read` to locate length-driven copies, allocation
   arithmetic, refcount/free paths, and callback/teardown ordering.

Output of this phase: a ranked list of parser functions with their input
source, length/count fields, and allocation sites.

## Phase 1 — Static triage

For each high-value function, check:

- Is the length or count attacker-controlled, and is it bounded before use?
- Does the bound check run on the same value that is later used?
- Can `count * size` or `len + header` wrap before allocation?
- Is the destination fixed-size, or growable with a remaining-space re-check?
- Are off-by-one and terminator bytes accounted for?
- For each allocation: where is it freed, and can the same pointer be used after
  that free on any path?
- Are error and teardown paths ordered so a free cannot outrun an in-flight use?
- Are callbacks, timers, and workqueues cancelled before their objects are
  released?

Treat a suspected bug as a lead. Read the surrounding callers and guards before
recording it; many scary-looking copies are safe.

## Phase 2 — Dynamic confirmation (bounded)

Build the smallest reproducer that reaches the suspected site:

1. Prefer an existing fuzz harness or unit test entry point.
2. Use a minimal crafted input; keep the file small and local.
3. Build with AddressSanitizer or run under the kernel sanitizer available in
   the local environment. Do not weaken the target's production configuration to
   make a crash appear.
4. Run with a timeout and a memory cap. Do not run unbounded fuzzing, do not
   fork-bomb, and do not target a remote host.
5. Capture the first sanitizer frame naming the vulnerable function, the
   allocation/free/use stack, and the input hash as `evidence`.

A crash is not yet a confirmed finding. Decode the report: is the access in the
target's code, reachable from untrusted input, and a real invariant violation
rather than a test-only assertion or an intentional panic? Only then record it.

## Phase 3 — Finding and proof

1. Record the finding with `finding {"action":"add"}`, including source
   `file:line` for the source, the sink, the free/use sites for a UAF, and the
   expected impact.
2. Encode the minimal reproducer as `poc_steps` (`setup`, `prerequisite`,
   `exploit`, `verify`) and run `finding {"action":"verify","id":"…"}`. Only
   `reproduced` with verdict `confirmed` confirms.
3. If it does not reproduce, do not mark it false positive. Record the
   environment, the sanitizer output, and whether the harness reached the site;
   `not_reproduced` and `error` are inconclusive.
4. Do not weaponize. No exploit chain, no bypass of mitigations, no persistence,
   no credential access. A controlled crash is the proof.

## Phase 4 — Scoped fix and regression

1. Fix the root cause: validate the length on the value used, check arithmetic
   before allocation, re-check remaining space, or correct the free/use
   ordering.
2. Keep the change localized to the vulnerable boundary; do not refactor the
   subsystem or add speculative hardening elsewhere.
3. Add a regression test or a sanitizer reproducer that fails before the fix.
   Use `test`, or a bounded `bash` run of the reproducer in the project's test
   harness.
4. Run `review` on the diff and confirm the fix does not merely narrow the
   input for the known PoC.
5. Update the finding with
   `finding {"action":"update","id":"…","status":"fixed"}` and cite the commit
   and test.

## Phase 5 — Report

End with `finding {"action":"report"}`. Include the affected function and
configuration, the sanitizer evidence, the reachable input path, the fix, and
the regression test. Name modules you did not review. A clean review of the
inspected surface is a valid result; do not invent findings.

## Stop conditions

Stop and ask if:

- the authorization boundary is unclear, or the code belongs to a third party;
- the reproducer needs a real device, destructive state, or a production host;
- the only way to confirm is to disable a security mitigation;
- the fix would touch ownership outside this review.
