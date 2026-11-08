//! Normalized attack-surface inventory.
//!
//! Command output is ingested after every evidence-worthy tool run. Parsers
//! cover nmap XML/grepable/normal output, URLs, HTTP headers, and TLS
//! fingerprints. Only assets inside the engagement scope are stored when a
//! scope is configured; identities and credentials are kept as references,
//! never plaintext secrets.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::security::evidence::sha256_hex;
use crate::security::scope::{Scope, is_security_command};

const MAX_ASSETS: usize = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Port {
    pub protocol: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub product: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default)]
    pub tls: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub host: String,
    #[serde(default)]
    pub in_scope: bool,
    #[serde(default)]
    pub aliases: BTreeSet<String>,
    /// Keyed by `port/protocol`, for example `443/tcp`.
    #[serde(default)]
    pub ports: BTreeMap<String, Port>,
    #[serde(default)]
    pub urls: BTreeSet<String>,
    #[serde(default)]
    pub technologies: BTreeSet<String>,
    #[serde(default)]
    pub identities: BTreeSet<String>,
    /// Hashed references only: `cred:<sha256 prefix>`.
    #[serde(default)]
    pub credentials: BTreeSet<String>,
    #[serde(default)]
    pub evidence: BTreeSet<String>,
    #[serde(default = "Utc::now")]
    pub updated: DateTime<Utc>,
}

impl Asset {
    fn new(host: &str, in_scope: bool) -> Self {
        Self {
            host: host.to_string(),
            in_scope,
            aliases: BTreeSet::new(),
            ports: BTreeMap::new(),
            urls: BTreeSet::new(),
            technologies: BTreeSet::new(),
            identities: BTreeSet::new(),
            credentials: BTreeSet::new(),
            evidence: BTreeSet::new(),
            updated: Utc::now(),
        }
    }

    pub fn port_summary(&self) -> String {
        if self.ports.is_empty() {
            return "no open ports".into();
        }
        self.ports
            .iter()
            .map(|(key, port)| {
                let label = if !port.service.is_empty() {
                    port.service.clone()
                } else {
                    key.clone()
                };
                format!("{key} {label}")
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct SurfaceFile {
    #[serde(default)]
    assets: BTreeMap<String, Asset>,
}

#[derive(Debug, Default)]
pub struct SurfaceMap {
    path: PathBuf,
    assets: BTreeMap<String, Asset>,
}

impl SurfaceMap {
    pub fn open(engagement_dir: &Path) -> Result<Self> {
        let path = engagement_dir.join("surface.json");
        let assets = if path.exists() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            let file: SurfaceFile = serde_json::from_str(&raw)
                .with_context(|| format!("invalid surface inventory {}", path.display()))?;
            file.assets
        } else {
            BTreeMap::new()
        };
        Ok(Self { path, assets })
    }

    pub fn assets(&self) -> &BTreeMap<String, Asset> {
        &self.assets
    }

    pub fn get(&self, host: &str) -> Option<&Asset> {
        let key = self.resolve_key(host)?;
        self.assets.get(&key)
    }

    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }

    pub fn service_count(&self) -> usize {
        self.assets.values().map(|asset| asset.ports.len()).sum()
    }

    pub fn technology_count(&self) -> usize {
        self.assets
            .values()
            .flat_map(|asset| asset.technologies.iter())
            .collect::<BTreeSet<_>>()
            .len()
    }

    fn resolve_key(&self, host: &str) -> Option<String> {
        let host = normalize_host(host);
        if host.is_empty() {
            return None;
        }
        if let Some(key) = self
            .assets
            .keys()
            .find(|key| key.eq_ignore_ascii_case(&host))
        {
            return Some(key.clone());
        }
        self.assets
            .iter()
            .find(|(_, asset)| {
                asset
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(&host))
            })
            .map(|(key, _)| key.clone())
    }

    fn can_add(&self, host: &str) -> bool {
        self.resolve_key(host).is_some() || self.assets.len() < MAX_ASSETS
    }

    fn asset_mut(&mut self, host: &str, in_scope: bool) -> &mut Asset {
        let host = normalize_host(host);
        let key = self.resolve_key(&host).unwrap_or_else(|| host.clone());
        if !self.assets.contains_key(&key) {
            self.assets.insert(key.clone(), Asset::new(&key, in_scope));
        }
        self.assets.get_mut(&key).expect("inserted above")
    }

    /// Ingest one command output. Returns the number of observations merged.
    pub fn ingest(
        &mut self,
        command: &str,
        output: &str,
        scope: &Scope,
        evidence_id: &str,
    ) -> usize {
        let mut observations = 0usize;
        let security_command = is_security_command(command);
        let primary_host = command
            .split_whitespace()
            .find_map(|token| host_from_url(token).map(|parsed| parsed.host))
            .or_else(|| {
                crate::security::extract_targets(command)
                    .into_iter()
                    .find_map(|target| {
                        let host = normalize_host(&target);
                        (!host.is_empty()).then_some(host)
                    })
            });

        let mut parsed = parse_nmap_xml(output);
        if parsed.is_empty() {
            parsed.extend(parse_nmap_text(output));
        }
        for observation in parsed {
            if let Some(count) =
                self.merge_observation(observation, scope, evidence_id, primary_host.as_deref())
            {
                observations += count;
            }
        }

        let web_command = security_command
            || command.contains("curl")
            || command.contains("wget")
            || output.contains("HTTP/")
            || output.contains("TLS");
        if web_command {
            for parsed in extract_urls(command)
                .into_iter()
                .chain(extract_urls(output))
            {
                let Some(count) = self.merge_url(parsed, scope, evidence_id) else {
                    continue;
                };
                observations += count;
            }
        }

        if security_command
            || output.contains("HTTP/")
            || output.contains("Server:")
            || output.contains("TLS")
        {
            if let Some(host) = primary_host.clone() {
                let headers = extract_headers(output);
                if !headers.is_empty()
                    && let Some(count) = self.merge_headers(&host, &headers, scope, evidence_id)
                {
                    observations += count;
                }
            }
            let tls = extract_tls(output);
            if !tls.is_empty()
                && let Some(host) = primary_host.clone()
                && let Some(count) = self.merge_technology(&host, &tls, scope, evidence_id)
            {
                observations += count;
            }
        }

        if security_command {
            for (host, identity) in extract_identities(output) {
                let host = if host.is_empty() {
                    primary_host.clone().unwrap_or_default()
                } else {
                    host
                };
                if host.is_empty() {
                    continue;
                }
                if let Some(count) = self.merge_identity(&host, &identity, scope, evidence_id) {
                    observations += count;
                }
            }
            for (host, credential) in extract_credentials(output) {
                let host = if host.is_empty() {
                    primary_host.clone().unwrap_or_default()
                } else {
                    host
                };
                if host.is_empty() {
                    continue;
                }
                if let Some(count) = self.merge_credential(&host, &credential, scope, evidence_id) {
                    observations += count;
                }
            }
        }

        if observations > 0
            && let Err(error) = self.save()
        {
            tracing::warn!(%error, "surface inventory was not saved");
            return 0;
        }
        observations
    }

    fn merge_observation(
        &mut self,
        observation: Observation,
        scope: &Scope,
        evidence_id: &str,
        primary_host: Option<&str>,
    ) -> Option<usize> {
        let mut hosts = vec![observation.host.clone()];
        hosts.extend(observation.aliases.iter().cloned());
        let host = hosts
            .iter()
            .find(|host| scope.is_empty() || scope.allows(host))
            .cloned()
            .unwrap_or_else(|| observation.host.clone());
        if !scope.is_empty() && !hosts.iter().any(|host| scope.allows(host)) {
            return None;
        }
        let in_scope = !scope.is_empty() && hosts.iter().any(|host| scope.allows(host));
        if !self.can_add(&host) {
            return None;
        }
        let mut count = 1usize;
        let asset = self.asset_mut(&host, in_scope);
        asset.in_scope |= in_scope;
        asset.evidence.insert(evidence_id.to_string());
        asset.updated = Utc::now();
        for alias in &observation.aliases {
            asset.aliases.insert(alias.clone());
        }
        if let Some(primary) = primary_host
            && !primary.eq_ignore_ascii_case(&host)
        {
            asset.aliases.insert(primary.to_string());
        }
        for (key, port) in observation.ports {
            let existing = asset.ports.entry(key).or_insert_with(|| Port {
                protocol: port.protocol.clone(),
                ..Port::default()
            });
            if !port.service.is_empty() {
                existing.service = port.service;
            }
            if !port.product.is_empty() {
                existing.product = port.product;
            }
            if !port.version.is_empty() {
                existing.version = port.version;
            }
            existing.tls |= port.tls;
            count += 1;
        }
        for url in observation.urls {
            if asset.urls.insert(url) {
                count += 1;
            }
        }
        for tech in observation.technologies {
            if insert_ci(&mut asset.technologies, tech) {
                count += 1;
            }
        }
        Some(count)
    }

    fn merge_url(&mut self, parsed: ParsedUrl, scope: &Scope, evidence_id: &str) -> Option<usize> {
        if !scope.is_empty() && !scope.allows(&parsed.host) {
            return None;
        }
        let in_scope = !scope.is_empty() && scope.allows(&parsed.host);
        if !self.can_add(&parsed.host) {
            return None;
        }
        let mut count = 0usize;
        let asset = self.asset_mut(&parsed.host, in_scope);
        asset.in_scope |= in_scope;
        asset.evidence.insert(evidence_id.to_string());
        if asset.urls.insert(parsed.clean_url.clone()) {
            count += 1;
        }
        if let Some(port) = parsed.port.or(match parsed.scheme.as_str() {
            "https" => Some(443),
            "http" => Some(80),
            _ => None,
        }) {
            let key = format!("{port}/tcp");
            let protocol = if parsed.scheme == "https" {
                "https"
            } else {
                "http"
            };
            let port_entry = asset.ports.entry(key).or_insert_with(|| Port {
                protocol: "tcp".into(),
                ..Port::default()
            });
            port_entry.tls |= parsed.scheme == "https";
            if port_entry.service.is_empty() {
                port_entry.service = protocol.to_string();
            }
            count += 1;
        }
        if let Some(identity) = parsed.identity
            && insert_ci(&mut asset.identities, identity)
        {
            count += 1;
        }
        if let Some(credential) = parsed.credential
            && asset.credentials.insert(credential)
        {
            count += 1;
        }
        asset.updated = Utc::now();
        Some(count)
    }

    fn merge_headers(
        &mut self,
        host: &str,
        headers: &[String],
        scope: &Scope,
        evidence_id: &str,
    ) -> Option<usize> {
        if !scope.is_empty() && !scope.allows(host) {
            return None;
        }
        let in_scope = !scope.is_empty() && scope.allows(host);
        if !self.can_add(host) {
            return None;
        }
        let asset = self.asset_mut(host, in_scope);
        asset.in_scope |= in_scope;
        asset.evidence.insert(evidence_id.to_string());
        let mut count = 0usize;
        for header in headers {
            if insert_ci(&mut asset.technologies, header.clone()) {
                count += 1;
            }
        }
        asset.updated = Utc::now();
        Some(count)
    }

    fn merge_technology(
        &mut self,
        host: &str,
        technologies: &[String],
        scope: &Scope,
        evidence_id: &str,
    ) -> Option<usize> {
        if !scope.is_empty() && !scope.allows(host) {
            return None;
        }
        let in_scope = !scope.is_empty() && scope.allows(host);
        if !self.can_add(host) {
            return None;
        }
        let asset = self.asset_mut(host, in_scope);
        asset.in_scope |= in_scope;
        asset.evidence.insert(evidence_id.to_string());
        let mut count = 0usize;
        for technology in technologies {
            if insert_ci(&mut asset.technologies, technology.clone()) {
                count += 1;
            }
        }
        asset.updated = Utc::now();
        Some(count)
    }

    fn merge_identity(
        &mut self,
        host: &str,
        identity: &str,
        scope: &Scope,
        evidence_id: &str,
    ) -> Option<usize> {
        if !scope.is_empty() && !scope.allows(host) {
            return None;
        }
        let in_scope = !scope.is_empty() && scope.allows(host);
        if !self.can_add(host) {
            return None;
        }
        let asset = self.asset_mut(host, in_scope);
        asset.in_scope |= in_scope;
        asset.evidence.insert(evidence_id.to_string());
        Some(usize::from(insert_ci(
            &mut asset.identities,
            identity.to_string(),
        )))
    }

    fn merge_credential(
        &mut self,
        host: &str,
        credential: &str,
        scope: &Scope,
        evidence_id: &str,
    ) -> Option<usize> {
        if !scope.is_empty() && !scope.allows(host) {
            return None;
        }
        let in_scope = !scope.is_empty() && scope.allows(host);
        if !self.can_add(host) {
            return None;
        }
        let asset = self.asset_mut(host, in_scope);
        asset.in_scope |= in_scope;
        asset.evidence.insert(evidence_id.to_string());
        Some(usize::from(
            asset.credentials.insert(credential.to_string()),
        ))
    }

    fn save(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        let raw = serde_json::to_string_pretty(&SurfaceFile {
            assets: self.assets.clone(),
        })
        .context("cannot serialize surface inventory")?;
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, raw).with_context(|| format!("cannot write {}", temp.display()))?;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&temp, &self.path)
            .with_context(|| format!("cannot replace {}", self.path.display()))?;
        Ok(())
    }
}

fn insert_ci(set: &mut BTreeSet<String>, value: String) -> bool {
    if value.trim().is_empty() {
        return false;
    }
    if set
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(&value))
    {
        return false;
    }
    set.insert(value)
}

#[derive(Debug, Clone, Default)]
struct Observation {
    host: String,
    aliases: Vec<String>,
    ports: BTreeMap<String, Port>,
    urls: BTreeSet<String>,
    technologies: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct ParsedUrl {
    host: String,
    port: Option<u16>,
    scheme: String,
    clean_url: String,
    identity: Option<String>,
    credential: Option<String>,
}

fn parse_nmap_xml(output: &str) -> Vec<Observation> {
    if !output.contains("<nmaprun") {
        return Vec::new();
    }
    let host_re = regex(r#"(?s)<host\b.*?</host>"#);
    let addr_re = regex(r#"(?i)<address\s+addr="([^"]+)"[^>]*addrtype="(ipv4|ipv6)""#);
    let hostname_re = regex(r#"(?i)<hostname\s+name="([^"]+)""#);
    let port_re = regex(
        r#"(?s)<port\s+protocol="([^"]+)"\s+portid="(\d+)".*?<state\s+state="([^"]+)".*?<service\b([^>]*)>"#,
    );
    let attr_re = regex(r#"(\w+)="([^"]*)""#);
    let mut observations = Vec::new();
    for found in host_re.find_iter(output) {
        let block = found.as_str();
        let ip = addr_re
            .captures(block)
            .and_then(|captures| captures.get(1))
            .map(|match_| match_.as_str().to_string());
        let hostname = hostname_re
            .captures(block)
            .and_then(|captures| captures.get(1))
            .map(|match_| normalize_host(match_.as_str()));
        let Some(host) = hostname.clone().or_else(|| ip.clone()) else {
            continue;
        };
        let mut observation = Observation {
            host: host.clone(),
            ..Observation::default()
        };
        if let Some(ip) = ip
            && !ip.eq_ignore_ascii_case(&host)
        {
            observation.aliases.push(ip);
        }
        for captures in port_re.captures_iter(block) {
            let protocol = captures.get(1).map(|m| m.as_str()).unwrap_or("tcp");
            let port = captures.get(2).map(|m| m.as_str()).unwrap_or("0");
            let state = captures.get(3).map(|m| m.as_str()).unwrap_or("");
            if state != "open" && state != "open|filtered" {
                continue;
            }
            let Ok(port_number) = port.parse::<u16>() else {
                continue;
            };
            let attrs = captures.get(4).map(|m| m.as_str()).unwrap_or("");
            let mut service = Port {
                protocol: protocol.to_string(),
                ..Port::default()
            };
            for attr in attr_re.captures_iter(attrs) {
                let key = attr.get(1).map(|m| m.as_str()).unwrap_or("");
                let value = attr.get(2).map(|m| m.as_str()).unwrap_or("");
                match key {
                    "name" => service.service = value.to_string(),
                    "product" => service.product = value.to_string(),
                    "version" => service.version = value.to_string(),
                    "tunnel" if value.eq_ignore_ascii_case("ssl") => service.tls = true,
                    _ => {}
                }
            }
            observation
                .ports
                .insert(format!("{port_number}/{protocol}"), service);
        }
        observations.push(observation);
    }
    observations
}

fn parse_nmap_text(output: &str) -> Vec<Observation> {
    let mut observations: Vec<Observation> = Vec::new();
    let mut current: Option<usize> = None;
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Nmap scan report for ") {
            let (host, alias) = parse_scan_report(rest);
            if host.is_empty() {
                current = None;
                continue;
            }
            observations.push(Observation {
                host: host.clone(),
                ..Observation::default()
            });
            let index = observations.len() - 1;
            if let Some(alias) = alias
                && !alias.eq_ignore_ascii_case(&host)
            {
                observations[index].aliases.push(alias);
            }
            current = Some(index);
            continue;
        }
        if let Some(index) = current {
            if let Some(captures) = port_line(trimmed) {
                observations[index]
                    .ports
                    .insert(format!("{}/{}", captures.0, captures.1), captures.2);
            } else if trimmed.starts_with("Host:") {
                current = None;
            }
        } else if let Some((_, ports)) = trimmed.split_once("Ports: ") {
            let mut tokens = trimmed.split_whitespace();
            if tokens.next() != Some("Host:") {
                continue;
            }
            let Some(host) = tokens.next() else {
                continue;
            };
            let host = normalize_host(host);
            let mut observation = Observation {
                host,
                ..Observation::default()
            };
            for item in ports.split(',') {
                let item = item.trim();
                let fields: Vec<&str> = item.split('/').collect();
                if fields.len() >= 5 && fields[1] == "open" {
                    let Ok(port) = fields[0].parse::<u16>() else {
                        continue;
                    };
                    let protocol = fields[2];
                    let mut service = Port {
                        protocol: protocol.to_string(),
                        service: fields[4].to_string(),
                        ..Port::default()
                    };
                    if fields.len() > 6 && !fields[6].is_empty() {
                        service.product = fields[6].to_string();
                    }
                    observation
                        .ports
                        .insert(format!("{port}/{protocol}"), service);
                }
            }
            observations.push(observation);
        }
    }
    observations
}

fn parse_scan_report(rest: &str) -> (String, Option<String>) {
    if let Some((name, address)) = rest.split_once(" (")
        && let Some(address) = address.strip_suffix(')')
    {
        return (normalize_host(name), Some(normalize_host(address)));
    }
    (normalize_host(rest), None)
}

fn port_line(line: &str) -> Option<(u16, String, Port)> {
    let mut parts = line.split_whitespace();
    let port_proto = parts.next()?;
    let (port, protocol) = port_proto.split_once('/')?;
    let state = parts.next()?;
    if state != "open" && state != "open|filtered" {
        return None;
    }
    let port: u16 = port.parse().ok()?;
    let service = parts.next().unwrap_or("").to_string();
    let mut port_info = Port {
        protocol: protocol.to_string(),
        service: service.clone(),
        ..Port::default()
    };
    let rest: Vec<&str> = parts.collect();
    if !rest.is_empty() {
        port_info.product = rest[0].to_string();
    }
    if rest.len() > 1 {
        port_info.version = rest[1].to_string();
    }
    Some((port, protocol.to_string(), port_info))
}

fn extract_urls(output: &str) -> Vec<ParsedUrl> {
    let re = regex(r#"https?://[^\s"'<>]+"#);
    let mut seen = BTreeSet::new();
    let mut urls = Vec::new();
    for found in re.find_iter(output) {
        let raw = found.as_str().trim_end_matches(['.', ',', ';', ')', ']']);
        if let Some(parsed) = parse_url(raw)
            && seen.insert(parsed.clean_url.clone())
        {
            urls.push(parsed);
        }
    }
    urls
}

fn parse_url(raw: &str) -> Option<ParsedUrl> {
    let (scheme, rest) = raw.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo), hostport),
        None => (None, authority),
    };
    let (host, port) = split_host_port(hostport)?;
    let host = normalize_host(&host);
    if host.is_empty() {
        return None;
    }
    let mut identity = None;
    let mut credential = None;
    if let Some(userinfo) = userinfo {
        let (user, password) = match userinfo.split_once(':') {
            Some((user, password)) => (user, Some(password)),
            None => (userinfo, None),
        };
        if !user.is_empty() {
            identity = Some(user.to_string());
        }
        if let Some(password) = password
            && !password.is_empty()
        {
            credential = Some(credential_ref(&format!("{user}:{password}")));
        }
    }
    let clean_url = format!("{scheme}://{hostport}{}", path_suffix(rest));
    Some(ParsedUrl {
        host,
        port,
        scheme,
        clean_url,
        identity,
        credential,
    })
}

fn path_suffix(rest: &str) -> &str {
    rest.find(['/', '?', '#'])
        .map(|index| &rest[index..])
        .unwrap_or("")
}

fn split_host_port(hostport: &str) -> Option<(String, Option<u16>)> {
    if let Some(rest) = hostport.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once(']')
    {
        let port = tail
            .strip_prefix(':')
            .and_then(|port| port.parse::<u16>().ok());
        return Some((host.to_string(), port));
    }
    if hostport.matches(':').count() == 1
        && let Some((host, port)) = hostport.rsplit_once(':')
        && port.chars().all(|character| character.is_ascii_digit())
    {
        return Some((host.to_string(), port.parse().ok()));
    }
    Some((hostport.to_string(), None))
}

fn host_from_url(raw: &str) -> Option<ParsedUrl> {
    parse_url(raw)
}

fn extract_headers(output: &str) -> Vec<String> {
    let server = regex(r#"(?im)^server:\s*(.+)$"#);
    let powered = regex(r#"(?im)^x-powered-by:\s*(.+)$"#);
    let generator = regex(r#"(?im)^x-generator:\s*(.+)$"#);
    let mut headers = Vec::new();
    for found in server.find_iter(output) {
        if let Some((_, value)) = found.as_str().split_once(':') {
            headers.push(value.trim().to_string());
        }
    }
    for found in powered.find_iter(output) {
        if let Some((_, value)) = found.as_str().split_once(':') {
            headers.push(value.trim().to_string());
        }
    }
    for found in generator.find_iter(output) {
        if let Some((_, value)) = found.as_str().split_once(':') {
            headers.push(value.trim().to_string());
        }
    }
    headers
}

fn extract_tls(output: &str) -> Vec<String> {
    let tls = regex(r#"TLSv1\.[0-9]"#);
    let cipher = regex(r#"TLS_[A-Z0-9_]+"#);
    let mut technologies = Vec::new();
    if tls.is_match(output) {
        technologies.push("TLS".to_string());
        if let Some(found) = tls.find(output) {
            technologies.push(found.as_str().to_string());
        }
    }
    if let Some(found) = cipher.find(output) {
        technologies.push(found.as_str().to_string());
    }
    technologies
}

fn extract_identities(output: &str) -> Vec<(String, String)> {
    let marker =
        regex(r#"(?im)^\s*(?:user(?:name)?|identity)\s*[:=]\s*([A-Za-z0-9._@\\-]{1,64})\s*$"#);
    let urls = extract_urls(output);
    let mut identities = Vec::new();
    for found in marker.captures_iter(output) {
        if let Some(identity) = found.get(1) {
            identities.push((String::new(), identity.as_str().to_string()));
        }
    }
    for url in urls {
        if let Some(identity) = url.identity {
            identities.push((url.host, identity));
        }
    }
    identities
}

fn extract_credentials(output: &str) -> Vec<(String, String)> {
    let marker = regex(
        r#"(?im)^\s*(?:password|passwd|pass|token|secret|ntlm|hash)\s*[:=]\s*([^\s,;]{3,128})"#,
    );
    let urls = extract_urls(output);
    let mut credentials = Vec::new();
    for found in marker.captures_iter(output) {
        if let Some(credential) = found.get(1) {
            credentials.push(("".to_string(), credential_ref(credential.as_str())));
        }
    }
    for url in urls {
        if let Some(credential) = url.credential {
            credentials.push((url.host, credential));
        }
    }
    credentials
}

fn credential_ref(value: &str) -> String {
    let digest = sha256_hex(value.as_bytes());
    format!("cred:{}", &digest[..16])
}

pub fn normalize_host(raw: &str) -> String {
    let raw = raw
        .trim()
        .trim_matches(|character| {
            character == '"' || character == '\'' || character == ',' || character == ';'
        })
        .trim_end_matches('.');
    let raw = raw.split_once("://").map(|(_, rest)| rest).unwrap_or(raw);
    let authority = raw.split(['/', '?', '#']).next().unwrap_or(raw);
    let authority = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split_once(']').map(|(host, _)| host).unwrap_or(rest)
    } else if authority.matches(':').count() == 1
        && let Some((host, port)) = authority.rsplit_once(':')
        && port.chars().all(|character| character.is_ascii_digit())
    {
        host
    } else {
        authority
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}

pub fn host_matches(asset: &Asset, target: &str) -> bool {
    let host = normalize_host(target);
    if host.is_empty() {
        return false;
    }
    asset.host.eq_ignore_ascii_case(&host)
        || asset
            .aliases
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(&host))
}

/// Scope is user-owned and can change after ingestion, so queries compute the
/// current flag from the asset and its aliases rather than trusting `in_scope`.
pub fn asset_in_scope(asset: &Asset, scope: &Scope) -> bool {
    if scope.is_empty() {
        return false;
    }
    scope.allows(&asset.host) || asset.aliases.iter().any(|alias| scope.allows(alias))
}

fn regex(pattern: &str) -> Regex {
    static CACHE: LazyLock<Mutex<BTreeMap<String, Regex>>> =
        LazyLock::new(|| Mutex::new(BTreeMap::new()));
    let mut cache = CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
    cache
        .entry(pattern.to_string())
        .or_insert_with(|| Regex::new(pattern).expect("static regex"))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(targets: &[&str]) -> Scope {
        let mut scope = Scope::default();
        for target in targets {
            scope.add(target).unwrap();
        }
        scope
    }

    #[test]
    fn parses_nmap_normal_output() {
        let mut map = SurfaceMap::default();
        map.ingest(
            "nmap -sV example.com",
            "Nmap scan report for example.com (192.0.2.10)\n\
             PORT    STATE SERVICE VERSION\n\
             22/tcp  open  ssh     OpenSSH 9.6\n\
             443/tcp open  https   nginx 1.25\n",
            &scope(&["example.com", "192.0.2.10"]),
            "e1",
        );
        let asset = map.get("example.com").expect("asset");
        assert_eq!(asset.ports.len(), 2);
        assert!(asset.ports.contains_key("443/tcp"));
        assert_eq!(asset.ports["443/tcp"].product, "nginx");
        assert!(asset.aliases.contains("192.0.2.10"));
        assert!(asset.evidence.contains("e1"));
    }

    #[test]
    fn parses_nmap_xml_output() {
        let xml = r#"<nmaprun><host><address addr="192.0.2.10" addrtype="ipv4"/><hostnames><hostname name="example.com"/></hostnames><ports><port protocol="tcp" portid="443"><state state="open"/><service name="https" product="nginx" version="1.25" tunnel="ssl"/></port></ports></host></nmaprun>"#;
        let mut map = SurfaceMap::default();
        map.ingest(
            "nmap -oX - example.com",
            xml,
            &scope(&["example.com"]),
            "e2",
        );
        let asset = map.get("example.com").expect("asset");
        assert!(asset.ports.contains_key("443/tcp"));
        assert!(asset.ports["443/tcp"].tls);
        assert_eq!(asset.ports["443/tcp"].product, "nginx");
    }

    #[test]
    fn scope_filters_out_of_scope_assets() {
        let mut map = SurfaceMap::default();
        map.ingest(
            "nmap evil.example",
            "Nmap scan report for evil.example (198.51.100.9)\n80/tcp open http nginx\n",
            &scope(&["example.com"]),
            "e3",
        );
        assert!(map.is_empty());
    }

    #[test]
    fn urls_store_no_userinfo_and_hash_credentials() {
        let mut map = SurfaceMap::default();
        map.ingest(
            "curl https://alice:secret@example.com/login",
            "HTTP/1.1 200 OK\nServer: nginx\n",
            &scope(&["example.com"]),
            "e4",
        );
        let asset = map.get("example.com").expect("asset");
        assert!(asset.urls.iter().all(|url| !url.contains("alice")));
        assert!(asset.identities.contains("alice"));
        assert!(
            asset
                .credentials
                .iter()
                .all(|cred| cred.starts_with("cred:"))
        );
        assert!(asset.technologies.iter().any(|tech| tech == "nginx"));
    }

    #[test]
    fn parses_nmap_grepable_output() {
        let mut map = SurfaceMap::default();
        map.ingest(
            "nmap -oG - example.com",
            "Host: example.com (192.0.2.10)\tPorts: 22/open/tcp//ssh//OpenSSH 9.6/, 443/open/tcp//https//nginx/\n",
            &scope(&["example.com"]),
            "e6",
        );
        let asset = map.get("example.com").expect("asset");
        assert!(asset.ports.contains_key("22/tcp"));
        assert!(asset.ports.contains_key("443/tcp"));
        assert_eq!(asset.ports["22/tcp"].service, "ssh");
        assert_eq!(asset.ports["22/tcp"].product, "OpenSSH 9.6");
    }

    #[test]
    fn reloads_the_inventory() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut map = SurfaceMap::open(dir.path()).unwrap();
            map.ingest(
                "nmap example.com",
                "Nmap scan report for example.com\n443/tcp open https nginx\n",
                &scope(&["example.com"]),
                "e5",
            );
            assert!(!map.is_empty());
        }
        let map = SurfaceMap::open(dir.path()).unwrap();
        assert!(map.get("example.com").is_some());
    }
}
