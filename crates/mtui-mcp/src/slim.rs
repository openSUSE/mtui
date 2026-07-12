//! Token-slimming helpers for the MCP wire surface.
//!
//! Currently this is just [`cap_output`], the per-tool-result size bound. The
//! schema-slimming pass (P7.9) lands its transforms here too, co-located exactly
//! as upstream `mtui/mcp/_slim.py` groups `cap_output` with the schema slimmer.

/// Truncate `text` to at most `limit` bytes (UTF-8), appending a notice.
///
/// A single tool result — a `run` over many hosts, a multi-thousand-line install
/// log — can dwarf the rest of the client's context. When the UTF-8 length of
/// `text` exceeds `limit` the **tail** is dropped (the head usually carries the
/// command echo and the first, most diagnostic output) and a one-line
/// `…[truncated N bytes; …]` notice is appended pointing at the paged readers.
///
/// `limit == 0` disables the cap and returns `text` unchanged. Under-cap text is
/// returned byte-identical. The cut is made on a `char` boundary so the result
/// is always valid UTF-8 even when the byte cut would split a codepoint.
///
/// Mirrors upstream `mtui.mcp._slim.cap_output`: the reported dropped-byte count
/// is `total − limit` (the budget overrun), independent of the small extra bytes
/// a codepoint-boundary trim may shed.
#[must_use]
pub fn cap_output(text: String, limit: usize) -> String {
    if limit == 0 {
        return text;
    }
    let total = text.len();
    if total <= limit {
        return text;
    }
    // Largest char boundary at or below `limit` (upstream decodes `[:limit]`
    // with errors="ignore", which likewise drops a split trailing codepoint).
    let cut = (0..=limit)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    let dropped = total - limit;
    let mut head = text;
    head.truncate(cut);
    head.push_str(&format!(
        "\n…[truncated {dropped} bytes; output exceeded the \
         [mcp] max_output_bytes={limit} budget — use a narrower command, or \
         the offset/limit paging on testreport reads]"
    ));
    head
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_limit_disables_the_cap() {
        let text = "x".repeat(1000);
        assert_eq!(cap_output(text.clone(), 0), text);
    }

    #[test]
    fn under_cap_is_byte_identical() {
        let text = "hello world".to_owned();
        assert_eq!(cap_output(text.clone(), 100), text);
    }

    #[test]
    fn at_cap_is_unchanged() {
        // len == limit is not "exceeded"; returned as-is.
        let text = "abcde".to_owned();
        assert_eq!(cap_output(text.clone(), 5), text);
    }

    #[test]
    fn over_cap_keeps_head_and_appends_notice() {
        let text = "abcdefghij".to_owned(); // 10 bytes
        let out = cap_output(text, 4);
        assert!(out.starts_with("abcd"), "head preserved: {out:?}");
        // Upstream reports the budget overrun: total(10) - limit(4) = 6.
        assert!(out.contains("truncated 6 bytes"), "notice count: {out:?}");
        assert!(out.contains("max_output_bytes=4"), "notice limit: {out:?}");
        // The dropped tail is gone.
        assert!(!out.contains("efghij"), "tail dropped: {out:?}");
    }

    #[test]
    fn cut_falls_on_a_char_boundary_for_multibyte_text() {
        // "€" is 3 bytes (E2 82 AC). A cut at limit=2 would split it; the head
        // must stop at the previous boundary (byte 0) rather than emit invalid
        // UTF-8. The whole String result must be valid UTF-8 (it is, by type).
        let text = "€€€".to_owned(); // 9 bytes
        let out = cap_output(text, 2);
        // Nothing before the first full codepoint fits, so the head is empty and
        // only the notice remains.
        assert!(out.starts_with('\n'), "head empty then notice: {out:?}");
        assert!(out.contains("truncated 7 bytes"), "9 - 2 = 7: {out:?}");
    }

    #[test]
    fn keeps_whole_codepoints_up_to_the_boundary() {
        // limit=4 with 3-byte codepoints: only the first "€" (bytes 0..3) fits.
        let text = "€€".to_owned(); // 6 bytes
        let out = cap_output(text, 4);
        assert!(out.starts_with('€'), "first codepoint kept: {out:?}");
        // Exactly one "€" then the notice — not one and a half.
        assert!(
            out.chars().filter(|&c| c == '€').count() == 1,
            "only whole codepoints kept: {out:?}"
        );
    }
}
