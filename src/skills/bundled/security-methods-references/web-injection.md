# Web injection, traversal, crypto misuse, and races

Server-side web bug classes with the highest yield. For each, the win is usually
the non-obvious path from untrusted data to a dangerous sink, or a guard that
does not cover the value actually used.

## OS command injection

### Find the subprocess sinks

- Node: `child_process.exec`/`execSync` (runs through `/bin/sh -c`),
  `spawn`/`execFile` with `shell: true`, template literals built into an exec
  string.
- Python: `os.system`, `subprocess.*(..., shell=True)`, `os.popen`.
  `subprocess.run([...])` is still exploitable through argument injection.
- C/C++: `system()`, `popen()`, `execlp`/`execvp` with a PATH-resolved name, or
  a command string handed to a shell.
- Delegates and external tools: image/video converters, git hooks, linters,
  build scripts. These expand `%f`, `%u`, or `{filename}` placeholders into a
  command.

### Trace the source

Ask what attacker-controlled value reaches the sink and through how many hops.
Productive sources:

- filenames of uploaded or processed files,
- URLs, git remotes, redirect targets, download refs,
- archive member names used in a later command,
- config/manifest fields (`package.json`, YAML/JSON, icon or asset paths),
- usernames, headers, or tokens echoed into a command.

### Argument injection

Even without a shell, a parameterized subprocess is exploitable when the
attacker controls a value that becomes a flag: a filename starting with `-`
becomes an option. Watch `--upload-pack`, `-K`/`-o`, `-o ProxyCommand=...`, and
similar. Look for argv arrays where an untrusted string lands where the tool
parses options.

### Confirm

- Show source -> sink with `file:line` for each hop.
- Name the metacharacter or flag that breaks out, and why quoting/escaping does
  not stop it (`shellescape`, `shlex.quote`, array argv, allowlist).
- PoC: the minimal filename, URL, or config value and the resulting command
  string. If it can run locally in a sandbox, capture `id` or a printed canary
  as `evidence`, then encode it in `finding verify`.

### Reject

Input controlled only by the developer at build time; an allowlist or array argv
with no shell; correct quoting upstream; a value validated and canonicalized
before the sink.

## SQL injection (including structural/identifier injection)

### Discovery

Test every parameter (query, body, headers, cookies, JSON values) with canary
payloads:

- string context: `' OR 'a'='a`, `" OR "a"="a`, `' AND '1'='2`
- numeric context: `1 OR 1=1`, `1 AND 1=2`
- comments: `--`, `-- -`, `#`, `/**/`, `;%00`

Compare true vs false response length and content. Error text leaks the engine.

Fingerprint: MySQL `@@version`/`version()`, PostgreSQL `version()`/`pg_`, MSSQL
`@@version`/ODBC, SQLite `sqlite_version()`/`no such column`, Oracle
`v$version`/`ORA-`.

### Blind boolean and time-based

Use a boolean differential, then extract one character at a time with a binary
search over ASCII.

- `' AND SUBSTRING((SELECT current_user()),1,1)='a'-- -`
- MySQL: `' AND IF(1=1,SLEEP(3),0)-- -`
- PostgreSQL: `'; SELECT CASE WHEN (1=1) THEN pg_sleep(3) ELSE pg_sleep(0) END-- -`
- MSSQL: `'; WAITFOR DELAY '0:0:3'-- -`
- SQLite: `' AND (SELECT CASE WHEN (1=1) THEN randomblob(100000000) ELSE 0 END)-- -`

Measure the time delta; a repeatable 3s+ delay confirms execution. Keep the
delay short and bounded.

### Out-of-band and second order

- DNS/HTTP callbacks when time-based is blocked.
- Stored payload triggered by a later query: register a username such as
  `admin'-- -`, then trigger a profile or reset lookup that reuses it.
- Common sinks for second order: usernames, addresses, search history, logs.

### Structural / JSON-key injection

Value-oriented scanners miss identifier-position injection. The surface is the
**key**, not the value:

- `ORDER BY ${key}`, `GROUP BY ${key}`
- `SELECT ${key} FROM ...`
- dynamic `WHERE ${key} = ?` builders
- request keys named `sort`, `order`, `column`, `field`, `group_by`, `select`,
  `filter[<key>]`, `columns[]`

Drive a blind error loop:

1. **Break:** inject a key with a trailing quote (`name'`). A DB error confirms
   the key reaches the parser.
2. **Fingerprint:** read the error to pin the dialect.
3. **Balance:** close and comment with the discovered syntax (`name'-- -`). If
   the bare break errored and the balanced form parses cleanly, the key is SQL.
4. **Iterate:** refine the close syntax within a small budget.

Prove impact with a boolean `ORDER BY` oracle or a one-row error-based proof
(version, `current_user`, one `information_schema` row). Do not dump data.

### WAF bypass notes

Inline comments (`/*!50000UNION*/`), case variation, double URL encoding,
whitespace alternatives (`/**/`, `%09`, `%0a`), hex concatenation, numeric
injection, HTTP parameter pollution, and a JSON/XML body when a form is
expected.

### Confirm, reject, minimize

Confirmed means a differential that repeats: broken input errors, balanced input
does not, or a boolean/time oracle flips deterministically. Encode the ordered
probe trail (payload -> response verdict -> dialect) in `poc_steps` and run
`finding verify`. Extract the minimum proof (DB version, `current_user`, one
metadata row). Do not dump databases.

Reject when the identifier is allowlisted, the value is parameterized, or the
error comes from a fixed query.

## Path traversal and arbitrary file write

Two flavors: read traversal (file disclosure) and **write traversal (arbitrary
file write -> code execution)**. Prioritize write sinks.

### Find the sinks

- Read: static file handlers, `sendFile`, `res.download`, stream readers,
  template loaders, download-by-name.
- Write: `fs.writeFile`/`createWriteStream`, `mkdir`, archive extraction
  (`adm-zip`, `tar`, `unzip`, `decompress`), download-to-path, scaffolders,
  log/cache writers keyed on input.

### Vectors

- `../../../etc/passwd`; encoded `%2e%2e%2f`, double-encoded `%252e`, overlong
  UTF-8; Windows `..\` and mixed `..%5c`.
- **Absolute-path injection:** an input like `/etc/cron.d/x` passed to
  `path.join` or `path.resolve`; know whether the absolute component replaces or
  escapes the base.
- **Zip-slip:** an archive member named `../../x` written during extraction
  without re-validating the resolved path.
- **Symlink in archive:** a member linking to `/`, then a later member writes
  through it.

### Defeat the guard

Broken guards: `replace('../','')` (non-recursive -> `....//`), checking the raw
input instead of the resolved path, `basename()` on only one component, or
decoding after the check. The correct guard resolves to an absolute path and
then asserts it starts with `baseDir + separator`.

### Confirm and reject

- Show the untrusted path -> sink (`file:line`) and that the resolved path
  escapes the base. For writes, name the file overwritten and the resulting
  impact.
- PoC: the exact traversal string plus a sandbox write outside the base, or a
  read returning out-of-tree content. Bound it to one file and a canary.
- Reject when the code resolves first, checks the prefix, and re-validates
  archive entry names against the resolved target.

## Cryptographic misuse (source audit)

Gate every check on **security-relevant** usage. A content checksum with MD5 is
fine; MD5 over a password is not.

1. **Weak hash for a security purpose.** MD5/SHA-1 for passwords, tokens,
   signatures, salts, or key derivation. HMAC-MD5/SHA-1 is keyed and weak.
   Prefer argon2id/scrypt/bcrypt for passwords and SHA-256+/HMAC-SHA-256+ for
   integrity.
2. **Hardcoded key, IV, or secret.** A literal cipher key or a static IV
   (catastrophic with GCM nonce reuse), or a literal HMAC/JWT secret. A fresh
   random IV per message; a secret manager or KMS for keys. Gate on entropy and
   skip placeholders and environment references.
3. **ECB mode.** `aes-*-ecb`/`MODE_ECB` maps equal plaintext blocks to equal
   ciphertext. Prefer AES-GCM, or CBC with a random IV and a MAC.
4. **JWT algorithm confusion.** `alg: none`, an allowlist mixing HS* with
   RS/ES/PS*, or `verify()` without a pinned `algorithms` option. Pin one
   explicit algorithm.
5. **Predictable RNG for secrets.** `Math.random()`, Python `random`, or a
   timestamp used for a token, key, nonce, OTP, or reset value. Use
   `crypto.randomBytes`/`getRandomValues` or Python `secrets`. Do not flag UI
   jitter or sampling.
6. **Weak KDF parameters.** PBKDF2 under ~100k iterations for SHA-256, a short
   or non-random salt. Prefer argon2id/scrypt with tuned cost.
7. **Missing authenticated-decryption checks.** GCM/ChaCha20-Poly1305 decrypt
   that ignores or swallows an auth-tag mismatch.

Proof for crypto misuse is usually source plus a minimal demonstration: forge a
token and submit it, or show a reused IV/ciphertext relationship locally. Do not
operate on live secrets. Use `finding verify` with a local `note`/`shell` step
rather than touching real credentials.

## Race conditions

### Identify race-prone endpoints

Balance/credit transfers, coupon redemption, limited inventory, account changes
(email/password), file upload-then-process, single-use token or OTP validation.

### Make requests arrive together

- **Last-byte synchronization:** open N connections, send the full request except
  the final byte, then release the last byte on all connections at once.
- **HTTP/2 single-packet attack:** multiplex N requests in one TCP packet for the
  tightest timing.
- A bounded local parallel burst (for example 20 concurrent `curl` calls) is
  enough to demonstrate many races; keep it small and rate-limited.

### Patterns

- **Limit overrun / double spend:** N concurrent debits all see the original
  balance. Proof: balance goes negative or the item is purchased more than once
  with one payment.
- **Coupon reuse:** the same single-use code accepted in more than one parallel
  request.
- **Registration/invite race:** all requests pass the "already used" check.
- **TOCTOU file race:** replace a checked file between check and processing.
- **Reset-token race:** use the same token in parallel before invalidation.

### Confirm and bound

- Record pre-race state, the within-milliseconds request timing, and the
  post-race state (negative balance, duplicate records, multiple redemptions).
- Use test accounts and test objects; never race production money or inventory.
  Stop after the smallest state change that proves the issue.
- Encode the pre-check, the parallel burst, and the post-check as `poc_steps`
  and run `finding verify`. Clean up test state if the platform allows it.

Races that use transactions, row locks, or atomic `DECR`/`SETNX` are usually
safe. If the first burst fails, increase concurrency modestly before concluding.

---

_Adapted from 0 (0sec-labs/0), MIT OR Apache-2.0, commit 63e057d._
_Source: skills/vulnerabilities/command-injection.yaml, sqli-advanced.yaml, structural-sqli.yaml, path-traversal.yaml; skills/techniques/crypto-misuse.yaml, race-condition.yaml._
