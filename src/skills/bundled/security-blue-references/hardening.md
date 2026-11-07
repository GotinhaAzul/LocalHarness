# Hardening

Hardening is risk reduction with a rollback plan. Every change needs a reason,
an owner, and a way to verify it did not break the service.

## Before changing anything

- Record the current state: config file hashes, listening ports, enabled
  services, users, firewall rules.
- Snapshot or back up the system if the change is broad.
- State the blast radius and the rollback command.
- Prefer one change at a time; do not combine an SSH change with a firewall
  change unless you can afford to lose access.
- Never lock yourself out: keep an existing session open until the new path is
  tested from a second connection.

## Linux baseline

### Accounts and authentication

- No shared accounts; each human has their own key/credential.
- `PermitRootLogin no`; `PasswordAuthentication no` where keys/MFA work.
- `AllowUsers`/`AllowGroups` where practical; deny by default.
- Disable unused accounts; expire stale keys; remove old `authorized_keys`.
- MFA for SSH and privileged consoles where available.
- `sudo` rules are specific commands/groups, not `ALL=(ALL) NOPASSWD: ALL`.
- Review `/etc/passwd`, `/etc/group`, `/etc/sudoers`, `/etc/sudoers.d` for
  unexpected UID 0 accounts and NOPASSWD entries.

### Services and network

- Disable services that are not needed; remove packages that are not used.
- Bind management services to localhost or a management network.
- Default-deny inbound firewall; allow only required ports and sources.
- Review listening sockets: `ss -tulpen`.
- Remove default credentials and sample apps.
- Keep egress in mind: a compromised host needs a way out; restrict where
  possible without breaking updates.

### Kernel and audit

- Enable `auditd` with rules for identity, privilege, persistence paths, and
  exec of interpreters from writable directories.
- Protect boot parameters and kernel module loading where the platform allows.
- Keep `sysctl` changes in a managed file, not ad-hoc commands.
- Ensure time sync; logs without correct time are nearly useless.

### Files and permissions

- World-writable files/directories outside temp are findings.
- SUID/SGID binaries: inventory and justify.
- Secrets are not in world-readable files, shell history, or environment dumps.
- Backups are encrypted and access-controlled; test a restore.

### Containers

- Non-root user, read-only root filesystem where possible.
- Drop capabilities; no `--privileged`; no host PID/network unless required.
- Seccomp/AppArmor/SELinux profiles; no Docker socket mount.
- Pin image digests; scan and rebuild on base-image updates.
- Mount only the paths needed; no host root bind mounts.
- Resource limits: CPU, memory, PID count.

## Web service baseline

- TLS: current protocol versions, valid certificate, HSTS where appropriate.
- Security headers: CSP, `X-Content-Type-Options`, `Referrer-Policy`,
  frame-ancestors, permissions policy. A header is not a substitute for output
  encoding.
- Cookies: `Secure`, `HttpOnly`, `SameSite`, scoped path/domain, short sessions.
- Errors: no stack traces, SQL, internal hostnames, or secrets.
- Admin surfaces: separate host/network, strong auth, audit logging.
- Rate limits on login, reset, OTP, search, and expensive endpoints.
- File uploads: size/type limits, storage outside the web root, random names,
  no execution.
- Dependencies: see `supply-chain.md`.

## Observability

- Authentication and authorization events are logged with actor, target,
  result, source, and UTC time.
- Logs are shipped off-host before an attacker can delete them.
- Alert on the high-value events in `detection-engineering.md`.
- Test that the log pipeline still works after a config change.

## Verification

For every change, record the command that proves it:

```bash
sshd -t                         # syntax before reload
ssh -o BatchMode=yes user@host  # new path from a second session
ss -tulpen                      # listeners after the change
systemctl --failed              # units after the change
```

Reload rather than restart where the service supports it, and keep the old
session open until the new one works.

## Rollback

Write the reverse command before applying the change. If a change cannot be
rolled back, it needs a stronger approval and a longer verification window.
