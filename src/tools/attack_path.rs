//! `attack_path` tool: ranked graph queries over the engagement surface.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::security::graph::Graph;
use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, object_schema, optional_str, optional_u64,
};

pub struct AttackPathTool;

impl AttackPathTool {
    fn graph(ctx: &ToolContext) -> Result<(Graph, String), ToolResult> {
        let mut state = match ctx.security.lock() {
            Ok(state) => state,
            Err(_) => {
                return Err(ToolResult::err(
                    "SECURITY_IO",
                    "security state is poisoned; restart lh",
                ));
            }
        };
        let scope = state.scope.clone();
        let engagement_name = state.engagement_name.clone();
        let engagement = match state.ensure_engagement(&ctx.paths) {
            Ok(engagement) => engagement,
            Err(error) => {
                return Err(ToolResult::err(
                    "SECURITY_IO",
                    format!("cannot open the engagement: {error}"),
                ));
            }
        };
        let graph = Graph::build(
            &engagement.surface,
            &engagement.findings,
            engagement.evidence.list(),
            &scope,
        );
        Ok((graph, engagement_name))
    }

    fn summary(ctx: &ToolContext, limit: usize) -> ToolResult {
        let (graph, engagement) = match Self::graph(ctx) {
            Ok(graph) => graph,
            Err(error) => return error,
        };
        let mut out = format!("engagement {engagement}\n");
        out.push_str(&graph.summary(limit));
        let choke = graph.choke_points(3);
        if !choke.is_empty() {
            out.push_str("\ntop choke points:\n");
            for (node, score) in choke {
                out.push_str(&format!("  {node} score={score:.2}\n"));
            }
        }
        ToolResult::ok(out)
    }

    fn paths(ctx: &ToolContext, args: &Value) -> ToolResult {
        let from = match optional_str(args, "from") {
            Some(from) => from,
            None => {
                return ToolResult::err(
                    "invalid_args",
                    "attack_path paths needs \"from\" (asset host, service host:port, or identity)",
                );
            }
        };
        let to = match optional_str(args, "to") {
            Some(to) => to,
            None => {
                return ToolResult::err(
                    "invalid_args",
                    "attack_path paths needs \"to\" (asset host, service host:port, or identity)",
                );
            }
        };
        let depth = optional_u64(args, "depth").unwrap_or(6).clamp(1, 10) as usize;
        let limit = optional_u64(args, "limit").unwrap_or(5).clamp(1, 20) as usize;
        let (graph, _) = match Self::graph(ctx) {
            Ok(graph) => graph,
            Err(error) => return error,
        };
        let paths = match graph.paths(&from, &to, depth, limit) {
            Ok(paths) => paths,
            Err(error) => return ToolResult::err("GRAPH_NODE", error),
        };
        if paths.is_empty() {
            return ToolResult::ok(format!(
                "no path from {from:?} to {to:?} within {depth} hops. The graph is directed: identity/credential -> asset -> service -> technology. Use `attack_path summary` to see available nodes."
            ));
        }
        let mut out = format!(
            "paths from {from:?} to {to:?} (ranked by risk, {} shown):\n",
            paths.len()
        );
        for (index, path) in paths.iter().enumerate() {
            out.push_str(&format!(
                "  {}. risk={} hops={} {}\n",
                index + 1,
                path.risk,
                path.nodes.len().saturating_sub(1),
                path.nodes.join(" -> ")
            ));
        }
        ToolResult::ok(out)
    }

    fn choke(ctx: &ToolContext, limit: usize) -> ToolResult {
        let (graph, _) = match Self::graph(ctx) {
            Ok(graph) => graph,
            Err(error) => return error,
        };
        let ranked = graph.choke_points(limit);
        if ranked.is_empty() {
            return ToolResult::ok(
                "attack graph is empty; ingest scan output with bash (nmap/curl/openssl) first",
            );
        }
        let mut out = format!("choke points (betweenness, top {}):\n", ranked.len());
        for (index, (node, score)) in ranked.iter().enumerate() {
            let risk = graph.nodes.get(node).map(|node| node.risk).unwrap_or(0);
            out.push_str(&format!(
                "  {}. {node} score={score:.2} risk={risk}\n",
                index + 1
            ));
        }
        ToolResult::ok(out)
    }

    fn asset(ctx: &ToolContext, name: &str) -> ToolResult {
        let (graph, _) = match Self::graph(ctx) {
            Ok(graph) => graph,
            Err(error) => return error,
        };
        match graph.node_view(name) {
            Ok(view) => ToolResult::ok(view),
            Err(error) => ToolResult::err(
                "GRAPH_NODE",
                format!("{error}; `attack_path summary` lists node counts and top risk"),
            ),
        }
    }
}

#[async_trait]
impl Tool for AttackPathTool {
    fn name(&self) -> &'static str {
        "attack_path"
    }

    fn description(&self) -> &'static str {
        "Query the attack-path graph: summary, ranked paths, choke points, node detail."
    }

    fn schema(&self) -> Value {
        object_schema(
            json!({
                "action": {"type": "string", "enum": ["summary", "paths", "choke", "asset"]},
                "from": {"type": "string", "description": "Start node for paths."},
                "to": {"type": "string", "description": "Target node for paths."},
                "name": {"type": "string", "description": "Node name for asset detail."},
                "depth": {"type": "integer", "minimum": 1, "maximum": 10, "description": "paths: maximum hops."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 20, "description": "paths/choke: results to return."}
            }),
            &["action"],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Security
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let action = optional_str(&args, "action").unwrap_or_else(|| "summary".into());
        let limit = optional_u64(&args, "limit").unwrap_or(5).clamp(1, 20) as usize;
        match action.as_str() {
            "summary" => Self::summary(ctx, limit),
            "paths" => Self::paths(ctx, &args),
            "choke" => Self::choke(ctx, limit),
            "asset" => match optional_str(&args, "name") {
                Some(name) => Self::asset(ctx, &name),
                None => ToolResult::err(
                    "invalid_args",
                    "attack_path asset needs \"name\"; `attack_path summary` shows node counts",
                ),
            },
            other => ToolResult::err(
                "invalid_args",
                format!(
                    "unknown attack_path action {other:?}; use summary, paths, choke, or asset"
                ),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::scope::Scope;

    #[tokio::test]
    async fn summarizes_paths_and_chokes() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::tools::write::test_context(dir.path());
        {
            let mut state = ctx.security.lock().unwrap();
            let engagement = state.ensure_engagement(&ctx.paths).unwrap();
            let mut scope = Scope::default();
            scope.add("example.com").unwrap();
            engagement.surface.ingest(
                "nmap example.com",
                "Nmap scan report for example.com\n443/tcp open https nginx\n",
                &scope,
                "e1",
            );
        }
        let summary = AttackPathTool
            .call(&ctx, json!({"action": "summary"}))
            .await;
        assert!(!summary.is_error, "{}", summary.content);
        assert!(
            summary.content.contains("asset:example.com"),
            "{}",
            summary.content
        );
        let choke = AttackPathTool.call(&ctx, json!({"action": "choke"})).await;
        assert!(!choke.is_error, "{}", choke.content);
        assert!(choke.content.contains("choke points"), "{}", choke.content);
        let paths = AttackPathTool
            .call(
                &ctx,
                json!({"action": "paths", "from": "example.com", "to": "service:example.com:443/tcp"}),
            )
            .await;
        assert!(!paths.is_error, "{}", paths.content);
        assert!(paths.content.contains("risk="), "{}", paths.content);
        assert!(paths.content.contains("->"), "{}", paths.content);
    }
}
