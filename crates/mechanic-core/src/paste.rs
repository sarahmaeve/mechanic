//! Clipboard payload filtering; this preserves embedded newlines and most control bytes.

/// Byte sequence DECSET 2004 uses to mark the start of a paste.
const BRACKETED_START: &str = "\x1b[200~";

/// Byte sequence DECSET 2004 uses to mark the end of a paste.
const BRACKETED_END: &str = "\x1b[201~";

/// Remove paste markers and normalize CRLF/CR to LF.
/// With bracketed mode off, remove exactly one final LF; preserve embedded newlines.
/// Other escapes and controls pass through. The caller adds bracketed markers when enabled.
pub fn filter(text: &str, bracketed: bool) -> String {
    let mut out = if text.contains(BRACKETED_END) || text.contains(BRACKETED_START) {
        text.replace(BRACKETED_END, "").replace(BRACKETED_START, "")
    } else {
        text.to_owned()
    };

    if out.contains("\r\n") {
        out = out.replace("\r\n", "\n");
    }
    if out.contains('\r') {
        out = out.replace('\r', "\n");
    }

    if !bracketed && out.ends_with('\n') {
        out.pop();
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_end_marker_bracketed() {
        let payload = "innocuous\x1b[201~; rm -rf /";
        assert_eq!(filter(payload, true), "innocuous; rm -rf /");
    }

    #[test]
    fn removes_end_marker_non_bracketed() {
        let payload = "innocuous\x1b[201~also-safe";
        assert_eq!(filter(payload, false), "innocuousalso-safe");
    }

    #[test]
    fn removes_start_marker() {
        let payload = "foo\x1b[200~bar";
        assert_eq!(filter(payload, true), "foobar");
        assert_eq!(filter(payload, false), "foobar");
    }

    #[test]
    fn removes_multiple_markers() {
        let payload = "\x1b[200~a\x1b[201~b\x1b[200~c\x1b[201~";
        assert_eq!(filter(payload, true), "abc");
    }

    #[test]
    fn removes_interleaved_markers() {
        let payload = "pre\x1b[200~\x1b[201~mid\x1b[201~\x1b[200~end";
        assert_eq!(filter(payload, true), "premidend");
    }

    #[test]
    fn preserves_other_escape_sequences() {
        let colored = "\x1b[31mred\x1b[0m";
        assert_eq!(filter(colored, true), colored);
        assert_eq!(filter(colored, false), colored);
    }

    #[test]
    fn preserves_csi_sequences_that_merely_share_a_prefix() {
        let payload = "\x1b[2~";
        assert_eq!(filter(payload, true), payload);
    }

    #[test]
    fn empty_payload_stays_empty() {
        assert_eq!(filter("", true), "");
        assert_eq!(filter("", false), "");
    }

    #[test]
    fn payload_of_only_markers_becomes_empty() {
        assert_eq!(filter("\x1b[200~\x1b[201~", true), "");
        assert_eq!(filter("\x1b[200~\x1b[201~", false), "");
    }

    #[test]
    fn crlf_becomes_lf_bracketed_keeps_trailing() {
        assert_eq!(filter("foo\r\nbar\r\n", true), "foo\nbar\n");
    }

    #[test]
    fn crlf_becomes_lf_non_bracketed_strips_trailing() {
        assert_eq!(filter("foo\r\nbar\r\n", false), "foo\nbar");
    }

    #[test]
    fn bare_cr_becomes_lf() {
        assert_eq!(filter("foo\rbar", true), "foo\nbar");
        assert_eq!(filter("cmd\r", false), "cmd");
    }

    #[test]
    fn mixed_cr_crlf_and_lf() {
        let payload = "a\r\nb\rc\nd";
        assert_eq!(filter(payload, true), "a\nb\nc\nd");
    }

    #[test]
    fn double_cr_becomes_double_lf() {
        assert_eq!(filter("a\r\r\nb", true), "a\n\nb");
    }

    #[test]
    fn bracketed_preserves_trailing_lf() {
        assert_eq!(filter("echo hi\n", true), "echo hi\n");
    }

    #[test]
    fn non_bracketed_strips_trailing_lf() {
        assert_eq!(filter("echo hi\n", false), "echo hi");
    }

    #[test]
    fn non_bracketed_preserves_embedded_newlines() {
        assert_eq!(filter("line1\nline2\n", false), "line1\nline2");
    }

    #[test]
    fn non_bracketed_strips_only_single_trailing() {
        assert_eq!(filter("a\n\n", false), "a\n");
    }

    #[test]
    fn non_bracketed_lone_newline_becomes_empty() {
        assert_eq!(filter("\n", false), "");
    }

    #[test]
    fn non_bracketed_no_newline_no_change_to_tail() {
        assert_eq!(filter("no newline", false), "no newline");
    }

    #[test]
    fn marker_then_trailing_newline_non_bracketed() {
        assert_eq!(filter("cmd\x1b[201~\n", false), "cmd");
    }

    #[test]
    fn crlf_and_marker_bracketed() {
        assert_eq!(filter("cmd\x1b[201~\r\nextra\r\n", true), "cmd\nextra\n");
    }

    #[test]
    fn utf8_preserved() {
        let payload = "héllo → wörld\n";
        assert_eq!(filter(payload, false), "héllo → wörld");
        assert_eq!(filter(payload, true), payload);
    }

    #[test]
    fn only_whitespace_non_bracketed() {
        assert_eq!(filter("   \n", false), "   ");
        assert_eq!(filter("\t\n", false), "\t");
    }

    #[test]
    fn marker_split_across_call_boundary_not_handled() {
        assert_eq!(filter("\x1b[201", true), "\x1b[201");
        assert_eq!(filter("~rest", true), "~rest");
    }

    #[test]
    fn single_word_paste_unchanged() {
        assert_eq!(filter("hello", true), "hello");
        assert_eq!(filter("hello", false), "hello");
    }

    #[test]
    fn url_paste_unchanged() {
        let url = "https://example.com/path?q=1&r=2";
        assert_eq!(filter(url, true), url);
        assert_eq!(filter(url, false), url);
    }

    #[test]
    fn code_snippet_paste_unchanged() {
        let code = "fn main() {\n    println!(\"hi\");\n}";
        assert_eq!(filter(code, true), code);
        assert_eq!(filter(code, false), code);
    }
}
