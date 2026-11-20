//! Themed, in-app notifications for daemon failures and available updates.

use std::time::Instant;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, WeakEntity, Window};

use crate::app_view::AppView;
use crate::i18n::{self, Tr, tr};
use crate::{settings, update_dialog};

struct AvailableUpdate;

/// Message of `last` if it was not shown yet; remembers it in `shown`.
pub fn take_new_error(
    last: Option<(String, String, Instant)>,
    shown: &mut Option<Instant>,
) -> Option<String> {
    let (_op, message, at) = last?;
    if *shown == Some(at) {
        return None;
    }
    *shown = Some(at);
    Some(message)
}

pub fn show_error(message: String, window: &mut Window, cx: &mut App) {
    window.push_notification(Notification::error(message), cx);
}

pub(crate) fn show_update(
    app: WeakEntity<AppView>,
    version: &semver::Version,
    window: &mut Window,
    cx: &mut App,
) {
    let lang = settings::language(cx);
    let notification = Notification::info(i18n::update_available(lang, &version.to_string()))
        .id::<AvailableUpdate>()
        .title(tr(lang, Tr::UpdateTitle))
        .action(move |_, _, cx| {
            let app = app.clone();
            Button::new("open-update")
                .primary()
                .label(tr(settings::language(cx), Tr::Update))
                .on_click(move |_, window, cx| {
                    if let Some(app) = app.upgrade() {
                        update_dialog::open(app, window, cx);
                    }
                })
        });
    window.push_notification(notification, cx);
}

pub(crate) fn clear_update(window: &mut Window, cx: &mut App) {
    window.remove_notification::<AvailableUpdate>(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_error_is_shown_once() {
        let at = Instant::now();
        let err = || Some(("add project".to_string(), "not a folder".to_string(), at));
        let mut shown = None;
        assert_eq!(take_new_error(err(), &mut shown).as_deref(), Some("not a folder"));
        assert_eq!(take_new_error(err(), &mut shown), None);
    }

    #[test]
    fn newer_error_is_shown_again() {
        let t0 = Instant::now();
        let mut shown = Some(t0);
        let later = t0 + std::time::Duration::from_millis(5);
        let err = Some(("op".to_string(), "boom".to_string(), later));
        assert_eq!(take_new_error(err, &mut shown).as_deref(), Some("boom"));
        assert_eq!(shown, Some(later));
    }

    #[test]
    fn no_error_shows_nothing() {
        let mut shown = None;
        assert_eq!(take_new_error(None, &mut shown), None);
    }
}
