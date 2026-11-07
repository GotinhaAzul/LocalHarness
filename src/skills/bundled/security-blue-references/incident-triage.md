# Incident triage

The goal of triage is to decide, quickly and defensibly, whether this is an
incident, what is in scope, and what must be preserved. It is not to fix
everything in the first ten minutes.

## 1. Confirm the signal

- Who or what raised it: user report, alert rule, scanner, EDR, log review?
- What exactly fired: rule name, query, raw event, timestamp.
- Is it reproducible, or was it a single event?
- Is the source trusted? A scanner hitting a decoy is not a compromise.

Write the first line of the timeline before running anything else.

## 2. Fix the scope

Answer these before deep analysis:

- Which host(s), account(s), service(s), and data set(s) are involved?
- Which time window matters? Start with the alert time and widen only when a
  fact requires it.
- Is the system production, shared, or a lab?
- Who owns it, and who can authorize isolation or credential rotation?

If the scope is unknown, say so. A triage that silently assumes one host is
worse than one that states the assumption.

## 3. Preserve volatile evidence first

Order matters because reboots and logins destroy state:

1. Current UTC time and host identity.
2. Logged-in users, sessions, and authentication events.
3. Running processes, parent/child relationships, and command lines.
4. Network sockets, listeners, and established connections.
5. Loaded kernel modules, mounts, and container state.
6. Recent files in `/tmp`, `/var/tmp`, `/dev/shm`, and the service directories.
7. Disk artifacts: copy or hash before deleting or rebuilding.

Use `scripts/host-triage.sh` for a read-only first pass. Save its output to a
file and hash that file if it will be cited as evidence.

Do not run untrusted binaries, "cleanup" tools, or an antivirus scan that
quarantines files before the files are preserved.

## 4. Build the timeline

Use one table, UTC, one row per event:

| Time (UTC) | Source | Event | Confidence |
| --- | --- | --- | --- |
| 2026-11-07T18:30:00Z | auth.log | Accepted publickey for deploy from 10.1.2.3 | observed |
| 2026-11-07T18:31:10Z | process table | `curl` child of `nginx` | observed |
| 2026-11-07T18:32:00Z | inference | Attacker used deploy key to fetch a payload | inferred |

Do not merge rows with different confidence into one "fact".

## 5. Classify the indicators

- **Network**: destination IP/domain, port, protocol, TLS SNI, proxy URL,
  beacon interval, user-agent.
- **Process**: executable path, hash, command line, parent, user, start time.
- **File**: path, hash, size, owner, timestamps, persistence location.
- **Identity**: account, key, token, session, MFA state, privilege change.
- **Persistence**: systemd unit, timer, cron, shell profile, `authorized_keys`,
  `ld.so.preload`, PAM, sudoers, container entrypoint.

Extract and defang them with `iocs`, then record the evidence path.

## 6. Containment decision

| Situation | Default recommendation |
| --- | --- |
| Active remote access or lateral movement | Isolate the host at the network layer, preserve memory/volatile state first, then rotate exposed credentials. |
| Credential theft suspected | Revoke sessions and rotate the credential, after preserving the auth logs. |
| Malware with no active operator | Quarantine the artifact and host for analysis; do not "clean" in place if the host is evidence. |
| Commodity scanner noise | Close the exposure, record the event, no incident unless a follow-up succeeded. |
| Business-critical host | Present the tradeoff; the owner chooses isolation vs monitored containment. |

Containment is a business decision as much as a technical one. State the blast
radius of the proposed action.

## 7. Common Linux persistence to check

- `systemctl list-units --type=service --state=running`, `list-timers --all`
- `/etc/systemd/system`, `/etc/systemd/user`, `~/.config/systemd/user`
- `crontab -l`, `/etc/cron.*`, `/var/spool/cron`
- `~/.ssh/authorized_keys`, `~/.ssh/config`, SSH agent sockets
- `/etc/ld.so.preload`, `LD_PRELOAD` in service units
- `~/.bashrc`, `~/.profile`, `/etc/profile.d`
- `sudoers`, PAM modules, `passwd`/`group` changes
- Container entrypoints, mounted host paths, privileged containers
- Kernel modules and eBPF programs

## 8. Handoff

End triage with:

- Verdict: confirmed incident, suspected, or benign.
- Scope and timeline.
- What was preserved and where.
- What was changed, if anything, and with whose approval.
- Open questions and the next owner.

If the verdict is benign, say which hypothesis was tested and what evidence
ruled it out. "No evidence" is not the same as "not compromised".
