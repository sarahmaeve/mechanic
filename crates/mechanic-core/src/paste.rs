//! Clipboard-paste payload sanitization.
//!
//! Raw clipboard contents are attacker-controlled: a malicious web page,
//! manpage, package post-install hook, or compromised clipboard manager
//! can all arrange for arbitrary bytes to land on the user's clipboard.
//! When those bytes reach the shell unfiltered, they can become command
//! execution.
//!
//! The canonical attack is *bracketed-paste end-marker injection*.  The
//! terminal wraps a paste in `\x1b[200~ … \x1b[201~` so readline treats
//! it as one edit (one undo step, history-expansion disabled, etc.).
//! A clipboard payload containing `\x1b[201~` exits bracketed-paste
//! mode mid-stream; the remainder is then interpreted as typed
//! keystrokes — including any trailing `\r` that auto-executes.
//!
//! [`filter`] addresses this and two adjacent hazards:
//!
//! - Bracketed-paste markers are stripped unconditionally so they can
//!   never escape the outer wrap.
//! - `\r\n` and lone `\r` are normalized to `\n` so that pastes from
//!   Windows / classic-Mac tools behave consistently, and so that a
//!   stray `\r` in the middle of a payload cannot act as "press
//!   Enter" in a non-bracketed shell.
//! - When bracketed paste is **not** active the trailing newline is
//!   stripped, so the final line stays in the shell's input editor
//!   for review instead of auto-executing.  Embedded newlines are
//!   preserved for legitimate multi-line uses (heredocs, SQL, etc.).
//!
//! # Non-goals
//!
//! Embedded ESC sequences and non-newline C0 controls are deliberately
//! **not** stripped.  Users legitimately paste ANSI-colored output
//! (from a previous terminal session, a logfile, a code example in
//! documentation) and blanket stripping breaks those workflows.  A
//! stricter "prompt on multi-line paste" mode (iTerm2 style) is a
//! reasonable future addition, gated behind a config option.

/// Byte sequence DECSET 2004 uses to mark the start of a paste.
const BRACKETED_START: &str = "\x1b[200~";

/// Byte sequence DECSET 2004 uses to mark the end of a paste.
///
/// The critical marker — its presence in a user-supplied payload is
/// the attack.  `filter` removes every occurrence unconditionally.
const BRACKETED_END: &str = "\x1b[201~";

/// Sanitize `text` for delivery to the PTY as a paste payload.
///
/// `bracketed` should reflect the terminal's current `BRACKETED_PASTE`
/// mode (DECSET 2004).  The caller is expected to wrap the returned
/// string in `\x1b[200~ … \x1b[201~` when `bracketed` is `true`;
/// `filter` removes any embedded markers so that wrap cannot be
/// escaped from inside the payload.
///
/// When `bracketed` is `false`, a trailing newline is stripped so the
/// shell doesn't auto-execute the last pasted line.  Embedded
/// newlines are preserved either way.
pub fn filter(text: &str, bracketed: bool) -> String {
    // Step 1 — strip bracketed-paste markers.
    //
    // The end marker is the canonical injection vector (ending paste
    // mode early so the rest is consumed as keystrokes).  The start
    // marker has no clean attack but it's equally meaningless inside
    // a payload, so we strip both.  Order doesn't matter: neither
    // marker appears as a substring of the other.
    let mut out = if text.contains(BRACKETED_END) || text.contains(BRACKETED_START) {
        text.replace(BRACKETED_END, "").replace(BRACKETED_START, "")
    } else {
        text.to_owned()
    };

    // Step 2 — normalize line endings to LF.
    //
    // Paste sources disagree: macOS Cocoa apps use LF; Windows apps
    // via iCloud Universal Clipboard use CRLF; pre-OS-X classic-Mac
    // apps and some web sources use bare CR.  The shell is happiest
    // with one convention — LF.
    //
    // Security-adjacent: a bare `\r` in non-bracketed mode is "press
    // Enter".  Converting it to `\n` doesn't fully defuse that (a LF
    // is also "press Enter" in the middle of a non-bracketed payload),
    // but combined with Step 3's trailing-newline strip it does
    // neutralize the common `"cmd\r"` single-line variant.
    //
    // `\r\n` must be collapsed before lone `\r`; otherwise every
    // Windows line break would become `\n\n`.
    if out.contains("\r\n") {
        out = out.replace("\r\n", "\n");
    }
    if out.contains('\r') {
        out = out.replace('\r', "\n");
    }

    // Step 3 — strip trailing newline when bracketed paste is off.
    //
    // Without bracketed paste, every byte is consumed as a keystroke.
    // A trailing LF is therefore "press Enter" — auto-executing the
    // last line.  A user pasting a command almost always wants to
    // review it first; strip the trailing LF so the cursor lands at
    // end-of-input and the user decides when to execute.
    //
    // Only the *final* newline is removed.  Embedded newlines stay
    // so heredocs, multi-line commands, and raw text dumps are
    // still usable.
    //
    // When bracketed paste IS on, readline treats the whole payload
    // as one edit and the trailing newline is literal data (the
    // character being inserted), not a command terminator — so we
    // leave it alone.
    if !bracketed && out.ends_with('\n') {
        out.pop();
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Marker stripping ──────────────────────────────────────────────────────

    #[test]
    fn removes_end_marker_bracketed() {
        // The attack: end marker inside payload escapes bracketed mode.
        let payload = "innocuous\x1b[201~; rm -rf /";
        assert_eq!(filter(payload, true), "innocuous; rm -rf /");
    }

    #[test]
    fn removes_end_marker_non_bracketed() {
        // Even without bracketed mode on, we strip the marker — it has
        // no legitimate meaning inside paste text, and tolerating it
        // would let an attacker's payload pre-compose the marker so a
        // *future* bracketed paste (same session) could be hijacked.
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
        // `\x1b[200~\x1b[201~\x1b[201~\x1b[200~` — alternating and
        // adjacent markers all get stripped regardless of order.
        let payload = "pre\x1b[200~\x1b[201~mid\x1b[201~\x1b[200~end";
        assert_eq!(filter(payload, true), "premidend");
    }

    #[test]
    fn preserves_other_escape_sequences() {
        // ANSI SGR: a common legitimate paste — colored logs, code
        // examples from manuals.  We must not touch these.
        let colored = "\x1b[31mred\x1b[0m";
        assert_eq!(filter(colored, true), colored);
        assert_eq!(filter(colored, false), colored);
    }

    #[test]
    fn preserves_csi_sequences_that_merely_share_a_prefix() {
        // `\x1b[2~` (Insert key) shares the `\x1b[2` prefix with the
        // start marker but is a different sequence — must survive.
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

    // ── Line-ending normalization ─────────────────────────────────────────────

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
        // `"cmd\r"` in a non-bracketed shell without this normalization
        // would execute `cmd` immediately.
        assert_eq!(filter("foo\rbar", true), "foo\nbar");
        assert_eq!(filter("cmd\r", false), "cmd");
    }

    #[test]
    fn mixed_cr_crlf_and_lf() {
        // Windows text pasted through a legacy tool that also produced
        // a bare CR — the combo must still normalize cleanly.
        let payload = "a\r\nb\rc\nd";
        assert_eq!(filter(payload, true), "a\nb\nc\nd");
    }

    #[test]
    fn double_cr_becomes_double_lf() {
        // `"a\r\r\nb"` → replace CRLF → `"a\r\nb"` → replace CR → `"a\n\nb"`.
        // Two line breaks is the right semantic: a blank line.
        assert_eq!(filter("a\r\r\nb", true), "a\n\nb");
    }

    // ── Trailing-newline behaviour ────────────────────────────────────────────

    #[test]
    fn bracketed_preserves_trailing_lf() {
        // Readline handles the paste as one edit; the trailing LF is
        // literal data, not an execute command.
        assert_eq!(filter("echo hi\n", true), "echo hi\n");
    }

    #[test]
    fn non_bracketed_strips_trailing_lf() {
        // Without DECSET 2004 the shell reads bytes as keystrokes — a
        // trailing LF auto-executes the command before the user can
        // review it.
        assert_eq!(filter("echo hi\n", false), "echo hi");
    }

    #[test]
    fn non_bracketed_preserves_embedded_newlines() {
        // Multi-line pastes (heredocs, SQL) must keep internal LFs.
        // Only the *last* newline is stripped.
        assert_eq!(filter("line1\nline2\n", false), "line1\nline2");
    }

    #[test]
    fn non_bracketed_strips_only_single_trailing() {
        // A payload ending in two LFs loses just one — the blank
        // final line stays (it matters for here-docs ending with an
        // empty line).
        assert_eq!(filter("a\n\n", false), "a\n");
    }

    #[test]
    fn non_bracketed_lone_newline_becomes_empty() {
        assert_eq!(filter("\n", false), "");
    }

    #[test]
    fn non_bracketed_no_newline_no_change_to_tail() {
        // No trailing newline to strip — payload stays as-is.
        assert_eq!(filter("no newline", false), "no newline");
    }

    // ── Combinations ──────────────────────────────────────────────────────────

    #[test]
    fn marker_then_trailing_newline_non_bracketed() {
        // Strip marker first, then strip trailing LF.
        assert_eq!(filter("cmd\x1b[201~\n", false), "cmd");
    }

    #[test]
    fn crlf_and_marker_bracketed() {
        // Marker removed, CRLF normalized, trailing LF kept.
        assert_eq!(filter("cmd\x1b[201~\r\nextra\r\n", true), "cmd\nextra\n");
    }

    #[test]
    fn utf8_preserved() {
        // Both the `\r` → `\n` path and the `String::pop` path must
        // leave multi-byte codepoints intact.
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
        // Documented limitation: filtering is stateless per call.  If
        // an attacker somehow split `\x1b[201~` across two separate
        // paste invocations, neither half alone is a complete marker
        // and they pass through.  Mitigation would require stateful
        // scanning at the PTY-write boundary, which is out of scope —
        // there's no practical attack when the clipboard is the
        // source (the clipboard delivers one payload atomically).
        //
        // This test pins the current behaviour so any future change
        // is deliberate.
        assert_eq!(filter("\x1b[201", true), "\x1b[201");
        assert_eq!(filter("~rest", true), "~rest");
    }

    // ── Common legitimate pastes (regression guards) ──────────────────────────

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
        // Non-bracketed: no trailing newline to strip, so also unchanged.
        assert_eq!(filter(code, false), code);
    }
}
