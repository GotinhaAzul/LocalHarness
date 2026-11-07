# Supply chain review

The question is not "is this package popular?" It is "what code will run, from
where, with which privileges, and can we prove what we shipped?"

## 1. Inventory

- Manifests: `package.json`, `pyproject.toml`/`requirements*.txt`, `Cargo.toml`,
  `go.mod`, `pom.xml`, `Gemfile`, `composer.json`, container base images.
- Lockfiles: `package-lock.json`, `pnpm-lock.yaml`, `yarn.lock`, `poetry.lock`,
  `Cargo.lock`, `go.sum`, `Gemfile.lock`.
- Build files and CI: Dockerfiles, Makefiles, GitHub Actions, install scripts,
  postinstall hooks.
- Artifacts: binaries, wheels, jars, container digests, release checksums.

If there is no lockfile, that is a finding: builds are not reproducible and a
transitive update can change production without a diff.

## 2. Review the change, not only the audit

For a dependency update or a new package:

- **Diff the lockfile**: direct and transitive additions, version jumps,
  integrity hashes, registry URLs, git/file dependencies.
- **Read install hooks**: `preinstall`/`postinstall`, `build.rs`, `setup.py`
  custom commands, Gradle plugins. A package that runs code at install time has
  the CI runner's privileges.
- **Check the source**: repository URL, release tag, maintainer history, signed
  releases, provenance/attestations if available.
- **Typosquatting**: `reqeusts`, `lodahs`, `python-dateutil` vs lookalikes,
  Unicode confusables, scope swaps (`@org/pkg` vs `@0rg/pkg`).
- **Dependency confusion**: internal package names that also exist publicly;
  pin the registry and scope.
- **Abandoned packages**: no release in years, unmaintained, or transferred to a
  new owner. A maintainer change is a supply-chain event.
- **Excessive privilege**: a test-only package in production, a native addon
  that needs a compiler, a CLI that phones home.

## 3. Automated evidence

Use the ecosystem tools when present, and keep their output with the review:

```bash
npm audit --omit=dev
pnpm audit --prod
pip-audit -r requirements.txt
cargo audit
govulncheck ./...
osv-scanner -r .
```

These tools report known advisories, not malicious packages and not logic bugs.
A clean audit is not a clean supply chain.

For containers:

```bash
docker image inspect <image> --format '{{.RepoDigests}}'
trivy image <image>        # if installed
```

Prefer digest pinning (`image@sha256:…`) over mutable tags.

## 4. Build and release integrity

- Are build dependencies pinned and cached with integrity checks?
- Does CI run untrusted pull-request code with repository secrets?
- Are release artifacts built from the tagged commit, or from a developer
  machine?
- Are signatures/attestations verified, or only published?
- Is the package registry the only source, or can a `.npmrc`/`pip.conf` redirect
  to an internal mirror without review?

## 5. Response to a bad dependency

1. Identify the exact versions and the code path that executes.
2. Pin or roll back to a known-good version; do not silently float.
3. Remove the package if it is not required; check transitive users.
4. Rotate credentials available to the install/build environment if install
   hooks ran.
5. Rebuild artifacts from a clean environment; do not trust previously built
   outputs.
6. Add a detection or review rule so the same introduction is caught again.

## 6. Report

For each finding, record:

- Package and version, direct or transitive.
- Where it runs: install time, build time, runtime, CI, container.
- The evidence: lockfile hunk, hook, advisory ID, provenance gap.
- Impact and prerequisites.
- The smallest fix: pin, replace, remove, isolate, rotate.

Do not report "outdated dependency" as a vulnerability without a reachable
advisory or a demonstrated risk.
