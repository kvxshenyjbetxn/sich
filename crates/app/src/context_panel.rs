//! Claude context usage UI: the status-bar item and the "Context" tab of the right panel.
//! View state, polling and the pure decisions live here; `claude_ctx` owns the data.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use futures::FutureExt as _;
use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, Theme, h_flex, v_flex};
use gpui_kit::{
    AnyElement, ClickEvent, Context, Div, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, px, relative,
};
use sich_proto::{AgentKind, TermId, TerminalInfo};

use crate::agent_badge::shorten_model;
use crate::app_view::AppView;
use crate::claude_ctx::{
    self, AgentRun, CatKind, Category, CtxView, Loader, Target, Tone, format_tokens, percent_label, percent_of,
};
use crate::ctx_files::{self, Env, Item, ResolveCache, Resolved};
use crate::daemon_client::TerminalOwner;
use crate::i18n::{Language, Tr, tr};
use crate::workspace::WorkspaceKey;

const POLL: Duration = Duration::from_secs(2);
const STALE_TOOLTIP_SECS: u64 = 10 * 60;

// ---------------------------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Section {
    Subagents,
    Mcp,
    Memory,
    Skills,
    Agents,
}

impl Section {
    fn default_open(self) -> bool {
        self == Section::Subagents
    }
}

/// A section is open when its default differs from the number of user toggles (odd = flipped).
pub(crate) fn section_open(section: Section, toggled: &HashSet<Section>) -> bool {
    section.default_open() != toggled.contains(&section)
}

/// Live Claude terminals with the ids their data files are keyed by.
pub(crate) fn claude_targets(
    infos: &[TerminalInfo],
    owner_of: impl Fn(TermId) -> Option<TerminalOwner>,
) -> Vec<Target> {
    infos
        .iter()
        .filter(|info| info.exited.is_none())
        .filter_map(|info| {
            let agent = info.agent.as_ref().filter(|agent| agent.kind == AgentKind::Claude)?;
            let owner = owner_of(info.id)?;
            Some(Target {
                key: info.id,
                session_id: agent.session_id.clone(),
                raw_id: owner.id.to_string(),
                socket: owner.socket,
                title: info.title.clone(),
                worktree: info.worktree.clone(),
            })
        })
        .collect()
}

/// The terminal whose context is shown: the pinned one while it still exists, else "Auto".
pub(crate) fn selected_terminal(
    ids: &[TermId],
    pinned: Option<TermId>,
    focused: Option<TermId>,
    active: Option<TermId>,
) -> Option<TermId> {
    pinned.filter(|id| ids.contains(id)).or_else(|| claude_ctx::auto_target(ids, focused, active))
}

/// Chip label: the terminal title cut to `max` characters.
pub(crate) fn short_title(title: &str, max: usize) -> String {
    let title = title.trim();
    if title.is_empty() {
        return "Claude".into();
    }
    if title.chars().count() <= max {
        return title.into();
    }
    let cut: String = title.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// The agent switcher is only useful when there is something to choose from.
pub(crate) fn switcher_visible(claude_count: usize) -> bool {
    claude_count > 1
}

/// Switcher button text: "<Auto> · <followed agent>" in auto mode, the pinned agent's title otherwise.
pub(crate) fn switcher_label(auto: &str, pinned: bool, selected_title: &str) -> String {
    let title = short_title(selected_title, 24);
    if pinned { title } else { format!("{auto} · {title}") }
}

/// Menu row of one terminal: "Title · 18%" ("–" while its percentage is unknown).
pub(crate) fn switcher_item_label(title: &str, percent: Option<f64>) -> String {
    format!("{} · {}", short_title(title, 24), percent.map(percent_label).unwrap_or_else(|| "–".into()))
}

/// "5s", "3m 4s", "2h 5m" with the units of the language.
pub(crate) fn span_text(lang: Language, secs: u64) -> String {
    let (s, m, h) = match lang {
        Language::Uk => ("с", "хв", "г"),
        Language::Ru => ("с", "мин", "ч"),
        Language::En => ("s", "m", "h"),
    };
    if secs < 60 {
        format!("{secs}{s}")
    } else if secs < 3600 {
        format!("{}{m} {}{s}", secs / 60, secs % 60)
    } else {
        format!("{}{h} {}{m}", secs / 3600, secs % 3600 / 60)
    }
}

pub(crate) fn updated_ago(lang: Language, view: &CtxView, now_ms: u64) -> String {
    tr(lang, Tr::CtxUpdatedAgo).replace("{t}", &span_text(lang, claude_ctx::age_secs(view.updated_ms, now_ms)))
}

/// Status-bar tooltip: "<model> · 55.7k / 1M", plus the age when the data is older than 10 minutes.
pub(crate) fn tooltip_text(lang: Language, view: &CtxView, now_ms: u64) -> String {
    let mut parts = Vec::new();
    parts.extend(view.model.clone());
    match (view.used, view.max) {
        (Some(used), Some(max)) => parts.push(format!("{} / {}", format_tokens(used), format_tokens(max))),
        (Some(used), None) => parts.push(format_tokens(used)),
        _ => {}
    }
    if view.updated_ms > 0 && claude_ctx::age_secs(view.updated_ms, now_ms) > STALE_TOOLTIP_SECS {
        parts.push(updated_ago(lang, view, now_ms));
    }
    parts.join(" · ")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunState {
    Running,
    Done,
    Failed,
    Other,
}

pub(crate) fn run_state(status: Option<&str>) -> RunState {
    match status.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("running" | "in_progress" | "active") => RunState::Running,
        Some("done" | "completed" | "complete" | "success" | "finished") => RunState::Done,
        Some("failed" | "error" | "errored" | "cancelled" | "canceled") => RunState::Failed,
        _ => RunState::Other,
    }
}

/// Newest run first (runs without a start time go last).
pub(crate) fn runs_newest_first(runs: &[AgentRun]) -> Vec<&AgentRun> {
    let mut out: Vec<&AgentRun> = runs.iter().collect();
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    out
}

/// Distinct colors that read on both dark and light themes.
const PALETTE: [u32; 10] = [
    0x4f8ff7, 0xf59e42, 0xa78bfa, 0x3fb97a, 0xf472b6, 0x2dd4bf, 0xe5c046, 0xf0645c, 0x38bdf8, 0x94a3b8,
];

/// The preferred palette slot of a Claude Code theme color name.
fn named_slot(name: &str) -> Option<usize> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "promptborder" => 0,
        "claude" => 1,
        "permission" => 2,
        "success" => 3,
        "autoaccept" => 4,
        "planmode" => 5,
        "warning" => 6,
        "error" => 7,
        "suggestion" => 8,
        "inactive" => 9,
        "bashborder" => 4,
        _ if lower.starts_with("blue") => 0,
        _ if lower.starts_with("orange") => 1,
        _ if lower.starts_with("purple") => 2,
        _ if lower.starts_with("green") => 3,
        _ if lower.starts_with("pink") => 4,
        _ if lower.starts_with("teal") || lower.starts_with("cyan") => 5,
        _ if lower.starts_with("yellow") => 6,
        _ if lower.starts_with("red") => 7,
        _ if lower.starts_with("gray") || lower.starts_with("grey") => 9,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Swatch {
    Color(usize),
    Free,
    Buffer,
    Deferred,
}

/// A swatch per category. Used rows get distinct palette slots: the named slot when free, else
/// the next unused one (unknown names are placed by row index).
pub(crate) fn assign_swatches(categories: &[Category]) -> Vec<Swatch> {
    let mut taken = [false; PALETTE.len()];
    categories
        .iter()
        .enumerate()
        .map(|(index, cat)| match cat.kind {
            CatKind::Free => Swatch::Free,
            CatKind::Buffer => Swatch::Buffer,
            CatKind::Deferred => Swatch::Deferred,
            CatKind::Used => {
                let preferred = cat.color.as_deref().and_then(named_slot).unwrap_or(index % PALETTE.len());
                let slot = (0..PALETTE.len())
                    .map(|step| (preferred + step) % PALETTE.len())
                    .find(|&slot| !taken[slot])
                    .unwrap_or(preferred);
                taken[slot] = true;
                Swatch::Color(slot)
            }
        })
        .collect()
}

/// Width fractions (of the bar) for the drawn categories; `None` = not drawn (deferred).
pub(crate) fn bar_fractions(categories: &[Category], max: Option<u64>) -> Vec<Option<f32>> {
    let drawn: u64 = categories.iter().filter(|c| c.kind != CatKind::Deferred).map(|c| c.tokens).sum();
    let total = max.filter(|&m| m > 0).unwrap_or(drawn).max(1);
    let mut left = 1.0_f32;
    categories
        .iter()
        .map(|cat| {
            (cat.kind != CatKind::Deferred).then(|| {
                let frac = (cat.tokens as f32 / total as f32).clamp(0., left);
                left -= frac;
                frac
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct ContextUi {
    /// Latest data per GUI terminal key.
    views: HashMap<TermId, CtxView>,
    pins: Vec<(WorkspaceKey, TermId)>,
    toggled: HashSet<Section>,
    /// Files of the panel items that exist on disk (see `ctx_files`).
    resolved: HashMap<Item, Resolved>,
}

impl ContextUi {
    pub(crate) fn view(&self, id: TermId) -> Option<&CtxView> {
        self.views.get(&id)
    }

    pub(crate) fn pinned(&self, key: &WorkspaceKey) -> Option<TermId> {
        self.pins.iter().find(|(k, _)| k == key).map(|&(_, id)| id)
    }

    pub(crate) fn set_pin(&mut self, key: WorkspaceKey, id: Option<TermId>) {
        self.pins.retain(|(k, _)| *k != key);
        if let Some(id) = id {
            self.pins.push((key, id));
        }
    }

    fn toggle(&mut self, section: Section) {
        if !self.toggled.remove(&section) {
            self.toggled.insert(section);
        }
    }

    /// Replaces the resolved files; true when anything changed.
    fn set_resolved(&mut self, resolved: HashMap<Item, Resolved>) -> bool {
        let changed = self.resolved != resolved;
        self.resolved = resolved;
        changed
    }

    /// Replaces the data; true when anything changed.
    fn replace(&mut self, views: HashMap<TermId, CtxView>) -> bool {
        let changed = self.views != views;
        self.views = views;
        changed
    }

    /// Background polling of the data directory. While the window is inactive nothing is read
    /// and the last data stays (the activation wake reloads it); with no targets or no data
    /// dir the store is cleared.
    pub(crate) fn watch(window: &mut Window, cx: &Context<AppView>) {
        let (wake, rx) = async_channel::bounded::<()>(1);
        cx.observe_window_activation(window, move |_, window, _| {
            if window.is_window_active() {
                let _ = wake.try_send(());
            }
        })
        .detach();
        cx.background_executor()
            .spawn(async {
                if let Some(dir) = claude_ctx::data_dir() {
                    let now = std::time::SystemTime::now();
                    claude_ctx::prune_dir(&dir, now, claude_ctx::PRUNE_AGE);
                    crate::claude_images::prune_img_dirs(&dir, now, claude_ctx::PRUNE_AGE);
                }
            })
            .detach();
        cx.spawn_in(window, async move |view, cx| {
            let mut loader = Loader::default();
            let mut cache = ResolveCache::default();
            loop {
                let timer = cx.background_executor().timer(POLL).fuse();
                let wakeup = rx.recv().fuse();
                futures::pin_mut!(timer, wakeup);
                futures::select! {
                    _ = timer => {},
                    result = wakeup => if result.is_err() { break },
                }
                let Ok((targets, active)) = view.update_in(cx, |view, window, _| {
                    (view.ctx_targets(), window.is_window_active())
                }) else {
                    break;
                };
                let data_dir = claude_ctx::data_dir();
                let step = poll_step(active, !targets.is_empty(), data_dir.is_some());
                if step == PollStep::Keep {
                    continue;
                }
                let Some(dir) = data_dir.filter(|_| step == PollStep::Load) else {
                    // Nothing to read: drop stale data of terminals that are gone.
                    if view.update(cx, |view, cx| {
                        if view.ctx_ui.replace(HashMap::new()) {
                            cx.notify();
                        }
                    }).is_err() {
                        break;
                    }
                    continue;
                };
                let (next, next_cache, views) = cx
                    .background_executor()
                    .spawn(async move {
                        let views = loader.load(&dir, &targets);
                        let mut cache = cache;
                        refresh_resolved(&mut cache, views.values(), claude_ctx::now_ms());
                        (loader, cache, views)
                    })
                    .await;
                loader = next;
                let resolved = next_cache.snapshot();
                cache = next_cache;
                if view
                    .update(cx, |view, cx| {
                        let changed = view.ctx_ui.replace(views);
                        if view.ctx_ui.set_resolved(resolved) || changed {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
}

/// What a poll tick does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollStep {
    /// Window inactive: keep the last data, read nothing.
    Keep,
    /// Nothing to show: clear the store.
    Clear,
    /// Read the data directory.
    Load,
}

fn poll_step(active: bool, has_targets: bool, has_dir: bool) -> PollStep {
    if !has_targets || !has_dir {
        PollStep::Clear
    } else if !active {
        PollStep::Keep
    } else {
        PollStep::Load
    }
}

/// Resolves the items that are new or stale in `cache` (blocking file reads: background only).
fn refresh_resolved<'a>(cache: &mut ResolveCache, views: impl Iterator<Item = &'a CtxView>, now_ms: u64) {
    let items: Vec<Item> = views.flat_map(ctx_files::items_of).collect();
    let due = cache.due(&items, now_ms);
    if due.is_empty() {
        return;
    }
    let results = match Env::from_system() {
        Some(env) => ctx_files::resolve_all(&env, due, &|path| path.is_file()),
        None => due.into_iter().map(|item| (item, None)).collect(),
    };
    cache.store(results, now_ms);
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

fn tone_color(tone: Tone, theme: &Theme) -> Hsla {
    match tone {
        Tone::Good => theme.success,
        Tone::Warning => theme.warning,
        Tone::Danger => theme.danger,
        Tone::Muted => theme.muted_foreground,
    }
}

fn palette_color(slot: usize) -> Hsla {
    gpui_kit::rgb(PALETTE[slot % PALETTE.len()]).into()
}

fn swatch_color(swatch: Swatch, theme: &Theme) -> Hsla {
    match swatch {
        Swatch::Color(slot) => palette_color(slot),
        Swatch::Free => theme.muted_foreground.opacity(0.25),
        Swatch::Buffer => theme.muted_foreground.opacity(0.5),
        Swatch::Deferred => theme.muted_foreground.opacity(0.15),
    }
}

/// "ctx [bar] 6%" for the focused Claude terminal; clicking opens the Context tab.
pub(crate) fn status_item(view: &CtxView, lang: Language, now_ms: u64, theme: &Theme, cx: &Context<AppView>) -> AnyElement {
    let tone = claude_ctx::tone(view.percent);
    let color = tone_color(tone, theme);
    let fill = view.percent.map_or(0., |p| (p / 100.).clamp(0., 1.)) as f32;
    let label = view.percent.map(percent_label).unwrap_or_else(|| "–".into());
    let tip = tooltip_text(lang, view, now_ms);
    h_flex()
        .id("ctx-status")
        .flex_shrink_0()
        .gap_1()
        .items_center()
        .cursor_pointer()
        // Same text size as the other status-bar items, so the baselines line up.
        .text_xs()
        .child(div().text_color(theme.muted_foreground).child("ctx"))
        .child(
            // 1 px down: centred on the lowercase label rather than the line box.
            div().mt(px(1.)).w(px(64.)).h(px(6.)).rounded_full().bg(theme.muted_foreground.opacity(0.25)).overflow_hidden()
                .child(div().h_full().w(relative(fill)).rounded_full().bg(color)),
        )
        .child(div().text_color(color).child(label))
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.show_context_tab(cx)))
        .into_any_element()
}

fn section_header(
    ui_id: &'static str,
    section: Section,
    count: String,
    open: bool,
    lang: Language,
    theme: &Theme,
    cx: &Context<AppView>,
) -> AnyElement {
    h_flex()
        .id(ui_id)
        .w_full()
        .gap_1()
        .items_center()
        .cursor_pointer()
        .text_size(px(12.))
        .child(Icon::new(if open { Ic::ChevronDown } else { Ic::ChevronRight }).small().text_color(theme.muted_foreground))
        .child(div().font_semibold().child(tr(lang, label_key(section))))
        .child(div().text_color(theme.muted_foreground).child(count))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.ctx_ui.toggle(section);
            cx.notify();
        }))
        .into_any_element()
}

fn label_key(section: Section) -> Tr {
    match section {
        Section::Subagents => Tr::CtxSubagents,
        Section::Mcp => Tr::CtxMcp,
        Section::Memory => Tr::CtxMemory,
        Section::Skills => Tr::CtxSkills,
        Section::Agents => Tr::CtxAgents,
    }
}

fn muted_row(theme: &Theme) -> Div {
    h_flex().w_full().gap_2().items_center().text_size(px(11.5)).text_color(theme.muted_foreground)
}

fn name_cell(text: impl Into<gpui_kit::SharedString>) -> Div {
    div().flex_1().min_w_0().truncate().child(text.into())
}

fn render_categories(view: &CtxView, lang: Language, theme: &Theme) -> AnyElement {
    let swatches = assign_swatches(&view.categories);
    let fractions = bar_fractions(&view.categories, view.max);
    let segments = view.categories.iter().zip(&swatches).zip(&fractions).filter_map(|((cat, swatch), frac)| {
        let frac = (*frac)?;
        (frac > 0.).then(|| {
            let seg = div().h_full().w(relative(frac));
            if cat.kind == CatKind::Free { seg } else { seg.bg(swatch_color(*swatch, theme)) }
        })
    });
    let bar = h_flex()
        .w_full()
        .h(px(8.))
        .rounded_full()
        .overflow_hidden()
        .bg(theme.muted_foreground.opacity(0.2))
        .children(segments);
    let rows = view.categories.iter().zip(&swatches).map(|(cat, swatch)| {
        let muted = matches!(cat.kind, CatKind::Free | CatKind::Buffer | CatKind::Deferred);
        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .text_size(px(12.))
            .text_color(if muted { theme.muted_foreground } else { theme.foreground })
            .child(div().flex_shrink_0().size(px(8.)).rounded(px(2.)).bg(swatch_color(*swatch, theme)))
            .child(name_cell(cat.name.clone()))
            .child(div().flex_shrink_0().child(format_tokens(cat.tokens)))
            .child(
                div().flex_shrink_0().w(px(48.)).text_right().text_color(theme.muted_foreground)
                    .child(percent_of(cat.tokens, view.max)),
            )
    });
    let note = (!view.detailed).then(|| {
        div().text_size(px(11.)).text_color(theme.muted_foreground).child(tr(lang, Tr::CtxNoMod))
    });
    v_flex().w_full().gap_2().child(bar).child(v_flex().w_full().gap_1().children(rows)).children(note).into_any_element()
}

fn render_header(view: &CtxView, lang: Language, theme: &Theme) -> AnyElement {
    let color = tone_color(claude_ctx::tone(view.percent), theme);
    let percent = view.percent.map(percent_label).unwrap_or_else(|| "–".into());
    let tokens = match (view.used, view.max) {
        (Some(used), Some(max)) => format!("{} / {} {}", format_tokens(used), format_tokens(max), tr(lang, Tr::CtxTokens)),
        (Some(used), None) => format!("{} {}", format_tokens(used), tr(lang, Tr::CtxTokens)),
        _ => String::new(),
    };
    let model = [view.model.clone(), view.effort.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    h_flex()
        .w_full()
        .gap_2()
        .items_start()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    h_flex().gap_1().items_end()
                        .child(div().text_size(px(22.)).font_semibold().text_color(color).child(percent))
                        .child(div().pb(px(3.)).text_size(px(12.)).text_color(theme.muted_foreground).child(tr(lang, Tr::CtxUsed))),
                )
                .child(div().text_size(px(12.)).child(tokens))
                .child(div().text_size(px(11.5)).text_color(theme.muted_foreground).child(model)),
        )
        .into_any_element()
}

fn render_runs(view: &CtxView, lang: Language, theme: &Theme) -> AnyElement {
    let rows = runs_newest_first(&view.runs).into_iter().map(|run| {
        let (state_text, state_color) = match run_state(run.status.as_deref()) {
            RunState::Running => (tr(lang, Tr::CtxRunning).to_string(), theme.primary),
            RunState::Done => (tr(lang, Tr::CtxDone).to_string(), theme.success),
            RunState::Failed => (tr(lang, Tr::CtxFailed).to_string(), theme.danger),
            RunState::Other => (run.status.clone().unwrap_or_default(), theme.muted_foreground),
        };
        let title = [run.kind.clone(), run.description.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        let mut meta = Vec::new();
        if let Some(ctx) = run.last_ctx {
            meta.push(format!("ctx {}", format_tokens(ctx)));
        }
        if let Some(total) = run.total {
            meta.push(format!("{} {}", format_tokens(total), tr(lang, Tr::CtxTokens)));
        }
        if let Some(model) = run.model.as_deref().map(shorten_model).filter(|m| !m.is_empty()) {
            meta.push(model);
        }
        v_flex()
            .w_full()
            .child(
                h_flex().w_full().gap_2().text_size(px(12.))
                    .child(name_cell(title))
                    .child(div().flex_shrink_0().text_size(px(11.)).text_color(state_color).child(state_text)),
            )
            .child(div().text_size(px(11.)).text_color(theme.muted_foreground).child(meta.join(" · ")))
    });
    v_flex().w_full().gap_1p5().pl_4().children(rows).into_any_element()
}

/// What the popup menu of a file row can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileAction {
    OpenInTab,
    OpenDefault,
    Reveal,
    CopyPath,
}

/// The menu entries in order, with their labels.
pub(crate) const FILE_ACTIONS: [(FileAction, Tr); 4] = [
    (FileAction::OpenInTab, Tr::CtxOpenInTab),
    (FileAction::OpenDefault, Tr::CtxOpenDefault),
    (FileAction::Reveal, Tr::CtxShowInFolder),
    (FileAction::CopyPath, Tr::CtxCopyPath),
];

impl AppView {
    pub(crate) fn run_file_action(&mut self, action: FileAction, file: &Resolved, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            FileAction::OpenInTab => self.open_file_tab(file.path.clone(), file.read_only, window, cx),
            FileAction::OpenDefault => crate::os::open_default(&file.path),
            FileAction::Reveal => crate::os::reveal_in_file_manager(&file.path),
            FileAction::CopyPath => {
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(file.path.display().to_string()));
            }
        }
    }
}

/// A row that opens a menu when its item resolves to a file; plain content otherwise.
fn file_row(
    id: String,
    content: Div,
    item: Option<&Item>,
    ui: &ContextUi,
    lang: Language,
    cx: &Context<AppView>,
) -> AnyElement {
    let Some(file) = item.and_then(|item| ui.resolved.get(item)).cloned() else {
        return content.into_any_element();
    };
    let app = cx.weak_entity();
    Button::new(gpui_kit::SharedString::from(id))
        .ghost()
        .xsmall()
        .w_full()
        .h_auto()
        .py_0p5()
        .justify_start()
        .tooltip(file.path.display().to_string())
        .child(content.w_full())
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu;
            for (action, label) in FILE_ACTIONS {
                let (app, file) = (app.clone(), file.clone());
                menu = menu.item(PopupMenuItem::new(tr(lang, label)).on_click(
                    move |_: &ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                        let _ = app.update(cx, |this, cx| this.run_file_action(action, &file, window, cx));
                    },
                ));
            }
            menu
        })
        .into_any_element()
}

fn render_mcp(view: &CtxView, ui: &ContextUi, lang: Language, theme: &Theme, cx: &Context<AppView>) -> AnyElement {
    let root = view.root.as_deref();
    let rows = view.mcp.iter().enumerate().map(|(index, item)| {
        let label = match item.detail.as_deref().filter(|d| !d.is_empty()) {
            Some(server) => format!("{} · {server}", item.name),
            None => item.name.clone(),
        };
        let row = muted_row(theme)
            .text_color(if item.loaded { theme.foreground } else { theme.muted_foreground })
            .child(name_cell(label))
            .child(div().flex_shrink_0().child(format_tokens(item.tokens)));
        file_row(format!("ctx-mcp-{index}"), row, ctx_files::mcp_item(item, root).as_ref(), ui, lang, cx)
    });
    v_flex().w_full().gap_1().pl_4().children(rows).into_any_element()
}

fn render_memory(view: &CtxView, ui: &ContextUi, lang: Language, theme: &Theme, cx: &Context<AppView>) -> AnyElement {
    let rows = view.memory.iter().enumerate().map(|(index, item)| {
        let row = v_flex()
            .w_full()
            .child(
                h_flex().w_full().gap_2().text_size(px(12.))
                    .child(name_cell(item.name.clone()))
                    .child(div().flex_shrink_0().child(format_tokens(item.tokens))),
            )
            .children(item.detail.clone().filter(|d| !d.is_empty()).map(|path| {
                div().w_full().truncate().text_size(px(11.)).text_color(theme.muted_foreground).child(path)
            }));
        file_row(format!("ctx-mem-{index}"), row, ctx_files::memory_item(item).as_ref(), ui, lang, cx)
    });
    v_flex().w_full().gap_1p5().pl_4().children(rows).into_any_element()
}

fn render_skills(view: &CtxView, ui: &ContextUi, lang: Language, theme: &Theme, cx: &Context<AppView>) -> AnyElement {
    let root = view.root.as_deref();
    let rows = view.skills.iter().flat_map(|s| s.items.iter()).enumerate().map(|(index, item)| {
        let row = muted_row(theme).child(name_cell(item.name.clone())).child(div().flex_shrink_0().child(format_tokens(item.tokens)));
        file_row(format!("ctx-skill-{index}"), row, Some(&ctx_files::skill_item(item, root)), ui, lang, cx)
    });
    v_flex().w_full().gap_1().pl_4().children(rows).into_any_element()
}

fn render_agents(view: &CtxView, ui: &ContextUi, lang: Language, theme: &Theme, cx: &Context<AppView>) -> AnyElement {
    let root = view.root.as_deref();
    let rows = view.agents.iter().enumerate().map(|(index, item)| {
        let source = item.source.as_deref().filter(|s| !s.is_empty()).map(|s| format!("{} · {s}", item.name));
        let row = muted_row(theme)
            .child(name_cell(source.unwrap_or_else(|| item.name.clone())))
            .child(div().flex_shrink_0().child(format_tokens(item.tokens)));
        file_row(format!("ctx-agent-{index}"), row, Some(&ctx_files::agent_item(item, root)), ui, lang, cx)
    });
    v_flex().w_full().gap_1().pl_4().children(rows).into_any_element()
}

fn session_line(label: &'static str, value: String, theme: &Theme) -> Div {
    h_flex().w_full().gap_2().text_size(px(12.))
        .child(div().flex_1().text_color(theme.muted_foreground).child(label))
        .child(div().flex_shrink_0().child(value))
}

fn render_session(view: &CtxView, ui: &ContextUi, lang: Language, theme: &Theme, cx: &Context<AppView>) -> AnyElement {
    let mut lines: Vec<AnyElement> = Vec::new();
    if let Some(cost) = view.cost_usd {
        lines.push(session_line(tr(lang, Tr::CtxCost), format!("${cost:.2}"), theme).into_any_element());
    }
    if let Some(ms) = view.duration_ms {
        lines.push(session_line(tr(lang, Tr::CtxDuration), span_text(lang, ms / 1000), theme).into_any_element());
    }
    if let Some(ms) = view.api_duration_ms {
        lines.push(session_line(tr(lang, Tr::CtxApiDuration), span_text(lang, ms / 1000), theme).into_any_element());
    }
    if view.lines_added.is_some() || view.lines_removed.is_some() {
        let text = format!("+{} / \u{2212}{}", view.lines_added.unwrap_or(0), view.lines_removed.unwrap_or(0));
        lines.push(session_line(tr(lang, Tr::CtxLines), text, theme).into_any_element());
    }
    if let Some(cache) = &view.cache {
        let mut parts = vec![tr(lang, if cache.warm == Some(true) { Tr::CtxCacheWarm } else { Tr::CtxCacheCold }).to_string()];
        parts.extend(cache.ttl.clone().filter(|t| !t.is_empty()));
        parts.extend(cache.hit_ratio.map(|r| format!("{}%", (r * 100.).round().clamp(0., 100.) as u32)));
        lines.push(session_line(tr(lang, Tr::CtxCache), parts.join(" · "), theme).into_any_element());
    }
    if !view.session_id.is_empty() {
        lines.push(session_line(tr(lang, Tr::CtxSessionId), view.session_id.chars().take(8).collect(), theme).into_any_element());
    }
    if let Some(path) = &view.transcript_path {
        let line = session_line(tr(lang, Tr::CtxTranscript), crate::file_doc::file_name(path), theme);
        lines.push(file_row("ctx-transcript".into(), line, Some(&Item::Transcript(path.clone())), ui, lang, cx));
    }
    v_flex().w_full().gap_1().children(lines).into_any_element()
}

fn divider(label: &'static str, theme: &Theme) -> Div {
    div().w_full().pt_2().border_t_1().border_color(theme.border).text_size(px(12.)).font_semibold().child(label)
}

pub(crate) fn centered(text: &'static str, theme: &Theme) -> AnyElement {
    div().flex_1().w_full().p_4().text_size(px(12.5)).text_color(theme.muted_foreground).child(text).into_any_element()
}

/// Everything the panel needs from the app, already narrowed to the selected workspace.
pub(crate) struct PanelInput {
    pub targets: Vec<Target>,
    pub key: WorkspaceKey,
    pub focused: Option<TermId>,
    pub active: Option<TermId>,
    pub now_ms: u64,
}

/// The selected Claude terminal of the panel and the pinned one (`None` = "Auto"). The pin is
/// per workspace and shared by every tab that shows a Claude session (Context, Images).
pub(crate) fn selection(ui: &ContextUi, input: &PanelInput) -> (Option<TermId>, Option<TermId>) {
    let ids: Vec<TermId> = input.targets.iter().map(|t| t.key).collect();
    let pinned = ui.pinned(&input.key).filter(|id| ids.contains(id));
    (selected_terminal(&ids, pinned, input.focused, input.active), pinned)
}

/// The agent dropdown (only when there is more than one Claude terminal to choose from).
pub(crate) fn render_switcher(
    button_id: &'static str,
    ui: &ContextUi,
    input: &PanelInput,
    selected: TermId,
    pinned: Option<TermId>,
    lang: Language,
    cx: &Context<AppView>,
) -> Option<Div> {
    switcher_visible(input.targets.len()).then(|| {
        let selected_title = input.targets.iter().find(|t| t.key == selected).map_or("", |t| t.title.as_str());
        let label = switcher_label(tr(lang, Tr::CtxAuto), pinned.is_some(), selected_title);
        let auto = tr(lang, Tr::CtxAuto).to_string();
        let items: Vec<(TermId, String)> = input
            .targets
            .iter()
            .map(|t| (t.key, switcher_item_label(&t.title, ui.view(t.key).and_then(|v| v.percent))))
            .collect();
        let (app, key) = (cx.weak_entity(), input.key.clone());
        let button = Button::new(button_id)
            .ghost()
            .small()
            .max_w_full()
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1()
                    .child(div().min_w_0().truncate().child(label))
                    .child(Icon::new(Ic::ChevronDown).size(px(12.)).flex_shrink_0()),
            )
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu.max_h(px(300.)).scrollable(true);
                let pin = |id: Option<TermId>| {
                    let (app, key) = (app.clone(), key.clone());
                    move |_: &ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                        let _ = app.update(cx, |this, cx| {
                            this.ctx_ui.set_pin(key.clone(), id);
                            this.images_ui.poke();
                            cx.notify();
                        });
                    }
                };
                menu = menu.item(PopupMenuItem::new(auto.clone()).checked(pinned.is_none()).on_click(pin(None)));
                for (id, text) in &items {
                    menu = menu.item(PopupMenuItem::new(text.clone()).checked(pinned == Some(*id)).on_click(pin(Some(*id))));
                }
                menu
            });
        div().flex_1().min_w_0().child(button)
    })
}

pub(crate) fn render_panel(ui: &ContextUi, input: PanelInput, lang: Language, cx: &Context<AppView>) -> AnyElement {
    let theme = cx.theme().clone();
    let theme = &theme;
    let (selected, pinned) = selection(ui, &input);
    let Some(selected) = selected else {
        return centered(tr(lang, Tr::CtxNoClaude), theme);
    };

    let switcher = render_switcher("ctx-switcher", ui, &input, selected, pinned, lang, cx);
    let updated = ui.view(selected).map(|view| {
        div().flex_shrink_0().text_size(px(11.)).text_color(theme.muted_foreground).child(updated_ago(lang, view, input.now_ms))
    });
    let top = (switcher.is_some() || updated.is_some()).then(|| {
        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(switcher.unwrap_or_else(|| div().flex_1().min_w_0()))
            .children(updated)
    });

    let body: Vec<AnyElement> = match ui.view(selected) {
        None => vec![centered(tr(lang, Tr::CtxWaiting), theme)],
        Some(view) => {
            let mut out = vec![render_header(view, lang, theme)];
            if !view.categories.is_empty() {
                out.push(render_categories(view, lang, theme));
            } else if !view.detailed {
                out.push(div().text_size(px(11.)).text_color(theme.muted_foreground).child(tr(lang, Tr::CtxNoMod)).into_any_element());
            }
            let sections: [(Section, &'static str, String, bool); 5] = [
                (Section::Subagents, "ctx-sec-runs", view.runs.len().to_string(), !view.runs.is_empty()),
                (
                    Section::Mcp,
                    "ctx-sec-mcp",
                    format!("{}/{}", view.mcp.iter().filter(|m| m.loaded).count(), view.mcp.len()),
                    !view.mcp.is_empty(),
                ),
                (Section::Memory, "ctx-sec-memory", view.memory.len().to_string(), !view.memory.is_empty()),
                (
                    Section::Skills,
                    "ctx-sec-skills",
                    view.skills.as_ref().map_or_else(String::new, |s| {
                        let count = s.count.unwrap_or(s.items.len() as u64);
                        match s.tokens {
                            Some(tokens) => format!("{count} · {}", format_tokens(tokens)),
                            None => count.to_string(),
                        }
                    }),
                    view.skills.as_ref().is_some_and(|s| s.count.unwrap_or(0) > 0 || !s.items.is_empty()),
                ),
                (Section::Agents, "ctx-sec-agents", view.agents.len().to_string(), !view.agents.is_empty()),
            ];
            for (section, id, count, present) in sections {
                if !present {
                    continue;
                }
                let open = section_open(section, &ui.toggled);
                out.push(section_header(id, section, count, open, lang, theme, cx));
                if open {
                    out.push(match section {
                        Section::Subagents => render_runs(view, lang, theme),
                        Section::Mcp => render_mcp(view, ui, lang, theme, cx),
                        Section::Memory => render_memory(view, ui, lang, theme, cx),
                        Section::Skills => render_skills(view, ui, lang, theme, cx),
                        Section::Agents => render_agents(view, ui, lang, theme, cx),
                    });
                }
            }
            out.push(divider(tr(lang, Tr::CtxSession), theme).into_any_element());
            out.push(render_session(view, ui, lang, theme, cx));
            out
        }
    };

    v_flex()
        .id("ctx-panel")
        .size_full()
        .min_h_0()
        .overflow_y_scroll()
        .p_3()
        .gap_3()
        .children(top)
        .children(body)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sich_proto::{AgentState, AgentStatus};

    #[test]
    fn poll_step_decisions() {
        use PollStep::*;
        // Regression: an inactive window with targets must keep its data.
        assert_eq!(poll_step(false, true, true), Keep);
        assert_eq!(poll_step(true, true, true), Load);
        for active in [false, true] {
            assert_eq!(poll_step(active, false, true), Clear);
            assert_eq!(poll_step(active, true, false), Clear);
            assert_eq!(poll_step(active, false, false), Clear);
        }
    }

    fn cat(name: &str, tokens: u64, color: Option<&str>, kind: CatKind) -> Category {
        Category { name: name.into(), tokens, color: color.map(Into::into), kind }
    }

    fn info(id: TermId, kind: Option<AgentKind>, session: Option<&str>, exited: bool) -> TerminalInfo {
        TerminalInfo {
            id,
            title: format!("t{id}"),
            cwd: "/".into(),
            cols: 80,
            rows: 24,
            exited: exited.then_some(Some(0)),
            created_unix_ms: 0,
            worktree: None,
            agent: kind.map(|kind| AgentStatus {
                kind,
                state: AgentState::Idle,
                unread: false,
                since_unix_ms: 0,
                detail: None,
                model: None,
                title: None,
                prompt: None,
                message: None,
                subagents: Vec::new(),
                session_id: session.map(Into::into),
            }),
        }
    }

    #[test]
    fn targets_are_live_claude_terminals_with_raw_ids() {
        let infos = [
            info(1, Some(AgentKind::Claude), Some("s1"), false),
            info(2, Some(AgentKind::Codex), None, false),
            info(3, None, None, false),
            info(4, Some(AgentKind::Claude), None, true),
            info(5, Some(AgentKind::Claude), None, false),
        ];
        let owner = |id: TermId| {
            (id != 5).then(|| TerminalOwner { socket: "sock".into(), id: id + 100, pid: 1, created_ms: 0 })
        };
        let targets = claude_targets(&infos, owner);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].key, 1);
        assert_eq!(targets[0].raw_id, "101");
        assert_eq!(targets[0].socket, "sock");
        assert_eq!(targets[0].session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn selection_prefers_pin_then_auto() {
        let ids = [1, 2, 3];
        assert_eq!(selected_terminal(&ids, Some(3), Some(1), Some(2)), Some(3));
        assert_eq!(selected_terminal(&ids, Some(9), Some(1), Some(2)), Some(1));
        assert_eq!(selected_terminal(&ids, None, None, Some(2)), Some(2));
        assert_eq!(selected_terminal(&[], Some(1), Some(1), Some(1)), None);
    }

    #[test]
    fn pins_are_per_workspace_and_sections_toggle() {
        let mut ui = ContextUi::default();
        let (a, b) = (WorkspaceKey::Home, WorkspaceKey::Worktree("/w".into()));
        ui.set_pin(a.clone(), Some(1));
        ui.set_pin(b.clone(), Some(2));
        ui.set_pin(a.clone(), Some(3));
        assert_eq!(ui.pinned(&a), Some(3));
        assert_eq!(ui.pinned(&b), Some(2));
        ui.set_pin(a.clone(), None);
        assert_eq!(ui.pinned(&a), None);

        assert!(section_open(Section::Subagents, &ui.toggled));
        assert!(!section_open(Section::Mcp, &ui.toggled));
        ui.toggle(Section::Subagents);
        ui.toggle(Section::Mcp);
        assert!(!section_open(Section::Subagents, &ui.toggled));
        assert!(section_open(Section::Mcp, &ui.toggled));
        ui.toggle(Section::Mcp);
        assert!(!section_open(Section::Mcp, &ui.toggled));
    }

    #[test]
    fn selection_follows_the_shared_per_workspace_pin() {
        let targets = ["a", "b", "c"].iter().enumerate().map(|(i, t)| Target {
            key: i as u64 + 1,
            session_id: None,
            raw_id: i.to_string(),
            socket: "s".into(),
            title: (*t).into(),
            worktree: None,
        });
        let input = PanelInput { targets: targets.collect(), key: WorkspaceKey::Home, focused: Some(2), active: Some(3), now_ms: 0 };
        let mut ui = ContextUi::default();
        assert_eq!(selection(&ui, &input), (Some(2), None));
        ui.set_pin(WorkspaceKey::Home, Some(3));
        assert_eq!(selection(&ui, &input), (Some(3), Some(3)));
        // A pin on a terminal that is gone, or on another workspace, is ignored.
        ui.set_pin(WorkspaceKey::Home, Some(9));
        assert_eq!(selection(&ui, &input), (Some(2), None));
        ui.set_pin(WorkspaceKey::Worktree("/w".into()), Some(1));
        assert_eq!(selection(&ui, &input), (Some(2), None));
        let empty = PanelInput { targets: Vec::new(), ..input };
        assert_eq!(selection(&ui, &empty), (None, None));
    }

    #[test]
    fn agents_section_starts_collapsed_and_file_menu_is_complete() {
        assert!(!section_open(Section::Agents, &HashSet::new()));
        assert!(section_open(Section::Agents, &HashSet::from([Section::Agents])));
        assert_eq!(label_key(Section::Agents), Tr::CtxAgents);
        let actions: Vec<_> = FILE_ACTIONS.iter().map(|(a, _)| *a).collect();
        assert_eq!(actions, [FileAction::OpenInTab, FileAction::OpenDefault, FileAction::Reveal, FileAction::CopyPath]);
        for (_, label) in FILE_ACTIONS {
            for lang in Language::ALL {
                assert!(!tr(lang, label).trim().is_empty());
            }
        }
    }

    #[test]
    fn resolved_files_replace_reports_changes_only() {
        let mut ui = ContextUi::default();
        let file = Resolved { path: "/a.md".into(), read_only: false };
        let map = HashMap::from([(Item::Memory("/a.md".into()), file)]);
        assert!(ui.set_resolved(map.clone()));
        assert!(!ui.set_resolved(map));
        assert!(ui.set_resolved(HashMap::new()));
    }

    #[test]
    fn refresh_resolved_only_touches_due_items() {
        let mut cache = ResolveCache::default();
        let view = CtxView { transcript_path: Some("/no/such/dir/s.jsonl".into()), ..Default::default() };
        refresh_resolved(&mut cache, [&view].into_iter(), 1_000);
        assert!(cache.snapshot().is_empty());
        assert!(cache.due(&ctx_files::items_of(&view), 1_000).is_empty());
        assert_eq!(cache.due(&ctx_files::items_of(&view), 1_000 + ctx_files::RESOLVE_TTL_MS).len(), 1);
    }

    #[test]
    fn replace_reports_changes_only() {
        let mut ui = ContextUi::default();
        let views = HashMap::from([(1, CtxView { percent: Some(5.), ..Default::default() })]);
        assert!(ui.replace(views.clone()));
        assert!(!ui.replace(views));
        assert!(ui.replace(HashMap::new()));
    }

    #[test]
    fn switcher_text_and_visibility() {
        assert!(!switcher_visible(0));
        assert!(!switcher_visible(1));
        assert!(switcher_visible(2));
        assert_eq!(switcher_label("Auto", false, "build"), "Auto · build");
        assert_eq!(switcher_label("Auto", true, "build"), "build");
        assert_eq!(switcher_label("Авто", false, ""), "Авто · Claude");
        assert_eq!(switcher_label("Auto", true, "a very long terminal title here"), "a very long terminal ti…");
        assert_eq!(switcher_item_label("build", Some(18.4)), format!("build · {}", percent_label(18.4)));
        assert_eq!(switcher_item_label("build", None), "build · –");
    }

    #[test]
    fn text_helpers() {
        assert_eq!(short_title("", 16), "Claude");
        assert_eq!(short_title("short", 16), "short");
        assert_eq!(short_title("a very long terminal title", 10), "a very lo…");
        assert_eq!(span_text(Language::En, 5), "5s");
        assert_eq!(span_text(Language::En, 184), "3m 4s");
        assert_eq!(span_text(Language::En, 7500), "2h 5m");
        assert_eq!(span_text(Language::Uk, 184), "3хв 4с");
    }

    #[test]
    fn tooltip_shows_age_only_when_stale() {
        let view = CtxView {
            model: Some("Opus 5.5".into()),
            used: Some(55_674),
            max: Some(1_000_000),
            updated_ms: 1_000_000,
            ..Default::default()
        };
        assert_eq!(tooltip_text(Language::En, &view, 1_000_000 + 60_000), "Opus 5.5 · 55.7k / 1M");
        let old = tooltip_text(Language::En, &view, 1_000_000 + 11 * 60_000);
        assert_eq!(old, "Opus 5.5 · 55.7k / 1M · updated 11m 0s ago");
        assert_eq!(tooltip_text(Language::En, &CtxView::default(), 5), "");
    }

    #[test]
    fn run_states_and_ordering() {
        assert_eq!(run_state(Some("running")), RunState::Running);
        assert_eq!(run_state(Some("Done")), RunState::Done);
        assert_eq!(run_state(Some("failed")), RunState::Failed);
        assert_eq!(run_state(Some("weird")), RunState::Other);
        assert_eq!(run_state(None), RunState::Other);
        let runs = [
            AgentRun { id: Some("a".into()), started_at: Some(1), ..Default::default() },
            AgentRun { id: Some("c".into()), started_at: None, ..Default::default() },
            AgentRun { id: Some("b".into()), started_at: Some(5), ..Default::default() },
        ];
        let ids: Vec<_> = runs_newest_first(&runs).iter().map(|r| r.id.clone().unwrap()).collect();
        assert_eq!(ids, ["b", "a", "c"]);
    }

    #[test]
    fn swatches_are_distinct_and_special_kinds_are_not_colored() {
        let cats = [
            cat("a", 1, Some("promptBorder"), CatKind::Used),
            cat("b", 1, Some("promptBorder"), CatKind::Used),
            cat("c", 1, Some("mystery"), CatKind::Used),
            cat("free", 1, Some("promptBorder"), CatKind::Free),
            cat("buf", 1, Some("inactive"), CatKind::Buffer),
            cat("def", 1, Some("inactive"), CatKind::Deferred),
        ];
        let swatches = assign_swatches(&cats);
        assert_eq!(swatches[0], Swatch::Color(0));
        assert_eq!(swatches[1], Swatch::Color(1));
        assert_eq!(swatches[2], Swatch::Color(2));
        assert_eq!(&swatches[3..], [Swatch::Free, Swatch::Buffer, Swatch::Deferred]);
        // More used rows than colors never panics.
        let many: Vec<_> = (0..25).map(|_| cat("x", 1, None, CatKind::Used)).collect();
        assert_eq!(assign_swatches(&many).len(), 25);
        assert_eq!(named_slot("green_FOR_VSCODE"), Some(3));
        assert_eq!(named_slot("nope"), None);
    }

    #[test]
    fn bar_fractions_skip_deferred_and_clamp() {
        let cats = [
            cat("a", 250, None, CatKind::Used),
            cat("d", 999, None, CatKind::Deferred),
            cat("b", 750, None, CatKind::Used),
            cat("big", 5000, None, CatKind::Used),
        ];
        let f = bar_fractions(&cats, Some(1000));
        assert_eq!(f[0], Some(0.25));
        assert_eq!(f[1], None);
        assert_eq!(f[2], Some(0.75));
        assert_eq!(f[3], Some(0.0));
        // No max: fractions of the drawn total.
        let f = bar_fractions(&cats[..1], None);
        assert_eq!(f[0], Some(1.0));
        assert!(bar_fractions(&[], None).is_empty());
    }

    #[test]
    fn context_keys_are_translated() {
        for key in [Tr::CtxTab, Tr::CtxNoClaude, Tr::CtxWaiting, Tr::CtxNoMod, Tr::CtxAuto, Tr::CtxUpdatedAgo] {
            for lang in Language::ALL {
                assert!(!tr(lang, key).trim().is_empty(), "{key:?} {lang:?}");
            }
        }
        for lang in Language::ALL {
            assert!(tr(lang, Tr::CtxUpdatedAgo).contains("{t}"));
        }
    }
}
