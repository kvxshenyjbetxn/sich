//! The "file" workspace tab: a small text editor for files opened from the Context panel
//! (memory files, skills, agents, MCP configs, transcripts). Editor = the UI kit's code editor
//! (`EditorState`, line numbers, markdown/json highlighting when the kit supports the language).
//! All file I/O and decisions live in `file_doc`; this file is the view around them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Render, Styled as _, Subscription, Window, div, px,
};

use crate::file_doc::{self, DiskAction, Eol, LoadError, Stamp};
use crate::i18n::{Language, Tr, tr};
use crate::settings;

/// How often the file is checked for changes on disk.
const POLL: Duration = Duration::from_secs(2);
/// Longest path text in the header before its middle is cut.
const PATH_CHARS: usize = 90;

enum Phase {
    Loading,
    Ready,
    Failed(LoadError),
}

pub(crate) struct FileView {
    path: PathBuf,
    /// The caller wants no editing (e.g. Claude's own state file).
    forced_read_only: bool,
    phase: Phase,
    editor: Entity<EditorState>,
    /// The text as last loaded or saved (with `\n` breaks).
    saved: String,
    eol: Eol,
    bom: bool,
    stamp: Option<Stamp>,
    dirty: bool,
    changed_on_disk: bool,
    loading: bool,
    saving: bool,
    error: Option<String>,
    focus: FocusHandle,
    _edits: Subscription,
}

impl FileView {
    pub(crate) fn new(path: PathBuf, forced_read_only: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let language = file_doc::language_for(&path);
        let editor = cx.new(|cx| {
            let state = EditorState::new(window, cx);
            match language {
                Some(language) => state.language(language),
                None => state,
            }
        });
        let edits = cx.subscribe_in(&editor, window, |view, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                view.on_edit(cx);
            }
        });
        let mut view = Self {
            path,
            forced_read_only,
            phase: Phase::Loading,
            editor,
            saved: String::new(),
            eol: Eol::Lf,
            bom: false,
            stamp: None,
            dirty: false,
            changed_on_disk: false,
            loading: false,
            saving: false,
            error: None,
            focus: cx.focus_handle(),
            _edits: edits,
        };
        view.start_load(window, cx);
        view.start_polling(window, cx);
        view
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.phase {
            Phase::Ready => self.editor.update(cx, |editor, cx| editor.focus(window, cx)),
            _ => self.focus.focus(window, cx),
        }
    }

    fn read_only(&self) -> bool {
        file_doc::is_read_only(&self.path, self.forced_read_only)
    }

    fn start_load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        let path = self.path.clone();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx.background_executor().spawn(async move { file_doc::load(&path) }).await;
            let _ = view.update_in(cx, |view, window, cx| view.finish_load(result, window, cx));
        })
        .detach();
    }

    fn finish_load(&mut self, result: Result<file_doc::Loaded, LoadError>, window: &mut Window, cx: &mut Context<Self>) {
        self.loading = false;
        match result {
            Ok(loaded) => {
                let read_only = self.read_only();
                let text = loaded.text.clone();
                self.editor.update(cx, |editor, cx| {
                    editor.set_value(text, window, cx);
                    editor.set_readonly(read_only, cx);
                });
                self.saved = loaded.text;
                self.eol = loaded.eol;
                self.bom = loaded.bom;
                self.stamp = Some(loaded.stamp);
                self.dirty = false;
                self.changed_on_disk = false;
                self.error = None;
                self.phase = Phase::Ready;
            }
            // A failed reload keeps what is on screen.
            Err(error) if !matches!(self.phase, Phase::Ready) => self.phase = Phase::Failed(error),
            Err(_) => {}
        }
        cx.notify();
    }

    fn start_polling(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let Ok(path) = view.read_with(cx, |view, _| view.path.clone()) else { break };
                let now = cx.background_executor().spawn(async move { file_doc::stamp_of(&path) }).await;
                if view.update_in(cx, |view, window, cx| view.on_poll(now, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn on_poll(&mut self, now: Option<Stamp>, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading || self.saving || !matches!(self.phase, Phase::Ready) {
            return;
        }
        match file_doc::disk_action(self.stamp, now, self.dirty) {
            DiskAction::Nothing => {}
            DiskAction::Reload => self.start_load(window, cx),
            DiskAction::Notice => {
                if !self.changed_on_disk {
                    self.changed_on_disk = true;
                    cx.notify();
                }
            }
        }
    }

    fn on_edit(&mut self, cx: &mut Context<Self>) {
        let dirty = file_doc::is_dirty(&self.editor.read(cx).value(), &self.saved);
        if dirty != self.dirty {
            self.dirty = dirty;
            cx.notify();
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.read_only() || !self.dirty || self.saving || !matches!(self.phase, Phase::Ready) {
            return;
        }
        let text = self.editor.read(cx).value().to_string();
        let bytes = file_doc::encode(&text, self.eol, self.bom);
        let path = self.path.clone();
        self.saving = true;
        self.error = None;
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    file_doc::save_atomic(&path, &bytes)?;
                    Ok::<_, std::io::Error>(file_doc::stamp_of(&path))
                })
                .await;
            let _ = view.update(cx, move |view, cx| view.finish_save(text, result, cx));
        })
        .detach();
        cx.notify();
    }

    fn finish_save(&mut self, text: String, result: std::io::Result<Option<Stamp>>, cx: &mut Context<Self>) {
        self.saving = false;
        match result {
            Ok(stamp) => {
                self.saved = text;
                self.stamp = stamp;
                self.changed_on_disk = false;
                self.dirty = file_doc::is_dirty(&self.editor.read(cx).value(), &self.saved);
            }
            Err(error) => {
                self.error = Some(format!("{}: {error}", tr(settings::language(cx), Tr::FileSaveFailed)));
            }
        }
        cx.notify();
    }

    fn banner(&self, lang: Language, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let theme = cx.theme().clone();
        let bar = |text: String| {
            h_flex()
                .w_full()
                .flex_shrink_0()
                .px_2()
                .py_1()
                .gap_2()
                .items_center()
                .text_size(px(12.))
                .bg(theme.warning.opacity(0.15))
                .border_b_1()
                .border_color(theme.border)
                .child(div().min_w_0().truncate().child(text))
        };
        if let Some(error) = &self.error {
            return Some(bar(error.clone()).into_any_element());
        }
        self.changed_on_disk.then(|| {
            bar(tr(lang, Tr::FileChangedOnDisk).to_string())
                .child(
                    Button::new("file-reload")
                        .small()
                        .label(tr(lang, Tr::FileReload))
                        .on_click(cx.listener(|view, _, window, cx| view.start_load(window, cx))),
                )
                .into_any_element()
        })
    }

    fn header(&self, lang: Language, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let read_only = self.read_only();
        let open_path = self.path.clone();
        let reveal_path = self.path.clone();
        h_flex()
            .h(px(30.))
            .w_full()
            .flex_shrink_0()
            .px_2()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.tab_bar)
            .children(self.dirty.then(|| div().flex_shrink_0().size(px(8.)).rounded_full().bg(theme.primary)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(file_doc::middle_ellipsis(&self.path.display().to_string(), PATH_CHARS)),
            )
            .children(read_only.then(|| {
                div().flex_shrink_0().text_size(px(11.)).text_color(theme.muted_foreground).child(tr(lang, Tr::FileReadOnly))
            }))
            .children((!read_only).then(|| {
                Button::new("file-save")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::Save))
                    .label(tr(lang, Tr::FileSave))
                    .disabled(!self.dirty || self.saving)
                    .on_click(cx.listener(|view, _, _, cx| view.save(cx)))
            }))
            .child(
                Button::new("file-open-default")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::ExternalLink))
                    .tooltip(tr(lang, Tr::CtxOpenDefault))
                    .on_click(move |_, _, _| crate::os::open_default(&open_path)),
            )
            .child(
                Button::new("file-reveal")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::FolderOpen))
                    .tooltip(tr(lang, Tr::CtxShowInFolder))
                    .on_click(move |_, _, _| crate::os::reveal_in_file_manager(&reveal_path)),
            )
    }
}

impl Render for FileView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = settings::language(cx);
        let theme = cx.theme().clone();
        let message = |text: &str| div().p_4().text_size(px(12.5)).text_color(theme.muted_foreground).child(text.to_string());
        let body = match &self.phase {
            Phase::Loading => message(tr(lang, Tr::FileLoading)).into_any_element(),
            Phase::Ready => div()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(Editor::new(&self.editor).size_full().bordered(false))
                .into_any_element(),
            Phase::Failed(error) => {
                let key = match error {
                    LoadError::TooLarge => Tr::FileTooLarge,
                    LoadError::NotUtf8 => Tr::FileNotText,
                    LoadError::Io(_) => Tr::FileUnreadable,
                };
                let path = self.path.clone();
                v_flex()
                    .p_4()
                    .gap_2()
                    .items_start()
                    .child(message(tr(lang, key)))
                    .child(
                        Button::new("file-open-default-large")
                            .small()
                            .label(tr(lang, Tr::CtxOpenDefault))
                            .on_click(move |_, _, _| crate::os::open_default(&path)),
                    )
                    .into_any_element()
            }
        };
        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .bg(theme.background)
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(|view, ev: &KeyDownEvent, window, cx| {
                let m = ev.keystroke.modifiers;
                if ev.keystroke.key.eq_ignore_ascii_case("s") && (m.control || m.platform) && !m.alt && !m.shift {
                    view.save(cx);
                    cx.stop_propagation();
                    window.prevent_default();
                }
            }))
            .child(self.header(lang, cx))
            .children(self.banner(lang, cx))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_keys_are_translated() {
        for key in [
            Tr::FileTab,
            Tr::FileClose,
            Tr::FileSave,
            Tr::FileLoading,
            Tr::FileTooLarge,
            Tr::FileUnreadable,
            Tr::FileNotText,
            Tr::FileChangedOnDisk,
            Tr::FileReload,
            Tr::FileReadOnly,
            Tr::FileSaveFailed,
            Tr::CtxOpenInTab,
            Tr::CtxOpenDefault,
            Tr::CtxShowInFolder,
            Tr::CtxCopyPath,
            Tr::CtxAgents,
            Tr::CtxTranscript,
        ] {
            for lang in Language::ALL {
                assert!(!tr(lang, key).trim().is_empty(), "{key:?} {lang:?}");
            }
        }
    }
}
