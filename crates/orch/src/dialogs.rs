//! Detects blocking startup dialogs in raw terminal output. They are never answered
//! automatically: an unattended Enter could approve something the user never saw.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialog {
    /// Codex asks to review newly installed hooks.
    CodexHooksReview,
    /// "Do you trust this folder?" style prompts.
    TrustPrompt,
    /// A generic "press Enter to confirm" screen.
    ConfirmPrompt,
}

impl Dialog {
    pub fn reason(self) -> &'static str {
        match self {
            Self::CodexHooksReview => "codex-hooks-review",
            Self::TrustPrompt => "trust-prompt",
            Self::ConfirmPrompt => "confirm-prompt",
        }
    }
}

/// Removes escape sequences. Cursor-movement sequences become a space so words drawn by a TUI
/// with absolute/relative positioning do not glue together; newlines become spaces too.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut final_byte = None;
                    for n in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            final_byte = Some(n);
                            break;
                        }
                    }
                    if matches!(final_byte, Some('A'..='H' | 'd' | 'f' | 'J' | 'K')) {
                        out.push(' ');
                    }
                }
                Some(']') => {
                    // OSC: until BEL or ESC \
                    while let Some(n) = chars.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            },
            '\r' | '\n' | '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn normalize(text: &str) -> String {
    strip_ansi(text).split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Which blocking dialog, if any, the (recent) terminal output shows.
pub fn detect(output: &str) -> Option<Dialog> {
    let text = normalize(output);
    if text.contains("hooks need review") || text.contains("review hooks") {
        return Some(Dialog::CodexHooksReview);
    }
    if text.contains("do you trust") || text.contains("trust this folder") || text.contains("yes, i trust") {
        return Some(Dialog::TrustPrompt);
    }
    if text.contains("press enter to confirm") || text.contains("enter to confirm") {
        return Some(Dialog::ConfirmPrompt);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colors_osc_and_cursor_moves() {
        assert_eq!(strip_ansi("\x1b[1;31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("a\x1b]0;title\x07b"), "ab");
        assert_eq!(strip_ansi("a\x1b]0;title\x1b\\b"), "ab");
        assert_eq!(strip_ansi("Press\x1b[1Center"), "Press enter");
        assert_eq!(strip_ansi("a\r\nb"), "a  b");
        assert_eq!(strip_ansi("trailing\x1b"), "trailing");
    }

    #[test]
    fn detects_the_codex_hooks_review_dialog() {
        let screen = "\x1b[2J\x1b[H\x1b[1m  2 hooks need review\x1b[0m\r\n\r\n  \x1b[36m\u{203a}\x1b[0m Review hooks\r\n    Skip for now\r\n\r\n  Press enter to confirm or esc to go back\r\n";
        assert_eq!(detect(screen), Some(Dialog::CodexHooksReview));
        assert_eq!(detect("1 Hook Needs Review before it can run"), None);
        assert_eq!(detect("REVIEW HOOKS"), Some(Dialog::CodexHooksReview));
    }

    #[test]
    fn detects_trust_prompts() {
        let claude = "\x1b[1mDo you trust the files in this folder?\x1b[0m\r\n\x1b[36m\u{276f} 1. Yes, proceed\x1b[0m\r\n";
        assert_eq!(detect(claude), Some(Dialog::TrustPrompt));
        assert_eq!(detect("> You are in C:\\work\r\n  Do you trust this folder?"), Some(Dialog::TrustPrompt));
        assert_eq!(detect("  1. Yes, I trust this folder"), Some(Dialog::TrustPrompt));
    }

    #[test]
    fn detects_a_bare_confirmation_screen() {
        assert_eq!(detect("\x1b[2mEnter to confirm\x1b[0m"), Some(Dialog::ConfirmPrompt));
        assert_eq!(detect("Press\x1b[1Center\x1b[1Cto\x1b[1Cconfirm"), Some(Dialog::ConfirmPrompt));
    }

    #[test]
    fn ordinary_output_is_not_a_dialog() {
        assert_eq!(detect(""), None);
        assert_eq!(detect("PS C:\\work> codex \"You are a Sich worker.\"\r\nWorking (3s)\r\n"), None);
        assert_eq!(detect("\x1b[32mAll tests passed\x1b[0m"), None);
    }

    #[test]
    fn reasons_are_stable_words() {
        assert_eq!(Dialog::CodexHooksReview.reason(), "codex-hooks-review");
        assert_eq!(Dialog::TrustPrompt.reason(), "trust-prompt");
        assert_eq!(Dialog::ConfirmPrompt.reason(), "confirm-prompt");
    }
}
