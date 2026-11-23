//! The "Hand off to another agent" dialog: pick the target agent, the folder and an optional
//! note. Everything else (brief, terminal, paste) is `AppView::start_handoff`.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    PathPromptOptions, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use sich_proto::{AgentKind, TermId};

use crate::agent_badge::agent_icon_element;
use crate::app_view::AppView;
use crate::handoff::{self, EntryKind, FolderChoice, FolderEntry, HandoffOrigin, HandoffRequest};
use crate::i18n::{Language, Tr, tr};
use crate::settings;
use crate::tab_bar::entry_title;
use crate::workspace::same_path;

/// Whether the terminal's menu offers the handoff: a live terminal with an agent in it.
pub(crate) fn can_handoff(app: &AppView, id: TermId) -> bool {
    app.client.snapshot().terminals.iter().any(|e| e.info.id == id && e.info.exited.is_none() && e.info.agent.is_some())
}

pub(crate) fn open(app: Entity<AppView>, id: TermId, window: &mut Window, cx: &mut App) {
    let snap = app.read(cx).client.snapshot();
    let Some(entry) = snap.terminals.iter().find(|e| e.info.id == id) else { return };
    let Some(status) = entry.info.agent.as_ref() else { return };
    let name = status.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| entry_title(entry));
    let origin = HandoffOrigin {
        kind: status.kind,
        name,
        cwd: entry.info.cwd.clone(),
        worktree: entry.info.worktree.clone(),
        session_id: status.session_id.clone(),
        transcript: None,
        prompt: status.prompt.clone(),
        message: status.message.clone(),
        model: status.model.clone(),
        terminal: Some(id),
    };
    open_origin(app, origin, window, cx);
}

/// The dialog for any origin: a live terminal or a session from the agent history.
pub(crate) fn open_origin(app: Entity<AppView>, origin: HandoffOrigin, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    let entries = handoff::folder_entries(&app.read(cx).projects, origin.worktree.as_deref(), &origin.cwd);
    let form = cx.new(|cx| HandoffDialog::new(app, origin, entries, window, cx));
    window.open_dialog(cx, move |dialog, _window, cx| {
        let lang = settings::language(cx);
        dialog.title(tr(lang, Tr::HandoffTitle)).w(px(480.)).child(form.clone())
    });
}

struct HandoffDialog {
    app: Entity<AppView>,
    origin: HandoffOrigin,
    entries: Vec<FolderEntry>,
    home: Option<PathBuf>,
    selected: usize,
    target: AgentKind,
    note: Entity<TextareaState>,
    _subscription: Subscription,
}

impl HandoffDialog {
    fn new(
        app: Entity<AppView>,
        origin: HandoffOrigin,
        entries: Vec<FolderEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let note = cx.new(|cx| TextareaState::new(window, cx).rows(3));
        let subscription = cx.subscribe(&note, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        Self {
            app,
            target: handoff::default_target(origin.kind),
            origin,
            entries,
            home: dirs::home_dir(),
            selected: 0,
            note,
            _subscription: subscription,
        }
    }

    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update(cx, |this, cx| {
                this.add_custom(path);
                cx.notify();
            });
        })
        .detach();
    }

    /// Adds `path` as an extra entry (or selects the equal one that is already listed).
    fn add_custom(&mut self, path: PathBuf) {
        let existing = self.entries.iter().position(|e| {
            matches!(&e.choice, FolderChoice::Path(p) if same_path(p, &path))
        });
        self.selected = match existing {
            Some(index) => index,
            None => {
                let at = self.entries.len();
                self.entries.push(FolderEntry { kind: EntryKind::Custom, choice: FolderChoice::Path(path) });
                at
            }
        };
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(self.selected) else { return };
        let request = HandoffRequest {
            origin: self.origin.clone(),
            target: self.target,
            folder: entry.choice.clone(),
            note: self.note.read(cx).value().to_string(),
        };
        self.app.update(cx, |app, cx| app.start_handoff(request, window, cx));
        window.close_dialog(cx);
    }

    fn entry_texts(&self, entry: &FolderEntry, lang: Language) -> (String, String) {
        let path = entry.path(self.home.as_deref()).map(|p| p.display().to_string()).unwrap_or_default();
        let label = match &entry.kind {
            EntryKind::Current => tr(lang, Tr::HandoffCurrent).to_owned(),
            EntryKind::Worktree { project, branch } => format!("{project} / {branch}"),
            EntryKind::Home => tr(lang, Tr::HandoffHome).to_owned(),
            EntryKind::Custom => path.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or(&path).to_owned(),
        };
        (label, path)
    }
}

impl Render for HandoffDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = settings::language(cx);
        let theme = cx.theme().clone();
        let label = |text: &'static str| div().text_sm().text_color(theme.muted_foreground).child(text);

        let from = h_flex()
            .gap_1()
            .items_center()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(format!("{}:", tr(lang, Tr::HandoffFrom)))
            .child(agent_icon_element(self.origin.kind, 14., theme.foreground, theme.muted_foreground))
            .child(div().min_w_0().truncate().child(format!(
                "{} \u{b7} {}", handoff::kind_name(self.origin.kind), self.origin.name
            )));

        let mut agents = h_flex().gap_2();
        for (index, kind) in handoff::TARGETS.into_iter().enumerate() {
            let button = Button::new(("handoff-agent", index)).small();
            let button = if self.target == kind { button.primary() } else { button };
            agents = agents.child(
                button
                    .label(handoff::kind_name(kind))
                    .selected(self.target == kind)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.target = kind;
                        cx.notify();
                    })),
            );
        }

        let mut list = v_flex().gap_px();
        for (index, entry) in self.entries.iter().enumerate() {
            let (title, path) = self.entry_texts(entry, lang);
            let selected = index == self.selected;
            let mut row = h_flex()
                .id(("handoff-folder", index))
                .w_full()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_sm()
                .child(div().flex_shrink_0().child(title));
            row = if selected { row.bg(theme.accent) } else { row.hover(|s| s.bg(theme.accent.opacity(0.5))) };
            list = list.child(
                row.child(div().flex_1().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(path))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.selected = index;
                        cx.notify();
                    })),
            );
        }
        let list = div().id("handoff-folders").max_h(px(220.)).overflow_y_scroll().child(list);

        v_flex()
            .gap_3()
            .child(from)
            .child(v_flex().gap_1().child(label(tr(lang, Tr::HandoffAgent))).child(agents))
            .child(
                v_flex()
                    .gap_1()
                    .child(label(tr(lang, Tr::HandoffFolder)))
                    .child(list)
                    .child(
                        h_flex().child(
                            Button::new("handoff-choose-folder")
                                .small()
                                .label(tr(lang, Tr::HandoffChooseFolder))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.choose_folder(window, cx))),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(label(tr(lang, Tr::HandoffNote)))
                    .child(Textarea::new(&self.note).small()),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("handoff-cancel")
                            .label(tr(lang, Tr::Cancel))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("handoff-submit")
                            .primary()
                            .label(tr(lang, Tr::HandoffSubmit))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
