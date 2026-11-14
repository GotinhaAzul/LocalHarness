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
    /// Effective tool mode from the conformance probe (`tools:native` or
    /// `tools:repair`); empty when unknown.
    pub tool_mode: String,
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
    /// The context window is a fallback or an upper bound, not a measurement.
    pub window_guess: bool,
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

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
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
    profile: bool,
    tool_mode: bool,
    session: bool,
    throughput: bool,
    cache: bool,
    gauge: bool,
}

fn dot(theme: &Theme) -> Span<'static> {
    Span::styled(" · ", theme.style(Role::Border))
}

fn join(groups: Vec<Vec<Span<'static>>>, theme: &Theme) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for group in groups.into_iter().filter(|group| !group.is_empty()) {
        if !out.is_empty() {
            out.push(dot(theme));
        }
        out.extend(group);
    }
    out
}

fn context_spans(data: &HeaderData, gauge: bool, theme: &Theme) -> Vec<Span<'static>> {
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
    let mut spans = Vec::new();
    if gauge {
        let cells = 8usize;
        let filled = ((ratio.clamp(0.0, 1.0)) * cells as f64).ceil() as usize;
        let filled = filled.min(cells);
        spans.push(Span::styled("▰".repeat(filled), theme.style(role)));
        spans.push(Span::styled(
            "▱".repeat(cells - filled),
            theme.style(Role::Border),
        ));
        spans.push(Span::styled(
            format!(" {}% ", (ratio * 100.0).round().clamp(0.0, 999.0) as u32),
            theme.style(role),
        ));
    }
    let mut text = format!("{used}/{window}");
    if data.completion > 0 {
        text.push('+');
        text.push_str(&format_tokens(data.completion));
    }
    spans.push(Span::styled(text, theme.style(Role::Muted)));
    if data.window_guess {
        spans.push(Span::styled("?", theme.bold(Role::Warning)));
    }
    spans
}

fn header_sides(
    data: &HeaderData,
    cwd: &str,
    flags: &HeaderFlags,
    theme: &Theme,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let git_role = if !data.git.in_repo {
        Role::Muted
    } else if data.git.dirty {
        Role::Warning
    } else {
        Role::Success
    };
    let mut left = vec![
        vec![Span::styled(cwd.to_string(), theme.style(Role::Fg))],
        vec![Span::styled(data.git.label(), theme.style(git_role))],
    ];
    if flags.profile && !data.profile.is_empty() {
        left.push(vec![Span::styled(
            data.profile.clone(),
            theme.style(Role::Muted),
        )]);
    }
    if flags.tool_mode && !data.tool_mode.is_empty() {
        let role = if data.tool_mode.ends_with("repair") {
            Role::Warning
        } else {
            Role::Muted
        };
        left.push(vec![Span::styled(
            data.tool_mode.clone(),
            theme.style(role),
        )]);
    }
    if !data.trust {
        left.push(vec![Span::styled("untrusted", theme.style(Role::Warning))]);
    }

    let (model, model_role) = if data.logged_in {
        (data.model.clone(), Role::Assistant)
    } else {
        ("not logged in".to_string(), Role::Warning)
    };
    let mut right = vec![
        vec![Span::styled(model, theme.bold(model_role))],
        context_spans(data, flags.gauge, theme),
    ];
    if flags.throughput {
        let mut speed = format!("{:.0} tok/s", data.tok_s.max(0.0));
        if flags.cache
            && let Some(cache) = data.cache
        {
            speed.push_str(&format!(" · cache {cache}%"));
        }
        right.push(vec![Span::styled(speed, theme.style(Role::Muted))]);
    }
    if flags.session && !data.session.is_empty() {
        right.push(vec![Span::styled(
            data.session.clone(),
            theme.style(Role::Muted),
        )]);
    }
    (join(left, theme), join(right, theme))
}

/// Build the one-line info bar for the given width: location on the left,
/// model and context on the right.
pub fn header_line(data: &HeaderData, width: usize, theme: &Theme) -> Line<'static> {
    let mut flags = HeaderFlags {
        profile: width >= 70,
        tool_mode: width >= 80,
        session: width >= 90,
        throughput: width >= 100,
        cache: width >= 110,
        gauge: width >= 70,
    };
    let limit = width.max(1);
    let mut cwd = display_cwd(&data.cwd);
    let measure = |left: &[Span<'static>], right: &[Span<'static>]| {
        // One leading space plus at least two between the sides.
        1 + spans_width(left) + 2 + spans_width(right)
    };
    let (mut left, mut right) = header_sides(data, &cwd, &flags, theme);

    // Drop optional items in order: cache, throughput, session, profile, and
    // the gauge. Working directory, model, and context always remain.
    while measure(&left, &right) > limit {
        if flags.cache {
            flags.cache = false;
        } else if flags.throughput {
            flags.throughput = false;
        } else if flags.session {
            flags.session = false;
        } else if flags.tool_mode {
            flags.tool_mode = false;
        } else if flags.profile {
            flags.profile = false;
        } else if flags.gauge {
            flags.gauge = false;
        } else {
            break;
        }
        (left, right) = header_sides(data, &cwd, &flags, theme);
    }

    // Middle-truncate the working directory when everything else is required.
    let mut cwd_width = UnicodeWidthStr::width(cwd.as_str());
    while measure(&left, &right) > limit && cwd_width > 4 {
        cwd_width -= 1;
        cwd = middle_truncate(&display_cwd(&data.cwd), cwd_width);
        (left, right) = header_sides(data, &cwd, &flags, theme);
    }
    let gap = limit
        .saturating_sub(1 + spans_width(&left) + spans_width(&right))
        .max(2);
    let mut spans = vec![Span::raw(" ")];
    spans.extend(left);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    Line::from(spans)
}

/// Build the footer line: hints or a transient notice on the left, status on
/// the right.
pub fn footer_line(data: &FooterData, width: usize, theme: &Theme) -> Line<'static> {
    let (left, left_role) = match &data.notice {
        Some(notice) => (notice.as_str(), data.notice_role),
        None => (data.hints.as_str(), Role::Muted),
    };
    let width = width.saturating_sub(2);
    let status_width = UnicodeWidthStr::width(data.status.as_str());
    let left_max = width.saturating_sub(status_width + 1);
    let left = clip_end(left, left_max.max(1));
    let left_width = UnicodeWidthStr::width(left.as_str());
    let padding = width.saturating_sub(left_width + status_width).max(1);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(left, theme.style(left_role)),
        Span::raw(" ".repeat(padding)),
    ];
    if status_width <= width {
        spans.push(Span::styled(
            data.status.clone(),
            theme.style(data.status_role),
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

/// What choosing a picker row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickAction {
    /// Run this slash command line, for example `/model qwen3`.
    Run(String),
    /// Put this text in the prompt so the user can finish it, for example
    /// `/sec scope add `.
    Fill(String),
}

/// One row of a [`PickerModal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickItem {
    pub label: String,
    pub description: String,
    pub action: PickAction,
}

impl PickItem {
    /// A row that runs `/{command} {label}`.
    pub fn run(command: &str, label: impl Into<String>, description: impl Into<String>) -> Self {
        let label = label.into();
        PickItem {
            action: PickAction::Run(format!("/{command} {label}")),
            label,
            description: description.into(),
        }
    }
}

/// A filterable menu behind every slash command that chooses from a list
/// (`/model`, `/theme`, `/profile`, `/skill`, `/workflow`, `/sec`). Typing
/// narrows the list with a case-insensitive match on the label and the
/// description; arrows move; Enter picks.
#[derive(Debug, Clone, Default)]
pub struct PickerModal {
    /// The command that opened the picker, for example `model`.
    pub command: String,
    pub title: String,
    /// Extra context for the title, for example the endpoint.
    pub subtitle: String,
    pub items: Vec<PickItem>,
    /// Label of the active choice, marked with `*` and preselected.
    pub current: String,
    pub filter: String,
    /// Index into [`PickerModal::filtered`].
    pub selected: usize,
    pub scroll: usize,
}

impl PickerModal {
    pub fn new(
        command: impl Into<String>,
        title: impl Into<String>,
        items: Vec<PickItem>,
        current: impl Into<String>,
    ) -> Self {
        let current = current.into();
        let selected = items
            .iter()
            .position(|item| item.label == current)
            .unwrap_or(0);
        PickerModal {
            command: command.into(),
            title: title.into(),
            subtitle: String::new(),
            items,
            current,
            filter: String::new(),
            selected,
            scroll: 0,
        }
    }

    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = subtitle.into();
        self
    }

    /// Rows matching the filter: label prefix matches first, then label
    /// substrings, then description matches.
    pub fn filtered(&self) -> Vec<&PickItem> {
        let needle = self.filter.to_lowercase();
        if needle.is_empty() {
            return self.items.iter().collect();
        }
        let mut ranked: Vec<(u8, &PickItem)> = self
            .items
            .iter()
            .filter_map(|item| {
                let label = item.label.to_lowercase();
                let rank = if label.starts_with(&needle) {
                    0
                } else if label.contains(&needle) {
                    1
                } else if item.description.to_lowercase().contains(&needle) {
                    2
                } else {
                    return None;
                };
                Some((rank, item))
            })
            .collect();
        ranked.sort_by_key(|(rank, _)| *rank);
        ranked.into_iter().map(|(_, item)| item).collect()
    }

    pub fn selected_item(&self) -> Option<&PickItem> {
        self.filtered().get(self.selected).copied()
    }

    pub fn selected_label(&self) -> Option<String> {
        self.selected_item().map(|item| item.label.clone())
    }

    pub fn move_by(&mut self, delta: isize) {
        let count = self.filtered().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, count as isize - 1) as usize;
    }

    pub fn push_filter(&mut self, ch: char) {
        self.filter.push(ch);
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.selected = 0;
        self.scroll = 0;
    }

    /// Replace the rows (a background refresh), keeping the filter and the
    /// highlighted row when it still exists.
    pub fn replace_items(&mut self, items: Vec<PickItem>) {
        let keep = self.selected_label();
        self.items = items;
        let filtered = self.filtered();
        self.selected = keep
            .and_then(|label| filtered.iter().position(|item| item.label == label))
            .unwrap_or(0);
    }
}

#[derive(Debug, Clone)]
pub enum ModalData {
    Tree(TreeView),
    Sessions(SessionsModal),
    Tools(ToolsModal),
    Picker(PickerModal),
}

impl ModalData {
    pub fn title(&self) -> String {
        match self {
            ModalData::Tree(_) => " Session tree ".into(),
            ModalData::Sessions(_) => " Sessions · Enter resume · Esc close ".into(),
            ModalData::Tools(_) => " Tools ".into(),
            ModalData::Picker(view) => {
                let shown = view.filtered().len();
                let filter = if view.filter.is_empty() {
                    "type to filter".to_string()
                } else {
                    format!("filter: {}", view.filter)
                };
                let subtitle = if view.subtitle.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", view.subtitle)
                };
                format!(
                    " {}{subtitle} · {shown}/{} · {filter} · ↑↓ move · Enter select · Esc close ",
                    view.title,
                    view.items.len()
                )
            }
        }
    }

    /// Number of rows the modal displays, for sizing the popup.
    pub fn row_count(&self) -> usize {
        match self {
            ModalData::Tree(view) => view.rows.len(),
            ModalData::Sessions(view) => view.items.len(),
            ModalData::Tools(view) => view.items.len(),
            ModalData::Picker(view) => view.filtered().len().max(1),
        }
    }

    pub fn scroll(&self) -> usize {
        match self {
            ModalData::Tree(view) => view.scroll,
            ModalData::Sessions(view) => view.scroll,
            ModalData::Tools(view) => view.scroll,
            ModalData::Picker(view) => view.scroll,
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
            ModalData::Picker(view) => {
                let count = view.filtered().len();
                view.selected = view.selected.min(count.saturating_sub(1));
                if view.selected < view.scroll {
                    view.scroll = view.selected;
                } else if view.selected >= view.scroll + height {
                    view.scroll = view.selected + 1 - height;
                }
                view.scroll = view.scroll.min(count.saturating_sub(height));
            }
        }
    }

    /// Rows to display, already styled for selection.
    pub fn lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        match self {
            ModalData::Tree(view) => tree_lines(view, width, theme),
            ModalData::Sessions(view) => sessions_lines(view, width, theme),
            ModalData::Tools(view) => tools_lines(view, width, theme),
            ModalData::Picker(view) => picker_lines(view, width, theme),
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

pub fn picker_lines(view: &PickerModal, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let rows = view.filtered();
    // Align descriptions in a column after the widest label (capped so a
    // long model id does not push them off screen).
    let label_width = rows
        .iter()
        .map(|item| UnicodeWidthStr::width(item.label.as_str()))
        .max()
        .unwrap_or(0)
        .min(width.saturating_sub(16).clamp(10, 48));
    let mut lines = Vec::new();
    for (index, item) in rows.into_iter().enumerate() {
        let current = !view.current.is_empty() && item.label == view.current;
        let label = clip_end(&item.label, label_width.max(1));
        let pad = label_width.saturating_sub(UnicodeWidthStr::width(label.as_str()));
        let mut spans = vec![
            Span::styled(if current { "* " } else { "  " }, theme.bold(Role::Accent)),
            Span::styled(
                label,
                if current {
                    theme.bold(Role::Fg)
                } else {
                    theme.style(Role::Fg)
                },
            ),
        ];
        let mut description = item.description.clone();
        if current {
            description = if description.is_empty() {
                "(current)".into()
            } else {
                format!("{description} (current)")
            };
        }
        if !description.is_empty() {
            let room = width.saturating_sub(label_width + 6);
            if room > 3 {
                spans.push(Span::raw(" ".repeat(pad + 2)));
                spans.push(Span::styled(
                    clip_end(&description, room),
                    theme.style(Role::Muted),
                ));
            }
        }
        let mut line = Line::from(spans);
        if index == view.selected {
            line = line.style(selected_style(theme));
        }
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            if view.items.is_empty() {
                "(nothing to choose from)"
            } else {
                "(nothing matches the filter · Backspace to widen)"
            },
            theme.style(Role::Muted),
        ));
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
