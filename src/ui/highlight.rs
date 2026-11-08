//! Lightweight, advisory syntax highlighting. Must never panic.
//!
//! `highlight_line` is intentionally a single-line scanner: it never carries
//! state across lines, so an unterminated block comment or string is styled
//! to the end of the line and the next line starts fresh. The exact input
//! text is always preserved across the returned spans.

use ratatui::style::Style;
use ratatui::text::Span;

use super::theme::{Role, Theme};

/// Highlight a single line. Unknown languages return a single code-colored span.
pub fn highlight_line<'a>(line: &'a str, language: &str, theme: &Theme) -> Vec<Span<'static>> {
    let language = normalize_language(language);
    let Some(spec) = spec_for(&language) else {
        return vec![Span::styled(line.to_string(), theme.style(Role::Code))];
    };

    let plain = theme.style(Role::Code);
    let keyword = theme.style(Role::Accent);
    let string = theme.style(Role::Success);
    let number = theme.style(Role::Warning);
    let comment = theme.style(Role::Muted);
    let typ = theme.style(Role::Tool);

    if line.is_empty() {
        return vec![Span::styled(String::new(), plain)];
    }

    let chars: Vec<char> = line.chars().collect();
    let mut pieces: Vec<(Style, String)> = Vec::new();
    let mut index = 0usize;

    while index < chars.len() {
        let current = chars[index];

        // Block comments (single-line scan only).
        if spec.block_comments && at(&chars, index, "/*") {
            let mut end = chars.len();
            let mut scan = index + 2;
            while scan + 1 < chars.len() {
                if chars[scan] == '*' && chars[scan + 1] == '/' {
                    end = scan + 2;
                    break;
                }
                scan += 1;
            }
            push(&mut pieces, comment, &chars[index..end]);
            index = end;
            continue;
        }

        // Line comments.
        if let Some(marker) = spec
            .line_comments
            .iter()
            .find(|marker| at(&chars, index, marker))
        {
            let _ = marker;
            push(&mut pieces, comment, &chars[index..]);
            index = chars.len();
            continue;
        }

        // Rust raw strings: r"..." / r#"..."# / br#"..."#.
        if spec.raw_strings {
            if let Some(end) = scan_rust_raw(&chars, index) {
                push(&mut pieces, string, &chars[index..end]);
                index = end;
                continue;
            }
        }

        // Quoted strings, including unterminated ones.
        if spec.quotes.contains(&current) && is_string_start(&spec, &chars, index, current) {
            let end = scan_quoted(&chars, index, current, spec.triple_quotes);
            push(&mut pieces, string, &chars[index..end]);
            index = end;
            continue;
        }

        if current.is_ascii_digit() {
            let end = scan_number(&chars, index);
            push(&mut pieces, number, &chars[index..end]);
            index = end;
            continue;
        }

        if current.is_alphabetic() || current == '_' {
            let mut end = index + 1;
            while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            let word: String = chars[index..end].iter().collect();
            let style = if spec.keywords.contains(&word.as_str()) {
                keyword
            } else if spec.types.contains(&word.as_str()) {
                typ
            } else if spec.capital_types
                && word
                    .chars()
                    .next()
                    .map(|first| first.is_uppercase())
                    .unwrap_or(false)
            {
                typ
            } else {
                plain
            };
            push(&mut pieces, style, &chars[index..end]);
            index = end;
            continue;
        }

        // Everything else (punctuation, whitespace) is plain code.
        push(&mut pieces, plain, &chars[index..index + 1]);
        index += 1;
    }

    if pieces.is_empty() {
        return vec![Span::styled(String::new(), plain)];
    }

    pieces
        .into_iter()
        .map(|(style, text)| Span::styled(text, style))
        .collect()
}

// ---------------------------------------------------------------------------
// Language definitions
// ---------------------------------------------------------------------------

struct Spec {
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    line_comments: &'static [&'static str],
    block_comments: bool,
    quotes: &'static [char],
    triple_quotes: bool,
    raw_strings: bool,
    capital_types: bool,
    /// Rust-style lifetimes: a single quote only starts a string when a
    /// matching closing quote exists on the same line.
    lifetime_quote: bool,
    /// C/Go-style rune literals: an unclosed single quote is punctuation.
    char_literal_quotes: bool,
}

fn normalize_language(language: &str) -> String {
    let token = language
        .trim()
        .split(|c: char| c.is_whitespace() || c == ',')
        .next()
        .unwrap_or("")
        .trim_matches(|c| c == '{' || c == '}' || c == '.')
        .to_ascii_lowercase();
    match token.as_str() {
        "rs" => "rust".into(),
        "py" => "python".into(),
        "js" | "jsx" | "mjs" | "cjs" => "javascript".into(),
        "ts" | "tsx" | "mts" | "cts" => "typescript".into(),
        "sh" | "bash" | "zsh" | "fish" | "shell" => "shell".into(),
        "c" | "h" => "c".into(),
        "golang" => "go".into(),
        "toml" => "toml".into(),
        "json" | "jsonc" | "json5" => "json".into(),
        other => other.to_string(),
    }
}

fn spec_for(language: &str) -> Option<Spec> {
    let spec = match language {
        "rust" => Spec {
            keywords: RUST_KEYWORDS,
            types: RUST_TYPES,
            line_comments: &["//"],
            block_comments: true,
            quotes: &['"', '\''],
            triple_quotes: false,
            raw_strings: true,
            capital_types: true,
            lifetime_quote: true,
            char_literal_quotes: false,
        },
        "python" => Spec {
            keywords: PYTHON_KEYWORDS,
            types: PYTHON_TYPES,
            line_comments: &["#"],
            block_comments: false,
            quotes: &['"', '\''],
            triple_quotes: true,
            raw_strings: false,
            capital_types: true,
            lifetime_quote: false,
            char_literal_quotes: false,
        },
        "javascript" | "typescript" => Spec {
            keywords: JS_KEYWORDS,
            types: JS_TYPES,
            line_comments: &["//"],
            block_comments: true,
            quotes: &['"', '\'', '`'],
            triple_quotes: false,
            raw_strings: false,
            capital_types: true,
            lifetime_quote: false,
            char_literal_quotes: false,
        },
        "shell" => Spec {
            keywords: SHELL_KEYWORDS,
            types: &[],
            line_comments: &["#"],
            block_comments: false,
            quotes: &['"', '\''],
            triple_quotes: false,
            raw_strings: false,
            capital_types: false,
            lifetime_quote: false,
            char_literal_quotes: false,
        },
        "json" => Spec {
            keywords: JSON_KEYWORDS,
            types: &[],
            line_comments: &[],
            block_comments: false,
            quotes: &['"'],
            triple_quotes: false,
            raw_strings: false,
            capital_types: false,
            lifetime_quote: false,
            char_literal_quotes: false,
        },
        "toml" => Spec {
            keywords: TOML_KEYWORDS,
            types: &[],
            line_comments: &["#"],
            block_comments: false,
            quotes: &['"', '\''],
            triple_quotes: false,
            raw_strings: false,
            capital_types: false,
            lifetime_quote: false,
            char_literal_quotes: false,
        },
        "c" => Spec {
            keywords: C_KEYWORDS,
            types: C_TYPES,
            line_comments: &["//"],
            block_comments: true,
            quotes: &['"', '\''],
            triple_quotes: false,
            raw_strings: false,
            capital_types: true,
            lifetime_quote: false,
            char_literal_quotes: true,
        },
        "go" => Spec {
            keywords: GO_KEYWORDS,
            types: GO_TYPES,
            line_comments: &["//"],
            block_comments: true,
            quotes: &['"', '\'', '`'],
            triple_quotes: false,
            raw_strings: false,
            capital_types: true,
            lifetime_quote: false,
            char_literal_quotes: true,
        },
        _ => return None,
    };
    Some(spec)
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
    "where", "while", "yield",
];

const RUST_TYPES: &[&str] = &[
    "Self", "String", "Vec", "Option", "Result", "Box", "Rc", "Arc", "Cell", "RefCell", "Cow",
    "HashMap", "BTreeMap", "HashSet", "BTreeSet", "u8", "u16", "u32", "u64", "u128", "usize", "i8",
    "i16", "i32", "i64", "i128", "isize", "f32", "f64", "bool", "char", "str",
];

const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "case", "class", "continue", "def", "del",
    "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in",
    "is", "lambda", "match", "None", "nonlocal", "not", "or", "pass", "raise", "return", "self",
    "True", "try", "while", "with", "yield",
];

const PYTHON_TYPES: &[&str] = &[
    "bool",
    "bytearray",
    "bytes",
    "cls",
    "complex",
    "dict",
    "Exception",
    "float",
    "frozenset",
    "int",
    "list",
    "memoryview",
    "object",
    "range",
    "set",
    "str",
    "tuple",
    "type",
];

const JS_KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "declare",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "get",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "of",
    "private",
    "protected",
    "public",
    "readonly",
    "return",
    "set",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

const JS_TYPES: &[&str] = &[
    "any", "Array", "bigint", "boolean", "Boolean", "Date", "Error", "Map", "never", "number",
    "Number", "Object", "Promise", "RegExp", "Set", "string", "String", "symbol", "Symbol",
    "unknown", "WeakMap", "WeakSet",
];

const SHELL_KEYWORDS: &[&str] = &[
    "break", "case", "continue", "coproc", "do", "done", "elif", "else", "esac", "exit", "export",
    "fi", "for", "function", "if", "in", "local", "readonly", "return", "select", "then", "time",
    "until", "while",
];

const JSON_KEYWORDS: &[&str] = &["true", "false", "null"];

const TOML_KEYWORDS: &[&str] = &["true", "false"];

const C_KEYWORDS: &[&str] = &[
    "_Atomic", "_Bool", "_Complex", "auto", "break", "case", "const", "continue", "default", "do",
    "else", "enum", "extern", "for", "goto", "if", "inline", "register", "restrict", "return",
    "sizeof", "static", "struct", "switch", "typedef", "union", "volatile", "while",
];

const C_TYPES: &[&str] = &[
    "char",
    "double",
    "float",
    "int",
    "int8_t",
    "int16_t",
    "int32_t",
    "int64_t",
    "long",
    "NULL",
    "ptrdiff_t",
    "short",
    "signed",
    "size_t",
    "ssize_t",
    "uint8_t",
    "uint16_t",
    "uint32_t",
    "uint64_t",
    "unsigned",
    "void",
];

const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "false",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "iota",
    "map",
    "nil",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "true",
    "type",
    "var",
];

const GO_TYPES: &[&str] = &[
    "any",
    "bool",
    "byte",
    "complex64",
    "complex128",
    "error",
    "float32",
    "float64",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "rune",
    "string",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
];

// ---------------------------------------------------------------------------
// Scanner helpers
// ---------------------------------------------------------------------------

fn push(pieces: &mut Vec<(Style, String)>, style: Style, chars: &[char]) {
    if chars.is_empty() {
        return;
    }
    let text: String = chars.iter().collect();
    if let Some((last_style, last_text)) = pieces.last_mut() {
        if *last_style == style {
            last_text.push_str(&text);
            return;
        }
    }
    pieces.push((style, text));
}

fn at(chars: &[char], index: usize, pattern: &str) -> bool {
    let mut position = index;
    for expected in pattern.chars() {
        if position >= chars.len() || chars[position] != expected {
            return false;
        }
        position += 1;
    }
    true
}

fn is_string_start(spec: &Spec, chars: &[char], index: usize, quote: char) -> bool {
    if quote == '\'' && spec.lifetime_quote {
        return is_rust_char_literal(chars, index);
    }
    // C and Go char literals must be closed; otherwise the apostrophe is
    // punctuation (for example in a lifetime-like identifier).
    if quote == '\'' && spec.char_literal_quotes {
        return has_closing_quote(chars, index + 1, quote);
    }
    true
}

/// A Rust `'x'` / `'\n'` / `'\u{1F600}'` character literal, as opposed to
/// a lifetime like `'a`.
fn is_rust_char_literal(chars: &[char], start: usize) -> bool {
    let mut index = start + 1;
    if index >= chars.len() {
        return false;
    }
    if chars[index] == '\\' {
        index += 1;
        if index >= chars.len() {
            return false;
        }
        if chars[index] == 'u' && chars.get(index + 1) == Some(&'{') {
            index += 2;
            while index < chars.len() && chars[index] != '}' {
                index += 1;
            }
            if index < chars.len() {
                index += 1;
            }
        } else if chars[index] == 'x' {
            index += 3;
        } else {
            index += 1;
        }
    } else {
        index += 1;
    }
    chars.get(index) == Some(&'\'')
}

fn has_closing_quote(chars: &[char], start: usize, quote: char) -> bool {
    let mut index = start;
    while index < chars.len() {
        if chars[index] == '\\' {
            index += 2;
            continue;
        }
        if chars[index] == quote {
            return true;
        }
        index += 1;
    }
    false
}

fn scan_quoted(chars: &[char], start: usize, quote: char, triple: bool) -> usize {
    let len = chars.len();
    if triple && start + 2 < len && chars[start + 1] == quote && chars[start + 2] == quote {
        let mut index = start + 3;
        while index + 2 < len {
            if chars[index] == quote && chars[index + 1] == quote && chars[index + 2] == quote {
                return index + 3;
            }
            index += 1;
        }
        return len;
    }
    let mut index = start + 1;
    while index < len {
        if chars[index] == '\\' {
            index += 2;
            continue;
        }
        if chars[index] == quote {
            return index + 1;
        }
        index += 1;
    }
    len
}

fn scan_rust_raw(chars: &[char], start: usize) -> Option<usize> {
    let mut index = start;
    // Optional b prefix before r.
    if chars.get(index) == Some(&'b') {
        index += 1;
    }
    if chars.get(index) != Some(&'r') {
        return None;
    }
    index += 1;
    let mut hashes = 0usize;
    while chars.get(index) == Some(&'#') {
        hashes += 1;
        index += 1;
    }
    if chars.get(index) != Some(&'"') {
        return None;
    }
    index += 1;
    while index < chars.len() {
        if chars[index] == '"' {
            let mut closing = index + 1;
            let mut matched = 0usize;
            while matched < hashes && chars.get(closing) == Some(&'#') {
                matched += 1;
                closing += 1;
            }
            if matched == hashes {
                return Some(closing);
            }
        }
        index += 1;
    }
    Some(chars.len())
}

fn scan_number(chars: &[char], start: usize) -> usize {
    let len = chars.len();
    let mut index = start;
    if chars[index] == '0'
        && index + 1 < len
        && matches!(chars[index + 1], 'x' | 'X' | 'b' | 'B' | 'o' | 'O')
    {
        index += 2;
        while index < len && (chars[index].is_ascii_hexdigit() || chars[index] == '_') {
            index += 1;
        }
        return index;
    }
    while index < len && (chars[index].is_ascii_digit() || chars[index] == '_') {
        index += 1;
    }
    if index < len && chars[index] == '.' {
        let next_is_digit = chars
            .get(index + 1)
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false);
        if next_is_digit {
            index += 1;
            while index < len && (chars[index].is_ascii_digit() || chars[index] == '_') {
                index += 1;
            }
        }
    }
    if index < len && matches!(chars[index], 'e' | 'E') {
        let mut exponent = index + 1;
        if exponent < len && matches!(chars[exponent], '+' | '-') {
            exponent += 1;
        }
        if exponent < len && chars[exponent].is_ascii_digit() {
            index = exponent;
            while index < len && chars[index].is_ascii_digit() {
                index += 1;
            }
        }
    }
    // Numeric suffixes: 1u32, 1.0f64, 10n.
    while index < len && (chars[index].is_alphanumeric() || chars[index] == '_') {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::bundled("omarchy")
    }

    fn joined(spans: &[Span<'_>]) -> String {
        spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn span_with<'a>(spans: &'a [Span<'static>], text: &str) -> Option<&'a Span<'static>> {
        spans.iter().find(|span| span.content.as_ref() == text)
    }

    #[test]
    fn unknown_language_is_one_plain_span() {
        let theme = theme();
        let line = "some << weird >> text";
        let spans = highlight_line(line, "brainfuck", &theme);
        assert_eq!(spans.len(), 1);
        assert_eq!(joined(&spans), line);
        assert_eq!(spans[0].style, theme.style(Role::Code));
    }

    #[test]
    fn preserves_text_for_all_supported_languages() {
        let theme = theme();
        let line = r#"let x: Vec<u8> = "hi"; // comment 42"#;
        for language in [
            "rust",
            "python",
            "javascript",
            "typescript",
            "shell",
            "json",
            "toml",
            "c",
            "go",
        ] {
            let spans = highlight_line(line, language, &theme);
            assert_eq!(joined(&spans), line, "language {language}");
        }
    }

    #[test]
    fn rust_keywords_strings_comments_numbers() {
        let theme = theme();
        let line = "let x: u8 = 42; // note";
        let spans = highlight_line(line, "rust", &theme);
        assert_eq!(
            span_with(&spans, "let").unwrap().style,
            theme.style(Role::Accent)
        );
        assert_eq!(
            span_with(&spans, "u8").unwrap().style,
            theme.style(Role::Tool)
        );
        assert_eq!(
            span_with(&spans, "42").unwrap().style,
            theme.style(Role::Warning)
        );
        assert!(
            spans
                .iter()
                .any(|span| span.content.contains("// note")
                    && span.style == theme.style(Role::Muted))
        );

        let spans = highlight_line(r#"let s = "hello";"#, "rust", &theme);
        assert_eq!(
            span_with(&spans, "\"hello\"").unwrap().style,
            theme.style(Role::Success)
        );
    }

    #[test]
    fn rust_lifetime_is_not_a_string() {
        let theme = theme();
        let spans = highlight_line("fn f<'a>(x: &'a str) {}", "rust", &theme);
        assert_eq!(joined(&spans), "fn f<'a>(x: &'a str) {}");
        // The line should not end covered by a Success string span.
        assert!(
            !spans
                .iter()
                .any(|span| span.style == theme.style(Role::Success))
        );
    }

    #[test]
    fn unterminated_strings_are_strings() {
        let theme = theme();
        let line = "x = \"unterminated";
        let spans = highlight_line(line, "python", &theme);
        assert_eq!(joined(&spans), line);
        assert!(
            spans
                .iter()
                .any(|span| span.style == theme.style(Role::Success))
        );
    }

    #[test]
    fn shell_comment_wins_at_hash() {
        let theme = theme();
        let spans = highlight_line("echo hi # comment", "bash", &theme);
        assert_eq!(joined(&spans), "echo hi # comment");
        assert!(
            spans
                .iter()
                .any(|span| span.content.contains("comment")
                    && span.style == theme.style(Role::Muted))
        );
    }

    #[test]
    fn json_keywords_and_numbers() {
        let theme = theme();
        let spans = highlight_line(r#"{"n": true, "v": null, "x": 12}"#, "json", &theme);
        assert_eq!(
            span_with(&spans, "true").unwrap().style,
            theme.style(Role::Accent)
        );
        assert_eq!(
            span_with(&spans, "null").unwrap().style,
            theme.style(Role::Accent)
        );
        assert_eq!(
            span_with(&spans, "12").unwrap().style,
            theme.style(Role::Warning)
        );
    }

    #[test]
    fn empty_line_does_not_panic() {
        let theme = theme();
        for language in ["rust", "python", "unknown"] {
            let spans = highlight_line("", language, &theme);
            assert_eq!(joined(&spans), "");
        }
    }
}
