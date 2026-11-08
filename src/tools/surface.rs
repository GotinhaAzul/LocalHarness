//! `surface` tool: read the normalized engagement attack-surface inventory.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::security::surface::{Asset, asset_in_scope, host_matches, normalize_host};
use crate::tools::{Tool, ToolContext, ToolPack, ToolResult, object_schema, optional_str};

pub struct SurfaceTool;

impl SurfaceTool {
    fn list(ctx: &ToolContext) -> ToolResult {
        let mut state = match ctx.security.lock() {
            Ok(state) => state,
            Err(_) => {
                return ToolResult::err("SECURITY_IO", "security state is poisoned; restart lh");
            }
        };
        let scope = state.scope.clone();
        let engagement = match state.ensure_engagement(&ctx.paths) {
            Ok(engagement) => engagement,
            Err(error) => {
                return ToolResult::err(
                    "SECURITY_IO",
                    format!("cannot open the surface inventory: {error}"),
                );
            }
        };
        let assets = engagement.surface.assets();
        if assets.is_empty() {
            return ToolResult::ok(format!(
                "engagement {}: no surface data yet. Run nmap, curl, or openssl through bash; the inventory is parsed from command output automatically.",
                state.engagement_name
            ));
        }
        let mut out = format!(
            "surface: {} assets, {} services, {} technologies\n",
            assets.len(),
            engagement.surface.service_count(),
            engagement.surface.technology_count()
        );
        for asset in assets.values() {
            let findings = findings_for(engagement, asset);
            out.push_str(&format!(
                "{} [{}] {} | tech: {} | urls: {} | evidence: {} | findings: {}\n",
                asset.host,
                if asset_in_scope(asset, &scope) {
                    "in scope"
                } else {
                    "out of scope"
                },
                asset.port_summary(),
                if asset.technologies.is_empty() {
                    "-".into()
                } else {
                    asset
                        .technologies
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                },
                asset.urls.len(),
                if asset.evidence.is_empty() {
                    "-".into()
                } else {
                    asset.evidence.iter().cloned().collect::<Vec<_>>().join(",")
                },
                if findings.is_empty() {
                    "-".into()
                } else {
                    findings.join(",")
                }
            ));
        }
        out.push_str(
            "next: `surface show host=example.com` for ports, URLs, identities, and credentials",
        );
        ToolResult::ok(out)
    }

    fn show(ctx: &ToolContext, host: &str) -> ToolResult {
        let mut state = match ctx.security.lock() {
            Ok(state) => state,
            Err(_) => {
                return ToolResult::err("SECURITY_IO", "security state is poisoned; restart lh");
            }
        };
        let scope = state.scope.clone();
        let engagement = match state.ensure_engagement(&ctx.paths) {
            Ok(engagement) => engagement,
            Err(error) => {
                return ToolResult::err(
                    "SECURITY_IO",
                    format!("cannot open the surface inventory: {error}"),
                );
            }
        };
        let Some(asset) = engagement.surface.get(host) else {
            return ToolResult::err(
                "SURFACE_NOT_FOUND",
                format!(
                    "no surface entry for {host:?}; `surface list` shows assets (ingest runs after nmap/curl/openssl commands)"
                ),
            );
        };
        ToolResult::ok(render_asset(engagement, asset, &scope))
    }
}

fn render_asset(
    engagement: &crate::security::Engagement,
    asset: &Asset,
    scope: &crate::security::Scope,
) -> String {
    let mut out = format!(
        "{} [{}]\n",
        asset.host,
        if asset_in_scope(asset, scope) {
            "in scope"
        } else {
            "out of scope"
        }
    );
    if !asset.aliases.is_empty() {
        out.push_str(&format!(
            "aliases: {}\n",
            asset.aliases.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if asset.ports.is_empty() {
        out.push_str("ports: none\n");
    } else {
        out.push_str("ports:\n");
        for (key, port) in &asset.ports {
            let mut details = vec![key.clone()];
            if !port.service.is_empty() {
                details.push(port.service.clone());
            }
            if !port.product.is_empty() {
                details.push(port.product.clone());
            }
            if !port.version.is_empty() {
                details.push(port.version.clone());
            }
            if port.tls {
                details.push("tls".into());
            }
            out.push_str(&format!("  {}\n", details.join(" ")));
        }
    }
    if !asset.technologies.is_empty() {
        out.push_str(&format!(
            "technologies: {}\n",
            asset
                .technologies
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !asset.urls.is_empty() {
        out.push_str("urls:\n");
        for url in &asset.urls {
            out.push_str(&format!("  {url}\n"));
        }
    }
    if !asset.identities.is_empty() {
        out.push_str(&format!(
            "identities: {}\n",
            asset
                .identities
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !asset.credentials.is_empty() {
        out.push_str(&format!(
            "credentials (hashed): {}\n",
            asset
                .credentials
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out.push_str(&format!(
        "evidence: {}\n",
        if asset.evidence.is_empty() {
            "-".into()
        } else {
            asset
                .evidence
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    let findings = findings_for(engagement, asset);
    if !findings.is_empty() {
        out.push_str(&format!("findings: {}\n", findings.join(", ")));
    }
    out
}

fn findings_for(engagement: &crate::security::Engagement, asset: &Asset) -> Vec<String> {
    engagement
        .findings
        .iter()
        .filter(|finding| {
            host_matches(asset, &finding.target)
                || normalize_host(&finding.target).eq_ignore_ascii_case(&asset.host)
        })
        .map(|finding| finding.id.clone())
        .collect()
}

#[async_trait]
impl Tool for SurfaceTool {
    fn name(&self) -> &'static str {
        "surface"
    }

    fn description(&self) -> &'static str {
        "Read the engagement attack surface: hosts, ports, services, URLs, and technologies."
    }

    fn schema(&self) -> Value {
        object_schema(
            json!({
                "action": {"type": "string", "enum": ["list", "show"]},
                "host": {"type": "string", "description": "Asset host for show."}
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
        let action = optional_str(&args, "action").unwrap_or_else(|| "list".into());
        match action.as_str() {
            "list" => Self::list(ctx),
            "show" => match optional_str(&args, "host") {
                Some(host) => Self::show(ctx, &host),
                None => ToolResult::err(
                    "invalid_args",
                    "surface show needs \"host\"; `surface list` shows assets",
                ),
            },
            other => ToolResult::err(
                "invalid_args",
                format!("unknown surface action {other:?}; use list or show"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::scope::Scope;

    #[tokio::test]
    async fn lists_and_shows_ingested_assets() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::tools::write::test_context(dir.path());
        {
            let mut state = ctx.security.lock().unwrap();
            let engagement = state.ensure_engagement(&ctx.paths).unwrap();
            let mut scope = Scope::default();
            scope.add("example.com").unwrap();
            engagement.surface.ingest(
                "nmap -sV example.com",
                "Nmap scan report for example.com\n443/tcp open https nginx 1.25\n",
                &scope,
                "e1",
            );
        }
        let list = SurfaceTool.call(&ctx, json!({"action": "list"})).await;
        assert!(!list.is_error, "{}", list.content);
        assert!(list.content.contains("example.com"), "{}", list.content);
        assert!(list.content.contains("443/tcp"), "{}", list.content);
        let show = SurfaceTool
            .call(&ctx, json!({"action": "show", "host": "example.com"}))
            .await;
        assert!(!show.is_error, "{}", show.content);
        assert!(show.content.contains("nginx"), "{}", show.content);
        assert!(show.content.contains("e1"), "{}", show.content);
    }
}
