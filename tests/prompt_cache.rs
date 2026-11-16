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
            permissions: None,
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
fn full_profile_first_call_estimate_is_within_eight_percent() {
    const PROSE_CHARS_PER_TOKEN: f64 = 3.2;
    let dir = tempfile::tempdir().unwrap();
    let (config, profile) = full_profile();
    let tools = full_tools(&profile);
    let specs = ToolRegistry::specs(&tools);
    let json = tools_json(&specs);
    let prompt = build_system_prompt(
        &config,
        &paths(dir.path()),
        &SkillIndex::bundled(),
        &tools,
        None,
    );
    let user = "Read src/context/budget.rs and explain how the token estimate is assembled.";
    let mut session = session(dir.path());
    session
        .append(EntryKind::User(UserData {
            attachments: Vec::new(),
            content: user.into(),
        }))
        .unwrap();

    // A first call against a tokenizer that spends 3.2 characters per token on
    // prose. The build path calibrates once from a `/tokenize` sample.
    let mut budget = TokenBudget::new(32_768, 4_096);
    let measured_system = (prompt.chars().count() as f64 / PROSE_CHARS_PER_TOKEN).ceil() as u32;
    budget.calibrate(prompt.chars().count(), measured_system);
    let cost = ToolSpecCost::from_json(&json, &budget);
    let assembled = assemble(&prompt, &session, None, &budget);
    let estimate = assembled.estimated_tokens + cost.tokens;
    let measured_tools = (json.chars().count() as f64 / JSON_CHARS_PER_TOKEN).round() as u32;
    let measured_user = (user.chars().count() as f64 / PROSE_CHARS_PER_TOKEN).ceil() as u32;
    let actual = measured_system + measured_tools + measured_user + 8;
    let error = |tokens: u32| (f64::from(tokens) - f64::from(actual)).abs() / f64::from(actual);
    assert!(
        error(estimate) < 0.08,
        "first-call estimate {estimate} vs prompt {actual}: {:.3}",
        error(estimate)
    );

    // The old prose ratio for the same schemas misses the target, which is
    // why the JSON density is a separate constant.
    let old_json_estimate = budget.estimate_char_len(json.chars().count());
    let old_estimate = assembled.estimated_tokens + old_json_estimate;
    assert!(
        error(old_estimate) > 0.08,
        "prose-priced schemas must not pass: {old_estimate} vs {actual}"
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

/// The serialized request up to (not including) its last message: what an
/// engine can reuse from the previous call when only the tail changed.
fn bytes_before_last(assembled: &AssembledContext) -> Vec<u8> {
    let trimmed = AssembledContext {
        messages: assembled.messages[..assembled.messages.len() - 1].to_vec(),
        estimated_tokens: 0,
        char_count: 0,
    };
    message_bytes(&trimmed)
}

fn leading_system(assembled: &AssembledContext) -> &str {
    match assembled.messages.first() {
        Some(ChatMessage::System { content }) => content,
        other => panic!("no leading system message: {other:?}"),
    }
}

fn tool_step(session: &mut Session, n: usize) {
    use localharness::session::{AssistantData, ToolCall, ToolResultData};
    session
        .append(EntryKind::Assistant(AssistantData {
            content: Some(format!("step {n}")),
            tool_calls: Some(vec![ToolCall {
                id: format!("call_{n}"),
                name: "read".into(),
                arguments: serde_json::json!({"path": format!("f{n}.rs")}),
            }]),
            ..AssistantData::default()
        }))
        .unwrap();
    session
        .append(EntryKind::ToolResult(ToolResultData {
            call_id: format!("call_{n}"),
            content: format!("body of f{n}.rs"),
            is_error: false,
            details: None,
            artifact: None,
        }))
        .unwrap();
}

fn hook_context(session: &mut Session, event: &str, text: &str) {
    use localharness::session::CustomData;
    session
        .append(EntryKind::Custom(
            CustomData::new("hook-context")
                .with("hook", "ctx")
                .with("event", event)
                .with("text", text),
        ))
        .unwrap();
}

#[test]
fn state_block_after_compaction_leaves_the_head_byte_identical() {
    use localharness::session::CompactionData;
    let dir = tempfile::tempdir().unwrap();
    let mut session = session(dir.path());
    session
        .append(EntryKind::Compaction(CompactionData {
            summary: "[localharness] summary of earlier work".into(),
            tokens_before: 9000,
            reason: "auto".into(),
            thread_id: None,
            keep_from: None,
        }))
        .unwrap();
    session
        .append(EntryKind::User(UserData {
            attachments: Vec::new(),
            content: "keep going".into(),
        }))
        .unwrap();
    let budget = TokenBudget::new(32_768, 4096);

    tool_step(&mut session, 1);
    let first = assemble(
        "system",
        &session,
        Some("[localharness] state\nverify: not run\ncontext: under 50% of usable prompt"),
        &budget,
    );
    tool_step(&mut session, 2);
    // The state changed between the calls (a file changed, the band moved).
    let second = assemble(
        "system",
        &session,
        Some(
            "[localharness] state\nchanged: f1.rs\nverify: failed\ncontext: over 50% of usable prompt",
        ),
        &budget,
    );
    assert_eq!(leading_system(&first), leading_system(&second));
    assert!(!leading_system(&second).contains("[localharness] state"));
    // Everything the first request sent before its last message is reused.
    let reused = bytes_before_last(&first);
    assert!(message_bytes(&second).starts_with(&reused));
    // The model still sees the state, on the last message.
    assert!(matches!(second.messages.last(),
        Some(ChatMessage::Tool { content, .. }) if content.ends_with("verify: failed\ncontext: over 50% of usable prompt")));
}

#[test]
fn hook_context_during_a_session_does_not_grow_the_head() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = session(dir.path());
    hook_context(&mut session, "session_start", "repo uses tabs");
    session
        .append(EntryKind::User(UserData {
            attachments: Vec::new(),
            content: "fix it".into(),
        }))
        .unwrap();
    hook_context(&mut session, "user_prompt", "ticket ABC-1 is open");
    let budget = TokenBudget::new(32_768, 4096);
    let mut previous: Option<AssembledContext> = None;
    for n in 1..=4 {
        tool_step(&mut session, n);
        // A post_tool hook returns context on every call (its text is part of
        // the tool result itself), and a stop-style hook adds more.
        hook_context(&mut session, "post_tool", &format!("lint ok {n}"));
        hook_context(&mut session, "stop", &format!("note {n}"));
        let assembled = assemble("system", &session, None, &budget);
        let head = leading_system(&assembled);
        assert_eq!(head, "system\n\n[hook context: ctx]\nrepo uses tabs");
        if let Some(previous) = &previous {
            assert_eq!(leading_system(previous), head);
            assert!(message_bytes(&assembled).starts_with(&bytes_before_last(previous)));
        }
        previous = Some(assembled);
    }
    let last = previous.unwrap();
    // The model still sees the in-session context, in place.
    let user = last
        .messages
        .iter()
        .find_map(|m| match m {
            ChatMessage::User { content, .. } => Some(content.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(user, "fix it\n\n[hook context: ctx]\nticket ABC-1 is open");
    assert!(last.messages.iter().any(|m| matches!(m,
        ChatMessage::Tool { content, .. } if content.contains("[hook context: ctx]\nnote 3"))));
}
