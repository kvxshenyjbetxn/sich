//! Dialogs: new worktree, remove worktree/project, and a generic confirmation.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Entity, IntoElement, ParentElement as _,
    Styled as _, Window, div, px,
};
use sich_proto::ProjectId;

use crate::app_view::AppView;
use crate::i18n::{self, Tr, tr};
use crate::settings;
use crate::worktree_from_pr::NewWorktreeForm;

/// Builds an extra element for the body of a confirmation dialog.
type Extra = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;
type Action = Rc<dyn Fn(&mut Window, &mut App)>;

/// Everything the remove-worktree dialog needs to know.
#[derive(Clone)]
pub struct WorktreeRef {
    pub project_id: ProjectId,
    pub path: PathBuf,
    /// Branch name, or the folder name when detached.
    pub label: String,
    /// Real branch (`None` when detached): only then "delete branch" is offered.
    pub branch: Option<String>,
    pub dirty: bool,
}

/// Generic two-button confirmation. `on_ok` runs at most once.
#[allow(clippy::too_many_arguments)]
pub fn open_confirm(
    window: &mut Window,
    cx: &mut App,
    title: String,
    message: String,
    extra: Option<Extra>,
    ok_label: &'static str,
    danger: bool,
    on_ok: Action,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let done = Rc::new(Cell::new(false));
    window.open_dialog(cx, move |dialog, window, cx| {
        let lang = settings::language(cx);
        let mut body = v_flex().gap_3().child(div().text_sm().child(message.clone()));
        if let Some(extra) = &extra {
            body = body.child(extra(window, cx));
        }
        let run = {
            let (on_ok, done) = (on_ok.clone(), done.clone());
            move |window: &mut Window, cx: &mut App| {
                if !done.replace(true) {
                    on_ok(window, cx);
                }
            }
        };
        let ok_btn = {
            let run = run.clone();
            let b = Button::new("dialog-ok").label(ok_label).on_click(move |_, window, cx| {
                run(window, cx);
                window.close_dialog(cx);
            });
            if danger { b.danger() } else { b.primary() }
        };
        let footer = DialogFooter::new()
            .child(
                Button::new("dialog-cancel")
                    .label(tr(lang, Tr::Cancel))
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(ok_btn);
        dialog
            .title(title.clone())
            .w(px(440.))
            .on_ok(move |_, window, cx| {
                run(window, cx);
                true
            })
            .child(body)
            .footer(footer)
    });
}

pub fn open_remove_project(
    app: Entity<AppView>,
    id: ProjectId,
    name: String,
    window: &mut Window,
    cx: &mut App,
) {
    let lang = settings::language(cx);
    open_confirm(
        window,
        cx,
        tr(lang, Tr::RemoveProject).to_string(),
        i18n::remove_project_text(lang, &name),
        None,
        tr(lang, Tr::Remove),
        true,
        Rc::new(move |_, cx| app.read(cx).client.remove_project(id)),
    );
}

pub fn open_incompatible_daemon(app: Entity<AppView>, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        window.close_dialog(cx);
    }
    window.open_dialog(cx, move |dialog, _window, cx| {
        let lang = settings::language(cx);
        let (restart, quit) = (app.clone(), app.clone());
        dialog.title(tr(lang, Tr::RestartDaemon))
            .w(px(460.))
            .close_button(false)
            .overlay_closable(false)
            .keyboard(false)
            .child(div().text_sm().child(tr(lang, Tr::DaemonUpdateRequired)))
            .footer(
                DialogFooter::new()
                    .child(Button::new("quit-for-daemon-update").label(tr(lang, Tr::Quit))
                        .on_click(move |_, _, cx| quit.update(cx, |this, cx| this.quit_ui_only(cx))))
                    .child(Button::new("restart-for-daemon-update").primary().label(tr(lang, Tr::RestartDaemon))
                        .on_click(move |_, window, cx| {
                            restart.update(cx, |this, cx| this.request_daemon_restart(cx));
                            window.close_dialog(cx);
                        })),
            )
    });
}

pub fn open_remove_worktree(app: Entity<AppView>, wt: WorktreeRef, window: &mut Window, cx: &mut App) {
    let lang = settings::language(cx);
    let delete_branch = Rc::new(Cell::new(true));
    let extra: Extra = {
        let (wt, delete_branch) = (wt.clone(), delete_branch.clone());
        Rc::new(move |_window, cx| {
            let lang = settings::language(cx);
            let mut col = v_flex().gap_2();
            if let Some(branch) = &wt.branch {
                let cell = delete_branch.clone();
                col = col.child(
                    Checkbox::new("delete-branch")
                        .checked(delete_branch.get())
                        .label(i18n::also_delete_branch(lang, branch))
                        .on_change(move |v, window, _| {
                            cell.set(*v);
                            window.refresh();
                        }),
                );
            }
            if wt.dirty {
                col = col.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().warning)
                        .child(tr(lang, Tr::DirtyWarning)),
                );
            }
            col.into_any_element()
        })
    };
    let dirty = wt.dirty;
    let ok_label = if dirty { tr(lang, Tr::RemoveAnyway) } else { tr(lang, Tr::Remove) };
    open_confirm(
        window,
        cx,
        tr(lang, Tr::RemoveWorktree).to_string(),
        i18n::remove_worktree_text(lang, &wt.label),
        Some(extra),
        ok_label,
        true,
        Rc::new(move |_, cx| {
            let delete = wt.branch.is_some() && delete_branch.get();
            app.read(cx).client.remove_worktree(wt.project_id, wt.path.clone(), dirty, delete);
        }),
    );
}

pub fn open_new_worktree(
    app: Entity<AppView>,
    project_id: ProjectId,
    project_name: String,
    window: &mut Window,
    cx: &mut App,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let Some(root) = app.read(cx).projects.iter().find(|p| p.id == project_id).map(|p| p.path.clone()) else {
        return;
    };
    let form = cx.new(|cx| NewWorktreeForm::new(app, project_id, root, window, cx));
    window.open_dialog(cx, move |dialog, _window, cx| {
        let lang = settings::language(cx);
        dialog
            .title(i18n::new_worktree_title(lang, &project_name))
            .w(px(480.))
            .child(form.clone())
    });
}
