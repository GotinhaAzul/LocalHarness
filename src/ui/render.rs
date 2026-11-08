//! Transcript model, block rendering, layout, and the frame painter.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as WidgetBlock, Borders, Clear, Paragraph};
use ratatui_textarea::TextArea;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::session::{EntryKind, Session};
use crate::ui::markdown::{render_diff, render_markdown, wrap_plain};
use crate::ui::theme::{Role, Theme};
use crate::ui::widgets::{FooterData, HeaderData, ModalData, footer_line, header_line};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Error,
}

#[derive(Debug, Clone)]
pub enum BlockKind {
    User(String),
    Assistant {
        text: String,
        reasoning: Option<String>,
    },
    Tool {
        call_id: Option<String>,
        name: String,
        summary: String,
        status: ToolStatus,
        duration_ms: Option<u64>,
        preview: String,
        artifact: Option<String>,
    },
    Error {
        code: String,
        message: String,
    },
    Compaction {
        reason: String,
        summary: String,
        tokens_before: Option<u64>,
    },
    Note {
        title: String,
        body: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Block {
    pub kind: BlockKind,
    pub ts: Option<DateTime<Utc>>,
}

impl Block {
    pub fn user(text: impl Into<String>) -> Self {
        Block {
            kind: BlockKind::User(text.into()),
            ts: Some(Utc::now()),
        }
    }

    pub fn assistant(text: impl Into<String>, reasoning: Option<String>) -> Self {
        Block {
            kind: BlockKind::Assistant {
                text: text.into(),
                reasoning,
            },
            ts: Some(Utc::now()),
        }
    }

    pub fn tool(
        call_id: Option<String>,
        name: impl Into<String>,
        summary: impl Into<String>,
        status: ToolStatus,
        duration_ms: Option<u64>,
        preview: impl Into<String>,
        artifact: Option<String>,
    ) -> Self {
        Block {
            kind: BlockKind::Tool {
                call_id,
                name: name.into(),
                summary: summary.into(),
                status,
                duration_ms,
                preview: preview.into(),
                artifact,
            },
            ts: None,
        }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Block {
            kind: BlockKind::Error {
                code: code.into(),
                message: message.into(),
            },
            ts: Some(Utc::now()),
        }
    }

    pub fn note(title: impl Into<String>, body: impl Into<String>, is_error: bool) -> Self {
        Block {
            kind: BlockKind::Note {
                title: title.into(),
                body: body.into(),
                is_error,
            },
            ts: None,
        }
    }
}

fn assistant_is_empty(block: &Block) -> bool {
    match &block.kind {
        BlockKind::Assistant { text, reasoning } => {
            text.trim().is_empty()
                && reasoning
                    .as_deref()
                    .map(str::trim)
                    .map(str::is_empty)
                    .unwrap_or(true)
        }
        _ => false,
    }
}

/// Scrolling transcript with a cached, pre-wrapped line buffer.
pub struct Transcript {
    session: Vec<Block>,
    extra: Vec<Block>,
    turn: Vec<Block>,
    live_user: Option<Block>,
    live_assistant: Option<Block>,
    lines: Vec<Line<'static>>,
    built_width: usize,
    show_reasoning: bool,
    dirty: bool,
    last_build: Option<Instant>,
    pub scroll: usize,
    pub follow: bool,
    pub new_output: bool,
    pub viewport: usize,
}

impl Transcript {
    pub fn new(show_reasoning: bool) -> Self {
        Transcript {
            session: Vec::new(),
            extra: Vec::new(),
            turn: Vec::new(),
            live_user: None,
            live_assistant: None,
            lines: Vec::new(),
            built_width: 0,
            show_reasoning,
            dirty: true,
            last_build: None,
            scroll: 0,
            follow: true,
            new_output: false,
            viewport: 0,
        }
    }

    pub fn set_session(&mut self, blocks: Vec<Block>) {
        self.session = blocks;
        self.mark_dirty();
    }

    pub fn set_show_reasoning(&mut self, show: bool) {
        if self.show_reasoning != show {
            self.show_reasoning = show;
            self.mark_dirty();
        }
    }

    pub fn clear_view(&mut self) {
        self.session.clear();
        self.extra.clear();
        self.turn.clear();
        self.live_user = None;
        self.live_assistant = None;
        self.follow = true;
        self.new_output = false;
        self.scroll = 0;
        self.mark_dirty();
    }

    pub fn push_note(&mut self, title: impl Into<String>, body: impl Into<String>, is_error: bool) {
        self.extra.push(Block::note(title, body, is_error));
        self.mark_dirty();
    }

    pub fn begin_turn(&mut self, user_text: &str) {
        self.turn.clear();
        self.live_assistant = None;
        self.live_user = Some(Block::user(user_text));
        self.follow = true;
        self.new_output = false;
        self.mark_dirty();
    }

    pub fn text_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let block = self
            .live_assistant
            .get_or_insert_with(|| Block::assistant(String::new(), None));
        if let BlockKind::Assistant { text, .. } = &mut block.kind {
            text.push_str(delta);
        }
        self.mark_dirty();
    }

    pub fn reasoning_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let block = self
            .live_assistant
            .get_or_insert_with(|| Block::assistant(String::new(), None));
        if let BlockKind::Assistant { reasoning, .. } = &mut block.kind {
            reasoning.get_or_insert_with(String::new).push_str(delta);
        }
        self.mark_dirty();
    }

    fn flush_live_assistant(&mut self) {
        if let Some(block) = self.live_assistant.take()
            && !assistant_is_empty(&block)
        {
            self.turn.push(block);
        }
    }

    pub fn tool_started(&mut self, call_id: &str, name: &str, summary: &str) {
        self.flush_live_assistant();
        self.turn.push(Block::tool(
            Some(call_id.to_string()),
            name,
            summary,
            ToolStatus::Running,
            None,
            String::new(),
            None,
        ));
        self.mark_dirty();
    }

    pub fn tool_finished(
        &mut self,
        call_id: &str,
        name: &str,
        is_error: bool,
        duration_ms: u64,
        preview: &str,
        artifact: Option<String>,
    ) {
        let status = if is_error {
            ToolStatus::Error
        } else {
            ToolStatus::Ok
        };
        let existing = self.turn.iter_mut().rev().find(|block| {
            matches!(&block.kind, BlockKind::Tool { call_id: Some(id), .. } if id == call_id)
        });
        if let Some(block) = existing {
            if let BlockKind::Tool {
                status: slot,
                duration_ms: duration,
                preview: preview_slot,
                artifact: artifact_slot,
                ..
            } = &mut block.kind
            {
                *slot = status;
                *duration = Some(duration_ms);
                *preview_slot = preview.to_string();
                *artifact_slot = artifact;
            }
        } else {
            self.turn.push(Block::tool(
                Some(call_id.to_string()),
                name,
                "",
                status,
                Some(duration_ms),
                preview,
                artifact,
            ));
        }
        self.mark_dirty();
    }

    pub fn turn_error(&mut self, code: &str, message: &str) {
        self.turn.push(Block::error(code, message));
        self.mark_dirty();
    }

    pub fn turn_note(&mut self, title: &str, body: &str, is_error: bool) {
        self.turn.push(Block::note(title, body, is_error));
        self.mark_dirty();
    }

    pub fn finish_turn(&mut self) {
        self.turn.clear();
        self.live_user = None;
        self.live_assistant = None;
        self.mark_dirty();
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
        if !self.follow {
            self.new_output = true;
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn last_build(&self) -> Option<Instant> {
        self.last_build
    }

    /// Rebuild wrapped lines when dirty, at most once per 30 ms.
    pub fn prepare(&mut self, width: usize, theme: &Theme, show_reasoning: bool, force: bool) {
        if width == 0 {
            return;
        }
        if show_reasoning != self.show_reasoning {
            self.show_reasoning = show_reasoning;
            self.dirty = true;
        }
        if !self.dirty && width == self.built_width {
            return;
        }
        if !force
            && let Some(last) = self.last_build
            && last.elapsed() < Duration::from_millis(30)
        {
            return;
        }
        let blocks = self
            .session
            .iter()
            .chain(self.extra.iter())
            .chain(self.turn.iter())
            .chain(self.live_user.iter())
            .chain(self.live_assistant.iter());
        let mut lines = Vec::new();
        for (index, block) in blocks.enumerate() {
            if index > 0 {
                lines.push(Line::raw(""));
            }
            lines.extend(render_block(block, width, theme, self.show_reasoning));
        }
        self.lines = lines;
        self.built_width = width;
        self.dirty = false;
        self.last_build = Some(Instant::now());
        if self.follow {
            self.scroll = self.max_scroll();
        } else {
            self.scroll = self.scroll.min(self.max_scroll());
        }
    }

    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    pub fn set_viewport(&mut self, height: usize) {
        self.viewport = height;
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.viewport.max(1))
    }

    pub fn scroll_for(&self, viewport: usize) -> usize {
        let max = self.lines.len().saturating_sub(viewport.max(1));
        if self.follow {
            max
        } else {
            self.scroll.min(max)
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        let top = self.scroll_for(self.viewport);
        self.follow = false;
        self.scroll = top.saturating_sub(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        let top = self.scroll_for(self.viewport);
        let max = self.max_scroll();
        let next = (top + lines).min(max);
        self.scroll = next;
        if next >= max {
            self.follow = true;
            self.new_output = false;
        }
    }

    pub fn scroll_home(&mut self) {
        self.follow = false;
        self.scroll = 0;
    }

    pub fn scroll_end(&mut self) {
        self.follow = true;
        self.new_output = false;
        self.scroll = self.max_scroll();
    }
}

/// Build display blocks for the active branch, hiding entries at or before
/// `floor_seq` (`/clear`).
pub fn blocks_from_session_after(session: &Session, floor_seq: u64) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut calls: HashMap<String, (String, String)> = HashMap::new();
    for entry in session.branch_entries() {
        if entry.seq <= floor_seq {
            continue;
        }
        match &entry.kind {
            EntryKind::User(data) => blocks.push(Block {
                kind: BlockKind::User(data.content.clone()),
                ts: Some(entry.ts),
            }),
            EntryKind::Assistant(data) => {
                if let Some(list) = &data.tool_calls {
                    for call in list {
                        calls.insert(
                            call.id.clone(),
                            (call.name.clone(), summarize_arguments(&call.arguments)),
                        );
                    }
                }
                let text = data.content.clone().unwrap_or_default();
                let reasoning = data.reasoning.clone().filter(|r| !r.trim().is_empty());
                if !text.trim().is_empty() || reasoning.is_some() {
                    blocks.push(Block {
                        kind: BlockKind::Assistant { text, reasoning },
                        ts: Some(entry.ts),
                    });
                }
            }
            EntryKind::ToolResult(data) => {
                let (name, summary) = calls
                    .get(&data.call_id)
                    .cloned()
                    .unwrap_or_else(|| ("tool".to_string(), data.call_id.clone()));
                blocks.push(Block {
                    kind: BlockKind::Tool {
                        call_id: Some(data.call_id.clone()),
                        name,
                        summary,
                        status: if data.is_error {
                            ToolStatus::Error
                        } else {
                            ToolStatus::Ok
                        },
                        duration_ms: None,
                        preview: preview_text(&data.content, 800, 12),
                        artifact: data.artifact.clone(),
                    },
                    ts: None,
                });
            }
            EntryKind::Compaction(data) => blocks.push(Block {
                kind: BlockKind::Compaction {
                    reason: data.reason.clone(),
                    summary: data.summary.clone(),
                    tokens_before: Some(data.tokens_before),
                },
                ts: Some(entry.ts),
            }),
            EntryKind::Error(data) => blocks.push(Block {
                kind: BlockKind::Error {
                    code: data.code.clone(),
                    message: data.message.clone(),
                },
                ts: Some(entry.ts),
            }),
            EntryKind::Session(_) | EntryKind::Checkout(_) | EntryKind::Custom(_) => {}
        }
    }
    blocks
}

pub fn blocks_from_session(session: &Session) -> Vec<Block> {
    blocks_from_session_after(session, 0)
}

fn summarize_arguments(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::Object(map) if !map.is_empty() => map
            .iter()
            .map(|(key, value)| {
                let rendered = match value {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                format!("{key}={rendered}")
            })
            .collect::<Vec<_>>()
            .join(" "),
        serde_json::Value::Object(_) => String::new(),
        other => other.to_string(),
    };
    clip_text(&text, 90)
}

fn clip_text(text: &str, max: usize) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    one_line
        .chars()
        .take(max.saturating_sub(1))
        .collect::<String>()
        + "…"
}

fn preview_text(text: &str, max_chars: usize, max_lines: usize) -> String {
    let mut out = String::new();
    for (index, line) in text.lines().enumerate() {
        if index >= max_lines {
            out.push('…');
            break;
        }
        if index > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    if out.chars().count() > max_chars {
        out = out.chars().take(max_chars).collect::<String>() + "…";
    }
    out
}

/// Render a slice of blocks into wrapped lines. Public for tests.
pub fn blocks_to_lines(
    blocks: &[Block],
    width: usize,
    theme: &Theme,
    show_reasoning: bool,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            lines.push(Line::raw(""));
        }
        lines.extend(render_block(block, width, theme, show_reasoning));
    }
    lines
}

fn label_line(role: Role, label: &str, ts: Option<DateTime<Utc>>, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(label.to_string(), theme.bold(role))];
    if let Some(ts) = ts {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            ts.format("%H:%M").to_string(),
            theme.style(Role::Muted),
        ));
    }
    Line::from(spans)
}

fn render_block(
    block: &Block,
    width: usize,
    theme: &Theme,
    show_reasoning: bool,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    match &block.kind {
        BlockKind::User(text) => {
            let mut lines = vec![label_line(Role::User, "You", block.ts, theme)];
            for raw in text.lines() {
                for wrapped in wrap_plain(raw, width) {
                    lines.push(Line::styled(wrapped, theme.style(Role::Fg)));
                }
            }
            if text.is_empty() {
                lines.push(Line::raw(""));
            }
            lines
        }
        BlockKind::Assistant { text, reasoning } => {
            let mut lines = vec![label_line(Role::Assistant, "Assistant", block.ts, theme)];
            if let Some(reasoning) = reasoning {
                if reasoning.trim().is_empty() {
                    // nothing
                } else if show_reasoning {
                    let tokens = (reasoning.chars().count() as u64).div_ceil(4);
                    lines.push(Line::from(vec![
                        Span::styled("reasoning", theme.bold(Role::Reasoning)),
                        Span::styled(format!(" · {tokens} tokens"), theme.style(Role::Reasoning)),
                    ]));
                    for raw in reasoning.lines() {
                        for wrapped in wrap_plain(raw, width.saturating_sub(2).max(1)) {
                            lines.push(Line::from(vec![
                                Span::styled("│ ", theme.style(Role::Border)),
                                Span::styled(
                                    wrapped,
                                    theme.style(Role::Reasoning).add_modifier(Modifier::ITALIC),
                                ),
                            ]));
                        }
                    }
                } else {
                    let tokens = (reasoning.chars().count() as u64).div_ceil(4);
                    lines.push(Line::styled(
                        format!("reasoning · {tokens} tokens · Ctrl+R to expand"),
                        theme.style(Role::Reasoning),
                    ));
                }
            }
            if !text.trim().is_empty() {
                lines.extend(assistant_markdown(text, width, theme));
            }
            lines
        }
        BlockKind::Tool {
            name,
            summary,
            status,
            duration_ms,
            preview,
            artifact,
            ..
        } => tool_card(
            name,
            summary,
            *status,
            *duration_ms,
            preview,
            artifact.as_deref(),
            width,
            theme,
        ),
        BlockKind::Error { code, message } => error_card(code, message, width, theme),
        BlockKind::Compaction {
            reason,
            summary,
            tokens_before,
        } => {
            let mut lines = vec![Line::from(vec![
                Span::styled("compaction", theme.bold(Role::Accent)),
                Span::styled(format!(" ({reason})"), theme.style(Role::Muted)),
                Span::styled(
                    (*tokens_before)
                        .map(|tokens| format!(" · {tokens} tokens before"))
                        .unwrap_or_default(),
                    theme.style(Role::Muted),
                ),
            ])];
            for raw in summary.lines().take(8) {
                for wrapped in wrap_plain(raw, width.saturating_sub(2).max(1)) {
                    lines.push(Line::from(vec![
                        Span::styled("│ ", theme.style(Role::Border)),
                        Span::styled(wrapped, theme.style(Role::Muted)),
                    ]));
                }
            }
            lines
        }
        BlockKind::Note {
            title,
            body,
            is_error,
        } => {
            let role = if *is_error { Role::Error } else { Role::Accent };
            let mut lines = vec![label_line(role, title, block.ts, theme)];
            for raw in body.lines() {
                for wrapped in wrap_plain(raw, width) {
                    lines.push(Line::styled(wrapped, theme.style(Role::Fg)));
                }
            }
            lines
        }
    }
}

fn status_label(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "running",
        ToolStatus::Ok => "ok",
        ToolStatus::Error => "error",
    }
}

fn status_role(status: ToolStatus) -> Role {
    match status {
        ToolStatus::Running => Role::ToolRunning,
        ToolStatus::Ok => Role::ToolOk,
        ToolStatus::Error => Role::ToolError,
    }
}

fn duration_label(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

fn artifact_note(artifact: &str) -> String {
    let stem = Path::new(artifact)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(artifact);
    format!("[{stem}]")
}

#[allow(clippy::too_many_arguments)]
fn tool_card(
    name: &str,
    summary: &str,
    status: ToolStatus,
    duration_ms: Option<u64>,
    preview: &str,
    artifact: Option<&str>,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let role = status_role(status);
    let style = theme.style(role);
    let border = theme.style(Role::Border);
    let mut title = format!("{name} · {summary} · {}", status_label(status));
    if let Some(ms) = duration_ms {
        title.push_str(&format!(" · {}", duration_label(ms)));
    }
    if let Some(artifact) = artifact {
        title.push_str(&format!(" · {}", artifact_note(artifact)));
    }
    if width < 10 {
        return vec![Line::styled(clip_text(&title, width.max(1)), style)];
    }
    let title = clip_text(&title, width.saturating_sub(6));
    let title_width = UnicodeWidthStr::width(title.as_str());
    let dashes = width.saturating_sub(5 + title_width);
    let mut lines = vec![Line::from(vec![
        Span::styled("╭─ ", border),
        Span::styled(title, theme.bold(role)),
        Span::raw(" "),
        Span::styled("─".repeat(dashes), border),
        Span::styled("╮", border),
    ])];
    let inner = width.saturating_sub(3).max(1);
    let looks_like_diff = preview.lines().take(6).any(|line| {
        line.starts_with("@@")
            || line.starts_with("+++ ")
            || line.starts_with("--- ")
            || (line.starts_with('+') || line.starts_with('-')) && preview.lines().count() > 1
    });
    let mut body: Vec<Line<'static>> = Vec::new();
    if looks_like_diff {
        body.extend(render_diff(preview, inner, theme));
    } else {
        for raw in preview.lines() {
            for wrapped in wrap_plain(raw, inner) {
                body.push(Line::styled(wrapped, theme.style(Role::Fg)));
            }
        }
    }
    if body.is_empty() {
        body.push(Line::raw(""));
    }
    for line in body {
        let content_width = line
            .spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum::<usize>();
        let padding = inner.saturating_sub(content_width);
        let mut spans = vec![Span::styled("│ ", border)];
        spans.extend(line.spans);
        spans.push(Span::raw(" ".repeat(padding)));
        spans.push(Span::styled("│", border));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(vec![
        Span::styled("╰", border),
        Span::styled("─".repeat(width.saturating_sub(2)), border),
        Span::styled("╯", border),
    ]));
    lines
}

fn error_card(code: &str, message: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let border = theme.style(Role::Error);
    let title = clip_text(&format!("error · {code}"), width.saturating_sub(6));
    if width < 10 {
        return vec![Line::styled(
            clip_text(&format!("error · {code}: {message}"), width.max(1)),
            theme.style(Role::Error),
        )];
    }
    let title_width = UnicodeWidthStr::width(title.as_str());
    let dashes = width.saturating_sub(5 + title_width);
    let mut lines = vec![Line::from(vec![
        Span::styled("╭─ ", border),
        Span::styled(title, theme.bold(Role::Error)),
        Span::raw(" "),
        Span::styled("─".repeat(dashes), border),
        Span::styled("╮", border),
    ])];
    let inner = width.saturating_sub(3).max(1);
    for raw in message.lines() {
        for wrapped in wrap_plain(raw, inner) {
            let padding = inner.saturating_sub(UnicodeWidthStr::width(wrapped.as_str()));
            lines.push(Line::from(vec![
                Span::styled("│ ", border),
                Span::styled(wrapped, theme.style(Role::Error)),
                Span::raw(" ".repeat(padding)),
                Span::styled("│", border),
            ]));
        }
    }
    lines.push(Line::from(vec![
        Span::styled("╰", border),
        Span::styled("─".repeat(width.saturating_sub(2)), border),
        Span::styled("╯", border),
    ]));
    lines
}

fn fence_looks_like_diff(language: &str, lines: &[&str]) -> bool {
    let language = language.to_ascii_lowercase();
    if language == "diff" || language == "patch" {
        return true;
    }
    lines
        .iter()
        .take(8)
        .any(|line| line.starts_with("@@ ") || line.starts_with("+++ ") || line.starts_with("--- "))
}

fn render_fence(
    language: &str,
    content: &[&str],
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
) {
    let border = theme.style(Role::Border);
    let label = if language.is_empty() {
        "code"
    } else {
        language
    };
    let label_width = UnicodeWidthStr::width(label);
    let dashes = width.saturating_sub(5 + label_width);
    out.push(Line::from(vec![
        Span::styled("╭─ ", border),
        Span::styled(label.to_string(), theme.style(Role::Muted)),
        Span::raw(" "),
        Span::styled("─".repeat(dashes), border),
        Span::styled("╮", border),
    ]));
    if fence_looks_like_diff(language, content) {
        for line in render_diff(&content.join("\n"), width.saturating_sub(2).max(1), theme) {
            let mut spans = vec![Span::styled("│ ", border)];
            spans.extend(line.spans);
            out.push(Line::from(spans));
        }
    } else {
        for raw in content {
            let spans = crate::ui::highlight::highlight_line(raw, language, theme);
            let line = Line::from(spans);
            for wrapped in wrap_styled(line, width.saturating_sub(2).max(1)) {
                let mut spans = vec![Span::styled("│ ", border)];
                spans.extend(wrapped.spans);
                out.push(Line::from(spans));
            }
        }
    }
    out.push(Line::from(vec![
        Span::styled("╰", border),
        Span::styled("─".repeat(width.saturating_sub(2)), border),
        Span::styled("╯", border),
    ]));
}

/// Markdown rendering with diff fences routed through `render_diff`.
fn assistant_markdown(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    let mut prose: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if let Some(info) = line.trim_start().strip_prefix("```") {
            if !prose.is_empty() {
                let segment = prose.join("\n");
                out.extend(render_markdown(&segment, width, theme));
                prose.clear();
            }
            let language = info.trim();
            let start = index + 1;
            let end = (start..lines.len())
                .find(|candidate| lines[*candidate].trim_start().starts_with("```"));
            match end {
                Some(end) => {
                    render_fence(language, &lines[start..end], width, theme, &mut out);
                    index = end + 1;
                }
                None => {
                    // Incomplete fences render as plain text until closed.
                    let rest = lines[index..].join("\n");
                    out.extend(render_markdown(&rest, width, theme));
                    return out;
                }
            }
        } else {
            prose.push(line);
            index += 1;
        }
    }
    if !prose.is_empty() {
        let segment = prose.join("\n");
        out.extend(render_markdown(&segment, width, theme));
    }
    out
}

/// Wrap one styled line to `width` display columns, breaking long words.
pub fn wrap_styled(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;
    for span in line.spans {
        let style = span.style;
        let mut chunk = String::new();
        for ch in span.content.chars() {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_width + char_width > width && current_width > 0 {
                if !chunk.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut chunk), style));
                }
                out.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }
            chunk.push(ch);
            current_width += char_width;
        }
        if !chunk.is_empty() {
            current.push(Span::styled(chunk, style));
        }
    }
    if !current.is_empty() || out.is_empty() {
        out.push(Line::from(current));
    }
    out
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutRects {
    pub header: Rect,
    pub transcript: Rect,
    pub input: Rect,
    pub debug: Rect,
    pub footer: Rect,
}

/// Split the screen into header, transcript, input, optional debug panel, and
/// footer.
pub fn split(area: Rect, input_lines: usize, debug_lines: usize) -> LayoutRects {
    let input_height = (input_lines as u16 + 2).clamp(3, 8);
    let debug_height = debug_lines.min(8) as u16;
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(input_height),
        Constraint::Length(debug_height),
        Constraint::Length(1),
    ])
    .split(area);
    LayoutRects {
        header: chunks[0],
        transcript: chunks[1],
        input: chunks[2],
        debug: chunks[3],
        footer: chunks[4],
    }
}

pub struct Scene<'a> {
    pub header: HeaderData,
    pub transcript: &'a Transcript,
    pub input: &'a TextArea<'static>,
    pub debug_lines: Vec<String>,
    pub footer: FooterData,
    pub completions: Vec<(&'static str, &'static str)>,
    pub completion_index: usize,
    pub modal: Option<&'a ModalData>,
}

/// Paint the whole UI. The caller prepares the transcript and input styling.
pub fn draw(frame: &mut Frame, scene: &Scene<'_>, theme: &Theme) {
    let area = frame.area();
    let rects = split(area, scene.input.lines().len(), scene.debug_lines.len());

    if rects.header.height > 0 {
        frame.render_widget(
            Paragraph::new(header_line(
                &scene.header,
                rects.header.width as usize,
                theme,
            )),
            rects.header,
        );
    }

    if rects.transcript.height > 0 {
        let scroll = scene
            .transcript
            .scroll_for(rects.transcript.height as usize);
        frame.render_widget(
            Paragraph::new(scene.transcript.lines().to_vec()).scroll((scroll as u16, 0)),
            rects.transcript,
        );
        if scene.transcript.new_output && !scene.transcript.follow {
            let text = "↓ new output";
            let width = UnicodeWidthStr::width(text);
            if width < rects.transcript.width as usize {
                frame.buffer_mut().set_stringn(
                    rects.transcript.right().saturating_sub(width as u16),
                    rects.transcript.bottom().saturating_sub(1),
                    text,
                    width,
                    theme.bold(Role::Warning),
                );
            }
        }
    }

    if rects.input.height > 0 {
        frame.render_widget(scene.input, rects.input);
    }

    if rects.debug.height > 0 && !scene.debug_lines.is_empty() {
        let lines: Vec<Line<'static>> = scene
            .debug_lines
            .iter()
            .take(rects.debug.height as usize)
            .map(|line| {
                Line::styled(
                    crate::ui::widgets::clip_end(line, rects.debug.width as usize),
                    theme.style(Role::Muted),
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rects.debug);
    }

    if rects.footer.height > 0 {
        frame.render_widget(
            Paragraph::new(footer_line(
                &scene.footer,
                rects.footer.width as usize,
                theme,
            )),
            rects.footer,
        );
    }

    if !scene.completions.is_empty() && scene.modal.is_none() {
        render_completions(frame, scene, rects.transcript, theme);
    }

    if let Some(modal) = scene.modal {
        render_modal(frame, modal, area, theme);
    }
}

fn render_completions(frame: &mut Frame, scene: &Scene<'_>, area: Rect, theme: &Theme) {
    if area.height < 3 || area.width < 12 {
        return;
    }
    let rows = scene.completions.len().min(7);
    let height = (rows as u16 + 2).min(area.height);
    let width = area.width.min(72);
    let popup = Rect::new(area.x, area.bottom().saturating_sub(height), width, height);
    frame.render_widget(Clear, popup);
    let block = WidgetBlock::default()
        .borders(Borders::ALL)
        .border_style(theme.style(Role::Border))
        .title(Span::styled(" commands ", theme.bold(Role::Muted)));
    let mut lines = Vec::new();
    for (index, (name, description)) in scene.completions.iter().take(rows).enumerate() {
        let text = format!(
            " /{:<10} {}",
            name,
            crate::ui::widgets::clip_end(description, width.saturating_sub(16) as usize)
        );
        let mut line = Line::styled(text, theme.style(Role::Fg));
        if index == scene.completion_index {
            line = line.style(ratatui::style::Style::default().bg(theme.color(Role::Selection)));
        }
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_modal(frame: &mut Frame, modal: &ModalData, area: Rect, theme: &Theme) {
    let line_count = match modal {
        ModalData::Tree(view) => view.rows.len(),
        ModalData::Sessions(view) => view.items.len(),
        ModalData::Tools(view) => view.items.len(),
    };
    let width = area
        .width
        .saturating_sub(4)
        .min(110)
        .max(area.width.min(20));
    let height = (line_count as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(area.height.min(5));
    if width == 0 || height == 0 {
        return;
    }
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);
    frame.render_widget(Clear, rect);
    let inner_width = width.saturating_sub(2) as usize;
    let lines = modal.lines(inner_width, theme);
    let scroll = modal.scroll().min(lines.len().saturating_sub(1)) as u16;
    let block = WidgetBlock::default()
        .borders(Borders::ALL)
        .border_style(theme.style(Role::Border))
        .title(Span::styled(modal.title(), theme.bold(Role::Accent)));
    frame.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn joined(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn tool_card_has_name_summary_status_duration_preview_and_artifact() {
        let theme = Theme::bundled("omarchy");
        let block = Block::tool(
            Some("call-1".into()),
            "bash",
            "cargo test",
            ToolStatus::Ok,
            Some(1200),
            "12 passed\n0 failed",
            Some("out-1.txt".into()),
        );
        let lines = blocks_to_lines(&[block], 80, &theme, false);
        let text = joined(&lines);
        assert!(text.contains("bash"));
        assert!(text.contains("cargo test"));
        assert!(text.contains("ok"));
        assert!(text.contains("1.2s"));
        assert!(text.contains("[out-1]"));
        assert!(text.contains("12 passed"));
        assert!(text.starts_with('╭'));
        assert!(lines.last().unwrap().spans[0].content.starts_with('╰'));
    }

    #[test]
    fn diff_fences_use_diff_colors() {
        let theme = Theme::bundled("omarchy");
        let block = Block::assistant(
            "Patch:\n```diff\n--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-old\n+new\n```",
            None,
        );
        let lines = blocks_to_lines(&[block], 80, &theme, false);
        let text = joined(&lines);
        assert!(text.contains("+new"));
        let add = theme.color(Role::DiffAdd);
        let del = theme.color(Role::DiffDel);
        let mut saw_add = false;
        let mut saw_del = false;
        for line in &lines {
            for span in &line.spans {
                if span.content.contains("+new") && span.style.fg == Some(add) {
                    saw_add = true;
                }
                if span.content.contains("-old") && span.style.fg == Some(del) {
                    saw_del = true;
                }
            }
        }
        assert!(saw_add, "diff addition should use the DiffAdd role");
        assert!(saw_del, "diff deletion should use the DiffDel role");
    }

    #[test]
    fn reasoning_is_collapsed_by_default_and_expanded_with_ctrl_r() {
        let theme = Theme::bundled("omarchy");
        let block = Block::assistant("answer", Some("secret thought".into()));
        let collapsed = joined(&blocks_to_lines(
            std::slice::from_ref(&block),
            80,
            &theme,
            false,
        ));
        assert!(collapsed.contains("reasoning"));
        assert!(collapsed.contains("Ctrl+R"));
        assert!(!collapsed.contains("secret thought"));

        let expanded = joined(&blocks_to_lines(&[block], 80, &theme, true));
        assert!(expanded.contains("secret thought"));
        assert!(!expanded.contains("Ctrl+R"));
    }

    #[test]
    fn errors_render_as_bordered_cards() {
        let theme = Theme::bundled("omarchy");
        let block = Block::error("timeout", "the request timed out");
        let lines = blocks_to_lines(&[block], 60, &theme, false);
        let text = joined(&lines);
        assert!(text.contains("timeout"));
        assert!(text.contains("the request timed out"));
        assert!(text.starts_with('╭'));
    }

    #[test]
    fn long_words_are_broken_not_clipped() {
        let lines = wrap_styled(Line::raw("abcdefghij"), 4);
        assert_eq!(lines.len(), 3);
        assert_eq!(joined(&lines).replace('\n', ""), "abcdefghij");
    }

    #[test]
    fn test_backend_renders_tool_card() {
        let theme = Theme::bundled("omarchy");
        let mut transcript = Transcript::new(false);
        transcript.set_session(vec![Block::tool(
            None,
            "read",
            "src/main.rs",
            ToolStatus::Running,
            None,
            "42 lines",
            None,
        )]);
        let input = TextArea::new(vec![String::new()]);
        let header = HeaderData {
            cwd: "/tmp/project".into(),
            model: "test".into(),
            window: 1000,
            ..HeaderData::default()
        };
        let footer = FooterData::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let rects = split(frame_area(&terminal), 1, 0);
        transcript.set_viewport(rects.transcript.height as usize);
        transcript.prepare(80, &theme, false, true);
        terminal
            .draw(|frame| {
                let scene = Scene {
                    header: header.clone(),
                    transcript: &transcript,
                    input: &input,
                    debug_lines: Vec::new(),
                    footer: footer.clone(),
                    completions: Vec::new(),
                    completion_index: 0,
                    modal: None,
                };
                draw(frame, &scene, &theme);
            })
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("read"));
        assert!(text.contains("src/main.rs"));
        assert!(text.contains("running"));
    }

    fn frame_area(terminal: &Terminal<TestBackend>) -> Rect {
        Rect::new(
            0,
            0,
            terminal.backend().buffer().area().width,
            terminal.backend().buffer().area().height,
        )
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = *buffer.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buffer.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }
}
