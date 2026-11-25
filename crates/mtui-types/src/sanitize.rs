//! Rendering text mtui did not author safe to print on a terminal.

/// Appended where [`sanitize_external`] cut its input, so a bounded row cannot
/// be mistaken for the whole of what the server sent.
pub const TRUNCATION_MARK: &str = "…[truncated]";

/// Renders externally-sourced `text` safe to print, optionally cut to `limit`
/// **kept** characters.
///
/// Text mtui did not author (a host command's stdout, a checker's output) could
/// otherwise repaint the screen, forge surrounding lines, or trigger a terminal
/// feature such as an OSC 52 clipboard write. Everything a terminal would *act*
/// on is dropped: `ESC`-introduced sequences (CSI, OSC/DCS/SOS/PM/APC and their
/// terminators, and the two-character forms), the 8-bit C1 introducers (U+009B
/// is CSI and U+009D is OSC, invisible to an `ESC`-only filter), every other
/// control character, and the bidi overrides/isolates.
///
/// `\n` and `\t` survive; `\r` does not, because alone it lets a line overwrite
/// the prefix already printed before it.
///
/// `limit` counts characters **after** filtering, so a body made of escapes
/// cannot spend the budget; the cut is on a `char` and is marked with
/// [`TRUNCATION_MARK`]. `None` is unbounded.
#[must_use]
pub fn sanitize_external(text: &str, limit: Option<usize>) -> String {
    let limit = limit.unwrap_or(usize::MAX);
    let mut out = String::with_capacity(text.len());
    let mut kept = 0usize;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        let keep = match ch {
            '\u{1b}' => {
                skip_escape(&mut chars);
                false
            }
            '\u{9b}' => {
                skip_csi(&mut chars);
                false
            }
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => {
                skip_control_string(&mut chars);
                false
            }
            '\n' | '\t' => true,
            c => !(c.is_control() || is_bidi_control(c)),
        };
        if keep {
            if kept == limit {
                out.push_str(TRUNCATION_MARK);
                break;
            }
            out.push(ch);
            kept += 1;
        }
    }
    out
}

/// The bidi overrides and isolates (U+202A–U+202E, U+2066–U+2069): not terminal
/// control, but the other way external text forges the line around it.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Consumes the rest of an `ESC`-introduced sequence, the `ESC` already taken.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars>) {
    match chars.next() {
        Some('[') => skip_csi(chars),
        Some(']' | 'P' | 'X' | '^' | '_') => skip_control_string(chars),
        // `ESC <intermediates> <final>` (`ESC ( B`); a lone final was just taken.
        Some(c) if is_intermediate(c) => {
            while chars.peek().is_some_and(|c| is_intermediate(*c)) {
                chars.next();
            }
            chars.next();
        }
        _ => {}
    }
}

/// Consumes a CSI's parameter/intermediate bytes and its final byte.
///
/// A missing final byte leaves the tail in place rather than swallowing text:
/// without the introducer it is inert.
fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while chars
        .peek()
        .is_some_and(|c| ('\u{20}'..='\u{3f}').contains(c))
    {
        chars.next();
    }
    if chars
        .peek()
        .is_some_and(|c| ('\u{40}'..='\u{7e}').contains(c))
    {
        chars.next();
    }
}

/// Consumes an OSC/DCS/SOS/PM/APC payload up to `BEL`, `ST` or end of input.
///
/// An unterminated one eats the remainder, exactly as the terminal would — this
/// is what keeps an OSC 52 clipboard write from reaching it.
fn skip_control_string(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(c) = chars.next() {
        match c {
            '\u{7}' | '\u{9c}' => return,
            '\u{1b}' => {
                if chars.peek() == Some(&'\\') {
                    chars.next();
                }
                return;
            }
            _ => {}
        }
    }
}

/// An ECMA-48 intermediate byte (0x20–0x2F).
fn is_intermediate(c: char) -> bool {
    ('\u{20}'..='\u{2f}').contains(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand for the unbounded filter.
    fn clean(text: &str) -> String {
        sanitize_external(text, None)
    }

    #[test]
    fn sanitize_external_strips_color_and_control_sequences() {
        // A colored string round-trips to its bare text.
        assert_eq!(clean("\u{1b}[31merr\u{1b}[39m"), "err");
        assert_eq!(clean("a\u{1b}[2Ab"), "ab");
        assert_eq!(clean("a\u{1b}[Kb"), "ab");
        assert_eq!(clean("plain"), "plain");
        // `ESC b` is a two-character escape: the terminal would eat the `b` too,
        // so dropping only the `ESC` (what the regex filter did) left text the
        // user would never have seen.
        assert_eq!(clean("a\u{1b}b"), "a");
    }

    /// The sequences a filter keyed on `ESC` alone lets through: the 8-bit C1
    /// introducers, and every stray control character.
    #[test]
    fn sanitize_external_strips_c1_and_stray_controls() {
        assert_eq!(clean("a\u{9b}2Jb"), "ab");
        assert_eq!(clean("a\u{9d}52;c;cGF5bG9hZA==\u{9c}b"), "ab");
        assert_eq!(clean("a\u{1b}]52;c;cGF5bG9hZA==\u{7}b"), "ab");
        // CR/BEL/BS/VT/FF/DEL/NUL: overwrite, ring, or fake a new row.
        assert_eq!(clean("a\r\u{7}\u{8}\u{b}\u{c}\u{7f}\0b"), "ab");
        // Bidi override — reorders the rendered line without any control byte.
        assert_eq!(clean("a\u{202e}b\u{2069}"), "ab");
    }

    #[test]
    fn sanitize_external_keeps_ordinary_whitespace() {
        assert_eq!(clean("a\tb  c\nd"), "a\tb  c\nd");
        assert_eq!(
            clean("  leading and trailing  "),
            "  leading and trailing  "
        );
    }

    #[test]
    fn sanitize_external_cuts_on_a_char_boundary_and_marks_it() {
        assert_eq!(clean("abcdef"), "abcdef");
        assert_eq!(
            sanitize_external("abcdef", Some(3)),
            format!("abc{TRUNCATION_MARK}")
        );
        // Exactly at the limit is not truncated.
        assert_eq!(sanitize_external("abc", Some(3)), "abc");
        // Multibyte: a byte-wise cut at 3 would slice mid-`é` and panic.
        assert_eq!(
            sanitize_external("ééééé", Some(3)),
            format!("ééé{TRUNCATION_MARK}")
        );
        // Filtered characters do not spend the budget, so an escape flood
        // cannot squeeze the visible text out of a bounded row.
        assert_eq!(sanitize_external("\u{1b}[31m\u{1b}[31mab", Some(2)), "ab");
    }
}
