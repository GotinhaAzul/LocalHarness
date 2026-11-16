//! `fetch` tool: readable text from a URL. Opt-in, GET only.
//!
//! Safe by construction: only http and https, no credentials in the URL, no
//! cookies, a size and time cap, and every hop (the first request and each
//! redirect) is resolved and checked before it is connected to. Loopback,
//! link-local, private, carrier-grade NAT and other non-public addresses are
//! refused unless `[fetch] allow_private` names the host. The connection is
//! pinned to the address that was checked, so a second DNS answer cannot
//! swap in an internal one (DNS rebinding).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use serde_json::{Value, json};

use crate::config::FetchConfig;
use crate::tools::{Tool, ToolContext, ToolPack, ToolResult, optional_u64, required_str};

/// Most redirects followed.
const MAX_REDIRECTS: usize = 5;
/// Most bytes read from a response body.
const MAX_BYTES: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct FetchTool {
    config: FetchConfig,
}

impl FetchTool {
    pub fn new(config: FetchConfig) -> Self {
        Self { config }
    }
}

impl Default for FetchTool {
    fn default() -> Self {
        Self::new(FetchConfig::default())
    }
}

#[async_trait]
impl Tool for FetchTool {
    fn name(&self) -> &'static str {
        "fetch"
    }

    fn description(&self) -> &'static str {
        "GET a public http(s) URL and return its readable text (HTML converted), paged by character offset."
    }

    fn schema(&self) -> Value {
        crate::tools::object_schema(
            json!({
                "url": {"type": "string", "description": "Absolute http or https URL."},
                "offset": {"type": "integer", "description": "Character offset to continue from (from a previous next offset=)."}
            }),
            &["url"],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        true
    }

    fn opt_in(&self) -> bool {
        true
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        match run(&self.config, ctx, &args).await {
            Ok(result) | Err(result) => result,
        }
    }
}

async fn run(
    config: &FetchConfig,
    ctx: &ToolContext,
    args: &Value,
) -> Result<ToolResult, ToolResult> {
    let raw = required_str(args, "url")?;
    let offset = optional_u64(args, "offset").unwrap_or(0) as usize;
    let mut url = parse_url(&raw)?;
    let mut response = None;
    for _ in 0..=MAX_REDIRECTS {
        let addrs = resolve_checked(config, &url).await?;
        let host = url.host_str().unwrap_or_default().to_string();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .resolve_to_addrs(&host, &addrs)
            .user_agent(concat!("localharness-fetch/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| {
                ToolResult::err("FETCH_FAILED", format!("cannot build the client: {err}"))
            })?;
        let reply = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(ToolResult::err("cancelled", "fetch was cancelled")),
            reply = client
                .get(url.clone())
                .header(
                    reqwest::header::ACCEPT,
                    "text/html, text/plain, text/markdown, application/json;q=0.9, */*;q=0.1",
                )
                .send() => reply,
        }
        .map_err(|err| ToolResult::err("FETCH_FAILED", format!("GET {url} failed: {err}")))?;
        if reply.status().is_redirection() {
            let location = reply
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    ToolResult::err(
                        "FETCH_FAILED",
                        format!("{url} answered {} without a Location", reply.status()),
                    )
                })?;
            let next = url.join(location).map_err(|err| {
                ToolResult::err("FETCH_FAILED", format!("bad redirect {location:?}: {err}"))
            })?;
            url = parse_url(next.as_str())?;
            continue;
        }
        response = Some(reply);
        break;
    }
    let Some(response) = response else {
        return Err(ToolResult::err(
            "FETCH_FAILED",
            format!("more than {MAX_REDIRECTS} redirects; last was {url}"),
        ));
    };
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let (body, clipped) = read_capped(response, ctx).await?;
    if !status.is_success() {
        let excerpt = crate::text::clip(&String::from_utf8_lossy(&body), 300);
        return Err(ToolResult::err(
            "FETCH_FAILED",
            format!("GET {url} returned {status}: {excerpt}"),
        ));
    }
    let text = to_text(&content_type, &body).ok_or_else(|| {
        ToolResult::err(
            "FETCH_UNSUPPORTED",
            format!(
                "{url} is {content_type:?}, not text; fetch returns HTML, plain text, Markdown, and JSON only"
            ),
        )
    })?;
    let total = text.chars().count();
    if offset > total {
        return Err(ToolResult::err(
            "invalid_args",
            format!("offset={offset} is past the end ({total} chars)"),
        ));
    }
    let header = format!(
        "{url} ({}; {total} chars{})\n",
        if content_type.is_empty() {
            "unknown type"
        } else {
            content_type.split(';').next().unwrap_or("").trim()
        },
        if clipped { ", body capped at 2 MB" } else { "" }
    );
    let budget = ctx
        .caps
        .tool_budget()
        .saturating_sub(header.chars().count() + 80);
    let (page, next) = crate::html_text::page_text(&text, offset, budget);
    let end = next.unwrap_or(total);
    let mut content = header;
    content.push_str(page.trim_end_matches('\n'));
    if let Some(next) = next {
        content.push_str(&format!(
            "\n\n[chars {offset}-{end} of {total}; next offset={next}]"
        ));
    }
    Ok(ToolResult::ok(content).with_details(json!({
        "url": url.as_str(),
        "status": status.as_u16(),
        "content_type": content_type,
        "chars": total,
        "offset": offset,
        "next_offset": next,
    })))
}

/// An absolute http(s) URL with no credentials in it.
fn parse_url(raw: &str) -> Result<Url, ToolResult> {
    let url = Url::parse(raw.trim()).map_err(|err| {
        ToolResult::err(
            "invalid_args",
            format!("{raw:?} is not an absolute URL ({err}); pass e.g. https://docs.rs/serde"),
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolResult::err(
            "FETCH_REFUSED",
            format!(
                "only http and https URLs are fetched, not {}:",
                url.scheme()
            ),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ToolResult::err(
            "FETCH_REFUSED",
            "the URL carries credentials; fetch never sends credentials",
        ));
    }
    if url.host_str().is_none() {
        return Err(ToolResult::err("invalid_args", "the URL has no host"));
    }
    Ok(url)
}

/// Resolve the URL's host and refuse it when any address is not public,
/// unless `[fetch] allow_private` names the host.
async fn resolve_checked(config: &FetchConfig, url: &Url) -> Result<Vec<SocketAddr>, ToolResult> {
    let host = url.host_str().unwrap_or_default();
    let port = url.port_or_known_default().unwrap_or(80);
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = match bare.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((bare, port))
            .await
            .map_err(|err| {
                ToolResult::err("FETCH_FAILED", format!("cannot resolve {host}: {err}"))
            })?
            .collect(),
    };
    if addrs.is_empty() {
        return Err(ToolResult::err(
            "FETCH_FAILED",
            format!("{host} resolved to no address"),
        ));
    }
    let allowed = config.allow_private.iter().any(|entry| {
        let entry = entry.trim().to_ascii_lowercase();
        entry == bare.to_ascii_lowercase()
            || entry == format!("{}:{port}", bare.to_ascii_lowercase())
    });
    if !allowed && let Some(blocked) = addrs.iter().find(|addr| !is_public(addr.ip())) {
        return Err(ToolResult::err(
            "FETCH_REFUSED",
            format!(
                "{host} resolves to {}, which is not a public address (loopback, private, link-local, or metadata range). Add the host to [fetch] allow_private to allow it.",
                blocked.ip()
            ),
        ));
    }
    Ok(addrs)
}

/// Whether an address is on the public internet.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let segments = v6.segments();
            // NAT64 (64:ff9b::/96) embeds an IPv4 address.
            if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let [a, b] = segments[6].to_be_bytes();
                let [c, d] = segments[7].to_be_bytes();
                return is_public_v4(Ipv4Addr::new(a, b, c, d));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00 // unique local
                || (segments[0] & 0xffc0) == 0xfe80 // link-local
                || (segments[0] & 0xffc0) == 0xfec0 // site-local (deprecated)
                || v6 == Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)
                || segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
        }
    }
}

fn is_public_v4(v4: Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    !(v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (18..20).contains(&b)) // benchmarking
        || a >= 240) // reserved
}

/// The body up to [`MAX_BYTES`], and whether it was cut.
async fn read_capped(
    mut response: reqwest::Response,
    ctx: &ToolContext,
) -> Result<(Vec<u8>, bool), ToolResult> {
    let mut body = Vec::new();
    loop {
        let chunk = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(ToolResult::err("cancelled", "fetch was cancelled")),
            chunk = response.chunk() => chunk,
        }
        .map_err(|err| ToolResult::err("FETCH_FAILED", format!("reading the body failed: {err}")))?;
        let Some(chunk) = chunk else {
            return Ok((body, false));
        };
        let room = MAX_BYTES - body.len();
        if chunk.len() >= room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
}

/// Readable text for a content type, or `None` for a type fetch does not
/// return.
fn to_text(content_type: &str, body: &[u8]) -> Option<String> {
    let mime = content_type.split(';').next().unwrap_or("").trim();
    let text = String::from_utf8_lossy(body);
    let sniff_html = || {
        let head: String = text
            .chars()
            .take(512)
            .collect::<String>()
            .to_ascii_lowercase();
        head.contains("<!doctype html") || head.contains("<html")
    };
    match mime {
        "text/html" | "application/xhtml+xml" => Some(crate::html_text::html_to_text(&text)),
        "" if sniff_html() => Some(crate::html_text::html_to_text(&text)),
        "" if !body.contains(&0) => Some(text.into_owned()),
        _ if mime.starts_with("text/") => Some(text.into_owned()),
        "application/json"
        | "application/xml"
        | "application/javascript"
        | "application/toml"
        | "application/yaml"
        | "application/x-yaml" => Some(text.into_owned()),
        _ if mime.ends_with("+json") || mime.ends_with("+xml") => Some(text.into_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::write::test_context;

    #[test]
    fn non_public_addresses_are_refused() {
        for blocked in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
        ] {
            assert!(!is_public(blocked.parse().unwrap()), "{blocked}");
        }
        for public in ["93.184.216.34", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
    }

    #[test]
    fn fetch_is_in_no_built_in_profile_unless_named() {
        let config = crate::config::Config::builtin();
        let registry = crate::tools::ToolRegistry::builtin();
        for (name, profile) in &config.profiles {
            let tools = registry.select(profile, &[]).unwrap();
            assert!(
                tools.iter().all(|tool| tool.name() != "fetch"),
                "profile {name} includes fetch"
            );
        }
        let mut profile = config.profiles["coding"].clone();
        profile.tools.push("fetch".into());
        let tools = registry.select(&profile, &[]).unwrap();
        assert!(tools.iter().any(|tool| tool.name() == "fetch"));
    }

    #[test]
    fn fetch_is_a_network_call_that_asks_with_the_full_url() {
        let facts = crate::policy::CallFacts::new(
            "fetch",
            true,
            &json!({"url": "https://docs.rs/serde/latest/serde/?q=1"}),
            std::path::Path::new("/tmp"),
        );
        assert!(facts.network);
        assert_eq!(facts.effect, crate::policy::Effect::Exec);
        assert_eq!(
            facts.subject(),
            "fetch https://docs.rs/serde/latest/serde/?q=1"
        );
    }

    #[test]
    fn only_plain_http_urls_are_accepted() {
        assert!(parse_url("https://docs.rs/serde").is_ok());
        assert!(parse_url("file:///etc/passwd").is_err());
        assert!(parse_url("ftp://example.com/").is_err());
        assert!(parse_url("https://user:pass@example.com/").is_err());
        assert!(parse_url("docs.rs/serde").is_err());
    }

    #[tokio::test]
    async fn private_targets_are_refused_unless_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        for url in [
            "http://127.0.0.1:9/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
            "http://localhost:9/",
            "http://[::1]:9/",
        ] {
            let result = FetchTool::default().call(&ctx, json!({"url": url})).await;
            assert!(
                result.content.starts_with("[FETCH_REFUSED]"),
                "{url}: {}",
                result.content
            );
        }
        // An allowed host gets past the address check (and then fails to
        // connect to the closed port, which is a different error).
        let tool = FetchTool::new(FetchConfig {
            allow_private: vec!["127.0.0.1".into()],
        });
        let result = tool.call(&ctx, json!({"url": "http://127.0.0.1:9/"})).await;
        assert!(
            result.content.starts_with("[FETCH_FAILED]"),
            "{}",
            result.content
        );
    }

    /// A one-shot HTTP server on loopback answering each connection with the
    /// next canned response.
    async fn serve(responses: Vec<String>) -> SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        addr
    }

    fn http(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn html_comes_back_as_paged_readable_text() {
        let body = format!(
            "<!doctype html><html><head><title>Docs</title><script>track()</script></head><body><nav>Home | About</nav><main><h1>Usage</h1><p>Call it like this:</p><pre><code class=\"language-rust\">fn main() {{\n    let x = a &lt; b;\n}}</code></pre>{}</main></body></html>",
            "<p>more text here to page through.</p>".repeat(300)
        );
        let addr = serve(vec![http(
            "200 OK",
            "Content-Type: text/html; charset=utf-8\r\n",
            &body,
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let tool = FetchTool::new(FetchConfig {
            allow_private: vec!["127.0.0.1".into()],
        });
        let result = tool
            .call(&ctx, json!({"url": format!("http://{addr}/docs")}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("# Usage"), "{}", result.content);
        assert!(
            result
                .content
                .contains("```rust\nfn main() {\n    let x = a < b;\n}\n```"),
            "{}",
            result.content
        );
        assert!(!result.content.contains("track()"));
        assert!(!result.content.contains("Home | About"));
        assert!(
            result.content.contains("; next offset="),
            "{}",
            result.content
        );
        assert!(result.content.chars().count() <= ctx.caps.tool_budget());
    }

    #[tokio::test]
    async fn a_redirect_to_a_private_address_is_refused() {
        // The first hop is allowed (the test server is on loopback); its
        // redirect to the metadata address is checked again and refused.
        let addr = serve(vec![http(
            "302 Found",
            "Location: http://169.254.169.254/latest/meta-data/\r\n",
            "",
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let tool = FetchTool::new(FetchConfig {
            allow_private: vec!["127.0.0.1".into()],
        });
        let result = tool
            .call(&ctx, json!({"url": format!("http://{addr}/")}))
            .await;
        assert!(
            result.content.starts_with("[FETCH_REFUSED]")
                && result.content.contains("169.254.169.254"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn binary_content_is_not_returned() {
        let addr = serve(vec![http(
            "200 OK",
            "Content-Type: application/octet-stream\r\n",
            "\u{0}\u{1}\u{2}",
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let tool = FetchTool::new(FetchConfig {
            allow_private: vec!["127.0.0.1".into()],
        });
        let result = tool
            .call(&ctx, json!({"url": format!("http://{addr}/blob")}))
            .await;
        assert!(
            result.content.starts_with("[FETCH_UNSUPPORTED]"),
            "{}",
            result.content
        );
    }
}
