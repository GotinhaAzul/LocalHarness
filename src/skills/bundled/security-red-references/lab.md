# Local lab and CTF

A lab is for practice and for reproducing a finding safely. It is not a place to
store real credentials or to reach production.

## Isolation

- Use a dedicated VM, a separate physical host, or a project-scoped container
  network.
- Take a snapshot before testing; restore after each exercise.
- No production credentials, SSH keys, cloud tokens, or mounted home directories
  inside the lab.
- Restrict egress. Pull images before isolating if the lab host needs registry
  access.
- Bind published ports to `127.0.0.1`, not `0.0.0.0`.
- Keep the lab off the corporate LAN unless the engagement explicitly allows it.

## Container lab

Pull images while you still have network access:

```bash
docker pull bkimminich/juice-shop
docker pull vulnerables/web-dvwa
docker pull webgoat/webgoat
```

Create a dedicated network and bind only to localhost:

```bash
docker network create sec-lab
docker run -d --name juice --network sec-lab -p 127.0.0.1:3000:3000 bkimminich/juice-shop
docker run -d --name dvwa --network sec-lab -p 127.0.0.1:8080:80 vulnerables/web-dvwa
docker run -d --name webgoat --network sec-lab -p 127.0.0.1:8081:8080 webgoat/webgoat
```

Check what is exposed:

```bash
docker ps --format 'table {{.Names}}\t{{.Ports}}'
ss -tulpen | grep -E '3000|8080|8081'
```

If a container needs to reach another lab service by name, use the `sec-lab`
network. Do not add `--privileged`, host networking, or a Docker socket mount.

## Vulnerable targets

- **OWASP Juice Shop** — modern web app, broad OWASP Top 10 coverage.
- **DVWA** — classic PHP app with adjustable difficulty.
- **OWASP WebGoat** — guided lessons.
- **VulnHub / OWASP Broken Web Apps** — VM images for host and web practice.
- **CTF platforms** (Hack The Box, TryHackMe, local CTFs) — use only within the
  platform's rules and your account's scope.

## Practice rules

- Start with the lowest difficulty and a narrow target.
- Record the objective, the commands, and the result; practice is more useful
  when it produces a report.
- Stop when the flag or proof is obtained. Do not pivot to unrelated hosts in
  the lab unless the exercise asks for it.
- Do not use real malware or public exploit code against a lab without reading
  it first; understand what it changes.
- Restore the snapshot after experiments that modify the target.

## Cleanup

```bash
docker rm -f juice dvwa webgoat
docker network rm sec-lab
```

Remove test files, callbacks, browser profiles, and any credentials created for
the lab. Confirm no listener or container is still running.

## From lab to engagement

A lab finding is not an engagement finding. Re-validate on the authorized target
with the rules of engagement in force. Label lab evidence as lab evidence in the
report, and never imply it came from the client's environment.
