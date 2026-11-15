//! Integration checks for the cache-stable prompt prefix and the tool schema
//! accounting (#41, #42, #43). No network or real model.

use std::path::Path;
use std::sync::Arc;

use localharness::agent::prompt::{
    SESSION_FACTS_HEADING, build_system_prompt, stable_prefix_chars,
};
use localharness::config::{Config, ProfileConfig};
use localharness::context::{
    AssembledContext, JSON_CHARS_PER_TOKEN, TokenBudget, ToolSpecCost, assemble,
    common_prefix_chars, prompt_prefix,
};
use localharness::paths::Paths;
use localharness::provider::{ChatMessage, ToolSpec, tools_json, tools_payload};
use localharness::session::{EntryKind, Session, SessionData, UserData};
use localharness::skills::SkillIndex;
use localharness::tools::{Tool, ToolRegistry};

fn paths(dir: &Path) -> Paths {
    let mut paths = Paths::discover(dir).unwrap();
    paths.data_dir = dir.join("data");
    paths.config_dir = dir.join("config");
    paths
}

fn full_profile() -> (Config, ProfileConfig) {
    let mut config = Config::builtin();
    config.profile_name = "full".into();
    let profile = config.profile().clone();
    (config, profile)
}

fn full_tools(profile: &ProfileConfig) -> Vec<Arc<dyn Tool>> {
    ToolRegistry::builtin().select(profile, &[]).unwrap()
}

fn session(dir: &Path) -> Session {
    Session::create(
        &paths(dir),
        SessionData {
            cwd: dir.display().to_string(),
            endpoint: "http://127.0.0.1:0".into(),
            model: "test-model".into(),
            profile: "full".into(),
            app: "test".into(),
            redact: true,
            context: None,
        },
        true,
    )
    .unwrap()
}

/// Concatenate the request messages the way the provider sends them, without
/// depending on serde field order.
fn message_bytes(assembled: &AssembledContext) -> Vec<u8> {
    let mut out = Vec::new();
    for message in &assembled.messages {
        match message {
            ChatMessage::System { content } => {
                out.extend_from_slice(b"system\0");
                out.extend_from_slice(content.as_bytes());
            }
            ChatMessage::User { content, images } => {
                out.extend_from_slice(b"user\0");
                out.extend_from_slice(content.as_bytes());
                out.extend_from_slice(b"\0images\0");
                out.extend_from_slice(images.len().to_string().as_bytes());
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                out.extend_from_slice(b"assistant\0");
                out.extend_from_slice(content.as_deref().unwrap_or("").as_bytes());
                for call in tool_calls {
                    out.extend_from_slice(b"\0call\0");
                    out.extend_from_slice(call.id.as_bytes());
                    out.extend_from_slice(b"\0");
                    out.extend_from_slice(call.name.as_bytes());
                    out.extend_from_slice(b"\0");
                    out.extend_from_slice(call.arguments.to_string().as_bytes());
                }
            }
            ChatMessage::Tool { call_id, content } => {
                out.extend_from_slice(b"tool\0");
                out.extend_from_slice(call_id.as_bytes());
                out.extend_from_slice(b"\0");
                out.extend_from_slice(content.as_bytes());
            }
        }
        out.push(b'\n');
    }
    out
}

fn assert_sorted_keys(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let keys: Vec<&String> = map.keys().collect();
            let mut sorted = keys.clone();
            sorted.sort();
            assert_eq!(keys, sorted, "JSON object keys must have a stable order");
            for value in map.values() {
                assert_sorted_keys(value);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(assert_sorted_keys),
        _ => {}
    }
}

#[test]
fn full_profile_prompt_never_repeats_a_tool_description() {
    let dir = tempfile::tempdir().unwrap();
    let (config, profile) = full_profile();
    let tools = full_tools(&profile);
    let specs = ToolRegistry::specs(&tools);
    let prompt = build_system_prompt(
        &config,
        &paths(dir.path()),
        &SkillIndex::bundled(),
        &tools,
        None,
    );

    let payload = tools_payload(&specs);
    let functions = payload.as_array().expect("tools array");
    for function in functions {
        let description = function["function"]["description"]
            .as_str()
            .expect("tool description");
        assert!(!description.is_empty());
        assert!(
            !prompt.contains(description),
            "the system prompt repeats tools[].function.description for {}",
            function["function"]["name"]
        );
    }

    // The request body still carries every active tool, with its schema.
    let json = tools_json(&specs);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let sent = parsed.as_array().expect("tools array");
    assert_eq!(sent.len(), tools.len());
    for spec in &specs {
        assert!(json.contains(&format!("\"name\":\"{}\"", spec.name)));
        assert!(json.contains("parameters"));
    }

    // The old inventory was one `- name: description` line per tool.
    let names = tools
        .iter()
        .map(|tool| tool.name())
        .collect::<Vec<_>>()
        .join(", ");
    let index_chars = format!(
        "Available tools (names only; descriptions and schemas travel with the request): {names}"
    )
    .chars()
    .count();
    let old_inventory_chars: usize = specs
        .iter()
        .map(|spec| {
            format!("- {}: {}\n", spec.name, spec.description)
                .chars()
                .count()
        })
        .sum();
    assert!(
        old_inventory_chars >= index_chars + 1_800,
        "the full-profile prompt shrank by only {} characters",
        old_inventory_chars - index_chars
    );
}

#[test]
fn tool_schema_cost_uses_json_density_and_is_fixed() {
    let (_config, profile) = full_profile();
    let tools = full_tools(&profile);
    let specs = ToolRegistry::specs(&tools);
    let json = tools_json(&specs);
    let budget = TokenBudget::new(32_768, 4_096);
    let cost = ToolSpecCost::from_json(&json, &budget);

    assert_eq!(cost.chars, json.chars().count());
    assert_eq!(
        cost.tokens,
        ((cost.chars as f64) / JSON_CHARS_PER_TOKEN).ceil() as u32
    );
    // Prose pricing would book the same schemas roughly a quarter cheaper.
    let prose = budget.estimate_char_len(cost.chars);
    assert!(cost.tokens > prose);

    // At the measured JSON density the fixed term is within the 8% first-call
    // target; the old prose ratio is not.
    let actual = ((cost.chars as f64) / JSON_CHARS_PER_TOKEN).round() as f64;
    let error = |estimate: u32| (f64::from(estimate) - actual).abs() / actual;
    assert!(error(cost.tokens) < 0.08, "JSON estimate error too high");
    assert!(
        error(prose) > 0.08,
        "the prose ratio must not price schemas"
    );

    // `/tokenize` replaces only the token estimate; the fixed chars remain.
    assert_eq!(cost.with_tokens(1_234).tokens, 1_234);
    assert_eq!(cost.with_tokens(1_234).chars, cost.chars);
    assert_eq!(
        ToolSpecCost::from_json("", &budget),
        ToolSpecCost::default()
    );
}

#[test]
fn same_branch_assembles_a_byte_identical_prefix_and_hash() {
    let dir = tempfile::tempdir().unwrap();
    let (config, profile) = full_profile();
    let tools = full_tools(&profile);
    let specs: Vec<ToolSpec> = ToolRegistry::specs(&tools);
    let tools_json = tools_json(&specs);
    let prompt = build_system_prompt(
        &config,
        &paths(dir.path()),
        &SkillIndex::bundled(),
        &tools,
        None,
    );
    let mut session = session(dir.path());
    session
        .append(EntryKind::User(UserData {
            attachments: Vec::new(),
            content: "inspect the parser".into(),
        }))
        .unwrap();

    let budget = TokenBudget::new(32_768, 4_096);
    let first = assemble(&prompt, &session, None, &budget);
    let second = assemble(&prompt, &session, None, &budget);
    assert_eq!(message_bytes(&first), message_bytes(&second));

    let prefix = prompt_prefix(&prompt, &tools_json);
    assert_eq!(prefix, prompt_prefix(&prompt, &tools_json));
    assert_eq!(prefix.hash.len(), 64);
    assert_eq!(
        prefix.chars,
        prompt.chars().count() + tools_json.chars().count()
    );

    // Volatile facts live behind the stable head, so a different checkout
    // keeps nearly the whole prefix.
    let stable = stable_prefix_chars(&prompt);
    assert!(stable > 0);
    assert!(prompt[..stable].starts_with("You are LocalHarness"));
    assert!(prompt[..stable].contains("## Operating rules"));
    assert!(!prompt[..stable].contains("Working directory:"));
    assert!(prompt[stable..].starts_with(SESSION_FACTS_HEADING));
    let mut other_paths = paths(dir.path());
    other_paths.cwd = dir.path().join("another checkout");
    let other = build_system_prompt(&config, &other_paths, &SkillIndex::bundled(), &tools, None);
    assert!(
        common_prefix_chars(&prompt, &other) >= prompt[..stable].chars().count(),
        "the stable head must survive a cwd change"
    );
}

#[test]
fn tool_order_and_schema_key_order_are_stable_across_restarts() {
    let (_config, profile) = full_profile();
    let first = full_tools(&profile);
    let second = full_tools(&profile);
    let first_specs = ToolRegistry::specs(&first);
    let second_specs = ToolRegistry::specs(&second);

    let names = |specs: &[ToolSpec]| -> Vec<String> {
        specs.iter().map(|spec| spec.name.clone()).collect()
    };
    assert_eq!(
        names(&first_specs),
        names(&second_specs),
        "a fresh process must select tools in the same order"
    );
    let first_json = tools_json(&first_specs);
    let second_json = tools_json(&second_specs);
    assert_eq!(
        first_json, second_json,
        "schema serialization must be byte-identical"
    );
    let parsed: serde_json::Value = serde_json::from_str(&first_json).unwrap();
    assert_sorted_keys(&parsed);
}
