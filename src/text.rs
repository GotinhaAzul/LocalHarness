//! Small text helpers shared by tools, the agent, and the TUI: clipping to a
//! length, flattening to one line, safe identifiers, and byte-bounded
//! excerpts. One definition each, so a fix lands everywhere.

/// `text` cut to at most `max_chars` characters, ending in `…` when cut.
pub fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

/// `text` with every run of whitespace (newlines included) collapsed to one
/// space, then [`clip`]ped.
pub fn one_line(text: &str, max_chars: usize) -> String {
    clip(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        max_chars,
    )
}

/// `text` as an identifier for file names and tool names: ASCII letters,
/// digits, `-`, and `_` are kept, anything else becomes `replacement`.
pub fn slug(text: &str, replacement: char) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                replacement
            }
        })
        .collect()
}

/// Take at most `max` bytes without splitting a UTF-8 code point. The flag
/// says whether anything was cut.
pub fn take_bytes(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// `text` within `head + tail` bytes: its start and its end, cut at line
/// boundaries where possible, with a marker for what was left out. Command
/// output keeps how it ended this way.
pub fn head_and_tail(text: &str, head: usize, tail: usize) -> String {
    if text.len() <= head + tail {
        return text.to_string();
    }
    let mut head_end = head;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    if let Some(newline) = text[..head_end].rfind('\n') {
        head_end = newline + 1;
    }
    let mut tail_start = text.len() - tail;
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if let Some(newline) = text[tail_start..].find('\n') {
        tail_start += newline + 1;
    }
    format!(
        "{}[... {} bytes of output omitted ...]\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_never_exceeds_the_limit() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("exactly", 7), "exactly");
        assert_eq!(clip("abcdefgh", 5), "abcd…");
        assert_eq!(clip("日本語のテキスト", 3), "日本…");
        assert_eq!(clip("abc", 0), "");
    }

    #[test]
    fn one_line_flattens_then_clips() {
        assert_eq!(one_line("a\n  b\tc  ", 20), "a b c");
        assert_eq!(one_line("one two three", 7), "one tw…");
    }

    #[test]
    fn slug_replaces_everything_unsafe() {
        assert_eq!(slug("my server/1.0", '_'), "my_server_1_0");
        assert_eq!(slug("ok-name_2", '-'), "ok-name_2");
    }

    #[test]
    fn take_bytes_respects_char_boundaries() {
        assert_eq!(take_bytes("héllo", 2), ("h".to_string(), true));
        assert_eq!(take_bytes("hi", 5), ("hi".to_string(), false));
    }

    #[test]
    fn head_and_tail_cuts_at_lines() {
        let text: String = (0..100).map(|n| format!("line {n}\n")).collect();
        assert_eq!(head_and_tail(&text, 1000, 1000), text);
        let cut = head_and_tail(&text, 30, 30);
        assert!(cut.starts_with("line 0\n"));
        assert!(cut.ends_with("line 99\n"));
        assert!(cut.contains("bytes of output omitted"));
        for line in cut.lines().filter(|line| !line.contains("omitted")) {
            assert!(line.starts_with("line "), "no partial lines: {line:?}");
        }
    }
}
