//! Commit graph column and per-commit actions of the history list.
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::{
    App, BorderStyle, Bounds, ClipboardItem, Context, Corners, Div, Edges, Hsla, IntoElement,
    PathBuilder, Styled as _, WeakEntity, Window, canvas, div, point, px, quad,
    size, transparent_black,
};
use sich_git::graph::{GraphRow, LANE_COLORS};
use sich_git::{Commit, commit_url};

use super::*;
use super::pr_card::open_url;

pub(super) const ROW_H: f32 = 26.;
pub(super) const LANE_W: f32 = 11.;
/// Lanes beyond this are folded into the last column so the subject keeps its room.
pub(super) const MAX_LANES: usize = 6;
const NODE_R: f32 = 3.5;
const HEAD_R: f32 = 5.;
const LINE_W: f32 = 1.5;

/// A line of a graph row in row-local coordinates (`y` runs 0..ROW_H).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Seg {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub color: u8,
}

pub(super) fn lane_x(index: usize) -> f32 {
    index.min(MAX_LANES - 1) as f32 * LANE_W + LANE_W / 2.
}

/// Number of lane columns the list needs (1..=MAX_LANES), the same for every row.
pub(super) fn lane_columns(rows: &[GraphRow]) -> usize {
    rows.iter()
        .map(|row| row.input.len().max(row.output.len()).max(row.node_index + 1))
        .max()
        .unwrap_or(1)
        .clamp(1, MAX_LANES)
}

pub(super) fn graph_width(columns: usize) -> f32 {
    columns as f32 * LANE_W
}

/// Lines of one row: lanes passing through (possibly shifting), lanes merging into the node,
/// and lanes leaving the node towards its parents.
pub(super) fn row_segments(row: &GraphRow, row_h: f32) -> Vec<Seg> {
    let mid = row_h / 2.;
    let node_x = lane_x(row.node_index);
    let node_id = row.input.get(row.node_index).map(|lane| lane.id.as_str());
    let mut segments = Vec::new();
    let mut out = 0usize;
    let mut first_parent = None;
    for (index, lane) in row.input.iter().enumerate() {
        if node_id == Some(lane.id.as_str()) {
            segments.push(Seg { x0: lane_x(index), y0: 0., x1: node_x, y1: mid, color: lane.color });
            if index == row.node_index && row.parent_count > 0 {
                first_parent = Some(out);
                out += 1;
            }
        } else {
            segments.push(Seg { x0: lane_x(index), y0: 0., x1: lane_x(out), y1: row_h, color: lane.color });
            out += 1;
        }
    }
    if node_id.is_none() && row.parent_count > 0 {
        first_parent = Some(out);
        out += 1;
    }
    let extras = row.parent_count.saturating_sub(1);
    let below = first_parent.into_iter().chain(out..out + extras);
    for index in below {
        let color = row.output.get(index).map_or(row.node_color, |lane| lane.color);
        segments.push(Seg { x0: node_x, y0: mid, x1: lane_x(index), y1: row_h, color });
    }
    segments
}

pub(super) fn lane_colors(theme: &gpui_kit::component::Theme) -> [Hsla; LANE_COLORS as usize] {
    [theme.blue, theme.green, theme.magenta, theme.yellow, theme.cyan]
}

fn lane_color(colors: &[Hsla; LANE_COLORS as usize], index: u8) -> Hsla {
    colors[(index % LANE_COLORS) as usize]
}

/// The graph cell of one commit row, painted on a canvas.
pub(super) fn graph_cell(
    row: &GraphRow,
    columns: usize,
    colors: [Hsla; LANE_COLORS as usize],
    hole: Hsla,
) -> impl IntoElement + use<> {
    let segments = row_segments(row, ROW_H);
    let node = (lane_x(row.node_index), ROW_H / 2., row.node_color, row.is_head);
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let at = |x: f32, y: f32| point(bounds.origin.x + px(x), bounds.origin.y + px(y));
            for seg in &segments {
                let mut path = PathBuilder::stroke(px(LINE_W));
                path.move_to(at(seg.x0, seg.y0));
                if (seg.x0 - seg.x1).abs() < f32::EPSILON {
                    path.line_to(at(seg.x1, seg.y1));
                } else {
                    let dy = seg.y1 - seg.y0;
                    path.cubic_bezier_to(
                        at(seg.x1, seg.y1),
                        at(seg.x0, seg.y0 + dy * 0.6),
                        at(seg.x1, seg.y1 - dy * 0.6),
                    );
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, lane_color(&colors, seg.color));
                }
            }
            let (x, y, color, head) = node;
            let color = lane_color(&colors, color);
            let r = if head { HEAD_R } else { NODE_R };
            let bounds = Bounds::new(at(x - r, y - r), size(px(2. * r), px(2. * r)));
            let quad = if head {
                // A ring: the lane color around the list background.
                quad(bounds, Corners::all(px(r)), hole, Edges::all(px(2.)), color, BorderStyle::Solid)
            } else {
                quad(bounds, Corners::all(px(r)), color, Edges::all(px(0.)), transparent_black(), BorderStyle::Solid)
            };
            window.paint_quad(quad);
        },
    )
    .w(px(graph_width(columns)))
    .h(px(ROW_H))
    .flex_shrink_0()
}

/// Vertical lane lines drawn through the expanded body of a commit.
pub(super) fn expansion_lines(row: &GraphRow, colors: &[Hsla; LANE_COLORS as usize]) -> Vec<Div> {
    row.output
        .iter()
        .enumerate()
        .take(MAX_LANES)
        .map(|(index, lane)| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(lane_x(index) - LINE_W / 2. + 8.))
                .w(px(LINE_W))
                .bg(lane_color(colors, lane.color))
        })
        .collect()
}

pub(super) fn show_info(message: &str, window: &mut Window, cx: &mut App) {
    window.push_notification(Notification::info(message.to_owned()), cx);
}

/// Text copied by "Copy message": subject, blank line, body.
pub(super) fn commit_message_text(commit: &Commit) -> String {
    if commit.body.trim().is_empty() {
        commit.subject.clone()
    } else {
        format!("{}\n\n{}", commit.subject, commit.body.trim_end())
    }
}

pub(super) fn commit_web_link(remote: Option<&RemoteRepo>, oid: &str) -> Option<String> {
    remote.map(|remote| commit_url(remote, oid))
}

/// Context menu of a history row.
pub(super) fn commit_menu(
    menu: PopupMenu,
    panel: WeakEntity<GitPanel>,
    commit: &Commit,
    url: Option<String>,
    busy: bool,
    lang: Language,
) -> PopupMenu {
    let (sha, message) = (commit.oid.clone(), commit_message_text(commit));
    let mut menu = menu
        .item(PopupMenuItem::new(tr(lang, Tr::GitCopySha)).on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(sha.clone()))
        }))
        .item(PopupMenuItem::new(tr(lang, Tr::GitCopyMessage)).on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(message.clone()))
        }));
    if let Some(url) = url {
        menu = menu.item(
            PopupMenuItem::new(tr(lang, Tr::GitOpenCommitOnGitHub))
                .on_click(move |_, _, cx| open_url(&url, cx)),
        );
    }
    let pick = (panel.clone(), commit.clone());
    let revert = (panel, commit.clone());
    menu.separator()
        .item(
            PopupMenuItem::new(tr(lang, Tr::GitCherryPick))
                .disabled(busy)
                .on_click(move |_, window, cx| {
                    let (panel, commit) = pick.clone();
                    let _ = panel.update(cx, |panel, cx| {
                        panel.confirm_commit_op(Mutation::CherryPick(commit.oid.clone()), &commit, lang, window, cx)
                    });
                }),
        )
        .item(
            PopupMenuItem::new(tr(lang, Tr::GitRevert))
                .disabled(busy)
                .on_click(move |_, window, cx| {
                    let (panel, commit) = revert.clone();
                    let _ = panel.update(cx, |panel, cx| {
                        panel.confirm_commit_op(Mutation::Revert(commit.oid.clone()), &commit, lang, window, cx)
                    });
                }),
        )
}

impl GitPanel {
    fn confirm_commit_op(
        &self,
        mutation: Mutation,
        commit: &Commit,
        lang: Language,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let generation = self.generation;
        let panel = cx.weak_entity();
        let (title, text) = match mutation {
            Mutation::Revert(_) => (Tr::GitRevert, Tr::GitRevertConfirm),
            _ => (Tr::GitCherryPick, Tr::GitCherryPickConfirm),
        };
        worktree_dialogs::open_confirm(
            window,
            cx,
            tr(lang, title).into(),
            format!("{}\n{} {}", tr(lang, text), commit.short, commit.subject),
            None,
            tr(lang, title),
            false,
            Rc::new(move |window, cx| {
                let _ = panel.update(cx, |panel, cx| {
                    panel
                        .generation
                        .apply(generation, || panel.mutate(mutation.clone(), window, cx));
                });
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sich_git::graph::GraphBuilder;

    fn p(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn seg(x0: usize, y0: f32, x1: usize, y1: f32) -> (f32, f32, f32, f32) {
        (lane_x(x0), y0, lane_x(x1), y1)
    }

    fn coords(segs: &[Seg]) -> Vec<(f32, f32, f32, f32)> {
        segs.iter().map(|s| (s.x0, s.y0, s.x1, s.y1)).collect()
    }

    #[test]
    fn linear_rows_are_vertical_lines_through_the_node() {
        let mut b = GraphBuilder::new();
        let first = b.push("c", &p(&["b"]), true);
        assert_eq!(coords(&row_segments(&first, ROW_H)), [seg(0, 13., 0, 26.)]);
        let second = b.push("b", &p(&["a"]), false);
        assert_eq!(coords(&row_segments(&second, ROW_H)), [seg(0, 0., 0, 13.), seg(0, 13., 0, 26.)]);
        let root = b.push("a", &[], false);
        assert_eq!(coords(&row_segments(&root, ROW_H)), [seg(0, 0., 0, 13.)]);
    }

    #[test]
    fn merge_row_branches_out_and_the_join_row_merges_in() {
        let mut b = GraphBuilder::new();
        let merge = b.push("m", &p(&["a", "f"]), false);
        assert_eq!(
            coords(&row_segments(&merge, ROW_H)),
            [seg(0, 13., 0, 26.), seg(0, 13., 1, 26.)]
        );
        b.push("f", &p(&["base"]), false);
        b.push("a", &p(&["base"]), false);
        let join = b.push("base", &[], false);
        let segs = row_segments(&join, ROW_H);
        assert_eq!(coords(&segs), [seg(0, 0., 0, 13.), seg(1, 0., 0, 13.)]);
    }

    #[test]
    fn passing_lane_shifts_when_an_earlier_lane_ends() {
        let mut b = GraphBuilder::new();
        b.push("t1", &p(&["r1"]), false);
        b.push("t2", &p(&["r2"]), false);
        let end = b.push("r1", &[], false);
        // Lane 0 ends at the node, lane 1 shifts to column 0.
        assert_eq!(coords(&row_segments(&end, ROW_H)), [seg(0, 0., 0, 13.), seg(1, 0., 0, 26.)]);
    }

    #[test]
    fn new_tip_to_the_right_of_open_lanes_has_no_line_above() {
        let mut b = GraphBuilder::new();
        b.push("h1", &p(&["x"]), true);
        let tip = b.push("h2", &p(&["y"]), false);
        assert_eq!(coords(&row_segments(&tip, ROW_H)), [seg(0, 0., 0, 26.), seg(1, 13., 1, 26.)]);
    }

    #[test]
    fn octopus_draws_one_line_per_parent() {
        let mut b = GraphBuilder::new();
        let row = b.push("m", &p(&["a", "b", "c"]), false);
        let segs = row_segments(&row, ROW_H);
        assert_eq!(segs.len(), 3);
        let colors: Vec<_> = segs.iter().map(|s| s.color).collect();
        assert_eq!(colors, [0, 1, 2]);
    }

    #[test]
    fn lane_columns_are_clamped_and_at_least_one() {
        assert_eq!(lane_columns(&[]), 1);
        let mut b = GraphBuilder::new();
        let ids: Vec<_> = (0..10).map(|i| format!("p{i}")).collect();
        let wide = b.push("m", &ids, false);
        assert_eq!(lane_columns(&[wide.clone()]), MAX_LANES);
        // Lanes past the limit fold into the last column.
        assert_eq!(lane_x(9), lane_x(MAX_LANES - 1));
        assert!(row_segments(&wide, ROW_H).iter().all(|s| s.x1 <= lane_x(MAX_LANES - 1)));
        assert_eq!(graph_width(3), 3. * LANE_W);
    }

    #[test]
    fn message_text_joins_subject_and_body() {
        let mut commit = Commit {
            oid: "a".repeat(40),
            short: "aaaaaaa".into(),
            author: String::new(),
            email: String::new(),
            time_unix: 0,
            parents: Vec::new(),
            refs: Vec::new(),
            subject: "subject".into(),
            body: String::new(),
        };
        assert_eq!(commit_message_text(&commit), "subject");
        commit.body = "line one\nline two\n".into();
        assert_eq!(commit_message_text(&commit), "subject\n\nline one\nline two");
    }

    #[test]
    fn web_link_only_exists_with_a_known_remote() {
        let remote = RemoteRepo::parse("https://github.com/o/r.git").unwrap();
        assert_eq!(
            commit_web_link(Some(&remote), "abc").as_deref(),
            Some("https://github.com/o/r/commit/abc")
        );
        assert_eq!(commit_web_link(None, "abc"), None);
    }

    #[test]
    fn history_extends_the_graph_across_pages_and_resets_on_refresh() {
        fn commit(oid: &str, parents: &[&str]) -> Commit {
            Commit {
                oid: oid.into(),
                short: oid.into(),
                author: String::new(),
                email: String::new(),
                time_unix: 0,
                parents: parents.iter().map(|s| s.to_string()).collect(),
                refs: Vec::new(),
                subject: String::new(),
                body: String::new(),
            }
        }
        let mut history = History::default();
        history.apply(vec![commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])], 2, true);
        assert!(history.more);
        assert_eq!(history.rows.len(), 2);
        history.apply(vec![commit("a", &[])], 2, false);
        assert_eq!(history.rows.len(), 3);
        assert_eq!(history.rows[2].input[0].id, "a");
        history.apply(vec![commit("z", &[])], 5, true);
        assert_eq!(history.rows.len(), 1);
        assert!(history.rows[0].input.is_empty());
    }
}
