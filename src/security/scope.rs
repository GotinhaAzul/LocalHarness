//! Engagement scope: target extraction and CIDR/wildcard matching.

use std::net::IpAddr;

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

/// Authorized targets. Set only by the user, never by the model or a project.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Scope {
    pub targets: Vec<String>,
    pub note: Option<String>,
}

impl Scope {
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub fn add(&mut self, target: &str) -> Result<(), String> {
        let target = target.trim();
        if target.is_empty() {
            return Err("scope target is empty; give a host, CIDR, or domain".into());
        }
        if self.targets.iter().any(|t| t.eq_ignore_ascii_case(target)) {
            return Err(format!("{target} is already in scope"));
        }
        self.targets.push(target.to_string());
        Ok(())
    }

    pub fn remove(&mut self, target: &str) -> bool {
        let before = self.targets.len();
        self.targets
            .retain(|t| !t.eq_ignore_ascii_case(target.trim()));
        self.targets.len() != before
    }

    pub fn clear(&mut self) {
        self.targets.clear();
    }

    /// Whether `target` matches an entry exactly, by CIDR, or by wildcard.
    pub fn allows(&self, target: &str) -> bool {
        let normalized = normalize_target(target);
        if normalized.is_empty() {
            return false;
        }
        let ip = parse_ip(&normalized);
        for entry in &self.targets {
            let entry = normalize_target(entry);
            if entry.is_empty() {
                continue;
            }
            if entry.eq_ignore_ascii_case(&normalized) {
                return true;
            }
            if let Some(net) = entry.parse::<IpNet>().ok()
                && let Some(ip) = ip
                && net.contains(&ip)
            {
                return true;
            }
            if entry.starts_with("*.")
                && normalized.len() > entry.len() - 1
                && normalized
                    .to_ascii_lowercase()
                    .ends_with(&entry[1..].to_ascii_lowercase())
            {
                return true;
            }
        }
        false
    }

    /// Check every target; the first out-of-scope target is reported.
    pub fn check(&self, targets: &[String]) -> Result<(), ScopeViolation> {
        for target in targets {
            if !self.allows(target) {
                return Err(ScopeViolation {
                    target: target.clone(),
                    scope_empty: self.is_empty(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeViolation {
    pub target: String,
    pub scope_empty: bool,
}

impl std::fmt::Display for ScopeViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.scope_empty {
            write!(
                f,
                "no engagement scope is set; add the target before running this command: /sec scope add {}",
                self.target
            )
        } else {
            write!(
                f,
                "{} is out of scope; add it with /sec scope add {} or use an in-scope target",
                self.target, self.target
            )
        }
    }
}

fn normalize_target(target: &str) -> String {
    let target = target
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == ',' || c == ';');
    let target = match target.split_once("://") {
        Some((_, rest)) => rest,
        None => target,
    };
    // CIDR ranges keep their prefix; only URLs and host:port forms lose paths.
    if target.parse::<IpNet>().is_ok() {
        return target.to_string();
    }
    let target = target
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(target)
        .split('@')
        .next_back()
        .unwrap_or(target);
    // Strip a port unless this is a bare IPv6 address.
    if target.matches(':').count() == 1
        && let Some((host, port)) = target.rsplit_once(':')
        && !host.is_empty()
        && port.chars().all(|c| c.is_ascii_digit())
    {
        return host.trim_end_matches('.').to_string();
    }
    target.trim_end_matches('.').to_string()
}

fn parse_ip(value: &str) -> Option<IpAddr> {
    value.parse::<IpAddr>().ok()
}

/// Commands whose targets are checked against scope.
const SECURITY_TOOLS: &[&str] = &[
    "nmap",
    "masscan",
    "sqlmap",
    "nikto",
    "nuclei",
    "ffuf",
    "gobuster",
    "feroxbuster",
    "dirb",
    "dirsearch",
    "hydra",
    "medusa",
    "msfconsole",
    "msfvenom",
    "meterpreter",
    "responder",
    "crackmapexec",
    "netexec",
    "bloodhound",
    "certipy",
    "rubeus",
    "chisel",
    "ligolo",
    "pangolin",
];

/// Extract candidate targets from a shell command. Best-effort by design.
pub fn extract_targets(command: &str) -> Vec<String> {
    let mut targets = Vec::new();
    for raw in command.split_whitespace() {
        let token = raw.trim_matches(|c: char| {
            c == '"' || c == '\'' || c == ',' || c == ';' || c == '(' || c == ')'
        });
        if token.is_empty()
            || token.starts_with('-')
            || token.starts_with('<')
            || token.starts_with('>')
        {
            continue;
        }
        // Skip shell operators and assignments.
        if matches!(token, "|" | "||" | "&&" | ";" | "&" | ">" | ">>" | "<") {
            continue;
        }
        if token.contains('=') && !token.contains("://") {
            continue;
        }
        if token.starts_with('/') || token.starts_with('.') || token.starts_with('$') {
            continue;
        }
        let candidate = normalize_target(token);
        if candidate.is_empty() {
            continue;
        }
        let looks_like_ip = candidate.parse::<IpNet>().is_ok();
        let looks_like_url = token.contains("://");
        let looks_like_domain = !looks_like_ip
            && candidate.contains('.')
            && candidate
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || c == ':');
        if (looks_like_ip || looks_like_url || looks_like_domain)
            && !targets
                .iter()
                .any(|t: &String| t.eq_ignore_ascii_case(&candidate))
        {
            targets.push(candidate);
        }
    }
    targets
}

/// True when the command invokes a known offensive tool.
pub fn is_security_command(command: &str) -> bool {
    command
        .split_whitespace()
        .map(|token| token.rsplit('/').next().unwrap_or(token))
        .any(|token| {
            SECURITY_TOOLS.contains(&token)
                || (token.starts_with("impacket-") && token.len() > "impacket-".len())
        })
}

/// Scope gate for `bash` and `bg`. `--help` and `--version` are always allowed.
pub fn gate_command(command: &str, scope: &Scope) -> Result<(), String> {
    if !is_security_command(command) {
        return Ok(());
    }
    let tokens: Vec<&str> = command.split_whitespace().collect();
    if tokens
        .iter()
        .any(|t| *t == "--help" || *t == "-h" || *t == "--version" || *t == "-V")
    {
        return Ok(());
    }
    let targets = extract_targets(command);
    if targets.is_empty() {
        return Ok(());
    }
    scope
        .check(&targets)
        .map_err(|violation| violation.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_cidr_and_wildcard_match() {
        let mut scope = Scope::default();
        scope.add("10.0.0.0/24").unwrap();
        scope.add("*.example.com").unwrap();
        scope.add("target.test").unwrap();
        assert!(scope.allows("10.0.0.7"));
        assert!(!scope.allows("10.0.1.7"));
        assert!(scope.allows("api.example.com"));
        assert!(!scope.allows("example.com.evil"));
        assert!(scope.allows("TARGET.test"));
        assert!(scope.allows("https://target.test:8443/path"));
    }

    #[test]
    fn extracts_ips_urls_domains() {
        let targets =
            extract_targets("nmap -sV 10.0.0.5 https://app.example.com:443/x example.org");
        assert!(targets.contains(&"10.0.0.5".to_string()));
        assert!(targets.contains(&"app.example.com".to_string()));
        assert!(targets.contains(&"example.org".to_string()));
    }

    #[test]
    fn security_gate_blocks_out_of_scope() {
        let mut scope = Scope::default();
        scope.add("10.0.0.0/24").unwrap();
        assert!(gate_command("nmap 10.0.0.1", &scope).is_ok());
        let err = gate_command("nmap 192.168.1.1", &scope).unwrap_err();
        assert!(err.contains("out of scope"));
        assert!(gate_command("nmap --help", &scope).is_ok());
        assert!(gate_command("cargo test", &scope).is_ok());
    }

    #[test]
    fn empty_scope_blocks_and_names_call() {
        let scope = Scope::default();
        let err = gate_command("sqlmap -u https://victim.test", &scope).unwrap_err();
        assert!(err.contains("/sec scope add"));
    }
}
