---
name: perf-profile
description: "Make a measured perf fix: pick one metric, take a baseline, profile before optimizing, change one thing, re-measure the same way. Use when something is slow or a benchmark regresses."
---

# Profile before optimizing

Performance work without a measurement is guessing. One metric, one baseline,
one change, one re-measurement.

## 1. Pick one metric

State the user-visible symptom and turn it into a number:

- latency: p50 and p95 of a named operation;
- throughput: requests or items per second;
- cost: tokens, allocations, bytes, or CPU seconds per operation;
- time to first byte, build time, or test-suite duration.

Name the command that measures it and the environment: machine, release or
debug build, dataset size, concurrency, warm or cold cache. Numbers from
different environments are not comparable.

## 2. Baseline

Run the measurement three times on the unchanged tree. Record every run and the
median. Also record correctness facts: tests pass, output is identical.

If the metric is noisy by more than the change you hope to make, improve the
measurement before optimizing. A benchmark that varies 30% cannot prove a 5%
win.

## 3. Profile

Find where time or memory actually goes before editing:

- CPU: a sampling profiler (`perf record`, `cargo flamegraph`, `py-spy`);
- allocation: a heap or allocation profiler;
- I/O: strace, tracing spans, or logs with timestamps;
- async: blocked-thread and queue-depth metrics.

Read the top entries and write down the call path and its share. A hotspot in a
third-party library needs evidence before you work around it. Use `repo_map`
and `lsp` to find the call sites of the function you intend to change.

## 4. One change

Choose the smallest change that addresses the measured hotspot:

- remove repeated work (allocation, parsing, I/O, query) on the hot path;
- add a cache only with a bound, an invalidation rule, and a key that captures
  every input;
- reduce data copied or serialized;
- batch calls instead of looping;
- change an algorithm only when the input size justifies it.

Do not optimize cold paths, and do not trade correctness or clarity for an
unmeasured gain.

## 5. Re-measure

Run the same command under the same conditions at least three times. Report
baseline and result with the spread. Run `verify` and the relevant tests; a
faster wrong answer is a regression.

If the change does not move the metric, revert it. Keep the measurement and the
negative result in the report; that saves the next person the same attempt.

## 6. Report

Give:

- the metric and how it is measured;
- baseline numbers and the profile that located the cost;
- the change and why it addresses that cost;
- after numbers, with the same conditions;
- correctness evidence;
- what remains slow and the next candidate.

## Stop and ask

Stop before changing code when:

- there is no way to measure the symptom reproducibly;
- the fix needs a new service, cache layer, or schema change;
- the profile points outside this repository;
- the improvement only appears by weakening a guarantee (durability,
  consistency, security).

Report the baseline and the bottleneck instead. This skill grants no tools,
scope, or permissions.
