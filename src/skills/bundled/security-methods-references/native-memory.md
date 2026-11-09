# Native (C/C++ / kernel) memory safety

Highest-value surface: **code that parses attacker-controlled binary input using
a length or count it does not fully trust-check**. This class produces accepted
kernel use-after-free and stack-overflow reports and userspace parser overflows.

## Phase 1: find the untrusted-input parsers

Prioritize functions that read network, file, or IPC data and walk it by a
length field:

- Protocol response and record parsers (SOCKS bound-address, TLV, ASN.1, packet
  headers, pcap/snoop records, font and image chunk tables).
- `recv`/`read` loops that compute `resp_len`/`record_len` from the first bytes
  and then read or copy that many more.
- Kernel: netlink and socket parsers, codec and driver ioctls, `skb` handling.
- Archive, compression, and container format decoders.

## Phase 2: the bug patterns

- **Trusting a length field.** `len = buf[4]; memcpy(dst, src, len)` with a
  fixed-size `dst` and attacker-controlled `len` overflows. A record parser that
  trusts the record length field is the classic shape.
- **Off-by-one / missing bound.** Index `<=` instead of `<`, a terminator not
  counted, `+1`/`-1` errors at a boundary.
- **Integer overflow into an undersized allocation.** `malloc(count * size)` or
  `len + hdr` wrapping on a 32-bit/`size_t` boundary allocates small and writes
  big. Check every arithmetic step on an attacker-influenced length *before* the
  allocation.
- **Append/concat without a remaining-space check.** A helper appends untrusted
  data into a buffer without checking space left; a heap overflow follows.
- **Use-after-free.** An object is freed on one path (error, teardown, refcount
  drop) and dereferenced on another: init/cleanup races, double `put`, freed
  `skb`/`dst` reused, namespace teardown vs in-flight work. Map the
  alloc -> free -> use triangle across call paths.

## Phase 3: validate the bound is actually missing

For a length-driven copy, establish:

1. Where the length comes from.
2. What bounds it (`min()`, `if (len > sizeof buf)`, a growable buffer).
3. Whether the check runs on the **same value** that is later used.

Modern code often uses a growable buffer plus a `have >= need` re-check; that is
safe even when it looks scary. Read the whole function before concluding. If a
large file is truncated, page through it; do not decide from the first screen.

## Phase 4: confirm before creating a finding

- Show the source (untrusted length/data) -> unchecked copy/free/use with
  `file:line`, and why the surrounding checks do not cover that value.
- For UAF: name the allocation site, the free site, the use site, and the path
  or race that orders free before use.
- PoC: a crafted input that produces an ASan/KASan splat. For kernel targets, a
  minimal reproducer under the local verifier; a decoded sanitizer report is the
  gold standard.
- Encode the reproducer as `poc_steps` and run `finding verify`. Capture the
  sanitizer output with `evidence`.

## Anti-false-positive checklist

- Is the destination fixed-size, or a growable/exact-sized allocation?
- Is the length re-checked against remaining space on the value actually used?
- Is the "freed" object really reachable after free on a real path?
- Is the input reachable from an untrusted boundary rather than a developer-only
  build step?

A bounded copy into a sized buffer with a correct check is not a finding. A
growable buffer with a re-check is safe. Say so and move on.

## Minimal proof

Prefer a single small input and the first sanitizer frame that names the
vulnerable function. Do not ship a full exploit chain when a crash with a
controlled input proves the memory-safety break. For kernel crashes, record the
exact config, kernel version, and call trace.

---

_Adapted from 0 (0sec-labs/0), MIT OR Apache-2.0, commit 63e057d._
_Source: skills/vulnerabilities/native-memory-safety.yaml._
