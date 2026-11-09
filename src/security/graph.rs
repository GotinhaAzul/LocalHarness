//! Attack-path graph over the surface inventory, findings, and evidence.
//!
//! Nodes are assets, exposed services, technologies, identities, and hashed
//! credential references. Edges are deterministic: identity/credential -> asset,
//! asset -> service, service -> technology. Findings add risk to matching nodes.
//! Queries return ranked paths, choke points, and node summaries; the raw graph
//! is never dumped at the model.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use crate::security::evidence::Evidence;
use crate::security::findings::{Finding, Severity};
use crate::security::scope::Scope;
use crate::security::surface::{SurfaceMap, asset_in_scope, normalize_host};

const MAX_PATH_NODES: usize = 400;
const MAX_PATHS: usize = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Asset,
    Service,
    Identity,
    Credential,
    Technology,
}

impl NodeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeKind::Asset => "asset",
            NodeKind::Service => "service",
            NodeKind::Identity => "identity",
            NodeKind::Credential => "credential",
            NodeKind::Technology => "technology",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    pub label: String,
    pub risk: u32,
    pub findings: Vec<String>,
    pub in_scope: bool,
}

#[derive(Debug, Default)]
pub struct Graph {
    pub nodes: BTreeMap<String, Node>,
    pub edges: BTreeSet<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Path {
    pub nodes: Vec<String>,
    pub risk: u32,
}

impl Graph {
    pub fn build(
        surface: &SurfaceMap,
        findings: &[Finding],
        evidence: &[Evidence],
        scope: &Scope,
    ) -> Self {
        let mut graph = Self::default();
        for asset in surface.assets().values() {
            let asset_id = format!("asset:{}", asset.host);
            let in_scope = asset_in_scope(asset, scope);
            if !scope.is_empty() && !in_scope {
                continue;
            }
            graph.ensure(&asset_id, NodeKind::Asset, asset.host.clone(), in_scope);
            for (key, port) in &asset.ports {
                let service_id = format!("service:{}:{key}", asset.host);
                let label = if !port.product.is_empty() {
                    format!("{key} {} {}", port.service, port.product)
                } else if !port.service.is_empty() {
                    format!("{key} {}", port.service)
                } else {
                    key.clone()
                };
                graph.ensure(&service_id, NodeKind::Service, label, in_scope);
                graph.edge(&asset_id, &service_id);
                if !port.product.is_empty() {
                    let tech_id = format!("tech:{}", port.product.to_ascii_lowercase());
                    graph.ensure(
                        &tech_id,
                        NodeKind::Technology,
                        port.product.clone(),
                        in_scope,
                    );
                    graph.edge(&service_id, &tech_id);
                }
                for technology in &asset.technologies {
                    let tech_id = format!("tech:{}", technology.to_ascii_lowercase());
                    graph.ensure(&tech_id, NodeKind::Technology, technology.clone(), in_scope);
                    graph.edge(&service_id, &tech_id);
                }
            }
            if asset.ports.is_empty() {
                for technology in &asset.technologies {
                    let tech_id = format!("tech:{}", technology.to_ascii_lowercase());
                    graph.ensure(&tech_id, NodeKind::Technology, technology.clone(), in_scope);
                    graph.edge(&asset_id, &tech_id);
                }
            }
            for identity in &asset.identities {
                let identity_id = format!("identity:{identity}");
                graph.ensure(&identity_id, NodeKind::Identity, identity.clone(), in_scope);
                graph.edge(&identity_id, &asset_id);
            }
            for credential in &asset.credentials {
                let credential_id = format!("credential:{credential}");
                graph.ensure(
                    &credential_id,
                    NodeKind::Credential,
                    credential.clone(),
                    in_scope,
                );
                graph.edge(&credential_id, &asset_id);
            }
        }

        for item in evidence {
            if item.tool == "detect" {
                continue;
            }
            for target in item.target.split(',').map(str::trim) {
                if is_file_target(target) {
                    continue;
                }
                let host = normalize_host(target);
                if host.is_empty() {
                    continue;
                }
                if !scope.is_empty() && !scope.allows(&host) {
                    continue;
                }
                if surface.get(&host).is_none() {
                    let id = format!("asset:{host}");
                    let in_scope = !scope.is_empty() && scope.allows(&host);
                    graph.ensure(&id, NodeKind::Asset, host, in_scope);
                }
            }
        }

        for finding in findings {
            if is_file_target(&finding.target) {
                continue;
            }
            let (host, port) = target_host_port(&finding.target);
            if host.is_empty() {
                continue;
            }
            if !scope.is_empty() && !scope.allows(&host) {
                continue;
            }
            let canonical = surface
                .get(&host)
                .map(|asset| asset.host.clone())
                .unwrap_or_else(|| host.clone());
            let asset_id = graph
                .asset_for(&canonical)
                .unwrap_or_else(|| format!("asset:{canonical}"));
            let in_scope = graph
                .nodes
                .get(&asset_id)
                .map(|node| node.in_scope)
                .unwrap_or(!scope.is_empty() && scope.allows(&canonical));
            graph.ensure(&asset_id, NodeKind::Asset, canonical.clone(), in_scope);
            graph.apply_finding(&asset_id, finding);
            if let Some(port) = port {
                let service_id = graph
                    .nodes
                    .keys()
                    .find(|id| id.starts_with(&format!("service:{canonical}:{port}/")))
                    .cloned();
                if let Some(service_id) = service_id {
                    graph.apply_finding(&service_id, finding);
                }
            }
        }
        graph
    }

    fn ensure(&mut self, id: &str, kind: NodeKind, label: String, in_scope: bool) {
        self.nodes.entry(id.to_string()).or_insert_with(|| Node {
            id: id.to_string(),
            kind,
            label,
            risk: 0,
            findings: Vec::new(),
            in_scope,
        });
    }

    fn edge(&mut self, from: &str, to: &str) {
        if from != to {
            self.edges.insert((from.to_string(), to.to_string()));
        }
    }

    fn asset_for(&self, host: &str) -> Option<String> {
        let host = normalize_host(host);
        self.nodes
            .iter()
            .find(|(_, node)| {
                node.kind == NodeKind::Asset
                    && (node.label.eq_ignore_ascii_case(&host)
                        || node.id.eq_ignore_ascii_case(&format!("asset:{host}")))
            })
            .map(|(id, _)| id.clone())
    }

    fn apply_finding(&mut self, id: &str, finding: &Finding) {
        let weight = severity_weight(finding.severity);
        if let Some(node) = self.nodes.get_mut(id) {
            node.risk = node.risk.saturating_add(weight);
            if !node.findings.contains(&finding.id) {
                node.findings.push(finding.id.clone());
            }
        }
    }

    pub fn resolve(&self, input: &str) -> Option<String> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }
        if self.nodes.contains_key(input) {
            return Some(input.to_string());
        }
        for prefix in ["asset:", "service:", "identity:", "credential:", "tech:"] {
            let id = format!("{prefix}{input}");
            if self.nodes.contains_key(&id) {
                return Some(id);
            }
        }
        self.nodes
            .iter()
            .find(|(id, node)| {
                node.label.eq_ignore_ascii_case(input)
                    || id.eq_ignore_ascii_case(input)
                    || id.ends_with(&format!(":{input}"))
            })
            .map(|(id, _)| id.clone())
    }

    pub fn summary(&self, limit: usize) -> String {
        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        for node in self.nodes.values() {
            *counts.entry(node.kind.as_str()).or_default() += 1;
        }
        let mut out = format!(
            "attack graph: {} nodes, {} edges\n",
            self.nodes.len(),
            self.edges.len()
        );
        for (kind, count) in &counts {
            out.push_str(&format!("  {kind}: {count}\n"));
        }
        let mut risk: Vec<&Node> = self.nodes.values().filter(|node| node.risk > 0).collect();
        risk.sort_by(|left, right| {
            right
                .risk
                .cmp(&left.risk)
                .then_with(|| left.id.cmp(&right.id))
        });
        if risk.is_empty() {
            out.push_str("  no risk-bearing nodes yet; add findings or ingest scan output\n");
        } else {
            out.push_str("\ntop risk:\n");
            for node in risk.into_iter().take(limit.clamp(1, 10)) {
                out.push_str(&format!(
                    "  {} risk={} findings={}\n",
                    node.id,
                    node.risk,
                    node.findings.join(",")
                ));
            }
        }
        out
    }

    pub fn paths(
        &self,
        from: &str,
        to: &str,
        max_depth: usize,
        limit: usize,
    ) -> Result<Vec<Path>, String> {
        let Some(start) = self.resolve(from) else {
            return Err(format!(
                "no graph node {from:?}; try an asset host, service host:port, or identity"
            ));
        };
        let Some(target) = self.resolve(to) else {
            return Err(format!(
                "no graph node {to:?}; try an asset host, service host:port, or identity"
            ));
        };
        let max_depth = max_depth.clamp(1, 10);
        let limit = limit.clamp(1, 20);
        let mut paths = Vec::new();
        let mut stack = vec![start.clone()];
        let mut visited = BTreeSet::new();
        visited.insert(start.clone());
        self.walk(
            &start,
            &target,
            max_depth,
            &mut stack,
            &mut visited,
            &mut paths,
        );
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        paths.sort_by(|left, right| {
            right
                .risk
                .cmp(&left.risk)
                .then_with(|| left.nodes.len().cmp(&right.nodes.len()))
                .then_with(|| left.nodes.cmp(&right.nodes))
        });
        paths.truncate(limit);
        Ok(paths)
    }

    fn walk(
        &self,
        current: &str,
        target: &str,
        depth_left: usize,
        stack: &mut Vec<String>,
        visited: &mut BTreeSet<String>,
        paths: &mut Vec<Path>,
    ) {
        if paths.len() >= MAX_PATHS || depth_left == 0 {
            return;
        }
        if current == target {
            let risk = stack
                .iter()
                .filter_map(|id| self.nodes.get(id))
                .map(|node| node.risk)
                .sum();
            paths.push(Path {
                nodes: stack.clone(),
                risk,
            });
            return;
        }
        for (_, next) in self.edges.iter().filter(|(from, _)| from == current) {
            if !visited.insert(next.clone()) {
                continue;
            }
            stack.push(next.clone());
            self.walk(next, target, depth_left - 1, stack, visited, paths);
            stack.pop();
            visited.remove(next);
        }
    }

    pub fn choke_points(&self, limit: usize) -> Vec<(String, f64)> {
        let limit = limit.clamp(1, 20);
        if self.nodes.is_empty() {
            return Vec::new();
        }
        if self.nodes.len() > MAX_PATH_NODES {
            let mut degree: Vec<(String, usize)> = self
                .nodes
                .keys()
                .map(|id| (id.clone(), self.neighbors(id).len()))
                .collect();
            degree.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
            return degree
                .into_iter()
                .take(limit)
                .map(|(id, degree)| (id, degree as f64))
                .collect();
        }

        let mut centrality: HashMap<String, f64> = HashMap::new();
        for source in self.nodes.keys() {
            let mut stack = Vec::new();
            let mut predecessors: HashMap<String, Vec<String>> = HashMap::new();
            let mut sigma: HashMap<String, f64> = HashMap::new();
            let mut distance: HashMap<String, i64> = HashMap::new();
            sigma.insert(source.clone(), 1.0);
            distance.insert(source.clone(), 0);
            let mut queue = VecDeque::new();
            queue.push_back(source.clone());
            while let Some(node) = queue.pop_front() {
                stack.push(node.clone());
                let next_distance = distance.get(&node).copied().unwrap_or(0) + 1;
                for next in self.neighbors(&node) {
                    if !distance.contains_key(&next) {
                        distance.insert(next.clone(), next_distance);
                        queue.push_back(next.clone());
                    }
                    if distance.get(&next).copied() == Some(next_distance) {
                        let node_sigma = sigma.get(&node).copied().unwrap_or(0.0);
                        *sigma.entry(next.clone()).or_insert(0.0) += node_sigma;
                        predecessors
                            .entry(next.clone())
                            .or_default()
                            .push(node.clone());
                    }
                }
            }
            let mut delta: HashMap<String, f64> = HashMap::new();
            while let Some(node) = stack.pop() {
                let node_sigma = sigma.get(&node).copied().unwrap_or(0.0);
                let node_delta = delta.get(&node).copied().unwrap_or(0.0);
                if let Some(preds) = predecessors.get(&node) {
                    for predecessor in preds {
                        let pred_sigma = sigma.get(predecessor).copied().unwrap_or(0.0);
                        let contribution = if node_sigma > 0.0 {
                            (pred_sigma / node_sigma) * (1.0 + node_delta)
                        } else {
                            0.0
                        };
                        *delta.entry(predecessor.clone()).or_insert(0.0) += contribution;
                    }
                }
                if node.as_str() != source.as_str() {
                    *centrality.entry(node).or_insert(0.0) += node_delta;
                }
            }
        }

        let mut ranked: Vec<(String, f64)> = centrality
            .into_iter()
            .map(|(node, score)| (node, score / 2.0))
            .collect();
        ranked.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        ranked.truncate(limit);
        ranked
    }

    fn neighbors(&self, node: &str) -> Vec<String> {
        let mut neighbors = BTreeSet::new();
        for (from, to) in &self.edges {
            if from == node {
                neighbors.insert(to.clone());
            } else if to == node {
                neighbors.insert(from.clone());
            }
        }
        neighbors.into_iter().collect()
    }

    pub fn node_view(&self, input: &str) -> Result<String, String> {
        let Some(id) = self.resolve(input) else {
            return Err(format!("no graph node {input:?}"));
        };
        let node = self.nodes.get(&id).expect("resolved node");
        let mut out = format!(
            "{} ({}) scope={} risk={}\n",
            node.id,
            node.kind.as_str(),
            if node.in_scope { "in" } else { "out" },
            node.risk
        );
        if !node.findings.is_empty() {
            out.push_str(&format!("findings: {}\n", node.findings.join(", ")));
        }
        let outgoing: Vec<&String> = self
            .edges
            .iter()
            .filter(|(from, _)| from == &id)
            .map(|(_, to)| to)
            .collect();
        let incoming: Vec<&String> = self
            .edges
            .iter()
            .filter(|(_, to)| to == &id)
            .map(|(from, _)| from)
            .collect();
        if !outgoing.is_empty() {
            out.push_str("edges:\n");
            for next in outgoing {
                out.push_str(&format!("  -> {next}\n"));
            }
        }
        if !incoming.is_empty() {
            out.push_str("reached from:\n");
            for previous in incoming {
                out.push_str(&format!("  <- {previous}\n"));
            }
        }
        Ok(out)
    }
}

fn is_file_target(target: &str) -> bool {
    let target = target.trim();
    if target.starts_with("file://") {
        return true;
    }
    if target.contains("://") {
        return false;
    }
    if target.starts_with("file:") || target.contains('/') || target.contains('\\') {
        return true;
    }
    let lower = target.to_ascii_lowercase();
    [
        ".log", ".txt", ".json", ".jsonl", ".ndjson", ".csv", ".evtx", ".pcap", ".pcapng",
        ".sample", ".bin", ".yar", ".yara", ".yml", ".yaml",
    ]
    .iter()
    .any(|extension| lower.ends_with(extension))
}

fn severity_weight(severity: Severity) -> u32 {
    match severity {
        Severity::Critical => 10,
        Severity::High => 7,
        Severity::Medium => 4,
        Severity::Low => 2,
        Severity::Info => 1,
    }
}

fn target_host_port(target: &str) -> (String, Option<u16>) {
    let host = normalize_host(target);
    let scheme_port = if target.starts_with("https://") {
        Some(443)
    } else if target.starts_with("http://") {
        Some(80)
    } else {
        None
    };
    let authority = target
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(target)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(target);
    let authority = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    let port = if let Some(rest) = authority.strip_prefix('[') {
        rest.split_once(']')
            .and_then(|(_, tail)| tail.strip_prefix(':'))
            .and_then(|port| port.parse().ok())
    } else if authority.matches(':').count() == 1
        && let Some((_, port)) = authority.rsplit_once(':')
        && port.chars().all(|character| character.is_ascii_digit())
    {
        port.parse().ok()
    } else {
        None
    };
    (host, port.or(scheme_port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::findings::{Severity, Status};
    use crate::security::surface::SurfaceMap;
    use chrono::Utc;

    fn finding(id: &str, target: &str, severity: Severity) -> Finding {
        Finding {
            id: id.into(),
            title: "finding".into(),
            severity,
            status: Status::Open,
            target: target.into(),
            evidence: None,
            evidence_ref: None,
            impact: None,
            remediation: None,
            created: Utc::now(),
            updated: Utc::now(),
            session_id: None,
            entry_id: None,
            poc_steps: Vec::new(),
            verification: None,
            confidence: None,
        }
    }

    fn surface() -> SurfaceMap {
        let mut map = SurfaceMap::default();
        map.ingest(
            "nmap example.com",
            "Nmap scan report for example.com\n\
             443/tcp open https nginx\n\
             22/tcp open ssh OpenSSH\n",
            &{
                let mut scope = Scope::default();
                scope.add("example.com").unwrap();
                scope
            },
            "e1",
        );
        map
    }

    #[test]
    fn builds_asset_service_and_technology_edges() {
        let graph = Graph::build(&surface(), &[], &[], &Scope::default());
        assert!(graph.nodes.contains_key("asset:example.com"));
        assert!(graph.nodes.contains_key("service:example.com:443/tcp"));
        assert!(graph.nodes.contains_key("tech:nginx"));
        assert!(graph.edges.contains(&(
            "asset:example.com".into(),
            "service:example.com:443/tcp".into()
        )));
    }

    #[test]
    fn findings_raise_asset_and_service_risk() {
        let graph = Graph::build(
            &surface(),
            &[finding("f1", "https://example.com/login", Severity::High)],
            &[],
            &Scope::default(),
        );
        assert_eq!(graph.nodes["asset:example.com"].risk, 7);
        assert_eq!(graph.nodes["service:example.com:443/tcp"].risk, 7);
        assert_eq!(graph.nodes["asset:example.com"].findings, ["f1"]);
    }

    #[test]
    fn ranked_paths_include_risk() {
        let graph = Graph::build(
            &surface(),
            &[finding("f1", "example.com", Severity::High)],
            &[],
            &Scope::default(),
        );
        let paths = graph
            .paths("asset:example.com", "service:example.com:443/tcp", 4, 5)
            .unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].risk, 7);
    }

    #[test]
    fn choke_points_are_ranked() {
        let graph = Graph::build(&surface(), &[], &[], &Scope::default());
        let choke = graph.choke_points(5);
        assert!(!choke.is_empty());
        let ids: Vec<&str> = choke.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"asset:example.com"));
    }

    #[test]
    fn node_view_is_a_summary_not_a_dump() {
        let graph = Graph::build(&surface(), &[], &[], &Scope::default());
        let view = graph.node_view("example.com").unwrap();
        assert!(view.contains("asset:example.com"));
        assert!(view.contains("-> service:example.com:443/tcp"));
        assert!(!view.contains("{\""));
    }

    #[test]
    fn identities_and_credentials_link_to_assets() {
        let mut surface = SurfaceMap::default();
        let mut scope = Scope::default();
        scope.add("example.com").unwrap();
        surface.ingest(
            "curl https://alice:secret@example.com",
            "HTTP/1.1 200 OK\n",
            &scope,
            "e1",
        );
        let graph = Graph::build(&surface, &[], &[], &scope);
        assert!(graph.nodes.contains_key("identity:alice"));
        assert!(
            graph
                .nodes
                .keys()
                .any(|id| id.starts_with("credential:cred:"))
        );
        assert!(
            graph
                .edges
                .contains(&("identity:alice".into(), "asset:example.com".into()))
        );
    }

    #[test]
    fn detection_file_targets_are_not_assets() {
        let graph = Graph::build(
            &SurfaceMap::default(),
            &[finding("f1", "samples/sample.log", Severity::High)],
            &[],
            &Scope::default(),
        );
        assert!(graph.nodes.is_empty(), "{:?}", graph.nodes.keys());
    }

    #[test]
    fn evidence_targets_add_assets() {
        let graph = Graph::build(
            &SurfaceMap::default(),
            &[],
            &[Evidence {
                id: "e1".into(),
                tool: "bash".into(),
                session: None,
                entry: None,
                target: "lonely.example".into(),
                command: "curl https://lonely.example".into(),
                started: Utc::now(),
                finished: Utc::now(),
                duration_ms: 1,
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
                is_error: false,
                artifact: "evidence/e1.txt".into(),
                sha256: "hash".into(),
                bytes: 0,
                summary: None,
            }],
            &Scope::default(),
        );
        assert!(graph.nodes.contains_key("asset:lonely.example"));
    }
}
