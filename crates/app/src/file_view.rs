//! The "file" workspace tab: a small text editor for files opened from the Context panel
//! (memory files, skills, agents, MCP configs, transcripts). Editor = the UI kit's code editor
//! (`EditorState`, line numbers, markdown/json highlighting when the kit supports the language).
//! All file I/O and decisions live in `file_doc`; this file is the view around them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, CursorStyle, DispatchPhase, Entity, FocusHandle, HitboxBehavior, InteractiveElement as _,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    ParentElement as _, Render, ScrollEvent, Styled as _, StyledImage as _, Subscription, Window, canvas, div, img, px,
};

use crate::file_doc::{self, DiskAction, Eol, LoadError, Stamp};
use crate::i18n::{Language, Tr, tr};
use crate::image_viewer::{self, ImageViewer, KeyAction};
use crate::settings;

/// How often the file is checked for changes on disk.
const POLL: Duration = Duration::from_secs(2);
/// Longest path text in the header before its middle is cut.
const PATH_CHARS: usize = 90;

enum Phase {
    Loading,
    Ready,
    /// An image file: shown in the zoom/pan viewer, never loaded as text. The size lives in the viewer state.
    Image,
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
    /// Zoom/pan state while the file is an image.
    viewer: Option<ImageViewer>,
    /// Last mouse position (window coordinates) of a pan drag in progress.
    drag: Option<(f32, f32)>,
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
            viewer: None,
            drag: None,
            _edits: edits,
        };
        if file_doc::is_image(&view.path) {
            view.start_image(window, cx);
        } else {
            view.start_load(window, cx);
            view.start_polling(window, cx);
        }
        view
    }

    /// Reads the size of an image file (header only); the pixels are loaded by the image element.
    fn start_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.path.clone();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx.background_executor().spawn(async move { file_doc::image_size(&path) }).await;
            let _ = view.update_in(cx, |view, _, cx| {
                view.phase = match result {
                    Ok((width, height)) => {
                        view.viewer = Some(ImageViewer::new(width, height));
                        Phase::Image
                    }
                    Err(error) => Phase::Failed(error),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Applies `change` to the image viewer and redraws when it moved.
    fn with_viewer(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut ImageViewer)) {
        if let Some(viewer) = self.viewer.as_mut() {
            let before = *viewer;
            change(viewer);
            if *viewer != before {
                cx.notify();
            }
        }
    }

    fn apply_key_action(&mut self, action: KeyAction, cx: &mut Context<Self>) {
        self.with_viewer(cx, |viewer| match action {
            KeyAction::ZoomIn => viewer.zoom_step(true),
            KeyAction::ZoomOut => viewer.zoom_step(false),
            KeyAction::Fit => viewer.set_fit(),
            KeyAction::Actual => viewer.set_actual(),
        });
    }

    /// The viewport element: measures itself every frame and owns the mouse input of the viewer.
    fn image_viewport(&self, viewer: ImageViewer, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let theme = cx.theme().clone();
        let (measure, owner) = (cx.weak_entity(), cx.weak_entity());
        let last = viewer.viewport();
        let input = canvas(
            move |bounds, window, cx| {
                let size = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                // Only a real change goes back to the view, so this cannot loop.
                if size != last {
                    let measure = measure.clone();
                    window.defer(cx, move |_, cx| {
                        let _ = measure.update(cx, |view, cx| {
                            view.with_viewer(cx, |v| {
                                v.set_viewport(size.0, size.1);
                            })
                        });
                    });
                }
                window.insert_hitbox(bounds, HitboxBehavior::Normal)
            },
            move |bounds, hitbox, window, _| {
                let origin = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
                let wheel = owner.clone();
                let wheel_hitbox = hitbox.clone();
                window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture || !wheel_hitbox.should_handle_scroll(window) {
                        return;
                    }
                    let dy = f32::from(event.delta.pixel_delta(px(image_viewer::WHEEL_LINE_PX)).y);
                    if dy == 0. {
                        return;
                    }
                    let cursor = (f32::from(event.position.x) - origin.0, f32::from(event.position.y) - origin.1);
                    let _ = wheel.update(cx, |view, cx| {
                        view.with_viewer(cx, |v| v.zoom_at(cursor, image_viewer::wheel_factor(dy)));
                    });
                    cx.stop_propagation();
                });

                let down = owner.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture
                        || event.button != MouseButton::Left
                        || !hitbox.is_hovered_at(event.position, window)
                    {
                        return;
                    }
                    let position = (f32::from(event.position.x), f32::from(event.position.y));
                    let cursor = (position.0 - origin.0, position.1 - origin.1);
                    let _ = down.update(cx, |view, cx| {
                        view.focus.focus(window, cx);
                        if event.click_count >= 2 {
                            view.drag = None;
                            view.with_viewer(cx, |v| v.toggle_at(cursor));
                        } else {
                            view.drag = Some(position);
                        }
                        cx.notify();
                    });
                    cx.stop_propagation();
                });

                let moved = owner.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    let position = (f32::from(event.position.x), f32::from(event.position.y));
                    let _ = moved.update(cx, |view, cx| {
                        let Some(last) = view.drag else { return };
                        if event.pressed_button != Some(MouseButton::Left) {
                            view.drag = None;
                            cx.notify();
                            return;
                        }
                        view.drag = Some(position);
                        view.with_viewer(cx, |v| v.pan((position.0 - last.0, position.1 - last.1)));
                    });
                });

                let up = owner.clone();
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture || event.button != MouseButton::Left {
                        return;
                    }
                    let _ = up.update(cx, |view, cx| {
                        if view.drag.take().is_some() {
                            cx.notify();
                        }
                    });
                });
            },
        )
        .absolute()
        .size_full();
        let (width, height) = viewer.display_size();
        let (left, top) = viewer.offset();
        let image = viewer.has_viewport().then(|| {
            img(self.path.clone())
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(width))
                .h(px(height))
                .object_fit(ObjectFit::Contain)
        });
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .bg(theme.background)
            .cursor(if self.drag.is_some() { CursorStyle::ClosedHand } else { CursorStyle::OpenHand })
            .child(input)
            .children(image)
            .into_any_element()
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
            .children(self.viewer.map(|viewer| {
                h_flex()
                    .flex_shrink_0()
                    .gap_0p5()
                    .items_center()
                    .child(
                        Button::new("img-zoom-out")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(Ic::Minus))
                            .tooltip(tr(lang, Tr::ImgZoomOut))
                            .on_click(cx.listener(|view, _, _, cx| view.apply_key_action(KeyAction::ZoomOut, cx))),
                    )
                    .child(
                        div()
                            .w(px(40.))
                            .text_center()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(format!("{}%", viewer.percent())),
                    )
                    .child(
                        Button::new("img-zoom-in")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(Ic::Plus))
                            .tooltip(tr(lang, Tr::ImgZoomIn))
                            .on_click(cx.listener(|view, _, _, cx| view.apply_key_action(KeyAction::ZoomIn, cx))),
                    )
                    .child(
                        Button::new("img-fit")
                            .ghost()
                            .xsmall()
                            .label(tr(lang, Tr::ImgFit))
                            .selected(viewer.is_fit())
                            .on_click(cx.listener(|view, _, _, cx| view.apply_key_action(KeyAction::Fit, cx))),
                    )
                    .child(
                        Button::new("img-actual")
                            .ghost()
                            .xsmall()
                            .label("100%")
                            .tooltip(tr(lang, Tr::ImgActualSize))
                            .on_click(cx.listener(|view, _, _, cx| view.apply_key_action(KeyAction::Actual, cx))),
                    )
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
            Phase::Image => match self.viewer {
                Some(viewer) => self.image_viewport(viewer, cx),
                None => div().into_any_element(),
            },
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
                    return;
                }
                if view.viewer.is_some() {
                    let held = m.control || m.alt || m.platform;
                    if let Some(action) = image_viewer::key_action(&ev.keystroke.key, held) {
                        view.apply_key_action(action, cx);
                        cx.stop_propagation();
                        window.prevent_default();
                    }
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
            Tr::ImgFit,
            Tr::ImgActualSize,
            Tr::ImgZoomIn,
            Tr::ImgZoomOut,
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
