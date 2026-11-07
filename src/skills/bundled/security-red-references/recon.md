# Reconnaissance

Recon is read-only until you deliberately move to enumeration. Keep every
command and its output; the log is part of the deliverable.

## Before a packet leaves the host

- Confirm the target is in `/sec scope` and in the written rules of engagement.
- Set a rate limit and a time window.
- Prefer the least intrusive tool that answers the question.
- Do not scan a shared/cloud IP range you do not own; confirm the specific host.

## Passive reconnaissance

No traffic to the target:

```bash
dig +short A <target>
dig +short AAAA <target>
dig +short MX <target>
dig +short TXT <target>
dig +short NS <target>
whois <target>
```

Certificate transparency and public archives:

- `crt.sh` for subdomains in issued certificates.
- Search engine results and public code search for leaked hostnames.
- Published OpenAPI/Swagger, sitemap, robots.txt, security.txt.
- Job postings and documentation can reveal technology and internal names, but
  do not treat them as authorization to test the hosts they mention.

Record the source and the retrieval time. A subdomain found in a certificate is
a lead, not an in-scope target until the rules of engagement say it is.

## Active reconnaissance

Start with a small port set and a conservative timing profile:

```bash
nmap -Pn -sV --version-light -p 22,80,443 <target>
nmap -Pn -sV --version-light -p- --min-rate 200 <target>   # only with approval
```

- `-T2` or `--min-rate` with a modest value; never `-T5` on shared
  infrastructure.
- Avoid `--script vuln` and intrusive NSE scripts unless the rules allow them.
- Use `-oA` to save the scan and cite the file.

HTTP fingerprinting:

```bash
curl -sS -D - -o /dev/null https://<target>/
curl -sS -D - -o /dev/null http://<target>/
curl -sS https://<target>/robots.txt
curl -sS https://<target>/.well-known/security.txt
```

TLS:

```bash
openssl s_client -connect <target>:443 -servername <target> </dev/null
```

Look at the certificate chain, protocols, and whether the service redirects
HTTP to HTTPS. Do not run broad TLS vulnerability scanners without approval.

## Content discovery

Only after the rules allow it, and at a low rate:

```bash
ffuf -u https://<target>/FUZZ -w <wordlist> -rate 20 -mc 200,204,301,302,401,403 -o ffuf.json
```

- Use a small, relevant wordlist first.
- Exclude logout, delete, and state-changing endpoints.
- Stop on a 5xx spike or a WAF block; that is the target telling you to slow
  down.
- Do not brute-force credentials, OTPs, or session tokens without explicit
  approval and a lockout plan.

## Web application mapping

- Crawl with a test account and record the authenticated surface.
- List parameters, JSON bodies, headers, cookies, and file uploads.
- Identify roles/tenants and the objects each can reach.
- Read JavaScript bundles for API routes and feature flags (public assets only).
- Check error handling with malformed but harmless input.

## Output

Produce:

- Asset table: host, IP, port, service, version, auth, scope status.
- Endpoint table: method, path, parameters, auth, role, notes.
- Hypotheses ranked by impact and validation cost.
- Raw command/output files as evidence.

Do not start validation until the asset list is stable enough to know what you
are testing.
