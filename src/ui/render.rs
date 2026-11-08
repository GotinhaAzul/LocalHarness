//! Transcript model, block rendering, layout, and the frame painter.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as WidgetBlock, BorderType, Borders, Clear, Paragraph};
use ratatui_textarea::TextArea;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::session::{EntryKind, Session};
use crate::tools::todo::PlanStatus;
use crate::ui::markdown::{render_diff, render_markdown, wrap_plain};
use crate::ui::theme::{Role, Theme};
use crate::ui::widgets::{FooterData, HeaderData, ModalData, clip_end, footer_line, header_line};

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
    /// Recap after a finished turn: how long it took and what it touched.
    TurnSummary {
        seconds: Option<i64>,
        tools: usize,
        failed: usize,
        edited: usize,
    },
    /// Welcome card at the top of the transcript.
    Banner {
        title: String,
        rows: Vec<(String, String, bool)>,
        hint: String,
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
    /// Local notes, each anchored before the session block at that index so
    /// later turns render below them.
    extra: Vec<(usize, Block)>,
    banner: Option<Block>,
    turn: Vec<Block>,
    live_user: Option<Block>,
    live_assistant: Option<Block>,
    lines: Vec<Line<'static>>,
    built_width: usize,
    show_reasoning: bool,
    expand_tools: bool,
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
            banner: None,
            turn: Vec::new(),
            live_user: None,
            live_assistant: None,
            lines: Vec::new(),
            built_width: 0,
            show_reasoning,
            expand_tools: false,
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

    pub fn set_banner(&mut self, banner: Option<Block>) {
        self.banner = banner;
        self.mark_dirty();
    }

    pub fn expand_tools(&self) -> bool {
        self.expand_tools
    }

    pub fn set_expand_tools(&mut self, expand: bool) {
        if self.expand_tools != expand {
            self.expand_tools = expand;
            self.mark_dirty();
        }
    }

    /// True while nothing but the banner would render.
    pub fn is_empty(&self) -> bool {
        self.session.is_empty()
            && self.extra.is_empty()
            && self.turn.is_empty()
            && self.live_user.is_none()
            && self.live_assistant.is_none()
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
        let anchor = self.session.len();
        self.extra
            .push((anchor, Block::note(title, body, is_error)));
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
        let mut ordered: Vec<(&Block, bool)> = Vec::new();
        let mut extra = self.extra.iter().peekable();
        for (index, block) in self.session.iter().enumerate() {
            while let Some((_, note)) = extra.next_if(|(anchor, _)| *anchor <= index) {
                ordered.push((note, false));
            }
            ordered.push((block, false));
        }
        ordered.extend(extra.map(|(_, note)| (note, false)));
        // The live user message precedes the turn's tool and text blocks.
        ordered.extend(self.live_user.iter().map(|block| (block, false)));
        ordered.extend(self.turn.iter().map(|block| (block, false)));
        ordered.extend(self.live_assistant.iter().map(|block| (block, true)));
        let opts = RenderOpts {
            show_reasoning: self.show_reasoning,
            expand_tools: self.expand_tools,
            live: false,
        };
        let mut lines = Vec::new();
        if let Some(banner) = &self.banner {
            lines.extend(render_block(banner, width, theme, opts));
        }
        for (block, live) in ordered {
            if !lines.is_empty() {
                lines.push(Line::raw(""));
            }
            lines.extend(render_block(
                block,
                width,
                theme,
                RenderOpts { live, ..opts },
            ));
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

    /// Half a viewport, the step for PgUp/PgDn.
    pub fn page(&self) -> usize {
        (self.viewport / 2).max(3)
    }

    /// Lines above and below the visible window.
    pub fn hidden_lines(&self) -> (usize, usize) {
        let top = self.scroll_for(self.viewport);
        let below = self.lines.len().saturating_sub(top + self.viewport.max(1));
        (top, below)
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
    let mut recap: Option<TurnRecap> = None;
    for entry in session.branch_entries() {
        if entry.seq <= floor_seq {
            continue;
        }
        match &entry.kind {
            EntryKind::User(data) => {
                if let Some(done) = recap.take() {
                    done.push_into(&mut blocks);
                }
                recap = Some(TurnRecap::new(entry.ts));
                blocks.push(Block {
                    kind: BlockKind::User(data.content.clone()),
                    ts: Some(entry.ts),
                });
            }
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
                if let Some(recap) = &mut recap {
                    recap.tools += 1;
                    if data.is_error {
                        recap.failed += 1;
                    } else if is_edit_tool(&name) {
                        recap.edited.insert(summary.clone());
                    }
                }
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
                        preview: preview_text(strip_tool_header(&data.content), 2000, 40),
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
        if let Some(recap) = &mut recap {
            recap.last = entry.ts;
        }
    }
    if let Some(done) = recap {
        done.push_into(&mut blocks);
    }
    blocks
}

fn is_edit_tool(name: &str) -> bool {
    matches!(name, "edit" | "write" | "patch")
}

/// Accumulates one user turn while building blocks from the session.
struct TurnRecap {
    start: DateTime<Utc>,
    last: DateTime<Utc>,
    tools: usize,
    failed: usize,
    edited: std::collections::HashSet<String>,
}

impl TurnRecap {
    fn new(start: DateTime<Utc>) -> Self {
        TurnRecap {
            start,
            last: start,
            tools: 0,
            failed: 0,
            edited: Default::default(),
        }
    }

    /// Only turns that did work get a recap; a plain question and answer
    /// needs none.
    fn push_into(self, blocks: &mut Vec<Block>) {
        if self.tools == 0 {
            return;
        }
        let seconds = (self.last - self.start).num_seconds();
        blocks.push(Block {
            kind: BlockKind::TurnSummary {
                seconds: (seconds >= 0).then_some(seconds),
                tools: self.tools,
                failed: self.failed,
                edited: self.edited.len(),
            },
            ts: None,
        });
    }
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

fn strip_tool_header(content: &str) -> &str {
    if content.starts_with("[tool] ") {
        content.split_once('\n').map(|(_, body)| body).unwrap_or("")
    } else {
        content
    }
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
        let opts = RenderOpts {
            show_reasoning,
            expand_tools: false,
            live: false,
        };
        lines.extend(render_block(block, width, theme, opts));
    }
    lines
}

/// Rendering switches shared by every block.
#[derive(Debug, Clone, Copy)]
struct RenderOpts {
    show_reasoning: bool,
    expand_tools: bool,
    /// The block is still streaming.
    live: bool,
}

/// Width of the glyph column every block starts with.
const GUTTER: usize = 2;
/// Collapsed tool output: lines shown for successes, failures, and diffs.
const PREVIEW_OK: usize = 3;
const PREVIEW_ERROR: usize = 6;
const PREVIEW_DIFF: usize = 12;

/// Prefix the first line with `marker` and indent the rest to match.
fn with_gutter(marker: Span<'static>, body: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if body.is_empty() {
        return vec![Line::from(vec![marker])];
    }
    body.into_iter()
        .enumerate()
        .map(|(index, line)| {
            let lead = if index == 0 {
                marker.clone()
            } else {
                Span::raw(" ".repeat(GUTTER))
            };
            let style = line.style;
            let mut spans = vec![lead];
            spans.extend(line.spans);
            Line::from(spans).style(style)
        })
        .collect()
}

/// Hang `body` under a block's head line with the `⎿` connector.
fn hang(body: Vec<Line<'static>>, theme: &Theme) -> Vec<Line<'static>> {
    let connector = Span::styled("⎿ ", theme.style(Role::Border));
    with_gutter(connector, body)
        .into_iter()
        .map(|line| {
            let style = line.style;
            let mut spans = vec![Span::raw(" ".repeat(GUTTER))];
            spans.extend(line.spans);
            Line::from(spans).style(style)
        })
        .collect()
}

fn plain_lines(text: &str, width: usize, style: ratatui::style::Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        for wrapped in wrap_plain(raw, width) {
            lines.push(Line::styled(wrapped, style));
        }
    }
    lines
}

fn render_block(
    block: &Block,
    width: usize,
    theme: &Theme,
    opts: RenderOpts,
) -> Vec<Line<'static>> {
    let width = width.max(GUTTER + 1);
    let inner = width - GUTTER;
    match &block.kind {
        BlockKind::User(text) => {
            let bar = Span::styled("▌ ", theme.style(Role::User));
            let mut lines = Vec::new();
            for raw in text.lines() {
                for wrapped in wrap_plain(raw, inner) {
                    lines.push(Line::from(vec![
                        bar.clone(),
                        Span::styled(wrapped, theme.style(Role::Fg)),
                    ]));
                }
            }
            if lines.is_empty() {
                lines.push(Line::from(vec![bar]));
            }
            lines
        }
        BlockKind::Assistant { text, reasoning } => {
            let mut lines = Vec::new();
            if let Some(reasoning) = reasoning.as_deref().filter(|r| !r.trim().is_empty()) {
                // Still thinking: no answer text yet on a streaming block.
                let thinking = opts.live && text.trim().is_empty();
                lines.extend(reasoning_lines(
                    reasoning,
                    thinking,
                    opts.show_reasoning,
                    inner,
                    theme,
                ));
            }
            if !text.trim().is_empty() {
                let marker = Span::styled("● ", theme.style(Role::Assistant));
                lines.extend(with_gutter(marker, assistant_markdown(text, inner, theme)));
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
        } => tool_lines(
            ToolView {
                name,
                summary,
                status: *status,
                duration_ms: *duration_ms,
                preview,
                artifact: artifact.as_deref(),
            },
            width,
            theme,
            opts.expand_tools,
        ),
        BlockKind::Error { code, message } => {
            let head = Line::from(vec![
                Span::styled("Error", theme.bold(Role::Error)),
                Span::styled(format!(" · {code}"), theme.style(Role::Muted)),
            ]);
            let mut lines = with_gutter(Span::styled("✗ ", theme.bold(Role::Error)), vec![head]);
            let body = plain_lines(
                message,
                inner.saturating_sub(GUTTER).max(1),
                theme.style(Role::Error),
            );
            lines.extend(hang(body, theme));
            lines
        }
        BlockKind::Compaction {
            reason,
            summary,
            tokens_before,
        } => {
            let mut head = vec![
                Span::styled("Context compacted", theme.bold(Role::Accent)),
                Span::styled(format!(" · {reason}"), theme.style(Role::Muted)),
            ];
            if let Some(tokens) = tokens_before {
                head.push(Span::styled(
                    format!(
                        " · {} before",
                        crate::ui::widgets::format_tokens(*tokens as u32)
                    ),
                    theme.style(Role::Muted),
                ));
            }
            let mut lines = with_gutter(
                Span::styled("≡ ", theme.style(Role::Accent)),
                vec![Line::from(head)],
            );
            let limit = if opts.expand_tools { 12 } else { 2 };
            let body: Vec<Line<'static>> = plain_lines(
                summary,
                inner.saturating_sub(GUTTER).max(1),
                theme.style(Role::Muted),
            );
            lines.extend(hang(truncate_body(body, limit, theme), theme));
            lines
        }
        BlockKind::Note {
            title,
            body,
            is_error,
        } => {
            let (glyph, role) = if *is_error {
                ("✗ ", Role::Error)
            } else {
                ("• ", Role::Accent)
            };
            let mut content = vec![Line::styled(title.clone(), theme.bold(role))];
            content.extend(plain_lines(body, inner, theme.style(Role::Fg)));
            with_gutter(Span::styled(glyph, theme.style(role)), content)
        }
        BlockKind::TurnSummary {
            seconds,
            tools,
            failed,
            edited,
        } => {
            let mut parts = Vec::new();
            if let Some(seconds) = seconds {
                parts.push(format!("Worked for {}", elapsed_label(*seconds as u64)));
            }
            parts.push(format!(
                "{tools} tool call{}",
                if *tools == 1 { "" } else { "s" }
            ));
            if *edited > 0 {
                parts.push(format!(
                    "{edited} file{} changed",
                    if *edited == 1 { "" } else { "s" }
                ));
            }
            let mut spans = vec![Span::styled(parts.join(" · "), theme.style(Role::Muted))];
            if *failed > 0 {
                spans.push(Span::styled(" · ", theme.style(Role::Muted)));
                spans.push(Span::styled(
                    format!("{failed} failed"),
                    theme.style(Role::Warning),
                ));
            }
            with_gutter(
                Span::styled("✻ ", theme.style(Role::Muted)),
                vec![Line::from(spans)],
            )
        }
        BlockKind::Banner { title, rows, hint } => banner_lines(title, rows, hint, width, theme),
    }
}

fn reasoning_lines(
    reasoning: &str,
    thinking: bool,
    expanded: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let tokens = (reasoning.chars().count() as u64).div_ceil(4);
    let italic = theme.style(Role::Reasoning).add_modifier(Modifier::ITALIC);
    let label = if thinking { "Thinking…" } else { "Thought" };
    let mut head = vec![
        Span::styled(label, italic),
        Span::styled(format!(" · {tokens} tokens"), theme.style(Role::Muted)),
    ];
    if !expanded {
        head.push(Span::styled("  ctrl+r to expand", theme.style(Role::Muted)));
    }
    let mut content = vec![Line::from(head)];
    if expanded {
        for raw in reasoning.trim().lines() {
            for wrapped in wrap_plain(raw, width.saturating_sub(2).max(1)) {
                content.push(Line::from(vec![
                    Span::styled("│ ", theme.style(Role::Border)),
                    Span::styled(wrapped, italic),
                ]));
            }
        }
    }
    with_gutter(Span::styled("∴ ", theme.style(Role::Reasoning)), content)
}

pub fn elapsed_label(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
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

/// Display name for a tool: `read` → `Read`, `repo_map` → `Repo map`.
fn tool_title(name: &str) -> String {
    let spaced = name.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

struct ToolView<'a> {
    name: &'a str,
    summary: &'a str,
    status: ToolStatus,
    duration_ms: Option<u64>,
    preview: &'a str,
    artifact: Option<&'a str>,
}

fn looks_like_diff(preview: &str) -> bool {
    let multi = preview.lines().nth(1).is_some();
    preview.lines().take(6).any(|line| {
        line.starts_with("@@")
            || line.starts_with("+++ ")
            || line.starts_with("--- ")
            || multi && (line.starts_with('+') || line.starts_with('-'))
    })
}

/// Keep the first `limit` lines and note how many were hidden.
fn truncate_body(mut body: Vec<Line<'static>>, limit: usize, theme: &Theme) -> Vec<Line<'static>> {
    if body.len() > limit {
        let hidden = body.len() - limit;
        body.truncate(limit);
        body.push(Line::styled(
            format!(
                "… +{hidden} line{} · ctrl+o to expand",
                if hidden == 1 { "" } else { "s" }
            ),
            theme.style(Role::Muted),
        ));
    }
    body
}

fn tool_lines(tool: ToolView<'_>, width: usize, theme: &Theme, expand: bool) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(GUTTER).max(1);
    let (glyph, role) = match tool.status {
        ToolStatus::Running => ("◌ ", Role::ToolRunning),
        ToolStatus::Ok => ("✓ ", Role::ToolOk),
        ToolStatus::Error => ("✗ ", Role::ToolError),
    };
    let title = tool_title(tool.name);
    let mut meta = String::new();
    match tool.status {
        ToolStatus::Running => meta.push_str(" · running…"),
        _ => {
            if let Some(ms) = tool.duration_ms {
                meta.push_str(&format!(" · {}", duration_label(ms)));
            }
        }
    }
    if let Some(artifact) = tool.artifact {
        meta.push_str(&format!(" · {}", artifact_note(artifact)));
    }
    let fixed = UnicodeWidthStr::width(title.as_str()) + UnicodeWidthStr::width(meta.as_str()) + 1;
    let mut head = vec![Span::styled(title, theme.bold(Role::Fg))];
    if !tool.summary.is_empty() && inner > fixed + 3 {
        head.push(Span::raw(" "));
        head.push(Span::styled(
            clip_end(tool.summary, inner - fixed),
            theme.style(Role::Fg),
        ));
    }
    head.push(Span::styled(meta, theme.style(Role::Muted)));
    let mut lines = with_gutter(
        Span::styled(glyph, theme.bold(role)),
        vec![Line::from(head)],
    );
    if tool.status == ToolStatus::Running {
        return lines;
    }

    let preview = tool.preview.trim_matches('\n');
    if preview.trim().is_empty() {
        return lines;
    }
    let body_width = inner.saturating_sub(GUTTER).max(1);
    let (body, limit) = if looks_like_diff(preview) {
        (render_diff(preview, body_width, theme), PREVIEW_DIFF)
    } else if tool.status == ToolStatus::Error {
        (
            plain_lines(preview, body_width, theme.style(Role::Error)),
            PREVIEW_ERROR,
        )
    } else {
        (
            plain_lines(preview, body_width, theme.style(Role::Muted)),
            PREVIEW_OK,
        )
    };
    let body = if expand {
        body
    } else {
        truncate_body(body, limit, theme)
    };
    lines.extend(hang(body, theme));
    lines
}

fn banner_lines(
    title: &str,
    rows: &[(String, String, bool)],
    hint: &str,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let border = theme.style(Role::Border);
    let box_width = width.min(72);
    if box_width < 20 {
        return vec![Line::styled(title.to_string(), theme.bold(Role::Accent))];
    }
    let inner = box_width - 4;
    let label_width = rows
        .iter()
        .map(|(label, _, _)| UnicodeWidthStr::width(label.as_str()))
        .max()
        .unwrap_or(0);
    let boxed = |spans: Vec<Span<'static>>| -> Line<'static> {
        let used: usize = spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum();
        let mut out = vec![Span::styled("│ ", border)];
        out.extend(spans);
        out.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
        out.push(Span::styled(" │", border));
        Line::from(out)
    };
    let mut lines = vec![Line::styled(
        format!("╭{}╮", "─".repeat(box_width - 2)),
        border,
    )];
    lines.push(boxed(vec![
        Span::styled("✻ ", theme.style(Role::Accent)),
        Span::styled(clip_end(title, inner - 2), theme.bold(Role::Fg)),
    ]));
    lines.push(boxed(Vec::new()));
    for (label, value, warn) in rows {
        let value_width = inner.saturating_sub(label_width + 4).max(1);
        lines.push(boxed(vec![
            Span::raw("  "),
            Span::styled(format!("{label:<label_width$}  "), theme.style(Role::Muted)),
            Span::styled(
                clip_end(value, value_width),
                theme.style(if *warn { Role::Warning } else { Role::Fg }),
            ),
        ]));
    }
    if !hint.is_empty() {
        lines.push(boxed(Vec::new()));
        lines.push(boxed(vec![Span::styled(
            clip_end(&format!("  {hint}"), inner),
            theme.style(Role::Muted),
        )]));
    }
    lines.push(Line::styled(
        format!("╰{}╯", "─".repeat(box_width - 2)),
        border,
    ));
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
    pub transcript: Rect,
    pub activity: Rect,
    pub plan: Rect,
    pub input: Rect,
    pub debug: Rect,
    /// Working directory, git, model, and context.
    pub header: Rect,
    /// Hints or a transient notice, plus the trust state.
    pub footer: Rect,
}

/// What the bottom panels need, so the app and the painter agree on sizes.
#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutSpec {
    /// Wrapped prompt rows.
    pub input_lines: usize,
    pub debug_lines: usize,
    pub activity: bool,
    pub plan_rows: usize,
}

/// Plan rows shown above the input at most.
pub const PLAN_MAX_ROWS: usize = 6;

/// Split the screen top to bottom: transcript, activity line, plan, input,
/// optional debug panel, info bar, and footer.
pub fn split(area: Rect, input_lines: usize, debug_lines: usize) -> LayoutRects {
    split_with(
        area,
        LayoutSpec {
            input_lines,
            debug_lines,
            ..LayoutSpec::default()
        },
    )
}

/// Prompt rows shown before the input scrolls internally.
pub const INPUT_MAX_ROWS: usize = 8;
/// Columns taken by the prompt glyph.
pub const PROMPT_WIDTH: u16 = 3;

pub fn split_with(area: Rect, spec: LayoutSpec) -> LayoutRects {
    // Text rows plus the rules above and below.
    let input_height = spec.input_lines.clamp(1, INPUT_MAX_ROWS) as u16 + 2;
    let debug_height = spec.debug_lines.min(8) as u16;
    // The activity line sits under a blank spacer so it never touches text.
    let activity_height = if spec.activity { 2 } else { 0 };
    let plan_height = spec.plan_rows.min(PLAN_MAX_ROWS) as u16;
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(activity_height),
        Constraint::Length(plan_height),
        Constraint::Length(input_height),
        Constraint::Length(debug_height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);
    let mut activity = chunks[1];
    if activity.height > 1 {
        activity.y += 1;
        activity.height = 1;
    }
    LayoutRects {
        transcript: chunks[0],
        activity,
        plan: chunks[2],
        input: chunks[3],
        debug: chunks[4],
        header: chunks[5],
        footer: chunks[6],
    }
}

/// The live status line shown while a turn runs.
#[derive(Debug, Clone)]
pub struct ActivityData {
    pub spinner: &'static str,
    /// What the agent is doing, e.g. `Running bash`.
    pub label: String,
    /// The subject, e.g. a command or path.
    pub detail: String,
    /// Elapsed time, tokens, and the interrupt hint.
    pub meta: String,
    pub role: Role,
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
    pub activity: Option<ActivityData>,
    pub plan: Vec<(String, PlanStatus)>,
    /// A turn is running: the prompt dims and cannot send.
    pub busy: bool,
}

impl Scene<'_> {
    pub fn layout_spec(&self, width: u16) -> LayoutSpec {
        let text_width = width.saturating_sub(PROMPT_WIDTH + 1) as usize;
        let input_lines = self
            .input
            .lines()
            .iter()
            .map(|line| wrap_plain(line, text_width.max(1)).len().max(1))
            .sum();
        LayoutSpec {
            input_lines,
            debug_lines: self.debug_lines.len(),
            activity: self.activity.is_some(),
            plan_rows: plan_rows(&self.plan),
        }
    }
}

/// Rows the plan panel needs.
pub fn plan_rows(plan: &[(String, PlanStatus)]) -> usize {
    plan.len().min(PLAN_MAX_ROWS)
}

/// Plan lines: in-progress first-class, finished steps dimmed and, when the
/// plan is longer than the panel, folded into one count.
pub fn plan_lines(
    plan: &[(String, PlanStatus)],
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let done = plan
        .iter()
        .filter(|(_, status)| *status == PlanStatus::Completed)
        .count();
    let mut items: Vec<(String, PlanStatus)> = plan.to_vec();
    let mut folded = None;
    if items.len() > PLAN_MAX_ROWS && done > 0 {
        items.retain(|(_, status)| *status != PlanStatus::Completed);
        folded = Some(done);
        items.truncate(PLAN_MAX_ROWS - 1);
    } else {
        items.truncate(PLAN_MAX_ROWS);
    }
    let text_width = width.saturating_sub(6).max(1);
    let mut lines = Vec::new();
    if let Some(done) = folded {
        lines.push(Line::from(vec![
            Span::styled("✓ ", theme.style(Role::Success)),
            Span::styled(format!("{done} done"), theme.style(Role::Muted)),
        ]));
    }
    for (text, status) in items {
        let (glyph, glyph_style, text_style) = match status {
            PlanStatus::Completed => (
                "✓ ",
                theme.style(Role::Success),
                theme.style(Role::Muted).add_modifier(Modifier::CROSSED_OUT),
            ),
            PlanStatus::InProgress => ("▸ ", theme.bold(Role::Accent), theme.bold(Role::Fg)),
            PlanStatus::Pending => ("○ ", theme.style(Role::Muted), theme.style(Role::Fg)),
        };
        lines.push(Line::from(vec![
            Span::styled(glyph, glyph_style),
            Span::styled(clip_end(&text, text_width), text_style),
        ]));
    }
    hang(lines, theme)
}

fn activity_line(activity: &ActivityData, width: usize, theme: &Theme) -> Line<'static> {
    let mut spans = vec![
        Span::styled(format!("{} ", activity.spinner), theme.bold(activity.role)),
        Span::styled(activity.label.clone(), theme.bold(activity.role)),
    ];
    let label_width = 2 + UnicodeWidthStr::width(activity.label.as_str());
    let meta = format!("  ({})", activity.meta);
    let meta_width = UnicodeWidthStr::width(meta.as_str());
    let room = width.saturating_sub(label_width + meta_width + 1);
    if !activity.detail.is_empty() && room > 6 {
        spans.push(Span::styled(
            format!(" {}", clip_end(&activity.detail, room)),
            theme.style(Role::Fg),
        ));
    }
    if label_width + meta_width <= width {
        spans.push(Span::styled(meta, theme.style(Role::Muted)));
    }
    Line::from(spans)
}

/// Paint the whole UI. The caller prepares the transcript and input styling.
pub fn draw(frame: &mut Frame, scene: &Scene<'_>, theme: &Theme) {
    let area = frame.area();
    frame.render_widget(WidgetBlock::default().style(theme.style(Role::Fg)), area);
    let rects = split_with(area, scene.layout_spec(area.width));

    if rects.transcript.height > 0 {
        let scroll = scene
            .transcript
            .scroll_for(rects.transcript.height as usize);
        frame.render_widget(
            Paragraph::new(scene.transcript.lines().to_vec()).scroll((scroll as u16, 0)),
            rects.transcript,
        );
        render_scroll_badge(frame, scene.transcript, rects.transcript, theme);
    }

    if rects.activity.height > 0
        && let Some(activity) = &scene.activity
    {
        frame.render_widget(
            Paragraph::new(activity_line(
                activity,
                rects.activity.width as usize,
                theme,
            )),
            rects.activity,
        );
    }

    if rects.plan.height > 0 {
        frame.render_widget(
            Paragraph::new(plan_lines(&scene.plan, rects.plan.width as usize, theme)),
            rects.plan,
        );
    }

    if rects.input.height > 0 {
        render_input(frame, scene, rects.input, theme);
    }

    if rects.debug.height > 0 && !scene.debug_lines.is_empty() {
        let lines: Vec<Line<'static>> = scene
            .debug_lines
            .iter()
            .take(rects.debug.height as usize)
            .map(|line| {
                Line::styled(
                    clip_end(line, rects.debug.width as usize),
                    theme.style(Role::Muted),
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rects.debug);
    }

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
        let above = Rect {
            height: rects.input.y.saturating_sub(area.y),
            ..area
        };
        render_completions(frame, scene, above, theme);
    }

    if let Some(modal) = scene.modal {
        render_modal(frame, modal, area, theme);
    }
}

/// Where the reader is when scrolled away from the bottom.
fn render_scroll_badge(frame: &mut Frame, transcript: &Transcript, area: Rect, theme: &Theme) {
    if transcript.follow || area.height == 0 {
        return;
    }
    let (_, below) = transcript.hidden_lines();
    if below == 0 {
        return;
    }
    let (text, role) = if transcript.new_output {
        (
            format!(" ↓ new output · {below} lines below · End "),
            Role::Warning,
        )
    } else {
        (format!(" ↓ {below} lines below · End "), Role::AccentDim)
    };
    let width = UnicodeWidthStr::width(text.as_str());
    if width >= area.width as usize {
        return;
    }
    frame.buffer_mut().set_stringn(
        area.right().saturating_sub(width as u16 + 1),
        area.bottom().saturating_sub(1),
        &text,
        width,
        theme.bold(Role::Bg).bg(theme.color(role)),
    );
}

/// The message bar: thin rules above and below, a `›` prompt, and soft-wrapped
/// text. The glyph turns into `↑` when earlier lines are scrolled off.
fn render_input(frame: &mut Frame, scene: &Scene<'_>, area: Rect, theme: &Theme) {
    if area.height < 3 || area.width < PROMPT_WIDTH + 2 {
        return;
    }
    let rule = Line::styled("─".repeat(area.width as usize), theme.style(Role::Border));
    frame.render_widget(Paragraph::new(rule.clone()), Rect { height: 1, ..area });
    frame.render_widget(
        Paragraph::new(rule),
        Rect {
            y: area.bottom() - 1,
            height: 1,
            ..area
        },
    );
    let body = Rect {
        y: area.y + 1,
        height: area.height - 2,
        ..area
    };
    let [prompt, text] =
        Layout::horizontal([Constraint::Length(PROMPT_WIDTH), Constraint::Min(1)]).areas(body);
    // Leave one column of air on the right.
    let text = Rect {
        width: text.width.saturating_sub(1),
        ..text
    };

    let empty = scene.input.lines().iter().all(|line| line.is_empty());
    if empty {
        // Draw the placeholder ourselves with the cursor on its first letter.
        let placeholder = scene.input.placeholder_text();
        let mut chars = placeholder.chars();
        let first = chars.next().map(String::from).unwrap_or_else(|| " ".into());
        let cursor = ratatui::style::Style::default()
            .fg(theme.color(Role::Bg))
            .bg(theme.color(if scene.busy {
                Role::Muted
            } else {
                Role::Cursor
            }));
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(first, cursor),
                Span::styled(chars.as_str().to_string(), theme.style(Role::Muted)),
            ])),
            Rect { height: 1, ..text },
        );
    } else {
        frame.render_widget(scene.input, text);
    }

    let scrolled = !empty && scene.input.scroll_offset().0 > 0;
    let (glyph, style) = if scrolled {
        (" ↑", theme.style(Role::Muted))
    } else if scene.busy {
        (" ›", theme.style(Role::Muted))
    } else {
        (" ›", theme.bold(Role::Accent))
    };
    frame.render_widget(
        Paragraph::new(Line::styled(glyph, style)),
        Rect {
            height: 1,
            ..prompt
        },
    );
}

fn render_completions(frame: &mut Frame, scene: &Scene<'_>, area: Rect, theme: &Theme) {
    if area.height < 3 || area.width < 12 {
        return;
    }
    let rows = scene.completions.len().min(8);
    let height = (rows as u16 + 2).min(area.height);
    let visible = height.saturating_sub(2) as usize;
    let width = area.width.min(76);
    let popup = Rect::new(area.x, area.bottom().saturating_sub(height), width, height);
    frame.render_widget(Clear, popup);
    let block = WidgetBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.style(Role::Border))
        .title(Span::styled(
            " commands · tab accept ",
            theme.style(Role::Muted),
        ));
    // Keep the selection in view when the list is longer than the popup.
    let first = scene
        .completion_index
        .saturating_sub(visible.saturating_sub(1));
    let mut lines = Vec::new();
    for (index, (name, description)) in scene
        .completions
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
    {
        let selected = index == scene.completion_index;
        let marker = if selected { "› " } else { "  " };
        let mut line = Line::from(vec![
            Span::styled(marker, theme.bold(Role::Accent)),
            Span::styled(
                format!("/{name:<10}"),
                if selected {
                    theme.bold(Role::Accent)
                } else {
                    theme.style(Role::Fg)
                },
            ),
            Span::styled(
                format!(
                    " {}",
                    clip_end(description, width.saturating_sub(17) as usize)
                ),
                theme.style(Role::Muted),
            ),
        ]);
        if selected {
            line = line.style(ratatui::style::Style::default().bg(theme.color(Role::Selection)));
        }
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_modal(frame: &mut Frame, modal: &ModalData, area: Rect, theme: &Theme) {
    let line_count = modal.row_count();
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
        .border_type(BorderType::Rounded)
        .border_style(theme.style(Role::AccentDim))
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
    fn tool_line_has_glyph_name_summary_duration_preview_and_artifact() {
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
        assert!(
            text.starts_with("✓ Bash cargo test · 1.2s · [out-1]"),
            "{text}"
        );
        assert!(text.contains("  ⎿ 12 passed"), "{text}");
        assert!(text.contains("0 failed"), "{text}");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn long_tool_output_collapses_until_expanded() {
        let theme = Theme::bundled("omarchy");
        let output: Vec<String> = (1..=10).map(|n| format!("line {n}")).collect();
        let block = Block::tool(
            None,
            "bash",
            "ls",
            ToolStatus::Ok,
            None,
            output.join("\n"),
            None,
        );
        let collapsed = joined(&blocks_to_lines(
            std::slice::from_ref(&block),
            80,
            &theme,
            false,
        ));
        assert!(collapsed.contains("line 3"));
        assert!(!collapsed.contains("line 4"));
        assert!(collapsed.contains("+7 lines · ctrl+o"), "{collapsed}");

        let opts = RenderOpts {
            show_reasoning: false,
            expand_tools: true,
            live: false,
        };
        let expanded = joined(&render_block(&block, 80, &theme, opts));
        assert!(expanded.contains("line 10"));
        assert!(!expanded.contains("ctrl+o"));
    }

    #[test]
    fn running_and_failed_tools_use_distinct_glyphs() {
        let theme = Theme::bundled("omarchy");
        let running = Block::tool(None, "read", "a.rs", ToolStatus::Running, None, "", None);
        let failed = Block::tool(
            None,
            "edit",
            "b.rs",
            ToolStatus::Error,
            Some(5),
            "no match",
            None,
        );
        let text = joined(&blocks_to_lines(&[running, failed], 80, &theme, false));
        assert!(text.contains("◌ Read a.rs · running…"), "{text}");
        assert!(text.contains("✗ Edit b.rs · 5ms"), "{text}");
        assert!(text.contains("⎿ no match"), "{text}");
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
        assert!(collapsed.contains("∴ Thought"), "{collapsed}");
        assert!(collapsed.contains("ctrl+r"));
        assert!(!collapsed.contains("secret thought"));
        assert!(collapsed.contains("● answer"), "{collapsed}");

        let expanded = joined(&blocks_to_lines(&[block], 80, &theme, true));
        assert!(expanded.contains("secret thought"));
        assert!(!expanded.contains("ctrl+r"));
    }

    #[test]
    fn errors_render_with_a_cross_and_hanging_message() {
        let theme = Theme::bundled("omarchy");
        let block = Block::error("timeout", "the request timed out");
        let lines = blocks_to_lines(&[block], 60, &theme, false);
        let text = joined(&lines);
        assert!(text.starts_with("✗ Error · timeout"), "{text}");
        assert!(text.contains("⎿ the request timed out"), "{text}");
    }

    #[test]
    fn live_user_message_renders_above_the_turns_tools() {
        let theme = Theme::bundled("omarchy");
        let mut transcript = Transcript::new(false);
        transcript.begin_turn("fix the bug");
        transcript.text_delta("Looking.");
        transcript.tool_started("c1", "read", "src/lib.rs");
        transcript.set_viewport(40);
        transcript.prepare(80, &theme, false, true);
        let text = joined(transcript.lines());
        let user = text.find("fix the bug").unwrap();
        let tool = text.find("Read src/lib.rs").unwrap();
        let reply = text.find("Looking.").unwrap();
        assert!(user < reply && reply < tool, "{text}");
    }

    #[test]
    fn notes_stay_where_they_were_pushed() {
        let theme = Theme::bundled("omarchy");
        let mut transcript = Transcript::new(false);
        transcript.set_session(vec![Block::user("first")]);
        transcript.push_note("help", "the help text", false);
        transcript.set_session(vec![Block::user("first"), Block::user("second")]);
        transcript.prepare(80, &theme, false, true);
        let text = joined(transcript.lines());
        let note = text.find("the help text").unwrap();
        assert!(text.find("first").unwrap() < note);
        assert!(note < text.find("second").unwrap(), "{text}");
    }

    #[test]
    fn turn_summary_reports_time_tools_failures_and_edits() {
        let theme = Theme::bundled("omarchy");
        let block = Block {
            kind: BlockKind::TurnSummary {
                seconds: Some(75),
                tools: 4,
                failed: 1,
                edited: 2,
            },
            ts: None,
        };
        let text = joined(&blocks_to_lines(&[block], 80, &theme, false));
        assert_eq!(
            text,
            "✻ Worked for 1m 15s · 4 tool calls · 2 files changed · 1 failed"
        );
    }

    #[test]
    fn plan_folds_finished_steps_when_long() {
        let theme = Theme::bundled("omarchy");
        let mut plan: Vec<(String, PlanStatus)> = (1..=5)
            .map(|n| (format!("done {n}"), PlanStatus::Completed))
            .collect();
        plan.push(("current".into(), PlanStatus::InProgress));
        plan.push(("next".into(), PlanStatus::Pending));
        let text = joined(&plan_lines(&plan, 60, &theme));
        assert!(text.starts_with("  ⎿ ✓ 5 done"), "{text}");
        assert!(text.contains("▸ current"));
        assert!(text.contains("○ next"));
        assert!(!text.contains("done 1"));

        let short = vec![
            ("a".to_string(), PlanStatus::Completed),
            ("b".to_string(), PlanStatus::InProgress),
        ];
        let text = joined(&plan_lines(&short, 60, &theme));
        assert!(text.contains("✓ a") && text.contains("▸ b"), "{text}");
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
            logged_in: true,
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
                    activity: None,
                    plan: Vec::new(),
                    busy: false,
                };
                draw(frame, &scene, &theme);
            })
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("◌ Read src/main.rs · running…"), "{text}");
        assert!(text.contains("›"), "prompt glyph missing:\n{text}");
        assert!(text.contains("──────"), "input rule missing:\n{text}");
    }

    #[test]
    fn activity_line_and_plan_render_above_the_input() {
        let theme = Theme::bundled("omarchy");
        let mut transcript = Transcript::new(false);
        let input = TextArea::new(vec![String::new()]);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        transcript.set_viewport(10);
        transcript.prepare(100, &theme, false, true);
        terminal
            .draw(|frame| {
                let scene = Scene {
                    header: HeaderData {
                        cwd: "/tmp/p".into(),
                        model: "m".into(),
                        logged_in: true,
                        window: 1000,
                        ..HeaderData::default()
                    },
                    transcript: &transcript,
                    input: &input,
                    debug_lines: Vec::new(),
                    footer: FooterData::default(),
                    completions: Vec::new(),
                    completion_index: 0,
                    modal: None,
                    activity: Some(ActivityData {
                        spinner: "⠋",
                        label: "Running bash".into(),
                        detail: "cargo test".into(),
                        meta: "12s · esc to interrupt".into(),
                        role: Role::Accent,
                    }),
                    plan: vec![("write tests".into(), PlanStatus::InProgress)],
                    busy: true,
                };
                draw(frame, &scene, &theme);
            })
            .unwrap();
        let text = buffer_text(&terminal);
        let rows: Vec<&str> = text.lines().collect();
        let activity = rows
            .iter()
            .position(|row| row.contains("⠋ Running bash cargo test  (12s · esc to interrupt)"))
            .expect(&text);
        let plan = rows
            .iter()
            .position(|row| row.contains("▸ write tests"))
            .expect(&text);
        let input_top = rows
            .iter()
            .position(|row| row.starts_with("───"))
            .expect(&text);
        assert!(activity < plan && plan < input_top, "{text}");
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
