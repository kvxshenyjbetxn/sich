//! The "Images" tab of the right panel: every image seen in the selected Claude Code session,
//! newest first. View state, polling and the pure decisions live here; `claude_images` owns the data.
//! The agent selection (switcher + pin) is the Context tab's, so both tabs always agree.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::{
    AnyElement, ClickEvent, Context, Hsla, InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Styled as _, StyledImage as _,
    Window, div, img, px, uniform_list,
};
use sich_proto::TermId;

use crate::app_view::AppView;
use crate::claude_ctx::{self, Target};
use crate::claude_images::{ImageList, ImageLoader, ImageRow, ImageState, Source};
use crate::context_panel::{self, ContextUi, FILE_ACTIONS, FileAction, PanelInput};
use crate::ctx_files::Resolved;
use crate::i18n::{self, Language, Tr, tr};

const POLL: Duration = Duration::from_secs(2);
/// Narrowest a tile may get; tiles stretch to fill the row.
const MIN_TILE: f32 = 150.;
const GAP: f32 = 8.;
const IMAGE_H: f32 = 140.;
/// Tile = padding 4 + picture + 4 + name 16 + 4 + subtitle 14 + padding 4 (constant: `uniform_list`).
const TILE_H: f32 = 4. + IMAGE_H + 4. + 16. + 4. + 14. + 4.;
const ROW_H: f32 = TILE_H + GAP;
/// Horizontal space of the right panel that is not tile area: its left border and the `px_2` padding.
const PANEL_CHROME_W: f32 = 1. + 16.;

// ---------------------------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------------------------

/// What one polling round should do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Poll {
    /// The tab is not visible or the window is inactive: read nothing, keep what is shown.
    Skip,
    /// Visible, but there is no Claude terminal to show: drop the shown lists.
    Clear,
    Load(Target),
}

pub(crate) fn poll_decision(tab_visible: bool, window_active: bool, target: Option<Target>) -> Poll {
    if !tab_visible || !window_active {
        return Poll::Skip;
    }
    match target {
        Some(target) => Poll::Load(target),
        None => Poll::Clear,
    }
}

/// The word shown for the source of an image; none for sources this version does not know.
pub(crate) fn source_key(source: Source) -> Option<Tr> {
    match source {
        Source::Read => Some(Tr::ImgSrcRead),
        Source::Paste => Some(Tr::ImgSrcPaste),
        Source::Mcp => Some(Tr::ImgSrcMcp),
        Source::Other => None,
    }
}

pub(crate) fn is_ready(row: &ImageRow) -> bool {
    matches!(row.state, ImageState::Ready(_))
}

/// Second line of a row: "<source> · <age>", or "<unavailable> · <age>" for a gone image.
pub(crate) fn row_subtitle(lang: Language, row: &ImageRow, now_ms: u64) -> String {
    let lead = if is_ready(row) { source_key(row.source).map(|key| tr(lang, key)) } else { Some(tr(lang, Tr::ImgGone)) };
    let age = (row.at > 0).then(|| i18n::relative_time(lang, now_ms, row.at));
    [lead.map(str::to_string), age].into_iter().flatten().collect::<Vec<_>>().join(" · ")
}

/// The menu of a row: the same file actions as the Context tab; nothing for an image that is gone.
pub(crate) fn row_actions(row: &ImageRow) -> &'static [(FileAction, Tr)] {
    if is_ready(row) { &FILE_ACTIONS } else { &[] }
}

/// The file behind a ready row, in the form the shared file actions take.
pub(crate) fn row_file(row: &ImageRow) -> Option<Resolved> {
    match &row.state {
        ImageState::Ready(path) => Some(Resolved { path: path.clone(), read_only: true }),
        _ => None,
    }
}

/// Width available for tiles inside a right panel of `panel_width`.
pub(crate) fn content_width(panel_width: f32) -> f32 {
    (panel_width - PANEL_CHROME_W).max(0.)
}

/// Tiles per row for `available` px: as many `MIN_TILE`s (with gaps) as fit, at least one.
pub(crate) fn columns(available: f32) -> usize {
    if !available.is_finite() || available <= 0. {
        return 1;
    }
    (((available + GAP) / (MIN_TILE + GAP)).floor() as usize).max(1)
}

/// Number of grid rows for `count` images.
pub(crate) fn grid_row_count(count: usize, cols: usize) -> usize {
    count.div_ceil(cols.max(1))
}

/// Indices of the images in grid row `row` (the last row may be short).
pub(crate) fn grid_row_range(row: usize, cols: usize, count: usize) -> std::ops::Range<usize> {
    let cols = cols.max(1);
    let start = (row * cols).min(count);
    start..(start + cols).min(count)
}

// ---------------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------------

pub(crate) struct ImagesUi {
    /// Latest list per GUI terminal key (only the selected terminal is loaded).
    lists: HashMap<TermId, ImageList>,
    wake: async_channel::Sender<()>,
    wake_rx: async_channel::Receiver<()>,
}

impl Default for ImagesUi {
    fn default() -> Self {
        let (wake, wake_rx) = async_channel::bounded(1);
        Self { lists: HashMap::new(), wake, wake_rx }
    }
}

impl ImagesUi {
    pub(crate) fn list(&self, id: TermId) -> Option<&ImageList> {
        self.lists.get(&id)
    }

    /// Asks the poller to read now (tab or agent switched) instead of at the next tick.
    pub(crate) fn poke(&self) {
        let _ = self.wake.try_send(());
    }

    pub(crate) fn receiver(&self) -> async_channel::Receiver<()> {
        self.wake_rx.clone()
    }

    /// Replaces the data; true when anything changed.
    fn replace(&mut self, lists: HashMap<TermId, ImageList>) -> bool {
        let changed = self.lists != lists;
        self.lists = lists;
        changed
    }

    /// Background polling of the selected session's manifest while the tab is visible and the
    /// window is active.
    pub(crate) fn watch(wake_rx: async_channel::Receiver<()>, window: &mut Window, cx: &Context<AppView>) {
        let (wake, activation_rx) = async_channel::bounded::<()>(1);
        cx.observe_window_activation(window, move |_, window, _| {
            if window.is_window_active() {
                let _ = wake.try_send(());
            }
        })
        .detach();
        cx.spawn_in(window, async move |view, cx| {
            let mut loader = ImageLoader::default();
            loop {
                let timer = cx.background_executor().timer(POLL).fuse();
                let poked = wake_rx.recv().fuse();
                let activated = activation_rx.recv().fuse();
                futures::pin_mut!(timer, poked, activated);
                futures::select! {
                    _ = timer => {},
                    result = poked => if result.is_err() { break },
                    result = activated => if result.is_err() { break },
                }
                let Ok(poll) = view.update_in(cx, |view, window, _| view.images_poll(window)) else { break };
                let target = match poll {
                    Poll::Skip => continue,
                    Poll::Clear => None,
                    Poll::Load(target) => Some(target),
                };
                let (Some(dir), Some(target)) = (claude_ctx::data_dir(), target) else {
                    if view.update(cx, |view, cx| {
                        if view.images_ui.replace(HashMap::new()) {
                            cx.notify();
                        }
                    }).is_err() {
                        break;
                    }
                    continue;
                };
                let (next, lists) = cx
                    .background_executor()
                    .spawn(async move {
                        let lists = loader.load(&dir, &[target]);
                        (loader, lists)
                    })
                    .await;
                loader = next;
                if view
                    .update(cx, |view, cx| {
                        if view.images_ui.replace(lists) {
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

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

fn placeholder(icon: Ic, color: Hsla) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon).size(px(16.)).text_color(color))
        .into_any_element()
}

/// One tile: the picture, then the name and "source · age". Constant height (`TILE_H`).
fn render_tile(
    row: &ImageRow,
    lang: Language,
    now_ms: u64,
    theme: &Theme,
    app: gpui_kit::WeakEntity<AppView>,
) -> AnyElement {
    let ready = is_ready(row);
    let muted = theme.muted_foreground;
    let frame = div()
        .w_full()
        .flex_shrink_0()
        .h(px(IMAGE_H))
        .rounded(px(6.))
        .border_1()
        .border_color(theme.border)
        .bg(muted.opacity(0.08))
        .overflow_hidden();
    let picture = match &row.state {
        ImageState::Ready(path) => frame.child(
            img(path.clone())
                .size_full()
                .object_fit(ObjectFit::Contain)
                .with_loading(move || placeholder(Ic::Image, muted.opacity(0.6)))
                .with_fallback(move || placeholder(Ic::ImageOff, muted.opacity(0.6))),
        ),
        _ => frame.child(placeholder(Ic::ImageOff, muted.opacity(0.6))),
    };
    let content = v_flex()
        .w_full()
        .h_full()
        .gap(px(4.))
        .items_start()
        .child(picture)
        .child(
            div()
                .w_full()
                .h(px(16.))
                .truncate()
                .text_left()
                .text_size(px(12.))
                .text_color(if ready { theme.foreground } else { muted })
                .child(row.name.clone()),
        )
        .child(div().w_full().h(px(14.)).truncate().text_left().text_size(px(11.)).text_color(muted).child(row_subtitle(lang, row, now_ms)));
    let Some(file) = row_file(row) else {
        return div().w_full().h(px(TILE_H)).p_1().child(content).into_any_element();
    };
    let actions = row_actions(row);
    Button::new(gpui_kit::SharedString::from(format!("img-{}", row.id)))
        .ghost()
        .xsmall()
        .w_full()
        .h(px(TILE_H))
        .p_1()
        .cursor_pointer()
        .tooltip(file.path.display().to_string())
        .child(content)
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu;
            for (action, label) in actions {
                let (app, file, action) = (app.clone(), file.clone(), *action);
                menu = menu.item(PopupMenuItem::new(tr(lang, *label)).on_click(
                    move |_: &ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                        let _ = app.update(cx, |this, cx| this.run_file_action(action, &file, window, cx));
                    },
                ));
            }
            menu
        })
        .into_any_element()
}

/// One list item: `cols` equal-width tiles; a short last row is padded so its tiles keep their width.
fn render_grid_row(
    rows: &[ImageRow],
    cols: usize,
    lang: Language,
    now_ms: u64,
    theme: &Theme,
    app: &gpui_kit::WeakEntity<AppView>,
) -> AnyElement {
    let slot = || div().flex_1().min_w_0().h(px(TILE_H));
    let tiles = rows.iter().map(|row| slot().child(render_tile(row, lang, now_ms, theme, app.clone())));
    let fillers = (rows.len()..cols).map(|_| slot());
    h_flex().w_full().h(px(ROW_H)).pb(px(GAP)).gap(px(GAP)).items_start().children(tiles).children(fillers).into_any_element()
}

pub(crate) fn render_panel(
    ui: &ContextUi,
    images: &ImagesUi,
    input: PanelInput,
    lang: Language,
    panel_width: f32,
    cx: &Context<AppView>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let (selected, pinned) = context_panel::selection(ui, &input);
    let Some(selected) = selected else {
        return context_panel::centered(tr(lang, Tr::CtxNoClaude), &theme);
    };
    let switcher = context_panel::render_switcher("img-switcher", ui, &input, selected, pinned, lang, cx);
    let rows: Arc<Vec<ImageRow>> = Arc::new(images.list(selected).map(|list| list.rows.clone()).unwrap_or_default());
    let body = if rows.is_empty() {
        context_panel::centered(tr(lang, Tr::ImgEmpty), &theme)
    } else {
        let cols = columns(content_width(panel_width));
        let (app, now_ms, count, row_theme) = (cx.weak_entity(), input.now_ms, rows.len(), theme.clone());
        uniform_list("img-list", grid_row_count(count, cols), move |range, _, _| {
            range
                .map(|row| render_grid_row(&rows[grid_row_range(row, cols, count)], cols, lang, now_ms, &row_theme, &app))
                .collect::<Vec<_>>()
        })
        .flex_1()
        .min_h_0()
        .w_full()
        .into_any_element()
    };
    v_flex()
        .id("img-panel")
        .size_full()
        .min_h_0()
        .px_2()
        .pt_2()
        .gap_2()
        .children(switcher.map(|switcher| h_flex().w_full().items_center().child(switcher)))
        .child(body)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn row(state: ImageState, source: Source, at: u64) -> ImageRow {
        ImageRow { id: "a".into(), name: "shot.png".into(), source, at, state }
    }

    fn ready() -> ImageState {
        ImageState::Ready(PathBuf::from("/x/shot.png"))
    }

    fn target() -> Target {
        Target { key: 1, session_id: None, raw_id: "1".into(), socket: "s".into(), title: String::new(), worktree: None }
    }

    #[test]
    fn polls_only_while_visible_and_active() {
        assert_eq!(poll_decision(false, true, Some(target())), Poll::Skip);
        assert_eq!(poll_decision(true, false, Some(target())), Poll::Skip);
        assert_eq!(poll_decision(false, false, None), Poll::Skip);
        assert_eq!(poll_decision(true, true, None), Poll::Clear);
        assert_eq!(poll_decision(true, true, Some(target())), Poll::Load(target()));
    }

    #[test]
    fn subtitle_shows_source_and_age_or_the_gone_label() {
        let now = 10 * 60_000;
        let r = row(ready(), Source::Paste, now - 5 * 60_000);
        assert_eq!(row_subtitle(Language::En, &r, now), "pasted · 5m");
        assert_eq!(row_subtitle(Language::Uk, &row(ready(), Source::Read, now), now), "прочитано · щойно");
        assert_eq!(row_subtitle(Language::En, &row(ready(), Source::Mcp, now), now), "MCP · now");
        // Unknown source: only the age; unknown time: only the source.
        assert_eq!(row_subtitle(Language::En, &row(ready(), Source::Other, now - 120_000), now), "2m");
        assert_eq!(row_subtitle(Language::En, &row(ready(), Source::Read, 0), now), "read");
        for state in [ImageState::Missing, ImageState::Unavailable] {
            assert_eq!(row_subtitle(Language::En, &row(state, Source::Read, now - 120_000), now), "file unavailable · 2m");
        }
    }

    #[test]
    fn only_ready_rows_have_a_menu() {
        let ok = row(ready(), Source::Read, 1);
        let actions: Vec<_> = row_actions(&ok).iter().map(|(a, _)| *a).collect();
        assert_eq!(actions, [FileAction::OpenInTab, FileAction::OpenDefault, FileAction::Reveal, FileAction::CopyPath]);
        let file = row_file(&ok).unwrap();
        assert_eq!(file.path, PathBuf::from("/x/shot.png"));
        assert!(file.read_only);
        for state in [ImageState::Missing, ImageState::Unavailable] {
            let gone = row(state, Source::Read, 1);
            assert!(row_actions(&gone).is_empty());
            assert!(row_file(&gone).is_none());
            assert!(!is_ready(&gone));
        }
    }

    #[test]
    fn source_labels_cover_known_sources() {
        assert_eq!(source_key(Source::Read), Some(Tr::ImgSrcRead));
        assert_eq!(source_key(Source::Paste), Some(Tr::ImgSrcPaste));
        assert_eq!(source_key(Source::Mcp), Some(Tr::ImgSrcMcp));
        assert_eq!(source_key(Source::Other), None);
    }

    #[test]
    fn replace_reports_changes_only() {
        let mut ui = ImagesUi::default();
        let lists = HashMap::from([(1, ImageList { session: "s".into(), updated_ms: 1, rows: vec![row(ready(), Source::Read, 1)] })]);
        assert!(ui.replace(lists.clone()));
        assert!(!ui.replace(lists));
        assert_eq!(ui.list(1).map(|l| l.rows.len()), Some(1));
        assert!(ui.list(2).is_none());
        assert!(ui.replace(HashMap::new()));
    }

    #[test]
    fn poke_is_non_blocking_and_coalesces() {
        let ui = ImagesUi::default();
        ui.poke();
        ui.poke();
        let rx = ui.receiver();
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn columns_follow_the_width() {
        assert_eq!(columns(0.), 1);
        assert_eq!(columns(-5.), 1);
        assert_eq!(columns(f32::NAN), 1);
        assert_eq!(columns(100.), 1);
        assert_eq!(columns(150.), 1);
        // Two tiles need 2 * 150 + 8.
        assert_eq!(columns(307.9), 1);
        assert_eq!(columns(308.), 2);
        assert_eq!(columns(465.9), 2);
        assert_eq!(columns(466.), 3);
        assert_eq!(columns(10_000.), 63);
    }

    #[test]
    fn content_width_removes_the_panel_chrome() {
        assert_eq!(content_width(340.), 323.);
        assert_eq!(content_width(10.), 0.);
    }

    #[test]
    fn rows_are_chunked_with_a_short_last_row() {
        assert_eq!(grid_row_count(0, 3), 0);
        assert_eq!(grid_row_count(6, 3), 2);
        assert_eq!(grid_row_count(7, 3), 3);
        assert_eq!(grid_row_count(5, 1), 5);
        assert_eq!(grid_row_count(5, 0), 5);
        assert_eq!(grid_row_range(0, 3, 7), 0..3);
        assert_eq!(grid_row_range(1, 3, 7), 3..6);
        assert_eq!(grid_row_range(2, 3, 7), 6..7);
        assert_eq!(grid_row_range(5, 3, 7), 7..7);
        // Every index is covered exactly once.
        let all: Vec<usize> = (0..grid_row_count(7, 3)).flat_map(|r| grid_row_range(r, 3, 7)).collect();
        assert_eq!(all, (0..7).collect::<Vec<_>>());
    }

    #[test]
    fn tile_height_is_constant() {
        assert_eq!(TILE_H, 186.);
        assert_eq!(ROW_H, 194.);
    }

    #[test]
    fn image_keys_are_translated() {
        for key in [Tr::ImgTab, Tr::ImgEmpty, Tr::ImgGone, Tr::ImgSrcRead, Tr::ImgSrcPaste, Tr::ImgSrcMcp] {
            for lang in Language::ALL {
                assert!(!tr(lang, key).trim().is_empty(), "{key:?} {lang:?}");
            }
        }
        assert_eq!(tr(Language::En, Tr::ImgEmpty), "No images in this session yet");
    }
}
