//! HTML to readable text and light Markdown.
//!
//! [`html_to_text`] is the converter behind the planned `fetch` tool: it drops
//! page chrome (scripts, styles, navigation, headers, footers, asides) and
//! keeps the title plus the main/article content, rendering headings, lists,
//! links, tables, and fenced code blocks as Markdown-ish text. [`page_text`]
//! splits the result into whole-line pages so a long page can be returned in
//! parts without a partial last line.
//!
//! Both are pure: no networking, no I/O, no new dependencies. Parsing is a
//! single-pass tokenizer over an explicit stack, so malformed markup cannot
//! panic and deeply nested input cannot overflow the call stack.

/// Convert an HTML document to plain text / light Markdown.
pub fn html_to_text(html: &str) -> String {
    let title = extract_title(html);
    let region = select_region(html);
    let raw = Renderer::new().run(region, title.as_deref());
    cleanup(&raw)
}

/// Page `text` by character offset into whole lines within `max_chars`.
///
/// `offset` is a character offset into `text`; the returned page is a prefix
/// of the remaining text made of whole lines, and the returned offset is the
/// character offset of the first line not included (`None` at the end). A
/// single line longer than `max_chars` is cut at a character boundary so
/// progress is always possible.
pub fn page_text(text: &str, offset: usize, max_chars: usize) -> (String, Option<usize>) {
    if max_chars == 0 || offset >= text.chars().count() {
        return (String::new(), None);
    }
    let Some(start) = byte_at_char(text, offset) else {
        return (String::new(), None);
    };
    let mut pos = start;
    let mut used = 0usize;
    let mut first = true;
    while pos < text.len() {
        let line_end = match text[pos..].find('\n') {
            Some(index) => pos + index + 1,
            None => text.len(),
        };
        let line_chars = text[pos..line_end].chars().count();
        if first && line_chars > max_chars {
            let cut = max_chars.max(1);
            let Some(cut_byte) = byte_at_char(&text[pos..], cut) else {
                return (text[pos..].to_string(), None);
            };
            return (text[pos..pos + cut_byte].to_string(), Some(offset + cut));
        }
        if !first && used + line_chars > max_chars {
            return (text[start..pos].to_string(), Some(offset + used));
        }
        used += line_chars;
        pos = line_end;
        first = false;
    }
    (text[start..pos].to_string(), None)
}

/// Byte offset of the `index`th character, or `None` past the end.
fn byte_at_char(text: &str, index: usize) -> Option<usize> {
    if index == 0 {
        return Some(0);
    }
    text.char_indices().nth(index).map(|(byte, _)| byte)
}

// ---------------------------------------------------------------------------
// Region selection
// ---------------------------------------------------------------------------

/// Content of the first `<main>`, else the first `<article>`, else `<body>`,
/// else the whole document. Elements whose content is dropped elsewhere are
/// skipped here too, so a `<main>` inside a template or a navigation block is
/// not mistaken for the page.
fn select_region(html: &str) -> &str {
    find_element_content(html, "main")
        .or_else(|| find_element_content(html, "article"))
        .or_else(|| find_element_content(html, "body"))
        .unwrap_or(html)
}

fn find_element_content<'a>(html: &'a str, wanted: &str) -> Option<&'a str> {
    let mut tokenizer = Tokenizer::new(html);
    let mut skipping: Vec<&str> = Vec::new();
    while let Some((_, token)) = tokenizer.next() {
        match token {
            Token::Start(tag) => {
                if is_dropped(tag.name) {
                    if !tag.self_closing {
                        skipping.push(tag.name);
                    }
                    continue;
                }
                if !skipping.is_empty() {
                    continue;
                }
                if tag.name.eq_ignore_ascii_case(wanted) {
                    let content_start = tokenizer.pos;
                    return Some(scan_to_close(&mut tokenizer, html, tag.name, content_start));
                }
            }
            Token::End(name) => {
                if let Some(index) = skipping
                    .iter()
                    .rposition(|open| open.eq_ignore_ascii_case(name))
                {
                    skipping.truncate(index);
                }
            }
            Token::Text(_) | Token::RawText(_) | Token::Skip => {}
        }
    }
    None
}

/// Content between the current position and the matching close tag, or the rest
/// of the document when the element is never closed.
fn scan_to_close<'a>(
    tokenizer: &mut Tokenizer<'a>,
    html: &'a str,
    name: &str,
    content_start: usize,
) -> &'a str {
    let mut depth = 1usize;
    while let Some((start, token)) = tokenizer.next() {
        match token {
            Token::Start(tag) if tag.name.eq_ignore_ascii_case(name) && !tag.self_closing => {
                depth += 1;
            }
            Token::End(end) if end.eq_ignore_ascii_case(name) => {
                depth -= 1;
                if depth == 0 {
                    return &html[content_start..start];
                }
            }
            _ => {}
        }
    }
    &html[content_start..]
}

/// The first `<title>` as one collapsed line, decoded.
fn extract_title(html: &str) -> Option<String> {
    let mut tokenizer = Tokenizer::new(html);
    while let Some((_, token)) = tokenizer.next() {
        let Token::Start(tag) = token else {
            continue;
        };
        if !tag.name.eq_ignore_ascii_case("title") || tag.self_closing {
            continue;
        }
        let Some((_, Token::RawText(text))) = tokenizer.next() else {
            return None;
        };
        let title = collapse_whitespace(&decode_entities(text));
        if !title.is_empty() {
            return Some(title);
        }
        return None;
    }
    None
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

struct Tag<'a> {
    name: &'a str,
    attrs: Vec<(&'a str, &'a str)>,
    self_closing: bool,
}

impl<'a> Tag<'a> {
    /// First attribute with `wanted` as its name (case-insensitive), decoded.
    fn attr(&self, wanted: &str) -> Option<String> {
        self.attrs
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| decode_entities(value))
    }
}

enum Token<'a> {
    /// Text that needs entity decoding and whitespace collapsing.
    Text(&'a str),
    /// Raw text (`script`, `style`, `title`, `textarea`): entities only.
    RawText(&'a str),
    Start(Tag<'a>),
    End(&'a str),
    /// Comment, doctype, or other declaration: dropped.
    Skip,
}

struct Tokenizer<'a> {
    src: &'a str,
    pos: usize,
    /// Set after a raw-text start tag: the next token is text up to the close.
    raw: Option<&'a str>,
}

impl<'a> Tokenizer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            raw: None,
        }
    }

    fn next(&mut self) -> Option<(usize, Token<'a>)> {
        if self.pos >= self.src.len() {
            return None;
        }
        let start = self.pos;
        if let Some(name) = self.raw.take() {
            let from = self.pos;
            let end = find_close_tag(self.src, from, name).unwrap_or(self.src.len());
            self.pos = end;
            return Some((start, Token::RawText(&self.src[from..end])));
        }
        let rest = &self.src[self.pos..];
        let Some(offset) = rest.find('<') else {
            self.pos = self.src.len();
            return Some((start, Token::Text(&self.src[start..])));
        };
        if offset > 0 {
            self.pos += offset;
            return Some((start, Token::Text(&self.src[start..self.pos])));
        }
        if self.src[self.pos..].starts_with("<!--") {
            self.pos = match self.src[self.pos + 4..].find("-->") {
                Some(index) => self.pos + 4 + index + 3,
                None => self.src.len(),
            };
            return Some((start, Token::Skip));
        }
        if self.src[self.pos..].starts_with("<!") || self.src[self.pos..].starts_with("<?") {
            self.pos = match find_tag_end(self.src, self.pos + 2) {
                Some(index) => index + 1,
                None => self.src.len(),
            };
            return Some((start, Token::Skip));
        }
        if self.src[self.pos..].starts_with("</") {
            let name_start = self.pos + 2;
            let bytes = self.src.as_bytes();
            let mut end = name_start;
            while end < bytes.len() && is_name_byte(bytes[end]) {
                end += 1;
            }
            if end == name_start {
                self.pos += 1;
                return Some((start, Token::Text(&self.src[start..self.pos])));
            }
            let name = &self.src[name_start..end];
            self.pos = match find_tag_end(self.src, end) {
                Some(index) => index + 1,
                None => self.src.len(),
            };
            return Some((start, Token::End(name)));
        }
        let bytes = self.src.as_bytes();
        if self.pos + 1 >= bytes.len() || !bytes[self.pos + 1].is_ascii_alphabetic() {
            self.pos += 1;
            return Some((start, Token::Text(&self.src[start..self.pos])));
        }
        let name_start = self.pos + 1;
        let mut end = name_start;
        while end < bytes.len() && is_name_byte(bytes[end]) {
            end += 1;
        }
        let name = &self.src[name_start..end];
        let Some(tag_end) = find_tag_end(self.src, end) else {
            // No `>` remains anywhere after this `<`: nothing else can be a
            // tag, so keep the rest as text instead of rescanning for every
            // stray `<` (which would be quadratic on hostile input).
            self.pos = self.src.len();
            return Some((start, Token::Text(&self.src[start..])));
        };
        let attr_text = &self.src[end..tag_end];
        let self_closing = attr_text.trim_end().ends_with('/');
        let attrs = parse_attrs(attr_text);
        self.pos = tag_end + 1;
        if is_raw_text(name) {
            self.raw = Some(name);
        }
        Some((
            start,
            Token::Start(Tag {
                name,
                attrs,
                self_closing,
            }),
        ))
    }
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':' | b'_')
}

fn is_raw_text(name: &str) -> bool {
    name.eq_ignore_ascii_case("script")
        || name.eq_ignore_ascii_case("style")
        || name.eq_ignore_ascii_case("title")
        || name.eq_ignore_ascii_case("textarea")
}

/// Byte index of the `>` that ends a tag, ignoring `>` inside quoted values.
fn find_tag_end(src: &str, from: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut quote = 0u8;
    for (index, &byte) in bytes.iter().enumerate().skip(from) {
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
        } else if byte == b'"' || byte == b'\'' {
            quote = byte;
        } else if byte == b'>' {
            return Some(index);
        }
    }
    None
}

/// Byte index of the `<` that opens the closing tag for a raw-text element.
fn find_close_tag(src: &str, from: usize, name: &str) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut index = from;
    while index + 2 + name.len() <= bytes.len() {
        if bytes[index] == b'<' && bytes[index + 1] == b'/' {
            let candidate = &src[index + 2..index + 2 + name.len()];
            if candidate.eq_ignore_ascii_case(name) {
                let boundary = match bytes.get(index + 2 + name.len()) {
                    None => true,
                    Some(&byte) => matches!(byte, b'>' | b'/') || byte.is_ascii_whitespace(),
                };
                if boundary {
                    return Some(index);
                }
            }
        }
        index += 1;
    }
    None
}

/// Attributes of a start tag. Values keep their raw text; callers decode them.
fn parse_attrs(text: &str) -> Vec<(&str, &str)> {
    let bytes = text.as_bytes();
    let mut attrs = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && (bytes[index].is_ascii_whitespace() || bytes[index] == b'/') {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let name_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && bytes[index] != b'='
            && bytes[index] != b'/'
        {
            index += 1;
        }
        if index == name_start {
            // A stray `=` or quote: skip it so every step makes progress.
            index += 1;
            continue;
        }
        let name = &text[name_start..index];
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let mut value = "";
        if index < bytes.len() && bytes[index] == b'=' {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            if index < bytes.len() && (bytes[index] == b'"' || bytes[index] == b'\'') {
                let quote = bytes[index];
                index += 1;
                let value_start = index;
                while index < bytes.len() && bytes[index] != quote {
                    index += 1;
                }
                value = &text[value_start..index];
                if index < bytes.len() {
                    index += 1;
                }
            } else {
                let value_start = index;
                while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                    index += 1;
                }
                value = &text[value_start..index];
            }
        }
        attrs.push((name, value));
    }
    attrs
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    decode_entities_into(text, &mut out);
    out
}

fn decode_entities_into(text: &str, out: &mut String) {
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        let semicolon = after
            .char_indices()
            .take(32)
            .find(|(_, ch)| *ch == ';')
            .map(|(offset, _)| offset);
        if let Some(semicolon) = semicolon
            && let Some(decoded) = decode_entity(&after[..semicolon])
        {
            out.push(decoded);
            rest = &after[semicolon + 1..];
            continue;
        }
        // Not an entity we know: keep the `&` and continue after it.
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
}

fn decode_entity(name: &str) -> Option<char> {
    if let Some(number) = name.strip_prefix('#') {
        let (radix, digits) = match number.strip_prefix(['x', 'X']) {
            Some(hex) => (16, hex),
            None => (10, number),
        };
        if digits.is_empty() {
            return None;
        }
        let mut value: u32 = 0;
        for digit in digits.chars() {
            value = value
                .checked_mul(radix)?
                .checked_add(digit.to_digit(radix)?)?;
            if value > 0x10_FFFF {
                return None;
            }
        }
        if value == 0 {
            return None;
        }
        char::from_u32(value)
    } else {
        named_entity(name)
    }
}

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" | "ensp" => ' ',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "copy" => '©',
        "rsquo" => '’',
        "lsquo" => '‘',
        "rdquo" => '”',
        "ldquo" => '“',
        "middot" => '·',
        "bull" => '•',
        "trade" => '™',
        "reg" => '®',
        "deg" => '°',
        "times" => '×',
        "laquo" => '«',
        "raquo" => '»',
        "para" => '¶',
        "sect" => '§',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "rarr" => '→',
        "larr" => '←',
        "harr" => '↔',
        "le" => '≤',
        "ge" => '≥',
        "ne" => '≠',
        "minus" => '−',
        "plusmn" => '±',
        "frac12" => '½',
        "sup2" => '²',
        _ => return None,
    })
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

const DROPPED: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "head", "title", "nav", "header", "footer",
    "aside",
];

const BLOCKS: &[&str] = &[
    "p",
    "div",
    "section",
    "article",
    "main",
    "body",
    "html",
    "blockquote",
    "figure",
    "figcaption",
    "form",
    "fieldset",
    "dl",
    "dt",
    "dd",
    "address",
    "details",
    "summary",
    "center",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "caption",
    "hgroup",
    "legend",
    "output",
    "marquee",
];

fn eq(name: &str, expected: &str) -> bool {
    name.eq_ignore_ascii_case(expected)
}

fn is_dropped(name: &str) -> bool {
    DROPPED.iter().any(|dropped| eq(name, dropped))
}

fn is_block(name: &str) -> bool {
    BLOCKS.iter().any(|block| eq(name, block))
}

fn heading_level(name: &str) -> Option<usize> {
    let bytes = name.as_bytes();
    if bytes.len() == 2
        && (bytes[0] == b'h' || bytes[0] == b'H')
        && (b'1'..=b'6').contains(&bytes[1])
    {
        Some(usize::from(bytes[1] - b'0'))
    } else {
        None
    }
}

fn list_indent(depth: usize) -> usize {
    depth.saturating_sub(1) * 2
}

enum ListKind {
    Unordered,
    Ordered(usize),
}

struct Link {
    href: Option<String>,
    /// Byte offset in the writer where the link's text starts.
    start: usize,
}

struct Renderer<'a> {
    writer: Writer,
    drop_stack: Vec<&'a str>,
    lists: Vec<ListKind>,
    links: Vec<Link>,
    inline_code: usize,
    row_cells: usize,
}

impl<'a> Renderer<'a> {
    fn new() -> Self {
        Self {
            writer: Writer::new(),
            drop_stack: Vec::new(),
            lists: Vec::new(),
            links: Vec::new(),
            inline_code: 0,
            row_cells: 0,
        }
    }

    fn run(mut self, region: &'a str, title: Option<&str>) -> String {
        if let Some(title) = title
            && !title.is_empty()
        {
            self.writer.raw("# ");
            self.writer.text(title);
            self.writer.blank_line();
        }
        let mut tokenizer = Tokenizer::new(region);
        while let Some((_, token)) = tokenizer.next() {
            match token {
                Token::Text(text) | Token::RawText(text) => {
                    if self.drop_stack.is_empty() {
                        push_text(&mut self.writer, text);
                    }
                }
                Token::Start(tag) => {
                    if is_dropped(tag.name) {
                        if !tag.self_closing {
                            self.drop_stack.push(tag.name);
                        }
                        continue;
                    }
                    if !self.drop_stack.is_empty() {
                        continue;
                    }
                    if eq(tag.name, "pre") && !tag.self_closing {
                        let (content, language) = read_pre(&mut tokenizer);
                        self.writer.code_block(&content, language.as_deref());
                        continue;
                    }
                    self.start_tag(&tag);
                }
                Token::End(name) => {
                    if !self.drop_stack.is_empty() {
                        if let Some(index) = self.drop_stack.iter().rposition(|open| eq(open, name))
                        {
                            self.drop_stack.truncate(index);
                        }
                        continue;
                    }
                    self.end_tag(name);
                }
                Token::Skip => {}
            }
        }
        self.writer.finish()
    }

    fn start_tag(&mut self, tag: &Tag<'a>) {
        let name = tag.name;
        if eq(name, "br") {
            self.writer.newline();
        } else if eq(name, "hr") {
            self.writer.blank_line();
        } else if eq(name, "img") {
            self.image(tag);
        } else if eq(name, "a") {
            if !tag.self_closing {
                self.links.push(Link {
                    href: http_href(tag),
                    start: self.writer.out.len(),
                });
            }
        } else if eq(name, "code") {
            if !tag.self_closing {
                self.inline_code += 1;
                if self.inline_code == 1 {
                    self.writer.raw("`");
                }
            }
        } else if eq(name, "ul") || eq(name, "ol") || eq(name, "menu") {
            if self.lists.is_empty() {
                self.writer.blank_line();
            } else {
                self.writer.newline();
            }
            if !tag.self_closing {
                self.lists.push(if eq(name, "ol") {
                    ListKind::Ordered(1)
                } else {
                    ListKind::Unordered
                });
            }
            self.writer.set_indent(list_indent(self.lists.len()));
        } else if eq(name, "li") {
            self.list_item();
        } else if eq(name, "tr") {
            self.writer.newline();
            self.row_cells = 0;
        } else if eq(name, "td") || eq(name, "th") {
            if self.row_cells > 0 {
                self.writer.text(" | ");
            }
            self.row_cells += 1;
        } else if let Some(level) = heading_level(name) {
            self.writer.blank_line();
            self.writer.raw(&"#".repeat(level));
            self.writer.raw(" ");
        } else if is_block(name) {
            if eq(name, "table") {
                self.row_cells = 0;
            }
            self.writer.blank_line();
        }
    }

    fn end_tag(&mut self, name: &str) {
        if eq(name, "li") || eq(name, "tr") {
            self.writer.newline();
        } else if eq(name, "ul") || eq(name, "ol") || eq(name, "menu") {
            self.lists.pop();
            self.writer.set_indent(list_indent(self.lists.len()));
            if self.lists.is_empty() {
                self.writer.blank_line();
            } else {
                self.writer.newline();
            }
        } else if eq(name, "a") {
            self.finish_link();
        } else if eq(name, "code") {
            if self.inline_code > 0 {
                self.inline_code -= 1;
                if self.inline_code == 0 {
                    self.writer.raw("`");
                }
            }
        } else if heading_level(name).is_some() {
            self.writer.blank_line();
        } else if is_block(name) {
            if eq(name, "table") {
                self.row_cells = 0;
            }
            self.writer.blank_line();
        }
    }

    fn list_item(&mut self) {
        self.writer.newline();
        self.writer.set_indent(list_indent(self.lists.len()));
        let marker = match self.lists.last_mut() {
            Some(ListKind::Ordered(next)) => {
                let marker = format!("{next}. ");
                *next += 1;
                marker
            }
            _ => "- ".to_string(),
        };
        self.writer.raw(&marker);
    }

    fn finish_link(&mut self) {
        let Some(link) = self.links.pop() else {
            return;
        };
        let Some(href) = link.href else {
            return;
        };
        let text = self.writer.out.get(link.start..).unwrap_or("").trim();
        if text.is_empty() || text == href {
            return;
        }
        self.writer.text(" (");
        self.writer.raw(&href);
        self.writer.raw(")");
    }

    fn image(&mut self, tag: &Tag<'a>) {
        let Some(alt) = tag.attr("alt") else {
            return;
        };
        let alt = collapse_whitespace(&alt);
        if alt.is_empty() {
            return;
        }
        self.writer.text("[image: ");
        self.writer.text(&alt);
        self.writer.text("]");
    }
}

fn push_text(writer: &mut Writer, text: &str) {
    if text.contains('&') {
        let decoded = decode_entities(text);
        writer.text(&decoded);
    } else {
        writer.text(text);
    }
}

/// Link targets only become a Markdown suffix when absolute and web-facing.
fn http_href(tag: &Tag<'_>) -> Option<String> {
    let href = tag.attr("href")?;
    let href = href.trim();
    let lower = href.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))?;
    if rest.is_empty() {
        return None;
    }
    Some(href.to_string())
}

/// Consume a `<pre>` element and return its text plus any `language-xxx` class.
fn read_pre(tokenizer: &mut Tokenizer<'_>) -> (String, Option<String>) {
    let content_start = tokenizer.pos;
    let mut depth = 1usize;
    let mut content_end = tokenizer.src.len();
    while let Some((start, token)) = tokenizer.next() {
        match token {
            Token::Start(tag) if eq(tag.name, "pre") && !tag.self_closing => depth += 1,
            Token::End(name) if eq(name, "pre") => {
                depth -= 1;
                if depth == 0 {
                    content_end = start;
                    break;
                }
            }
            _ => {}
        }
    }
    extract_pre(&tokenizer.src[content_start..content_end])
}

fn extract_pre(inner: &str) -> (String, Option<String>) {
    let mut tokenizer = Tokenizer::new(inner);
    let mut content = String::with_capacity(inner.len());
    let mut language = None;
    let mut code_seen = false;
    while let Some((_, token)) = tokenizer.next() {
        match token {
            Token::Text(text) | Token::RawText(text) => {
                if text.contains('&') {
                    decode_entities_into(text, &mut content);
                } else {
                    content.push_str(text);
                }
            }
            Token::Start(tag) => {
                if !code_seen && eq(tag.name, "code") {
                    code_seen = true;
                    language = language_of(&tag);
                }
            }
            Token::End(_) | Token::Skip => {}
        }
    }
    // HTML drops one newline directly after `<pre>`; trailing breaks before the
    // closing tag are not content either.
    let cut = if content.starts_with("\r\n") {
        2
    } else if content.starts_with('\n') {
        1
    } else {
        0
    };
    if cut > 0 {
        content = content[cut..].to_string();
    }
    while content.ends_with(['\n', '\r']) {
        content.pop();
    }
    (content, language)
}

fn language_of(tag: &Tag<'_>) -> Option<String> {
    let class = tag.attr("class")?;
    for token in class.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        let Some(rest) = lower.strip_prefix("language-") else {
            continue;
        };
        let cleaned: String = rest
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '+' | '.' | '#'))
            .take(24)
            .collect();
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Output writer
// ---------------------------------------------------------------------------

/// Accumulates text while deferring line breaks and spaces, so block structure
/// and whitespace collapsing stay in one place.
struct Writer {
    out: String,
    pending_space: bool,
    pending_newlines: usize,
    at_line_start: bool,
    indent: usize,
}

impl Writer {
    fn new() -> Self {
        Self {
            out: String::new(),
            pending_space: false,
            pending_newlines: 0,
            at_line_start: true,
            indent: 0,
        }
    }

    fn text(&mut self, text: &str) {
        for ch in text.chars() {
            if ch.is_whitespace() {
                self.pending_space = true;
            } else {
                self.begin();
                self.out.push(ch);
            }
        }
    }

    fn raw(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.begin();
        self.out.push_str(text);
    }

    fn newline(&mut self) {
        self.pending_newlines = self.pending_newlines.max(1);
        self.pending_space = false;
    }

    fn blank_line(&mut self) {
        self.pending_newlines = self.pending_newlines.max(2);
        self.pending_space = false;
    }

    fn set_indent(&mut self, indent: usize) {
        self.indent = indent;
    }

    fn begin(&mut self) {
        if self.pending_newlines > 0 {
            if !self.out.is_empty() {
                for _ in 0..self.pending_newlines.min(2) {
                    self.out.push('\n');
                }
            }
            self.pending_newlines = 0;
            self.at_line_start = true;
            self.pending_space = false;
        }
        if self.at_line_start {
            if !self.out.is_empty() {
                for _ in 0..self.indent {
                    self.out.push(' ');
                }
            }
            self.at_line_start = false;
        } else if self.pending_space {
            self.out.push(' ');
        }
        self.pending_space = false;
    }

    /// A fenced block whose content is written byte for byte.
    fn code_block(&mut self, content: &str, language: Option<&str>) {
        let mut longest_run = 0usize;
        let mut run = 0usize;
        for byte in content.bytes() {
            if byte == b'`' {
                run += 1;
                longest_run = longest_run.max(run);
            } else {
                run = 0;
            }
        }
        let fence = "`".repeat((longest_run + 1).max(3));
        self.blank_line();
        self.begin();
        self.out.push_str(&fence);
        if let Some(language) = language {
            self.out.push_str(language);
        }
        self.out.push('\n');
        self.out.push_str(content);
        if !content.is_empty() && !content.ends_with('\n') {
            self.out.push('\n');
        }
        self.at_line_start = true;
        self.begin();
        self.out.push_str(&fence);
        self.at_line_start = false;
        self.pending_space = false;
        self.blank_line();
    }

    fn finish(mut self) -> String {
        self.pending_newlines = 0;
        self.pending_space = false;
        self.out
    }
}

/// Trim line ends and collapse extra blank lines outside fenced code.
fn cleanup(raw: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut fence = 0usize;
    for line in raw.split('\n') {
        if fence > 0 {
            kept.push(line);
            let trimmed = line.trim_start();
            let ticks = trimmed.bytes().take_while(|&byte| byte == b'`').count();
            if ticks >= fence && trimmed[ticks..].trim().is_empty() {
                fence = 0;
            }
            continue;
        }
        let trimmed = line.trim_start();
        let ticks = trimmed.bytes().take_while(|&byte| byte == b'`').count();
        if ticks >= 3 {
            fence = ticks;
            kept.push(line);
            continue;
        }
        let line = line.trim_end_matches([' ', '\t', '\r']);
        if line.is_empty() {
            if kept.is_empty() || kept.last().is_some_and(|last| last.is_empty()) {
                continue;
            }
            kept.push(line);
        } else {
            kept.push(line);
        }
    }
    while kept.last().is_some_and(|line| line.is_empty()) {
        kept.pop();
    }
    kept.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Widget API &mdash; Reference</title>
  <script>var leak = "script text";</script>
  <style>.secret { color: red; }</style>
</head>
<body>
  <header><p>header junk</p></header>
  <nav><a href="/">Home</a> nav-link-text</nav>
  <main>
    <h1>Widget API</h1>
    <p>Use the <code>widget()</code> function to build &amp; run things.</p>
    <p>A literal a &lt; b stays readable.</p>
    <h2>Example</h2>
    <pre><code class="language-rust">let a = 1 &lt; 2;
if a &amp;&amp; b {
    println!("&lt;tag&gt;");
}
</code></pre>
    <ul>
      <li>first item</li>
      <li>second item</li>
    </ul>
    <p>See <a href="https://example.com/docs">the docs</a> or
      <a href="https://example.com/x">https://example.com/x</a>.</p>
    <table>
      <tr><th>Name</th><th>Value</th></tr>
      <tr><td>alpha</td><td>1</td></tr>
    </table>
  </main>
  <footer><p>footer junk</p></footer>
</body>
</html>
"#;

    #[test]
    fn documentation_page_becomes_readable_text() {
        let text = html_to_text(DOC);
        assert!(
            text.starts_with("# Widget API — Reference\n\n"),
            "title first: {text:?}"
        );
        assert!(text.contains("\n# Widget API\n"), "h1: {text:?}");
        assert!(text.contains("## Example"), "h2: {text:?}");
        assert!(text.contains("`widget()`"), "inline code: {text:?}");
        assert!(
            text.contains("A literal a < b stays readable."),
            "decoded text: {text:?}"
        );
        assert!(
            text.contains("- first item\n- second item"),
            "list: {text:?}"
        );
        assert!(
            text.contains("the docs (https://example.com/docs)"),
            "link: {text:?}"
        );
        assert!(
            !text.contains("(https://example.com/x)"),
            "link text that is its href stays bare: {text:?}"
        );
        assert!(text.contains("Name | Value\nalpha | 1"), "table: {text:?}");
        assert!(text.contains("```rust"), "fence: {text:?}");
        assert!(text.contains("```\n"), "closing fence: {text:?}");
    }

    #[test]
    fn code_blocks_keep_whitespace_and_entities() {
        let text = html_to_text(DOC);
        let code = "```rust\nlet a = 1 < 2;\nif a && b {\n    println!(\"<tag>\");\n}\n```";
        assert!(text.contains(code), "{text:?}");
        assert!(!text.contains("&lt;"), "entities decoded: {text:?}");
    }

    #[test]
    fn chrome_and_scripts_are_dropped() {
        let text = html_to_text(DOC);
        for junk in [
            "script text",
            "nav-link-text",
            "header junk",
            "footer junk",
            ".secret",
            "<main",
            "<li>",
            "<table",
        ] {
            assert!(!text.contains(junk), "{junk:?} leaked: {text:?}");
        }
    }

    #[test]
    fn title_becomes_a_leading_heading() {
        let text = html_to_text(
            "<html><head><title>T &amp; U</title><style>x{}</style></head>\
             <body><p>Body</p></body></html>",
        );
        assert_eq!(text, "# T & U\n\nBody");
    }

    #[test]
    fn pre_keeps_indentation_and_gets_a_language() {
        let text = html_to_text(
            "<pre><code class=\"language-sh\">  echo <b>hi</b>\n\tx &amp; y</code></pre>",
        );
        assert_eq!(text, "```sh\n  echo hi\n\tx & y\n```");
    }

    #[test]
    fn unclosed_pre_is_still_fenced() {
        let text = html_to_text("<pre><code class=\"language-sh\">echo &amp; hi</code>");
        assert_eq!(text, "```sh\necho & hi\n```");
    }

    #[test]
    fn pre_content_containing_backticks_gets_a_longer_fence() {
        let text = html_to_text("<pre>```\ncode\n```</pre>");
        assert!(text.starts_with("````\n```\ncode\n```\n````"), "{text:?}");
    }

    #[test]
    fn lists_indent_nested_items() {
        let text =
            html_to_text("<ul><li>one</li><li>two<ul><li>nested</li></ul></li><li>three</li></ul>");
        assert_eq!(text, "- one\n- two\n  - nested\n- three");
        let ordered = html_to_text("<ol><li>a</li><li>b</li></ol>");
        assert_eq!(ordered, "1. a\n2. b");
    }

    #[test]
    fn images_and_links_follow_the_rules() {
        assert_eq!(
            html_to_text("<p>x <img alt=\"A chart\" src=\"c.png\"> y</p>"),
            "x [image: A chart] y"
        );
        assert_eq!(
            html_to_text("<p>x <img alt=\"\" src=\"c.png\"> y</p>"),
            "x y"
        );
        assert_eq!(html_to_text("<p>x <img src=\"c.png\"> y</p>"), "x y");
        assert_eq!(html_to_text("<p><a href=\"/docs\">docs</a></p>"), "docs");
        assert_eq!(
            html_to_text("<p><a href=\"mailto:a@b\">mail me</a></p>"),
            "mail me"
        );
        assert_eq!(
            html_to_text("<p><a href=\"https://e.com/\">see</a></p>"),
            "see (https://e.com/)"
        );
        assert_eq!(html_to_text("<p><a href=\"https://e.com/\"></a></p>"), "");
        assert_eq!(
            html_to_text("<p><a href=\"HTTPS://E.com/A\">x</a></p>"),
            "x (HTTPS://E.com/A)"
        );
    }

    #[test]
    fn entities_decode_named_and_numeric() {
        assert_eq!(
            html_to_text("<p>&amp; &lt; &gt; &quot; &apos;</p>"),
            "& < > \" '"
        );
        assert_eq!(html_to_text("<p>a&nbsp;b</p>"), "a b");
        assert_eq!(
            html_to_text("<p>&mdash;&ndash;&hellip;&copy;&rsquo;&lsquo;&rdquo;&ldquo;</p>"),
            "—–…©’‘”“"
        );
        assert_eq!(html_to_text("<p>&#123; &#x7B;</p>"), "{ {");
        assert_eq!(html_to_text("<p>&#x1F600;</p>"), "😀");
    }

    #[test]
    fn unknown_and_broken_entities_are_left_alone() {
        assert_eq!(
            html_to_text("<p>&unknown; &amp &#xZZ; &#0;</p>"),
            "&unknown; &amp &#xZZ; &#0;"
        );
        assert_eq!(html_to_text("<p>&AMP;</p>"), "&AMP;");
    }

    #[test]
    fn malformed_html_never_panics_or_leaks_tags() {
        assert_eq!(html_to_text(""), "");
        assert_eq!(html_to_text("<"), "<");
        assert_eq!(html_to_text("<>"), "<>");
        assert_eq!(html_to_text("</"), "</");
        assert_eq!(html_to_text("<!-- open"), "");
        assert_eq!(html_to_text("<!DOCTYPE html>"), "");
        assert_eq!(
            html_to_text("<!DOCTYPE html><!-- hidden --><p>shown</p>"),
            "shown"
        );
        assert_eq!(html_to_text("<p>a < b</p>"), "a < b");
        assert_eq!(html_to_text("<p>1 < 2 && 3 > 2</p>"), "1 < 2 && 3 > 2");
        assert_eq!(html_to_text("<p>one <b>two"), "one two");
        assert_eq!(html_to_text("<div title=\"a > b\">quoted</div>"), "quoted");
        assert_eq!(html_to_text("<P>UPPER</P>"), "UPPER");
        assert_eq!(html_to_text("a <b"), "a <b");
        assert_eq!(html_to_text("<p><div><span>x"), "x");
        assert_eq!(html_to_text("<script>var x = '<p>no</p>';"), "");
        assert_eq!(
            html_to_text("<script>alert('<p>no</p>')</script><p>yes</p>"),
            "yes"
        );
    }

    #[test]
    fn unclosed_tag_soup_stays_linear_and_keeps_its_text() {
        let hostile = "<a".repeat(10_000);
        assert_eq!(html_to_text(&hostile), hostile);
        assert_eq!(html_to_text("<p>a < b < c"), "a < b < c");
    }

    #[test]
    fn main_is_preferred_over_body_and_article_is_a_fallback() {
        assert_eq!(
            html_to_text("<body><p>body</p><main><p>main</p></main></body>"),
            "main"
        );
        assert_eq!(
            html_to_text("<body><p>body</p><article><p>article</p></article></body>"),
            "article"
        );
        assert_eq!(html_to_text("<main><p>only"), "only");
        assert_eq!(
            html_to_text("<nav><main>hidden</main></nav><body><p>shown</p></body>"),
            "shown"
        );
        assert_eq!(html_to_text("<body><p>body</p></body>"), "body");
    }

    #[test]
    fn deeply_nested_input_does_not_overflow_the_stack() {
        let mut html = String::from("<main>");
        for _ in 0..10_000 {
            html.push_str("<div>");
        }
        html.push('x');
        for _ in 0..10_000 {
            html.push_str("</div>");
        }
        html.push_str("</main>");
        assert_eq!(html_to_text(&html), "x");
    }

    #[test]
    fn page_text_covers_every_line_once() {
        let mut text = String::new();
        for index in 0..60 {
            text.push_str(&format!("line {index} {}\n", "x".repeat(index % 13)));
        }
        let mut offset = 0;
        let mut collected = String::new();
        let mut pages = 0;
        loop {
            let (page, next) = page_text(&text, offset, 40);
            assert!(
                page.ends_with('\n'),
                "page {pages} ends on a whole line: {page:?}"
            );
            collected.push_str(&page);
            pages += 1;
            assert!(pages < 1_000, "paging must terminate");
            match next {
                Some(next) => {
                    assert!(next > offset, "offset must advance");
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(collected, text, "each character appears exactly once");
    }

    #[test]
    fn page_text_falls_back_to_a_partial_line() {
        let text = "a".repeat(100);
        let (page, next) = page_text(&text, 0, 10);
        assert_eq!(page, "a".repeat(10));
        assert_eq!(next, Some(10));
        let (page, next) = page_text(&text, 90, 10);
        assert_eq!(page, "a".repeat(10));
        assert_eq!(next, None);
    }

    #[test]
    fn page_text_cuts_only_lines_that_do_not_fit() {
        let text = "short\nthis line is far longer than the budget\nend\n";
        let (first, next) = page_text(text, 0, 20);
        assert_eq!(first, "short\n");
        assert_eq!(next, Some(6));
        let (second, next) = page_text(text, 6, 20);
        assert_eq!(second.chars().count(), 20);
        assert_eq!(next, Some(26));
        // Walk the rest and confirm the text is reproduced exactly.
        let mut collected = first;
        collected.push_str(&second);
        let mut offset = 26;
        loop {
            let (page, next) = page_text(text, offset, 20);
            collected.push_str(&page);
            match next {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert_eq!(collected, text);
    }

    #[test]
    fn page_text_handles_offsets_at_the_edges() {
        assert_eq!(page_text("", 0, 10), (String::new(), None));
        assert_eq!(page_text("abc", 3, 10), (String::new(), None));
        assert_eq!(page_text("abc", 99, 10), (String::new(), None));
        assert_eq!(page_text("abc", 0, 0), (String::new(), None));
        assert_eq!(page_text("abc", 0, 10), ("abc".to_string(), None));
    }

    #[test]
    fn page_text_never_splits_a_character() {
        let text = "é".repeat(25);
        let (page, next) = page_text(&text, 0, 4);
        assert_eq!(page.chars().count(), 4);
        assert_eq!(next, Some(4));
        let mut offset = 0;
        let mut collected = String::new();
        loop {
            let (page, next) = page_text(&text, offset, 4);
            collected.push_str(&page);
            match next {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert_eq!(collected, text);
    }
}
