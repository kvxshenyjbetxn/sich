//! System tray: icon, tooltip with agent counts and the show/quit menu (Windows only; elsewhere
//! `Tray::new` fails and the app simply has no tray). The pure helpers are tested everywhere.

use futures::channel::mpsc::UnboundedSender;
use sich_proto::{AgentState, TerminalInfo};
#[cfg(windows)]
use sich_proto::TermId;

use crate::i18n::{Language, Tr, tr};

/// Requests that reach the UI from the tray, Windows toasts, or a second `sich` launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiEvent {
    /// Bring the window to the front.
    Show,
    #[cfg(windows)]
    FocusTerminal(TermId),
    Takeover,
    Restart,
    /// Quit Sich (daemon included), asking first when agents are busy.
    Quit,
}

/// `(waiting, working)` counts over all terminals.
pub fn agent_counts(terminals: &[TerminalInfo]) -> (usize, usize) {
    let mut waiting = 0;
    let mut working = 0;
    for t in terminals {
        match t.agent.as_ref().map(|a| a.state) {
            Some(AgentState::Waiting) => waiting += 1,
            Some(AgentState::Working) => working += 1,
            _ => {}
        }
    }
    (waiting, working)
}

/// Quitting stops every terminal; ask first when an agent is working or waiting for the user.
pub fn needs_quit_confirm(terminals: &[TerminalInfo]) -> bool {
    let (waiting, working) = agent_counts(terminals);
    waiting + working > 0
}

/// Tray tooltip: "Sich", or "Sich \u{2014} 1 waiting, 2 working" when agents are busy.
pub fn tray_tooltip(lang: Language, waiting: usize, working: usize) -> String {
    let mut parts = Vec::new();
    if waiting > 0 {
        parts.push(format!("{waiting} {}", tr(lang, Tr::TrayWaiting)));
    }
    if working > 0 {
        parts.push(format!("{working} {}", tr(lang, Tr::TrayWorking)));
    }
    if parts.is_empty() { "Sich".to_string() } else { format!("Sich \u{2014} {}", parts.join(", ")) }
}

#[cfg(windows)]
pub use imp::Tray;

#[cfg(not(windows))]
pub struct Tray;

#[cfg(not(windows))]
impl Tray {
    pub fn new(_lang: Language, _tooltip: &str, _tx: UnboundedSender<UiEvent>) -> anyhow::Result<Self> {
        anyhow::bail!("system tray is only supported on Windows")
    }
    pub fn set_language(&mut self, _lang: Language) {}
    pub fn set_tooltip(&mut self, _text: &str) {}
}

#[cfg(windows)]
mod imp {
    use super::*;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    const ID_SHOW: &str = "sich-tray-show";
    const ID_RESTART: &str = "sich-tray-restart";
    const ID_QUIT: &str = "sich-tray-quit";
    /// Ordinal of the icon embedded by `app.rc` (`1 ICON assets/icon.ico`).
    const ICON_ORDINAL: u16 = 1;

    pub struct Tray {
        icon: TrayIcon,
        tooltip: String,
    }

    fn build_menu(lang: Language) -> anyhow::Result<Menu> {
        let menu = Menu::new();
        menu.append(&MenuItem::with_id(ID_SHOW, tr(lang, Tr::TrayShow), true, None))?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&MenuItem::with_id(ID_RESTART, tr(lang, Tr::TrayRestart), true, None))?;
        menu.append(&MenuItem::with_id(ID_QUIT, tr(lang, Tr::TrayQuit), true, None))?;
        Ok(menu)
    }

    impl Tray {
        /// Creates the icon. Must run on the thread that pumps Win32 messages (the GPUI main
        /// thread). Events are forwarded into `tx`.
        pub fn new(lang: Language, tooltip: &str, tx: UnboundedSender<UiEvent>) -> anyhow::Result<Self> {
            let icon = Icon::from_resource(ICON_ORDINAL, None)
                .map_err(|e| anyhow::anyhow!("load tray icon: {e}"))?;
            let tray = TrayIconBuilder::new()
                .with_icon(icon)
                .with_tooltip(tooltip)
                .with_menu(Box::new(build_menu(lang)?))
                .with_menu_on_left_click(false)
                .build()
                .map_err(|e| anyhow::anyhow!("create tray icon: {e}"))?;

            let click_tx = tx.clone();
            TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| match ev {
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => {
                    let _ = click_tx.unbounded_send(UiEvent::Show);
                }
                _ => {}
            }));
            MenuEvent::set_event_handler(Some(move |ev: MenuEvent| match ev.id.0.as_str() {
                ID_SHOW => {
                    let _ = tx.unbounded_send(UiEvent::Show);
                }
                ID_RESTART => {
                    let _ = tx.unbounded_send(UiEvent::Restart);
                }
                ID_QUIT => {
                    let _ = tx.unbounded_send(UiEvent::Quit);
                }
                _ => {}
            }));
            Ok(Self { icon: tray, tooltip: tooltip.to_string() })
        }

        /// Rebuilds the menu labels in `lang`.
        pub fn set_language(&mut self, lang: Language) {
            match build_menu(lang) {
                Ok(menu) => self.icon.set_menu(Some(Box::new(menu))),
                Err(e) => tracing::warn!("tray menu rebuild failed: {e:#}"),
            }
        }

        pub fn set_tooltip(&mut self, text: &str) {
            if self.tooltip == text {
                return;
            }
            if let Err(e) = self.icon.set_tooltip(Some(text)) {
                tracing::warn!("tray tooltip update failed: {e}");
            }
            self.tooltip = text.to_string();
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            // Stop forwarding events to a channel that is going away.
            TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
            MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sich_proto::{AgentKind, AgentStatus};
    use std::path::PathBuf;

    fn term(id: u64, state: Option<AgentState>) -> TerminalInfo {
        TerminalInfo {
            id,
            title: String::new(),
            cwd: PathBuf::new(),
            cols: 80,
            rows: 24,
            exited: None,
            created_unix_ms: id,
            worktree: None,
            agent: state.map(|state| AgentStatus {
                kind: AgentKind::Claude,
                state,
                unread: false,
                since_unix_ms: 0,
                detail: None,
                model: None,
                title: None,
                prompt: None,
                message: None,
                subagents: Vec::new(),
                session_id: None,
            }),
        }
    }

    #[test]
    fn tooltip_is_plain_without_busy_agents() {
        for lang in Language::ALL {
            assert_eq!(tray_tooltip(lang, 0, 0), "Sich");
        }
    }

    #[test]
    fn tooltip_lists_nonzero_counts_waiting_first() {
        assert_eq!(tray_tooltip(Language::En, 1, 2), "Sich \u{2014} 1 waiting, 2 working");
        assert_eq!(tray_tooltip(Language::En, 0, 3), "Sich \u{2014} 3 working");
        assert_eq!(tray_tooltip(Language::En, 2, 0), "Sich \u{2014} 2 waiting");
        for lang in Language::ALL {
            let t = tray_tooltip(lang, 1, 2);
            assert!(t.starts_with("Sich \u{2014} 1 ") && t.contains(", 2 "), "{t}");
        }
        assert_ne!(tray_tooltip(Language::Uk, 1, 0), tray_tooltip(Language::En, 1, 0));
    }

    #[test]
    fn counts_only_working_and_waiting() {
        let list = vec![
            term(1, Some(AgentState::Working)),
            term(2, Some(AgentState::Waiting)),
            term(3, Some(AgentState::Working)),
            term(4, Some(AgentState::Done)),
            term(5, Some(AgentState::Failed)),
            term(6, Some(AgentState::Idle)),
            term(7, None),
        ];
        assert_eq!(agent_counts(&list), (1, 2));
        assert_eq!(agent_counts(&[]), (0, 0));
    }

    #[test]
    fn quit_needs_confirmation_only_for_busy_agents() {
        assert!(!needs_quit_confirm(&[]));
        assert!(!needs_quit_confirm(&[term(1, None), term(2, Some(AgentState::Idle)), term(3, Some(AgentState::Done))]));
        assert!(needs_quit_confirm(&[term(1, None), term(2, Some(AgentState::Working))]));
        assert!(needs_quit_confirm(&[term(1, Some(AgentState::Waiting))]));
    }
}
