//! Add, edit and remove the quick commands stored in application settings.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::i18n::{Tr, tr};
use crate::{quick_commands, settings};

pub(crate) fn open_manager(window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    window.open_dialog(cx, move |dialog, _window, cx| {
        let lang = settings::language(cx);
        let commands = settings::get(cx).quick_commands;
        let rows = commands.iter().enumerate().map(|(index, command)| {
            let edit = Button::new(("quick-command-edit", index))
                .ghost()
                .small()
                .flex_1()
                .min_w_0()
                .label(format!("{} — {}", command.name, command.command))
                .tooltip(tr(lang, Tr::QuickCommandEdit))
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    window.defer(cx, move |window, cx| open_editor(Some(index), window, cx));
                });
            h_flex()
                .w_full()
                .gap_1()
                .items_center()
                .child(edit)
                .child(
                    Button::new(("quick-command-remove", index))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::Close))
                        .tooltip(tr(lang, Tr::Remove))
                        .on_click(move |_, _, cx| settings::remove_quick_command(index, cx)),
                )
        });
        dialog
            .title(tr(lang, Tr::QuickCommands))
            .w(px(600.))
            .child(
                v_flex()
                    .gap_2()
                    .children(commands.is_empty().then(|| {
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(tr(lang, Tr::QuickCommandsEmpty))
                    }))
                    .child(
                        div()
                            .id("quick-command-list")
                            .w_full()
                            .max_h(px(320.))
                            .overflow_y_scroll()
                            .child(v_flex().w_full().gap_1().children(rows)),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("quick-commands-close")
                            .label(tr(lang, Tr::Close))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("quick-command-add")
                            .primary()
                            .label(tr(lang, Tr::QuickCommandAdd))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                                window.defer(cx, |window, cx| open_editor(None, window, cx));
                            }),
                    ),
            )
    });
}

fn open_editor(index: Option<usize>, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    let current = index.and_then(|index| settings::get(cx).quick_commands.get(index).cloned());
    if index.is_some() && current.is_none() {
        open_manager(window, cx);
        return;
    }
    let name = cx.new(|cx| InputState::new(window, cx));
    let command = cx.new(|cx| InputState::new(window, cx));
    if let Some(current) = current {
        name.update(cx, |input, cx| {
            let value = current.name;
            input.set_value(value, window, cx);
        });
        command.update(cx, |input, cx| {
            let value = current.command;
            input.set_value(value, window, cx);
        });
    }

    let done = Rc::new(Cell::new(false));
    let submit: Rc<dyn Fn(&mut App) -> bool> = {
        let (name, command, done) = (name.clone(), command.clone(), done.clone());
        Rc::new(move |cx: &mut App| {
            if done.get() {
                return true;
            }
            let Some(value) = quick_commands::clean(
                name.read(cx).value().as_ref(),
                command.read(cx).value().as_ref(),
            ) else {
                return false;
            };
            done.set(true);
            settings::save_quick_command(index, value, cx);
            true
        })
    };
    for input in [&name, &command] {
        let submit = submit.clone();
        window
            .subscribe(input, cx, move |_, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) && submit(cx) {
                    window.close_dialog(cx);
                    window.defer(cx, open_manager);
                }
            })
            .detach();
    }

    let focus_first = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        let lang = settings::language(cx);
        if focus_first.replace(false) {
            name.focus_handle(cx).focus(window, cx);
        }
        let save = {
            let submit = submit.clone();
            Button::new("quick-command-save")
                .primary()
                .label(tr(lang, Tr::QuickCommandSave))
                .on_click(move |_, window, cx| {
                    if submit(cx) {
                        window.close_dialog(cx);
                        window.defer(cx, open_manager);
                    }
                })
        };
        let cancel = Button::new("quick-command-cancel")
            .label(tr(lang, Tr::Cancel))
            .on_click(|_, window, cx| {
                window.close_dialog(cx);
                window.defer(cx, open_manager);
            });
        let submit = submit.clone();
        dialog
            .title(tr(lang, if index.is_some() { Tr::QuickCommandEdit } else { Tr::QuickCommandAdd }))
            .w(px(520.))
            .on_ok(move |_, _, cx| submit(cx))
            .child(
                v_flex()
                    .gap_3()
                    .child(field(tr(lang, Tr::QuickCommandName), &name, cx))
                    .child(field(tr(lang, Tr::QuickCommandCommand), &command, cx)),
            )
            .footer(DialogFooter::new().child(cancel).child(save))
    });
}

fn field(
    label: &str,
    input: &Entity<InputState>,
    cx: &App,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(div().text_sm().text_color(cx.theme().muted_foreground).child(label.to_owned()))
        .child(Input::new(input))
}
