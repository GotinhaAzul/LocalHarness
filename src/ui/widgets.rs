//! Header, footer, and modal widgets.

use std::path::PathBuf;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::git::GitStatus;
use crate::ui::theme::{Role, Theme};
use crate::ui::tree::TreeView;

#[derive(Debug, Clone, Default)]
pub struct HeaderData {
    pub cwd: String,
    pub git: GitStatus,
    pub profile: String,
    pub model: String,
    /// False until a model is selected; the header then shows `not logged in`.
    pub logged_in: bool,
    pub used: u32,
    pub window: u32,
    /// Completion tokens streamed in the current turn, shown separately.
    pub completion: u32,
    pub tok_s: f32,
    /// Share of the last prompt served from the server's prefix cache.
    pub cache: Option<u32>,
    pub session: String,
    pub trust: bool,
}

#[derive(Debug, Clone)]
pub struct FooterData {
    pub hints: String,
    pub status: String,
    pub status_role: Role,
    pub notice: Option<String>,
    pub notice_role: Role,
}

impl Default for FooterData {
    fn default() -> Self {
        Self {
            hints: String::new(),
            status: String::new(),
            status_role: Role::Muted,
            notice: None,
            notice_role: Role::Warning,
        }
    }
}

fn sep() -> Span<'static> {
    Span::raw("  ")
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

fn item(spans: &mut Vec<Span<'static>>, next: Vec<Span<'static>>) {
    if !spans.is_empty() {
        spans.push(sep());
    }
    spans.extend(next);
}

/// Format a token count as `999`, `12.4k`, or `128k`.
pub fn format_tokens(value: u32) -> String {
    if value < 1000 {
        return value.to_string();
    }
    let k = value as f64 / 1000.0;
    if k < 100.0 {
        let mut text = format!("{k:.1}");
        if text.ends_with(".0") {
            text.truncate(text.len() - 2);
        }
        format!("{text}k")
    } else {
        format!("{k:.0}k")
    }
}

/// Replace the home directory with `~`.
pub fn display_cwd(cwd: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return cwd.to_string();
    };
    let home = home.to_string_lossy().trim_end_matches('/').to_string();
    if home.is_empty() {
        return cwd.to_string();
    }
    if cwd == home {
        return "~".to_string();
    }
    if let Some(rest) = cwd.strip_prefix(&format!("{home}/")) {
        return format!("~/{rest}");
    }
    cwd.to_string()
}

/// Middle-truncate to at most `max` display columns, keeping both ends.
pub fn middle_truncate(text: &str, max: usize) -> String {
    if UnicodeWidthStr::width(text) <= max {
        return text.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let target = max - 1;
    let left_budget = target / 2;
    let right_budget = target - left_budget;
    let mut left = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > left_budget {
            break;
        }
        left.push(ch);
        width += char_width;
    }
    let mut right_chars: Vec<char> = Vec::new();
    let mut width = 0;
    for ch in text.chars().rev() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > right_budget {
            break;
        }
        right_chars.push(ch);
        width += char_width;
    }
    right_chars.reverse();
    let right: String = right_chars.into_iter().collect();
    format!("{left}…{right}")
}

/// Clip from the end to at most `max` display columns.
pub fn clip_end(text: &str, max: usize) -> String {
    if UnicodeWidthStr::width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > max.saturating_sub(1) {
            break;
        }
        out.push(ch);
        width += char_width;
    }
    out.push('…');
    out
}

struct HeaderFlags {
    app: bool,
    profile: bool,
    session: bool,
    throughput: bool,
    cache: bool,
}

fn context_spans(data: &HeaderData, compact: bool, theme: &Theme) -> Vec<Span<'static>> {
    let used = format_tokens(data.used);
    let window = format_tokens(data.window);
    let ratio = if data.window == 0 {
        0.0
    } else {
        f64::from(data.used) / f64::from(data.window)
    };
    let role = if ratio >= 0.85 {
        Role::Error
    } else if ratio >= 0.70 {
        Role::Warning
    } else {
        Role::Success
    };
    let mut text = format!("{used}/{window}");
    if data.completion > 0 {
        text.push('+');
        text.push_str(&format_tokens(data.completion));
    }
    let mut spans = vec![Span::styled(text, theme.style(Role::Fg))];
    if !compact {
        let cells = 6usize;
        let filled = ((ratio.clamp(0.0, 1.0)) * cells as f64).round() as usize;
        let filled = filled.min(cells);
        let gauge = format!(
            "[{}{}] {}%",
            "#".repeat(filled),
            "-".repeat(cells - filled),
            (ratio * 100.0).round().clamp(0.0, 999.0) as u32
        );
        spans.push(Span::raw(" "));
        spans.push(Span::styled(gauge, theme.bold(role)));
    }
    spans
}

fn header_spans(
    data: &HeaderData,
    cwd: &str,
    flags: &HeaderFlags,
    compact_context: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if flags.app {
        item(
            &mut spans,
            vec![Span::styled("LocalHarness", theme.bold(Role::Accent))],
        );
    }
    item(&mut spans, vec![Span::raw(cwd.to_string())]);
    let git_role = if !data.git.in_repo {
        Role::Muted
    } else if data.git.dirty {
        Role::Warning
    } else {
        Role::Success
    };
    item(
        &mut spans,
        vec![Span::styled(data.git.label(), theme.style(git_role))],
    );
    if flags.profile && !data.profile.is_empty() {
        item(
            &mut spans,
            vec![Span::styled(data.profile.clone(), theme.style(Role::Muted))],
        );
    }
    let (model, model_role) = if data.logged_in {
        (data.model.clone(), Role::Assistant)
    } else {
        ("not logged in".to_string(), Role::Warning)
    };
    item(
        &mut spans,
        vec![Span::styled(model, theme.bold(model_role))],
    );
    item(&mut spans, context_spans(data, compact_context, theme));
    if flags.throughput {
        let mut speed = format!("{:.0} tok/s", data.tok_s.max(0.0));
        if flags.cache
            && let Some(cache) = data.cache
        {
            speed.push_str(&format!(" · cache {cache}%"));
        }
        item(
            &mut spans,
            vec![Span::styled(speed, theme.style(Role::Muted))],
        );
    }
    if flags.session && !data.session.is_empty() {
        let trust_role = if data.trust {
            Role::Success
        } else {
            Role::Warning
        };
        let trust = if data.trust { "trusted" } else { "untrusted" };
        item(
            &mut spans,
            vec![
                Span::styled(data.session.clone(), theme.style(Role::Muted)),
                Span::raw(" "),
                Span::styled(trust, theme.style(trust_role)),
            ],
        );
    }
    spans
}

/// Build the one-line header for the given width.
pub fn header_line(data: &HeaderData, width: usize, theme: &Theme) -> Line<'static> {
    let compact_context = width < 80;
    let mut flags = HeaderFlags {
        app: width >= 70,
        profile: width >= 70,
        session: width >= 80,
        throughput: width >= 100,
        cache: width >= 100,
    };
    let mut cwd = display_cwd(&data.cwd);
    let mut spans = header_spans(data, &cwd, &flags, compact_context, theme);
    let limit = width.max(1);

    // Drop optional items in the required order: cache, throughput, session,
    // profile, then the app name. Working directory, model, and context
    // always remain.
    while spans_width(&spans) > limit {
        if flags.cache {
            flags.cache = false;
        } else if flags.throughput {
            flags.throughput = false;
        } else if flags.session {
            flags.session = false;
        } else if flags.profile {
            flags.profile = false;
        } else if flags.app {
            flags.app = false;
        } else {
            break;
        }
        spans = header_spans(data, &cwd, &flags, compact_context, theme);
    }

    // Middle-truncate the working directory when everything else is required.
    let mut cwd_width = UnicodeWidthStr::width(cwd.as_str());
    while spans_width(&spans) > limit && cwd_width > 4 {
        cwd_width -= 1;
        cwd = middle_truncate(&display_cwd(&data.cwd), cwd_width);
        spans = header_spans(data, &cwd, &flags, compact_context, theme);
    }
    Line::from(spans)
}

/// Build the footer line: hints or a transient notice on the left, status on
/// the right.
pub fn footer_line(data: &FooterData, width: usize, theme: &Theme) -> Line<'static> {
    let (left, left_role) = match &data.notice {
        Some(notice) => (notice.as_str(), data.notice_role),
        None => (data.hints.as_str(), Role::Muted),
    };
    let status_width = UnicodeWidthStr::width(data.status.as_str());
    let left_max = width.saturating_sub(status_width + 1);
    let left = clip_end(left, left_max.max(1));
    let left_width = UnicodeWidthStr::width(left.as_str());
    let padding = width.saturating_sub(left_width + status_width).max(1);
    let mut spans = vec![
        Span::styled(left, theme.style(left_role)),
        Span::raw(" ".repeat(padding)),
    ];
    if status_width <= width {
        spans.push(Span::styled(
            data.status.clone(),
            theme.bold(data.status_role),
        ));
    }
    Line::from(spans)
}

#[derive(Debug, Clone)]
pub struct SessionItem {
    pub id: String,
    pub preview: String,
    pub when: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ToolItem {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Default)]
pub struct SessionsModal {
    pub items: Vec<SessionItem>,
    pub selected: usize,
    pub scroll: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ToolsModal {
    pub items: Vec<ToolItem>,
    pub selected: usize,
    pub scroll: usize,
}

#[derive(Debug, Clone)]
pub enum ModalData {
    Tree(TreeView),
    Sessions(SessionsModal),
    Tools(ToolsModal),
}

impl ModalData {
    pub fn title(&self) -> &'static str {
        match self {
            ModalData::Tree(_) => " Session tree ",
            ModalData::Sessions(_) => " Sessions ",
            ModalData::Tools(_) => " Tools ",
        }
    }

    pub fn scroll(&self) -> usize {
        match self {
            ModalData::Tree(view) => view.scroll,
            ModalData::Sessions(view) => view.scroll,
            ModalData::Tools(view) => view.scroll,
        }
    }

    pub fn ensure_visible(&mut self, height: usize) {
        let height = height.max(1);
        match self {
            ModalData::Tree(view) => view.ensure_visible(height),
            ModalData::Sessions(view) => {
                if view.selected < view.scroll {
                    view.scroll = view.selected;
                } else if view.selected >= view.scroll + height {
                    view.scroll = view.selected + 1 - height;
                }
                view.scroll = view.scroll.min(view.items.len().saturating_sub(height));
            }
            ModalData::Tools(view) => {
                if view.selected < view.scroll {
                    view.scroll = view.selected;
                } else if view.selected >= view.scroll + height {
                    view.scroll = view.selected + 1 - height;
                }
                view.scroll = view.scroll.min(view.items.len().saturating_sub(height));
            }
        }
    }

    /// Rows to display, already styled for selection.
    pub fn lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        match self {
            ModalData::Tree(view) => tree_lines(view, width, theme),
            ModalData::Sessions(view) => sessions_lines(view, width, theme),
            ModalData::Tools(view) => tools_lines(view, width, theme),
        }
    }
}

fn selected_style(theme: &Theme) -> Style {
    Style::default().bg(theme.color(Role::Selection))
}

fn role_style(role: char, theme: &Theme) -> Style {
    let role = match role {
        'U' => Role::User,
        'A' => Role::Assistant,
        'R' => Role::Tool,
        'C' => Role::Accent,
        'E' => Role::Error,
        _ => Role::Muted,
    };
    theme.style(role)
}

pub fn tree_lines(view: &TreeView, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, row) in view.rows.iter().enumerate() {
        let when = row
            .ts
            .map(|ts| ts.format("%H:%M").to_string())
            .unwrap_or_default();
        let tokens = if row.tokens > 0 {
            format!("~{}t", row.tokens)
        } else {
            String::new()
        };
        let mut spans = vec![
            Span::styled(row.prefix.clone(), theme.style(Role::Border)),
            Span::styled(format!("{}  ", row.id), theme.style(Role::Muted)),
            Span::styled(format!("{}  ", row.role), role_style(row.role, theme)),
            Span::styled(
                clip_end(&row.preview, width.saturating_sub(28).max(10)),
                theme.style(Role::Fg),
            ),
        ];
        if !when.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(when, theme.style(Role::Muted)));
        }
        if !tokens.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(tokens, theme.style(Role::Muted)));
        }
        if row.active {
            spans.push(Span::raw("  "));
            spans.push(Span::styled("*", theme.bold(Role::Accent)));
        }
        let mut line = Line::from(spans);
        if index == view.selected {
            line = line.style(selected_style(theme));
        }
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(Line::styled("(empty session)", theme.style(Role::Muted)));
    }
    lines
}

pub fn sessions_lines(view: &SessionsModal, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, item) in view.items.iter().enumerate() {
        let preview_max = width
            .saturating_sub(item.id.len() + item.when.len() + 8)
            .max(10);
        let spans = vec![
            Span::styled(item.id.clone(), theme.style(Role::Muted)),
            Span::raw("  "),
            Span::styled(clip_end(&item.preview, preview_max), theme.style(Role::Fg)),
            Span::raw("  "),
            Span::styled(item.when.clone(), theme.style(Role::Muted)),
        ];
        let mut line = Line::from(spans);
        if index == view.selected {
            line = line.style(selected_style(theme));
        }
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(Line::styled("(no sessions)", theme.style(Role::Muted)));
    }
    lines
}

pub fn tools_lines(view: &ToolsModal, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, item) in view.items.iter().enumerate() {
        let description_max = width.saturating_sub(item.name.len() + 4).max(10);
        let spans = vec![
            Span::styled(format!("{:<14}", item.name), theme.bold(Role::Tool)),
            Span::styled(
                clip_end(&item.description, description_max),
                theme.style(Role::Muted),
            ),
        ];
        let mut line = Line::from(spans);
        if index == view.selected {
            line = line.style(selected_style(theme).add_modifier(Modifier::BOLD));
        }
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(Line::styled("(no active tools)", theme.style(Role::Muted)));
    }
    lines
}
