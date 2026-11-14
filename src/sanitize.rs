//! Make program output and model text safe to paint or to read.
//!
//! Ratatui assumes every cell it writes advances the terminal cursor by the
//! width `unicode-width` reports. Text that breaks that assumption leaves the
//! screen out of step with ratatui's back buffer, and the cells it then skips
//! as "unchanged" keep stale characters until something else overwrites them.
//! Model replies and tool output routinely contain such text:
//!
//! - ANSI escape sequences (colored compiler or test output): ratatui drops
//!   the ESC byte and prints the rest (`[31m`) as garbage.
//! - Tabs: dropped as control characters, which loses indentation.
//! - Carriage returns from progress bars (`10%\r20%\r30%`).
//! - Emoji presentation selectors (U+FE0F), zero-width joiners, and skin-tone
//!   modifiers: terminals and multiplexers (tmux in particular) disagree with
//!   `unicode-width` about how wide the resulting cluster is.
//! - Bidirectional controls, which can reorder the rest of the line.
//!
//! [`display`] rewrites these into plain, predictable text and borrows the
//! input untouched when there is nothing to change, so it is cheap to call on
//! every string that reaches the screen.
//!
//! [`terminal_output`] applies the same escape and carriage-return handling to
//! command output a tool hands the model, where color codes and redrawn
//! progress lines are noise that costs tokens.

use std::borrow::Cow;

use unicode_width::UnicodeWidthChar;

/// Columns a tab advances to (the next multiple of this).
pub const TAB_WIDTH: usize = 4;

/// Whether `ch` needs rewriting before it can be painted.
fn is_unsafe(ch: char) -> bool {
    ch != '\n' && (ch.is_control() || is_invisible(ch))
}

/// Format and presentation characters that are dropped for display.
fn is_invisible(ch: char) -> bool {
    matches!(
        ch,
        '\u{00AD}'                      // soft hyphen
            | '\u{200B}'                // zero-width space
            | '\u{200D}'                // zero-width joiner
            | '\u{200E}'..='\u{200F}'   // LRM, RLM
            | '\u{202A}'..='\u{202E}'   // bidi embeddings and overrides
            | '\u{2060}'..='\u{2064}'   // word joiner, invisible operators
            | '\u{2066}'..='\u{2069}'   // bidi isolates
            | '\u{FE00}'..='\u{FE0F}'   // variation selectors (incl. emoji VS16)
            | '\u{FEFF}'                // byte-order mark / ZWNBSP
            | '\u{1F3FB}'..='\u{1F3FF}' // emoji skin-tone modifiers
            | '\u{E0000}'..='\u{E007F}' // tag characters
    ) || ch == '\u{2028}'
        || ch == '\u{2029}'
}

/// Whether [`display`] would change `text`.
pub fn needs_cleaning(text: &str) -> bool {
    text.chars().any(is_unsafe)
}

/// Text that paints exactly as ratatui measures it. See the module docs.
pub fn display(text: &str) -> Cow<'_, str> {
    clean(text, true, true)
}

/// Pasted text for the prompt editor: like [`display`], but tabs are kept
/// (the editor expands them itself, and they may matter to the model).
pub fn paste(text: &str) -> Cow<'_, str> {
    clean(text, false, true)
}

/// Command output for the model: escape sequences removed, carriage-return
/// redraws resolved to their final text, other control characters dropped.
/// Tabs and all printable text, emoji included, are kept as they are.
pub fn terminal_output(text: &str) -> Cow<'_, str> {
    clean(text, false, false)
}

fn clean(text: &str, expand_tabs: bool, drop_invisible: bool) -> Cow<'_, str> {
    let drop = |ch: char| match ch {
        '\n' => false,
        '\t' => expand_tabs,
        ch if ch.is_control() => true,
        ch => drop_invisible && is_invisible(ch),
    };
    if !text.chars().any(drop) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    // Display column within the current output line, for tab stops.
    let mut column = 0usize;
    // Where the current output line starts, for carriage returns.
    let mut line_start = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' => {
                out.push('\n');
                column = 0;
                line_start = out.len();
            }
            '\u{2028}' | '\u{2029}' if drop_invisible => {
                out.push('\n');
                column = 0;
                line_start = out.len();
            }
            '\t' if !expand_tabs => {
                out.push('\t');
                column += TAB_WIDTH - column % TAB_WIDTH;
            }
            '\t' => {
                let spaces = TAB_WIDTH - column % TAB_WIDTH;
                out.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            }
            '\r' => {
                // CRLF is a newline; a CR followed by more text rewinds the
                // line, so only what was written after it stays (progress
                // bars). A CR that ends the line or the text changes nothing.
                if !matches!(chars.peek(), None | Some('\n' | '\r')) {
                    out.truncate(line_start);
                    column = 0;
                }
            }
            '\u{1b}' => skip_escape(&mut chars),
            '\u{9b}' => skip_csi(&mut chars),
            '\u{9d}' => skip_string(&mut chars),
            ch if drop(ch) => {}
            ch => {
                out.push(ch);
                column += UnicodeWidthChar::width(ch).unwrap_or(0);
            }
        }
    }
    Cow::Owned(out)
}

/// Skip the rest of an escape sequence whose ESC was just consumed.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            skip_csi(chars);
        }
        // OSC, DCS, SOS, PM, APC: a string terminated by BEL or ST.
        Some(']' | 'P' | 'X' | '^' | '_') => {
            chars.next();
            skip_string(chars);
        }
        // Two-character sequences (charset selection takes one more).
        Some('(' | ')' | '*' | '+' | '#' | '%') => {
            chars.next();
            chars.next();
        }
        Some(c) if ('\u{40}'..='\u{7e}').contains(&c) || c == '=' || c == '>' => {
            chars.next();
        }
        _ => {}
    }
}

/// Skip CSI parameters through the final byte.
fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for c in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&c) {
            break;
        }
        // A malformed sequence ends at the line; never swallow text past it.
        if c == '\n' {
            break;
        }
    }
}

/// Skip a control string through BEL or ST (ESC \).
fn skip_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(c) = chars.next() {
        match c {
            '\u{7}' | '\u{9c}' | '\n' => break,
            '\u{1b}' => {
                if chars.peek() == Some(&'\\') {
                    chars.next();
                }
                break;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_borrowed() {
        assert!(matches!(display("hello\nworld — ✓ 日本"), Cow::Borrowed(_)));
    }

    #[test]
    fn ansi_sequences_are_removed() {
        assert_eq!(display("\u{1b}[1;31merror\u{1b}[0m: x"), "error: x");
        assert_eq!(
            display("\u{1b}]8;;https://example.com\u{7}link\u{1b}]8;;\u{1b}\\ done"),
            "link done"
        );
        assert_eq!(display("a\u{1b}(Bb\u{1b}=c"), "abc");
    }

    #[test]
    fn tabs_expand_to_tab_stops() {
        assert_eq!(display("\tx"), "    x");
        assert_eq!(display("ab\tc"), "ab  c");
        assert_eq!(display("a\n\tb"), "a\n    b");
        assert_eq!(display("日\tx"), "日  x");
    }

    #[test]
    fn carriage_returns_keep_the_last_overwrite() {
        assert_eq!(display("line\r\nnext"), "line\nnext");
        assert_eq!(display("start\n10%\r20%\r30%"), "start\n30%");
        assert_eq!(display("done 100%\r"), "done 100%");
        assert_eq!(display("a\r\r\nb"), "a\nb");
    }

    #[test]
    fn width_ambiguous_format_characters_are_dropped() {
        assert_eq!(display("⚠\u{fe0f} warn"), "⚠ warn");
        assert_eq!(display("👨\u{200d}💻"), "👨💻");
        assert_eq!(display("👍\u{1f3fd}"), "👍");
        assert_eq!(display("\u{feff}text\u{200b}"), "text");
        assert_eq!(display("a\u{202e}b"), "ab");
    }

    #[test]
    fn paste_keeps_tabs_but_normalizes_newlines() {
        assert!(matches!(paste("\tfn x() {}"), Cow::Borrowed(_)));
        assert_eq!(paste("a\r\n\tb\u{1b}[0m"), "a\n\tb");
    }

    #[test]
    fn terminal_output_keeps_text_and_drops_noise() {
        assert!(matches!(
            terminal_output("\tok ✅ ⚠\u{fe0f}\n"),
            Cow::Borrowed(_)
        ));
        assert_eq!(
            terminal_output("\u{1b}[32mok\u{1b}[0m\r\nfetch 10%\rfetch 100%\n\u{7}"),
            "ok\nfetch 100%\n"
        );
    }

    #[test]
    fn other_controls_are_dropped() {
        assert_eq!(display("a\u{7}b\u{8}c\u{7f}d"), "abcd");
    }
}
