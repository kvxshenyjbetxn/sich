//! `SendText` payload building (pure).

use std::time::Duration;

use sich_proto::AgentKind;

/// Delay between the message and the separate Enter keypress.
pub const SUBMIT_DELAY: Duration = Duration::from_millis(30);

/// Gap between two consecutive image pastes.
pub const IMAGE_GAP: Duration = Duration::from_millis(150);
/// Wait after the last image paste: Claude Code registers the attachment asynchronously, so the
/// text and Enter must not follow immediately.
pub const IMAGE_SETTLE: Duration = Duration::from_millis(600);

/// One write to the PTY, made `delay_before` after the previous step.
#[derive(Debug, PartialEq, Eq)]
pub struct Step {
    pub bytes: Vec<u8>,
    pub delay_before: Duration,
}

/// Agents that treat a bracketed paste of a path as an attachment (same rule as the desktop's
/// image paste).
pub fn force_bracketed(agent: Option<AgentKind>) -> bool {
    matches!(agent, Some(AgentKind::Claude | AgentKind::Codex))
}

/// A path as typed into the terminal: quoted when it contains whitespace.
pub fn path_text(path: &str) -> String {
    if path.contains(char::is_whitespace) { format!("\"{path}\"") } else { path.to_string() }
}

/// Newlines become CR; bracketed paste strips ESC so the text cannot end the paste early.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let mut s = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        s.retain(|c| c != '\x1b');
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(s.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        s.into_bytes()
    }
}

/// Ordered PTY writes. Image paths first, then the text. Bracketed agents get one paste per part
/// (a paste that is just a path becomes an attachment), spaced so the attachment can register;
/// others get the parts joined by single spaces in one write. Enter is a separate last step.
pub fn plan(text: &str, image_paths: &[String], agent: Option<AgentKind>, submit: bool) -> Vec<Step> {
    let mut steps = Vec::new();
    let step = |bytes: Vec<u8>, delay_before: Duration| Step { bytes, delay_before };
    if force_bracketed(agent) {
        for (i, p) in image_paths.iter().enumerate() {
            let delay = if i == 0 { Duration::ZERO } else { IMAGE_GAP };
            steps.push(step(paste_bytes(&path_text(p), true), delay));
        }
        if !text.is_empty() {
            let delay = if image_paths.is_empty() { Duration::ZERO } else { IMAGE_SETTLE };
            steps.push(step(paste_bytes(text, true), delay));
        }
        if submit {
            let delay = if text.is_empty() && !image_paths.is_empty() { IMAGE_SETTLE } else { SUBMIT_DELAY };
            steps.push(step(b"\r".to_vec(), delay));
        }
    } else {
        let mut parts: Vec<String> = image_paths.iter().map(|p| path_text(p)).collect();
        if !text.is_empty() {
            parts.push(text.to_string());
        }
        if !parts.is_empty() {
            steps.push(step(paste_bytes(&parts.join(" "), false), Duration::ZERO));
        }
        if submit {
            steps.push(step(b"\r".to_vec(), SUBMIT_DELAY));
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    const Z: Duration = Duration::ZERO;

    fn st(bytes: &[u8], delay_before: Duration) -> Step {
        Step { bytes: bytes.to_vec(), delay_before }
    }

    /// All bytes of the plan joined, ignoring timing.
    fn bytes(steps: &[Step]) -> Vec<u8> {
        steps.iter().flat_map(|s| s.bytes.clone()).collect()
    }

    #[test]
    fn claude_and_codex_are_bracketed_others_raw() {
        for kind in [AgentKind::Claude, AgentKind::Codex] {
            assert_eq!(plan("hi", &[], Some(kind), false), vec![st(b"\x1b[200~hi\x1b[201~", Z)]);
        }
        for kind in [Some(AgentKind::Gemini), Some(AgentKind::Omp), Some(AgentKind::Other), None] {
            assert_eq!(plan("hi", &[], kind, false), vec![st(b"hi", Z)]);
        }
    }

    #[test]
    fn newlines_become_carriage_returns() {
        assert_eq!(bytes(&plan("a\nb\r\nc", &[], None, false)), b"a\rb\rc");
        assert_eq!(bytes(&plan("a\nb", &[], Some(AgentKind::Claude), false)), b"\x1b[200~a\rb\x1b[201~");
    }

    #[test]
    fn bracketed_strips_escape_but_raw_keeps_it() {
        assert_eq!(bytes(&plan("a\x1b[201~b", &[], Some(AgentKind::Claude), false)), b"\x1b[200~a[201~b\x1b[201~");
        assert_eq!(bytes(&plan("a\x1bb", &[], None, false)), b"a\x1bb");
    }

    #[test]
    fn image_paths_come_first_and_are_quoted_when_they_have_whitespace() {
        let imgs = vec!["/t/a.png".to_string(), "/my t/b.png".to_string()];
        assert_eq!(bytes(&plan("look", &imgs, None, false)), b"/t/a.png \"/my t/b.png\" look");
        assert_eq!(bytes(&plan("", &imgs, None, false)), b"/t/a.png \"/my t/b.png\"");
    }

    /// Claude Code turns a paste into an `[Image #N]` attachment only when the paste is just the
    /// path, so each image gets its own bracketed paste and the text a separate one. The text and
    /// Enter wait for the attachment to register.
    #[test]
    fn bracketed_agents_get_one_paste_per_image_then_the_text_then_enter() {
        let imgs = vec!["/t/a.png".to_string(), "/my t/b.png".to_string()];
        assert_eq!(
            plan("look", &imgs, Some(AgentKind::Claude), true),
            vec![
                st(b"\x1b[200~/t/a.png\x1b[201~", Z),
                st(b"\x1b[200~\"/my t/b.png\"\x1b[201~", IMAGE_GAP),
                st(b"\x1b[200~look\x1b[201~", IMAGE_SETTLE),
                st(b"\r", SUBMIT_DELAY),
            ]
        );
    }

    #[test]
    fn image_only_message_waits_for_settle_before_enter() {
        let imgs = vec!["/tmp/a.png".to_string()];
        assert_eq!(
            plan("", &imgs, Some(AgentKind::Codex), true),
            vec![st(b"\x1b[200~/tmp/a.png\x1b[201~", Z), st(b"\r", IMAGE_SETTLE)]
        );
        assert_eq!(plan("", &imgs, Some(AgentKind::Codex), false), vec![st(b"\x1b[200~/tmp/a.png\x1b[201~", Z)]);
    }

    #[test]
    fn no_image_and_raw_paths_keep_the_short_timing() {
        assert_eq!(
            plan("hi", &[], Some(AgentKind::Claude), true),
            vec![st(b"\x1b[200~hi\x1b[201~", Z), st(b"\r", SUBMIT_DELAY)]
        );
        let imgs = vec!["/tmp/a.png".to_string()];
        assert_eq!(plan("hi", &imgs, None, true), vec![st(b"/tmp/a.png hi", Z), st(b"\r", SUBMIT_DELAY)]);
    }

    #[test]
    fn empty_message_has_nothing_to_paste_but_may_still_submit() {
        assert_eq!(plan("", &[], Some(AgentKind::Claude), true), vec![st(b"\r", SUBMIT_DELAY)]);
        assert!(plan("", &[], None, false).is_empty());
    }

    #[test]
    fn path_quoting_rules() {
        assert_eq!(path_text("/tmp/a.png"), "/tmp/a.png");
        assert_eq!(path_text("/tmp/a b.png"), "\"/tmp/a b.png\"");
    }
}
