//! Dialogs for project groups: create, rename (a name input) and delete (a confirmation).

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::{
    App, AppContext as _, Entity, Focusable as _, ParentElement as _, Styled as _, Window, div, px,
};
use sich_proto::GroupId;

use crate::app_view::AppView;
use crate::i18n::{self, Tr, tr};
use crate::settings;
use crate::worktree_dialogs::open_confirm;

/// Normalized group name from raw input; `None` when empty after trimming.
pub fn clean_group_name(raw: &str) -> Option<String> {
    let t = raw.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// What the name dialog does with a valid name.
type OnName = Rc<dyn Fn(String, &mut App)>;

fn open_name_dialog(
    window: &mut Window,
    cx: &mut App,
    title: Tr,
    initial: Option<String>,
    ok: Tr,
    on_name: OnName,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let input = cx.new(|cx| {
        let mut s = InputState::new(window, cx);
        if let Some(v) = &initial {
            s.set_value(v.clone(), window, cx);
        }
        s
    });
    let done = Rc::new(Cell::new(false));
    // Returns true when the dialog may close.
    let submit: Rc<dyn Fn(&mut App) -> bool> = {
        let (input, done) = (input.clone(), done.clone());
        Rc::new(move |cx: &mut App| {
            if done.get() {
                return true;
            }
            let Some(name) = clean_group_name(input.read(cx).value().as_ref()) else {
                return false;
            };
            done.set(true);
            on_name(name, cx);
            true
        })
    };
    {
        let submit = submit.clone();
        window
            .subscribe(&input, cx, move |_, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) && submit(cx) {
                    window.close_dialog(cx);
                }
            })
            .detach();
    }
    let focus_first = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        let lang = settings::language(cx);
        if focus_first.replace(false) {
            input.focus_handle(cx).focus(window, cx);
        }
        let body = v_flex().gap_1().child(
            div().text_sm().text_color(cx.theme().muted_foreground).child(tr(lang, Tr::GroupName)),
        )
        .child(Input::new(&input));
        let ok_btn = {
            let submit = submit.clone();
            Button::new("group-ok").primary().label(tr(lang, ok)).on_click(move |_, window, cx| {
                if submit(cx) {
                    window.close_dialog(cx);
                }
            })
        };
        let footer = DialogFooter::new()
            .child(
                Button::new("group-cancel")
                    .label(tr(lang, Tr::Cancel))
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(ok_btn);
        let submit = submit.clone();
        dialog
            .title(tr(lang, title))
            .w(px(440.))
            .on_ok(move |_, _, cx| submit(cx))
            .child(body)
            .footer(footer)
    });
}

pub fn open_create_group(app: Entity<AppView>, window: &mut Window, cx: &mut App) {
    open_name_dialog(
        window,
        cx,
        Tr::NewGroup,
        None,
        Tr::Create,
        Rc::new(move |name, cx| app.read(cx).client.create_group(name)),
    );
}

pub fn open_rename_group(
    app: Entity<AppView>,
    id: GroupId,
    current: String,
    window: &mut Window,
    cx: &mut App,
) {
    open_name_dialog(
        window,
        cx,
        Tr::RenameGroup,
        Some(current),
        Tr::Rename,
        Rc::new(move |name, cx| app.read(cx).client.rename_group(id, name)),
    );
}

pub fn open_delete_group(
    app: Entity<AppView>,
    id: GroupId,
    name: String,
    window: &mut Window,
    cx: &mut App,
) {
    let lang = settings::language(cx);
    open_confirm(
        window,
        cx,
        tr(lang, Tr::DeleteGroup).to_string(),
        i18n::delete_group_text(lang, &name),
        None,
        tr(lang, Tr::Remove),
        true,
        Rc::new(move |_, cx| app.read(cx).client.remove_group(id)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_name_is_trimmed_and_non_empty() {
        assert_eq!(clean_group_name("  game "), Some("game".into()));
        assert_eq!(clean_group_name("a b"), Some("a b".into()));
        assert_eq!(clean_group_name(""), None);
        assert_eq!(clean_group_name(" \t "), None);
    }
}
