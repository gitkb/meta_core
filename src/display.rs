//! Escaping for untrusted text in human-readable output.
//!
//! Names, paths, tags and URLs come from `.meta` files, git and the filesystem.
//! Printed raw, a control character in one of them can move the cursor, clear
//! the screen, set the window title or the clipboard, or break a line-based
//! format such as the `meta context` table. Every human-readable printer
//! passes such values through [`escape`]; `--json` output needs nothing more,
//! since JSON already escapes them.

use std::borrow::Cow;
use std::fmt::Write;

/// True for characters that must never reach a terminal raw: C0 and C1
/// controls, DEL, and the Unicode bidirectional controls that reorder how a
/// line is displayed.
pub fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Return `s` with every unsafe character written as a visible escape
/// (`\n`, `\t`, `\r`, or `\u{1b}`). Borrows `s` unchanged when it is clean.
pub fn escape(s: &str) -> Cow<'_, str> {
    if !s.chars().any(is_unsafe_char) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if is_unsafe_char(c) => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// [`escape`] for multi-line text such as an error message: newlines stay,
/// every other unsafe character is escaped.
pub fn escape_lines(s: &str) -> Cow<'_, str> {
    if !s.chars().any(|c| c != '\n' && is_unsafe_char(c)) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.split('\n').map(escape).collect::<Vec<_>>().join("\n"))
}

/// [`escape`] for a path, which may also hold bytes that are not UTF-8.
pub fn escape_path(path: &std::path::Path) -> String {
    escape(&path.to_string_lossy()).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_is_borrowed() {
        assert!(matches!(escape("api/core-1.2 ✓"), Cow::Borrowed(_)));
    }

    #[test]
    fn terminal_sequences_are_made_visible() {
        assert_eq!(
            escape("x\u{1b}]0;PWNED\u{7}\u{1b}[2J"),
            "x\\u{1b}]0;PWNED\\u{7}\\u{1b}[2J"
        );
        assert_eq!(escape("n\nfake\r\t"), "n\\nfake\\r\\t");
        assert_eq!(escape("c1\u{9b}31m del\u{7f}"), "c1\\u{9b}31m del\\u{7f}");
        assert_eq!(escape("rtl\u{202e}txt"), "rtl\\u{202e}txt");
    }

    #[test]
    fn escape_lines_keeps_newlines_only() {
        assert!(matches!(escape_lines("a\nb"), Cow::Borrowed(_)));
        assert_eq!(escape_lines("a\u{1b}[2J\nb\r"), "a\\u{1b}[2J\nb\\r");
    }
}
