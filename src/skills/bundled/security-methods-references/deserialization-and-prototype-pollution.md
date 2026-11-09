# Deserialization, prototype pollution, and protocol/spec drift

## Deserialization attack chains

Deserialization of attacker-controlled data is direct code execution when a
gadget chain exists. Identify the format, confirm the parser, and prove with a
benign command.

### Identify the format

- Java: magic `\xac\xed\x00\x05` or base64 `rO0AB`; Content-Type
  `application/x-java-serialized-object`.
- PHP: `O:8:"stdClass":0:{}` in cookies, bodies, or hidden fields.
- Python pickle: base64 blobs in Flask-Session cookies, `.pkl` files, Celery
  task bodies.
- .NET: `__VIEWSTATE`, `BinaryFormatter`/`SoapFormatter` bodies.
- Ruby: `\x04\x08` header in base64 Rack sessions.

### Java

- Match a gadget chain to the classpath. `URLDNS` needs no gadget dependency and
  is the first blind detector: a DNS callback proves deserialization.
- Common chains: CommonsCollections, CommonsBeanutils, Spring, Hibernate.
- Jackson with default typing or `@JsonTypeInfo`, Fastjson `@type`, and JNDI
  lookups through RMI/LDAP are common sinks.
- Other sinks: RMI, JMX, WebLogic T3, JMS, JSF ViewState, Struts2 OGNL.

### PHP

- `unserialize()` plus a POP chain. Build chains for the fingerprinted framework
  (Laravel, Symfony, Monolog, Guzzle, Slim).
- Phar deserialization needs no direct `unserialize` call: upload a phar
  polyglot and trigger any file operation on a `phar://` path. Metadata is
  deserialized automatically.

### Python

- `pickle.loads` on attacker data is direct execution. Common sinks:
  Flask-Session, Redis-cached objects, Celery, `shelve`, `joblib.load`.
- PyYAML `yaml.load` without `SafeLoader` allows
  `!!python/object/apply:os.system ["id"]`.

### .NET

- `BinaryFormatter`, `SoapFormatter`, `LosFormatter`, and ViewState without MAC
  validation.
- Json.NET `TypeNameHandling` other than `None` enables type-driven gadget
  construction.

### Workflow and proof

1. Identify the format from magic bytes, headers, or source patterns.
2. Send a benign baseline blob; the error shape reveals the parser.
3. Start with blind/OOB detection (URLDNS or a DNS/HTTP callback).
4. Match the chain to the fingerprinted library versions.
5. Prove with `id`/`whoami` or a DNS callback, not by reading flags or files.
6. Encode the payload and the observed callback/command output in `poc_steps`
   and run `finding verify`. Keep the payload minimal and non-destructive.

### Reject

Data is not deserialized by an unsafe parser (for example JSON with no type
metadata, `SafeLoader`, or a signed/encrypted blob with a verified MAC). A
format match alone is not a finding.

## JavaScript prototype pollution

### Find merge/assign sinks

- `Object.assign(target, userInput)`
- lodash `_.merge` / `_.defaultsDeep`
- jQuery `$.extend(true, target, userInput)`
- custom recursive `deepMerge`/`deepExtend`/`defaults`
- any endpoint that merges nested JSON into config or options

### Detection

Send `{"__proto__":{"polluted":"yes"}}` and
`{"constructor":{"prototype":{"polluted":"yes"}}}` to merge-capable endpoints.
Confirm by reading a new empty object's `.polluted` property, or by observing an
unexpected field in a later response.

### Impact gadgets

- Node RCE through `child_process` options (`shell`, `argv0`, `NODE_OPTIONS`).
- Template engines: EJS `outputFunctionName`, Pug `block`, Handlebars `main`.
- Client-side XSS through polluted `innerHTML`, jQuery `$.html`, `src`/`href`.
- Auth bypass by polluting `isAdmin`, `role`, `admin`, `verified`, or
  jsonwebtoken's `algorithms` option.

### Confirm and reject

- Prove pollution with a canary property first, then one benign gadget effect
  (a command output, a callback, or an admin page reachable only as a test
  account).
- Encode the pollution request and the post-condition in `poc_steps` and run
  `finding verify`.
- Reject when the merge rejects `__proto__`/`constructor`/`prototype` keys,
  uses `Object.create(null)`, or the polluted property is not consulted on a
  security decision.

## HTTP conformance differential

Many high-impact HTTP bugs are conformance violations: the server parses a
request in a way the spec forbids, and the gap becomes smuggling, cache
poisoning, or desync.

- Send crafted requests that probe specific invariants: CL vs TE precedence,
  header folding, ambiguous request lines, duplicate headers, whitespace before
  the colon, absolute-form targets.
- Diff the real behavior against the spec requirement. A violation is a lead
  toward a concrete attack, not the attack itself.
- Confirm the downstream impact (front-end/back-end disagreement) before
  reporting. Conformant behavior on the probed invariants does not prove the
  whole stack is safe.
- Use `bash` with raw sockets or `curl --http1.1 --raw` so the client does not
  normalize the ambiguity away. Encode the probes in `poc_steps` with a bounded
  timeout and run `finding verify`.

## Specification differential

An API spec states intent; the implementation is reality. Bugs live where they
drift.

- Derive invariants from an OpenAPI/schema document: required auth, allowed
  parameters, response fields, status codes, and object ownership rules.
- Probe the live target for each invariant: an endpoint reachable without the
  required credential (BOLA/BFLA), an undocumented shadow parameter, a response
  field the schema hides, or a status contract the server breaks.
- Each drift is a lead. Confirm the security impact (who can reach it, what it
  exposes) before creating a finding.
- Encode one probe and the observed mismatch in `poc_steps` and run
  `finding verify`. Use only in-scope accounts and read-only requests.

### Failure modes

- Reporting a documentation mismatch with no security impact.
- Treating a conformance probe or a spec drift as confirmed exploitation.
- Sending ambiguous or spec-violating requests to shared production
  infrastructure without a bounded, non-destructive plan.

---

_Adapted from 0 (0sec-labs/0), MIT OR Apache-2.0, commit 63e057d._
_Source: skills/vulnerabilities/deserialization-chains.yaml, prototype-pollution.yaml; skills/techniques/http-conformance-diff.yaml, spec-differential.yaml._
