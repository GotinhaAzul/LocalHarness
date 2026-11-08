//! Markdown to styled ratatui lines.
//!
//! The renderer is deliberately forgiving: incomplete or malformed Markdown
//! must never panic, it simply renders what the parser produced so far.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::highlight::highlight_line;
use super::theme::{Role, Theme};

/// Render Markdown for the transcript. Must never panic on incomplete input.
pub fn render_markdown(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(1);
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut renderer = Renderer::new(width, theme);
    let parser = Parser::new_ext(text, options);
    for event in parser {
        renderer.event(event);
    }
    renderer.finish()
}

/// Render a diff-like block with added/removed/hunk styling.
pub fn render_diff(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for raw in text.lines() {
        let role = if raw.starts_with('+') && !raw.starts_with("+++") {
            Role::DiffAdd
        } else if raw.starts_with('-') && !raw.starts_with("---") {
            Role::DiffDel
        } else if raw.starts_with("@@") {
            Role::DiffHunk
        } else {
            Role::Muted
        };
        for line in wrap_plain(raw, width) {
            let style = theme.style(role);
            lines.push(Line::from(vec![Span::styled(line, style)]).style(style));
        }
    }
    if lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines
}

/// Wrap a plain string to `width`, never clipping. Leading indentation is
/// repeated on continuation lines when it leaves room for content.
pub fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let indent: String = text
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let indent_width = UnicodeWidthStr::width(indent.as_str());
    let available = width.saturating_sub(indent_width).max(1);
    let body = &text[indent.len()..];
    let glyphs: Vec<Glyph> = body
        .chars()
        .map(|c| Glyph {
            c,
            style: Style::default(),
        })
        .collect();
    let chunks = wrap_glyphs(&glyphs, available, available, false);
    let mut out = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let mut line = indent.clone();
        line.extend(chunk.iter().map(|glyph| glyph.c));
        out.push(line);
    }
    if out.is_empty() {
        out.push(text.to_string());
    }
    out
}

// ---------------------------------------------------------------------------
// Styled line building blocks
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Glyph {
    c: char,
    style: Style,
}

fn glyph_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

fn glyphs_width(glyphs: &[Glyph]) -> usize {
    glyphs.iter().map(|glyph| glyph_width(glyph.c)).sum()
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

fn spans_to_glyphs(spans: Vec<Span<'static>>) -> Vec<Glyph> {
    let mut glyphs = Vec::new();
    for span in spans {
        for c in span.content.chars() {
            glyphs.push(Glyph {
                c,
                style: span.style,
            });
        }
    }
    glyphs
}

fn glyphs_to_spans(glyphs: Vec<Glyph>) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for glyph in glyphs {
        if let Some(last) = spans.last_mut()
            && last.style == glyph.style
        {
            last.content.to_mut().push(glyph.c);
            continue;
        }
        spans.push(Span::styled(glyph.c.to_string(), glyph.style));
    }
    spans
}

/// Wrap glyphs to at most `limit` display columns. `first_limit` applies to
/// the first output line and `cont_limit` to the following ones. In `hard`
/// mode every character is preserved and words may be split anywhere (used
/// for code); otherwise wrapping happens at spaces and long words are split.
fn wrap_glyphs(
    glyphs: &[Glyph],
    first_limit: usize,
    cont_limit: usize,
    hard: bool,
) -> Vec<Vec<Glyph>> {
    let first_limit = first_limit.max(1);
    let cont_limit = cont_limit.max(1);
    if glyphs.is_empty() {
        return vec![Vec::new()];
    }
    let mut out: Vec<Vec<Glyph>> = Vec::new();
    let mut line: Vec<Glyph> = Vec::new();
    let mut line_width = 0usize;
    let mut limit = first_limit;
    let mut index = 0usize;

    if hard {
        while index < glyphs.len() {
            let width = glyph_width(glyphs[index].c);
            if line_width + width > limit && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                line_width = 0;
                limit = cont_limit;
            }
            line.push(glyphs[index]);
            line_width += width;
            index += 1;
        }
        if !line.is_empty() || out.is_empty() {
            out.push(line);
        }
        return out;
    }

    while index < glyphs.len() {
        let word_start = index;
        while index < glyphs.len() && glyphs[index].c != ' ' {
            index += 1;
        }
        let word = &glyphs[word_start..index];
        let word_width = glyphs_width(word);

        let space_start = index;
        while index < glyphs.len() && glyphs[index].c == ' ' {
            index += 1;
        }
        let spaces = &glyphs[space_start..index];

        if !line.is_empty() && line_width + word_width > limit {
            out.push(std::mem::take(&mut line));
            line_width = 0;
            limit = cont_limit;
        }

        if word_width > limit {
            for glyph in word {
                let width = glyph_width(glyph.c);
                if line_width + width > limit && !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                    line_width = 0;
                    limit = cont_limit;
                }
                line.push(*glyph);
                line_width += width;
            }
        } else {
            line.extend_from_slice(word);
            line_width += word_width;
        }

        let space_width = glyphs_width(spaces);
        if line_width + space_width <= limit {
            line.extend_from_slice(spaces);
            line_width += space_width;
        }
    }

    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ListState {
    ordered: bool,
    next: u64,
    first_prefix: String,
    cont_prefix: String,
    started: bool,
}

#[derive(Debug)]
struct LinkFrame {
    dest: String,
    start: usize,
}

#[derive(Debug)]
struct CodeFrame {
    language: String,
    text: String,
}

#[derive(Debug)]
struct TableRow {
    cells: Vec<Vec<Span<'static>>>,
    header: bool,
}

#[derive(Debug)]
struct TableFrame {
    aligns: Vec<Alignment>,
    rows: Vec<TableRow>,
    current_row: Vec<Vec<Span<'static>>>,
    current_cell: Vec<Span<'static>>,
    cell_active: bool,
    in_head: bool,
    header_rows: usize,
}

impl TableFrame {
    fn new(aligns: Vec<Alignment>) -> Self {
        Self {
            aligns,
            rows: Vec::new(),
            current_row: Vec::new(),
            current_cell: Vec::new(),
            cell_active: false,
            in_head: false,
            header_rows: 0,
        }
    }
}

struct Renderer<'t> {
    theme: &'t Theme,
    width: usize,
    lines: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
    quote_depth: usize,
    lists: Vec<ListState>,
    strong: usize,
    emphasis: usize,
    strike: usize,
    heading: Option<HeadingLevel>,
    link: Option<LinkFrame>,
    code: Option<CodeFrame>,
    table: Option<TableFrame>,
}

impl<'t> Renderer<'t> {
    fn new(width: usize, theme: &'t Theme) -> Self {
        Self {
            theme,
            width: width.max(1),
            lines: Vec::new(),
            cur: Vec::new(),
            quote_depth: 0,
            lists: Vec::new(),
            strong: 0,
            emphasis: 0,
            strike: 0,
            heading: None,
            link: None,
            code: None,
            table: None,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(text.as_ref()),
            Event::Code(code) => {
                let style = self
                    .theme
                    .style(Role::Code)
                    .bg(self.theme.color(Role::CodeBg));
                self.push_span(Span::styled(code.to_string(), style));
            }
            Event::InlineMath(math) => {
                let style = self
                    .theme
                    .style(Role::Code)
                    .bg(self.theme.color(Role::CodeBg));
                self.push_span(Span::styled(format!("${math}$"), style));
            }
            Event::DisplayMath(math) => {
                let style = self
                    .theme
                    .style(Role::Code)
                    .bg(self.theme.color(Role::CodeBg));
                self.push_span(Span::styled(format!("$${math}$$"), style));
            }
            Event::Html(html) => {
                self.push_span(Span::styled(
                    html.to_string(),
                    self.theme.style(Role::Muted),
                ));
            }
            Event::InlineHtml(html) => {
                self.push_span(Span::styled(
                    html.to_string(),
                    self.theme.style(Role::Muted),
                ));
            }
            Event::FootnoteReference(label) => {
                self.push_span(Span::styled(
                    format!("[^{label}]"),
                    self.theme.style(Role::Muted),
                ));
            }
            Event::SoftBreak => {
                if self.table_cell_active() {
                    self.push_span(Span::styled(" ", Style::default()));
                } else {
                    self.text(" ");
                }
            }
            Event::HardBreak => {
                if self.table_cell_active() {
                    self.push_span(Span::styled(" ", Style::default()));
                } else {
                    self.flush_line();
                }
            }
            Event::Rule => {
                self.flush_line();
                let rule = "─".repeat(self.width);
                self.lines
                    .push(Line::styled(rule, self.theme.style(Role::Muted)));
                self.blank();
            }
            Event::TaskListMarker(checked) => {
                let (glyph, role) = if checked {
                    ("☑ ", Role::Success)
                } else {
                    ("☐ ", Role::Muted)
                };
                self.push_span(Span::styled(glyph, self.theme.style(role)));
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if !self.cur.is_empty() {
                    self.flush_line();
                }
            }
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.heading = Some(level);
                self.push_span(Span::styled(
                    format!("{} ", "#".repeat(level as usize)),
                    self.theme.bold(Role::Accent),
                ));
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                let language = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeFrame {
                    language,
                    text: String::new(),
                });
            }
            Tag::HtmlBlock => {
                self.flush_line();
            }
            Tag::List(start) => {
                self.flush_line();
                self.lists.push(ListState {
                    ordered: start.is_some(),
                    next: start.unwrap_or(1),
                    first_prefix: String::new(),
                    cont_prefix: String::new(),
                    started: false,
                });
            }
            Tag::Item => {
                self.flush_line();
                let depth = self.lists.len().saturating_sub(1);
                if let Some(list) = self.lists.last_mut() {
                    let indent = "  ".repeat(depth);
                    let marker = if list.ordered {
                        let marker = format!("{}. ", list.next);
                        list.next = list.next.saturating_add(1);
                        marker
                    } else {
                        "- ".to_string()
                    };
                    let marker_width = UnicodeWidthStr::width(marker.as_str());
                    list.first_prefix = format!("{indent}{marker}");
                    list.cont_prefix = format!("{indent}{}", " ".repeat(marker_width));
                    list.started = false;
                }
            }
            Tag::Table(aligns) => {
                self.flush_line();
                self.table = Some(TableFrame::new(aligns));
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_head = true;
                }
            }
            Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.current_row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.cell_active = true;
                    table.current_cell.clear();
                }
            }
            Tag::Emphasis => self.emphasis += 1,
            Tag::Strong => self.strong += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                let start = self.inline_len();
                self.link = Some(LinkFrame {
                    dest: dest_url.to_string(),
                    start,
                });
            }
            Tag::Image { dest_url, .. } => {
                let start = self.inline_len();
                self.link = Some(LinkFrame {
                    dest: dest_url.to_string(),
                    start,
                });
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                if self.lists.is_empty() && self.quote_depth == 0 {
                    self.blank();
                }
            }
            TagEnd::Heading(_) => {
                self.heading = None;
                self.flush_line();
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                if self.quote_depth == 0 {
                    self.blank();
                }
            }
            TagEnd::CodeBlock => {
                if let Some(frame) = self.code.take() {
                    self.emit_code(frame);
                }
            }
            TagEnd::HtmlBlock => {
                self.flush_line();
                self.blank();
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => {
                self.flush_line();
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.emit_table(table);
                }
                self.blank();
            }
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    if !table.current_row.is_empty() {
                        let row = TableRow {
                            cells: std::mem::take(&mut table.current_row),
                            header: true,
                        };
                        table.rows.push(row);
                    }
                    table.header_rows = table.rows.len();
                    table.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    let row = TableRow {
                        cells: std::mem::take(&mut table.current_row),
                        header: table.in_head,
                    };
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.cell_active = false;
                    let cell = std::mem::take(&mut table.current_cell);
                    table.current_row.push(cell);
                }
            }
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => self.finish_link(),
            TagEnd::Image => self.finish_link(),
            _ => {}
        }
    }

    fn finish(&mut self) -> Vec<Line<'static>> {
        if !self.cur.is_empty() {
            self.flush_line();
        }
        while self.lines.len() > 1
            && self
                .lines
                .last()
                .map(|line| line.width() == 0)
                .unwrap_or(false)
        {
            self.lines.pop();
        }
        if self.lines.is_empty() {
            self.lines.push(Line::raw(""));
        }
        std::mem::take(&mut self.lines)
    }

    // -- inline helpers ----------------------------------------------------

    fn base_style(&self) -> Style {
        let role = if self.heading.is_some() {
            Role::Accent
        } else {
            Role::Assistant
        };
        let mut style = self.theme.style(role);
        if self.heading.is_some() || self.strong > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.emphasis > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        style
    }

    fn table_cell_active(&self) -> bool {
        self.table
            .as_ref()
            .map(|table| table.cell_active)
            .unwrap_or(false)
    }

    fn inline_len(&self) -> usize {
        if self.table_cell_active() {
            self.table
                .as_ref()
                .map(|table| table.current_cell.len())
                .unwrap_or(0)
        } else {
            self.cur.len()
        }
    }

    fn push_span(&mut self, span: Span<'static>) {
        if self.table_cell_active() {
            if let Some(table) = self.table.as_mut() {
                table.current_cell.push(span);
            }
        } else {
            self.cur.push(span);
        }
    }

    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(frame) = &mut self.code {
            frame.text.push_str(text);
            return;
        }
        let style = self.base_style();
        if self.table_cell_active() {
            let flattened = text.replace('\n', " ");
            if !flattened.is_empty() {
                self.push_span(Span::styled(flattened, style));
            }
            return;
        }
        let mut first = true;
        for part in text.split('\n') {
            if !first {
                self.flush_line();
            }
            first = false;
            if !part.is_empty() {
                self.push_span(Span::styled(part.to_string(), style));
            }
        }
    }

    fn finish_link(&mut self) {
        let Some(frame) = self.link.take() else {
            return;
        };
        let accent = self.theme.color(Role::Accent);
        let mut text = String::new();
        if self.table_cell_active() {
            if let Some(table) = self.table.as_mut()
                && frame.start <= table.current_cell.len()
            {
                let slice = &mut table.current_cell[frame.start..];
                for span in slice.iter_mut() {
                    span.style = span.style.fg(accent).add_modifier(Modifier::UNDERLINED);
                    text.push_str(span.content.as_ref());
                }
            }
        } else if frame.start <= self.cur.len() {
            let slice = &mut self.cur[frame.start..];
            for span in slice.iter_mut() {
                span.style = span.style.fg(accent).add_modifier(Modifier::UNDERLINED);
                text.push_str(span.content.as_ref());
            }
        }
        if !frame.dest.is_empty() && frame.dest != text {
            self.push_span(Span::styled(
                format!(" ({})", frame.dest),
                self.theme.style(Role::Muted),
            ));
        }
    }

    // -- block helpers -----------------------------------------------------

    fn quote_spans(&self) -> Vec<Span<'static>> {
        if self.quote_depth == 0 {
            Vec::new()
        } else {
            vec![Span::styled(
                "│ ".repeat(self.quote_depth),
                self.theme.style(Role::Muted),
            )]
        }
    }

    fn line_prefixes(&self) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let mut first = self.quote_spans();
        let mut cont = self.quote_spans();
        if let Some(list) = self.lists.last() {
            let (first_marker, cont_marker) = if list.started {
                (&list.cont_prefix, &list.cont_prefix)
            } else {
                (&list.first_prefix, &list.cont_prefix)
            };
            first.push(Span::styled(
                first_marker.clone(),
                self.theme.style(Role::AccentDim),
            ));
            cont.push(Span::styled(
                cont_marker.clone(),
                self.theme.style(Role::AccentDim),
            ));
        }
        (first, cont)
    }

    fn flush_line(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.cur);
        let glyphs = spans_to_glyphs(spans);
        let (first, cont) = self.line_prefixes();
        let first_width = self.width.saturating_sub(spans_width(&first)).max(1);
        let cont_width = self.width.saturating_sub(spans_width(&cont)).max(1);
        let chunks = wrap_glyphs(&glyphs, first_width, cont_width, false);
        for (index, chunk) in chunks.into_iter().enumerate() {
            let mut line_spans = if index == 0 {
                first.clone()
            } else {
                cont.clone()
            };
            line_spans.extend(glyphs_to_spans(chunk));
            self.lines.push(Line::from(line_spans));
        }
        if let Some(list) = self.lists.last_mut() {
            list.started = true;
        }
    }

    fn blank(&mut self) {
        if self
            .lines
            .last()
            .map(|line| line.width() > 0)
            .unwrap_or(false)
        {
            self.lines.push(Line::raw(""));
        }
    }

    fn emit_code(&mut self, frame: CodeFrame) {
        let language = frame.language.trim().to_string();
        let (first, cont) = self.line_prefixes();
        let cont_width = spans_width(&cont);

        let mut header_spans = first;
        header_spans.push(Span::styled("┌ ", self.theme.style(Role::Border)));
        if !language.is_empty() {
            header_spans.push(Span::styled(
                language.clone(),
                self.theme.bold(Role::Accent),
            ));
        }
        self.lines.push(Line::from(header_spans));

        if language.eq_ignore_ascii_case("diff") {
            let available = self.width.saturating_sub(cont_width).max(1);
            let diff_lines = render_diff(&frame.text, available, self.theme);
            for diff_line in diff_lines {
                if cont.is_empty() {
                    self.lines.push(diff_line);
                } else {
                    let mut spans = cont.clone();
                    spans.extend(diff_line.spans);
                    self.lines.push(Line::from(spans));
                }
            }
        } else {
            let available = self.width.saturating_sub(cont_width).max(1);
            let mut emitted = false;
            for raw in frame.text.lines() {
                emitted = true;
                let spans = highlight_line(raw, &language, self.theme);
                let glyphs = spans_to_glyphs(spans);
                let chunks = wrap_glyphs(&glyphs, available, available, true);
                for chunk in chunks {
                    let mut line_spans = cont.clone();
                    line_spans.extend(glyphs_to_spans(chunk));
                    self.lines.push(Line::from(line_spans));
                }
            }
            if !emitted {
                let mut line_spans = cont.clone();
                line_spans.push(Span::styled(String::new(), self.theme.style(Role::Code)));
                self.lines.push(Line::from(line_spans));
            }
        }
        self.blank();
    }

    fn emit_table(&mut self, table: TableFrame) {
        let columns = table.aligns.len().max(
            table
                .rows
                .iter()
                .map(|row| row.cells.len())
                .max()
                .unwrap_or(0),
        );
        if columns == 0 {
            return;
        }

        let mut natural = vec![1usize; columns];
        for row in &table.rows {
            for (index, cell) in row.cells.iter().enumerate() {
                if index < columns {
                    natural[index] = natural[index].max(spans_width(cell));
                }
            }
        }

        let separator = " │ ";
        let separator_width = UnicodeWidthStr::width(separator);
        let available = self
            .width
            .saturating_sub(separator_width.saturating_mul(columns.saturating_sub(1)))
            .max(columns);
        let mut widths = natural.clone();
        let mut total: usize = widths.iter().sum();
        while total > available {
            let mut widest: Option<usize> = None;
            let mut widest_value = 1usize;
            for (index, width) in widths.iter().enumerate() {
                if *width > widest_value {
                    widest_value = *width;
                    widest = Some(index);
                }
            }
            match widest {
                Some(index) => {
                    widths[index] -= 1;
                    total -= 1;
                }
                None => break,
            }
        }

        for (row_index, row) in table.rows.iter().enumerate() {
            if row_index == table.header_rows && table.header_rows > 0 {
                let separator_line = self.table_separator(&widths);
                self.lines.push(separator_line);
            }
            let mut wrapped: Vec<Vec<Vec<Glyph>>> = Vec::with_capacity(columns);
            for (column, width) in widths.iter().enumerate().take(columns) {
                let glyphs = row
                    .cells
                    .get(column)
                    .map(|cell| spans_to_glyphs(cell.clone()))
                    .unwrap_or_default();
                wrapped.push(wrap_glyphs(
                    &glyphs,
                    (*width).max(1),
                    (*width).max(1),
                    false,
                ));
            }
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
            for physical in 0..height {
                let mut spans: Vec<Span<'static>> = Vec::new();
                for column in 0..columns {
                    if column > 0 {
                        spans.push(Span::styled(separator, self.theme.style(Role::Border)));
                    }
                    let chunk = wrapped[column].get(physical).cloned().unwrap_or_default();
                    let content_width = glyphs_width(&chunk);
                    let padding = widths[column].saturating_sub(content_width);
                    let (left, right) = match table.aligns.get(column) {
                        Some(Alignment::Right) => (padding, 0),
                        Some(Alignment::Center) => (padding / 2, padding - padding / 2),
                        _ => (0, padding),
                    };
                    if left > 0 {
                        spans.push(Span::raw(" ".repeat(left)));
                    }
                    if row.header {
                        let style = self.theme.bold(Role::Accent);
                        for glyph in chunk {
                            let text = glyph.c.to_string();
                            if let Some(last) = spans.last_mut()
                                && last.style == style
                            {
                                last.content.to_mut().push_str(&text);
                                continue;
                            }
                            spans.push(Span::styled(text, style));
                        }
                    } else {
                        spans.extend(glyphs_to_spans(chunk));
                    }
                    if right > 0 {
                        spans.push(Span::raw(" ".repeat(right)));
                    }
                }
                self.lines.push(Line::from(spans));
            }
        }
    }

    fn table_separator(&self, widths: &[usize]) -> Line<'static> {
        let style = self.theme.style(Role::Muted);
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (index, width) in widths.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled("─┼─", style));
            }
            spans.push(Span::styled("─".repeat(*width), style));
        }
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::bundled("omarchy")
    }

    fn text_of(lines: &[Line<'_>]) -> String {
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

    fn has_style(lines: &[Line<'_>], style: Style) -> bool {
        lines
            .iter()
            .any(|line| line.style == style || line.spans.iter().any(|span| span.style == style))
    }

    #[test]
    fn empty_input_is_one_line() {
        let theme = theme();
        let lines = render_markdown("", 40, &theme);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].width(), 0);
    }

    #[test]
    fn headings_are_bold_accent() {
        let theme = theme();
        let lines = render_markdown("# Title", 40, &theme);
        assert!(text_of(&lines).contains("Title"));
        assert!(has_style(&lines, theme.bold(Role::Accent)));
    }

    #[test]
    fn lists_and_nesting() {
        let theme = theme();
        let lines = render_markdown("- one\n  - nested\n- two", 40, &theme);
        let text = text_of(&lines);
        assert!(text.contains("- one"));
        assert!(text.contains("- nested"));
        assert!(text.contains("- two"));
        assert!(text.find("nested").unwrap() > text.find("one").unwrap());
    }

    #[test]
    fn fenced_code_has_language_header() {
        let theme = theme();
        let lines = render_markdown("```rust\nlet x = 1;\n```", 40, &theme);
        let text = text_of(&lines);
        assert!(text.contains("┌ rust"));
        assert!(text.contains("let x = 1;"));
        assert!(has_style(&lines, theme.bold(Role::Accent)));
    }

    #[test]
    fn diff_fence_is_highlighted() {
        let theme = theme();
        let lines = render_markdown("```diff\n+added\n-removed\n@@ hunk\n```", 40, &theme);
        assert!(has_style(&lines, theme.style(Role::DiffAdd)));
        assert!(has_style(&lines, theme.style(Role::DiffDel)));
        assert!(has_style(&lines, theme.style(Role::DiffHunk)));
        assert!(text_of(&lines).contains("+added"));
    }

    #[test]
    fn incomplete_fence_does_not_panic() {
        let theme = theme();
        let lines = render_markdown("```rust\nlet x = 1;", 30, &theme);
        let text = text_of(&lines);
        assert!(text.contains("rust"));
        assert!(text.contains("let x = 1;"));
    }

    #[test]
    fn long_words_are_broken_never_clipped() {
        let word = "a".repeat(45);
        let lines = render_markdown(&word, 10, &theme());
        assert!(lines.len() >= 5);
        assert_eq!(text_of(&lines).replace('\n', ""), word);
        for line in &lines {
            assert!(line.width() <= 10);
        }
    }

    #[test]
    fn tables_render_columns() {
        let theme = theme();
        let lines = render_markdown("| Name | Value |\n| --- | --- |\n| a | 1 |", 40, &theme);
        let text = text_of(&lines);
        assert!(text.contains("Name"));
        assert!(text.contains("Value"));
        assert!(text.contains("a"));
        assert!(text.contains("1"));
        assert!(text.contains('┼'));
    }

    #[test]
    fn links_show_url_when_different() {
        let theme = theme();
        let lines = render_markdown("[docs](https://example.com)", 60, &theme);
        let text = text_of(&lines);
        assert!(text.contains("docs"));
        assert!(text.contains("(https://example.com)"));
    }

    #[test]
    fn inline_styles_are_applied() {
        let theme = theme();
        let lines = render_markdown("**bold** *italic* ~~gone~~", 60, &theme);
        assert!(has_style(
            &lines,
            theme.style(Role::Assistant).add_modifier(Modifier::BOLD)
        ));
        assert!(has_style(
            &lines,
            theme.style(Role::Assistant).add_modifier(Modifier::ITALIC)
        ));
        assert!(has_style(
            &lines,
            theme
                .style(Role::Assistant)
                .add_modifier(Modifier::CROSSED_OUT)
        ));
    }

    #[test]
    fn diff_helper_styles_lines() {
        let theme = theme();
        let lines = render_diff("+a\n-b\n@@ c\n ctx", 20, &theme);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].style, theme.style(Role::DiffAdd));
        assert_eq!(lines[1].style, theme.style(Role::DiffDel));
        assert_eq!(lines[2].style, theme.style(Role::DiffHunk));
        assert_eq!(lines[3].style, theme.style(Role::Muted));
    }

    #[test]
    fn no_color_theme_still_renders_text() {
        let mut theme = theme();
        theme.no_color = true;
        for line in render_markdown("# h\n\ntext", 40, &theme) {
            assert!(line.width() <= 40);
        }
    }
}
