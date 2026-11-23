//! `SendText` payload building (pure).

use std::time::Duration;

use sich_proto::AgentKind;

/// Delay between the message and the separate Enter keypress.
pub const SUBMIT_DELAY: Duration = Duration::from_millis(30);

#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// Bytes for the PTY; empty means nothing to paste.
    pub payload: Vec<u8>,
    /// Send a separate `\r` after `SUBMIT_DELAY`.
    pub submit: bool,
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

/// Image paths first, then the text. Bracketed agents get one paste per part (a paste that is
/// just a path becomes an attachment); others get the parts joined by single spaces.
pub fn plan(text: &str, image_paths: &[String], agent: Option<AgentKind>, submit: bool) -> Plan {
    let mut parts: Vec<String> = image_paths.iter().map(|p| path_text(p)).collect();
    if !text.is_empty() {
        parts.push(text.to_string());
    }
    let payload = if parts.is_empty() {
        Vec::new()
    } else if force_bracketed(agent) {
        parts.iter().flat_map(|p| paste_bytes(p, true)).collect()
    } else {
        paste_bytes(&parts.join(" "), false)
    };
    Plan { payload, submit }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_and_codex_are_bracketed_others_raw() {
        for kind in [AgentKind::Claude, AgentKind::Codex] {
            assert_eq!(plan("hi", &[], Some(kind), false).payload, b"\x1b[200~hi\x1b[201~");
        }
        for kind in [Some(AgentKind::Gemini), Some(AgentKind::Omp), Some(AgentKind::Other), None] {
            assert_eq!(plan("hi", &[], kind, false).payload, b"hi");
        }
    }

    #[test]
    fn newlines_become_carriage_returns() {
        assert_eq!(plan("a\nb\r\nc", &[], None, false).payload, b"a\rb\rc");
        assert_eq!(plan("a\nb", &[], Some(AgentKind::Claude), false).payload, b"\x1b[200~a\rb\x1b[201~");
    }

    #[test]
    fn bracketed_strips_escape_but_raw_keeps_it() {
        assert_eq!(plan("a\x1b[201~b", &[], Some(AgentKind::Claude), false).payload, b"\x1b[200~a[201~b\x1b[201~");
        assert_eq!(plan("a\x1bb", &[], None, false).payload, b"a\x1bb");
    }

    #[test]
    fn image_paths_come_first_and_are_quoted_when_they_have_whitespace() {
        let imgs = vec!["C:\\Temp\\a.png".to_string(), "C:\\My Temp\\b.png".to_string()];
        assert_eq!(plan("look", &imgs, None, false).payload, b"C:\\Temp\\a.png \"C:\\My Temp\\b.png\" look");
        assert_eq!(plan("", &imgs, None, false).payload, b"C:\\Temp\\a.png \"C:\\My Temp\\b.png\"");
    }

    /// Claude Code turns a paste into an `[Image #N]` attachment only when the paste is just the
    /// path, so each image gets its own bracketed paste and the text a separate one.
    #[test]
    fn bracketed_agents_get_one_paste_per_image_then_the_text() {
        let imgs = vec!["C:\\Temp\\a.png".to_string(), "C:\\My Temp\\b.png".to_string()];
        assert_eq!(
            plan("look", &imgs, Some(AgentKind::Claude), false).payload,
            b"\x1b[200~C:\\Temp\\a.png\x1b[201~\x1b[200~\"C:\\My Temp\\b.png\"\x1b[201~\x1b[200~look\x1b[201~"
        );
        assert_eq!(plan("", &imgs[..1], Some(AgentKind::Codex), false).payload, b"\x1b[200~C:\\Temp\\a.png\x1b[201~");
    }

    #[test]
    fn empty_message_has_no_payload_but_may_still_submit() {
        let p = plan("", &[], Some(AgentKind::Claude), true);
        assert!(p.payload.is_empty());
        assert!(p.submit);
        assert!(!plan("x", &[], None, false).submit);
    }

    #[test]
    fn path_quoting_rules() {
        assert_eq!(path_text("/tmp/a.png"), "/tmp/a.png");
        assert_eq!(path_text("/tmp/a b.png"), "\"/tmp/a b.png\"");
    }
}
