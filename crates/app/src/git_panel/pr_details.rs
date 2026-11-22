//! PR tab section: details (description, reviewers, labels, assignees). Everything is fetched
//! from and edited in the base repository through `gh`; state is kept per (worktree, PR number).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::InputState;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Hsla, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, px, rgb,
};
use sich_git::forge::{DetailsEdit, GitHub, PrDetails, PullRequest};

use super::pr_edit_dialog::{self, PickKind, PrTargetCtx};
use super::pr_files::FilesEntry;
use super::render::{chevron, tool_button};
use super::*;
use crate::i18n::relative_time;
use crate::sidebar::now_unix_ms;

/// A fetched entry older than this is read again the next time the tab is drawn.
pub(super) const REFRESH_AFTER: Duration = Duration::from_secs(15);
const CLAMP_LINES: usize = 8;

#[derive(Default)]
pub(super) struct DetailsEntry {
    pub data: Option<PrDetails>,
    pub fetched_at: Option<Instant>,
    pub loading: bool,
    pub error: Option<String>,
    /// Edits sent to GitHub that have not finished yet.
    pub pending: usize,
    pub edit_error: Option<String>,
    /// `updated_at` of the PR when this was read: a change means the PR moved on.
    pub pr_updated: String,
}

impl DetailsEntry {
    /// Whether the entry should be read again (stale, never read, or the PR changed).
    pub(super) fn needs_fetch(&self, pr_updated: &str, now: Instant) -> bool {
        if self.loading || self.pending > 0 {
            return false;
        }
        match self.fetched_at {
            None => true,
            Some(at) => now.duration_since(at) >= REFRESH_AFTER || self.pr_updated != pr_updated,
        }
    }
}

/// State of the extra PR sections (details and files), owned by the panel.
pub(super) struct PrExtras {
    pub details: HashMap<(PathBuf, u64), DetailsEntry>,
    pub files: HashMap<(PathBuf, u64), FilesEntry>,
    pub details_collapsed: bool,
    pub body_expanded: bool,
    pub files_collapsed: bool,
    pub files_filter: Option<Entity<InputState>>,
    pub files_filter_sub: Option<Subscription>,
}

impl Default for PrExtras {
    fn default() -> Self {
        Self {
            details: HashMap::new(),
            files: HashMap::new(),
            details_collapsed: false,
            body_expanded: false,
            files_collapsed: true,
            files_filter: None,
            files_filter_sub: None,
        }
    }
}

/// How to reach a PR's repository from the panel.
pub(super) struct Gh {
    pub target: PrTargetCtx,
    pub base_ref: Option<String>,
    pub local: Repo,
}

impl Gh {
    pub(super) fn github(&self) -> GitHub {
        GitHub::new(self.target.dir.clone(), self.target.repo.clone())
    }
}

/// Hex colour of a label (`d73a4a`, with or without `#`); grey when it cannot be read.
pub(super) fn hex_color(hex: &str) -> Hsla {
    let value = u32::from_str_radix(hex.trim().trim_start_matches('#'), 16)
        .ok()
        .filter(|_| hex.trim().trim_start_matches('#').len() == 6)
        .unwrap_or(0x8b949e);
    rgb(value).into()
}

/// Milliseconds since the epoch of an RFC 3339 timestamp.
pub(super) fn timestamp_ms(text: &str) -> Option<u64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(text).ok()?;
    u64::try_from(parsed.timestamp_millis()).ok()
}

/// Whether the description is long enough to need "Show more".
pub(super) fn body_is_long(body: &str) -> bool {
    body.lines().count() > CLAMP_LINES || body.chars().count() > 600
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReviewMark {
    Approved,
    ChangesRequested,
    Commented,
    Pending,
}

impl ReviewMark {
    pub(super) fn from_state(state: Option<&str>) -> Self {
        match state {
            Some("APPROVED") => Self::Approved,
            Some("CHANGES_REQUESTED") => Self::ChangesRequested,
            Some("COMMENTED") => Self::Commented,
            _ => Self::Pending,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReviewerRow {
    pub login: String,
    pub mark: ReviewMark,
    /// Requested reviewers can be removed; people who already reviewed cannot.
    pub removable: bool,
}

/// Requested reviewers first (with their previous review, if any), then everyone else who reviewed.
pub(super) fn reviewer_rows(details: &PrDetails) -> Vec<ReviewerRow> {
    let mut rows: Vec<ReviewerRow> = details
        .requested_reviewers
        .iter()
        .map(|r| ReviewerRow {
            login: r.login.clone(),
            mark: if r.is_team { ReviewMark::Pending } else { ReviewMark::from_state(details.review_of(&r.login)) },
            removable: true,
        })
        .collect();
    for (login, state) in &details.reviews {
        if !rows.iter().any(|row| row.login.eq_ignore_ascii_case(login)) {
            rows.push(ReviewerRow {
                login: login.clone(),
                mark: ReviewMark::from_state(Some(state)),
                removable: false,
            });
        }
    }
    rows
}

impl GitPanel {
    /// The way to reach the base repository of the PR of the selected worktree.
    pub(super) fn pr_gh(&self, number: u64, cx: &Context<Self>) -> Option<Gh> {
        let path = self.worktree.clone()?;
        let local = self.repo.clone()?;
        let target = self.store.read(cx).entry(&path)?.target.clone()?;
        Some(Gh {
            target: PrTargetCtx {
                panel: cx.weak_entity(),
                key: (path, number),
                dir: local.root().to_path_buf(),
                repo: target.repo,
            },
            base_ref: target.base_ref,
            local,
        })
    }

    /// Reads the details when they are missing or stale (no-op while a read or an edit runs).
    pub(super) fn pr_details_ensure(&mut self, number: u64, pr_updated: &str, cx: &mut Context<Self>) {
        let Some(gh) = self.pr_gh(number, cx) else { return };
        let key = gh.target.key.clone();
        let entry = self.pr_extras.details.entry(key.clone()).or_default();
        if !entry.needs_fetch(pr_updated, Instant::now()) {
            return;
        }
        entry.loading = true;
        entry.pr_updated = pr_updated.to_owned();
        let github = gh.github();
        let job = cx.background_spawn(async move { github.pr_details(number).map_err(|e| e.to_string()) });
        cx.spawn(async move |panel, cx| {
            let result = job.await;
            let _ = panel.update(cx, |panel, cx| {
                let entry = panel.pr_extras.details.entry(key).or_default();
                entry.loading = false;
                entry.fetched_at = Some(Instant::now());
                match result {
                    Ok(details) => {
                        if entry.pending == 0 {
                            entry.data = Some(details);
                        }
                        entry.error = None;
                    }
                    Err(error) => entry.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Applies `edit` at once and sends it to GitHub; a failure restores the previous state.
    pub(super) fn pr_apply_edit(&mut self, key: (PathBuf, u64), edit: DetailsEdit, cx: &mut Context<Self>) {
        let number = key.1;
        let Some(gh) = self.pr_gh(number, cx).filter(|gh| gh.target.key == key) else { return };
        let Some(entry) = self.pr_extras.details.get_mut(&key) else { return };
        let Some(current) = entry.data.clone() else { return };
        entry.data = Some(current.with_edit(&edit));
        entry.pending += 1;
        entry.edit_error = None;
        cx.notify();
        let github = gh.github();
        let sent = edit.clone();
        let job = cx.background_spawn(async move { github.apply_edit(number, &sent).map_err(|e| e.to_string()) });
        cx.spawn(async move |panel, cx| {
            let result = job.await;
            let _ = panel.update(cx, |panel, cx| {
                let Some(entry) = panel.pr_extras.details.get_mut(&key) else { return };
                entry.pending = entry.pending.saturating_sub(1);
                if let Err(error) = result {
                    if entry.pending == 0 {
                        entry.data = Some(current);
                    }
                    entry.edit_error = Some(error);
                }
                if entry.pending == 0 {
                    // Pull the truth (review states, label colours) once everything settled.
                    entry.fetched_at = None;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The edit dialog saved the title and description.
    pub(super) fn pr_text_saved(&mut self, key: (PathBuf, u64), body: String, cx: &mut Context<Self>) {
        if let Some(entry) = self.pr_extras.details.get_mut(&key) {
            if let Some(data) = &entry.data {
                entry.data = Some(data.with_edit(&DetailsEdit::SetBody(body)));
            }
            entry.fetched_at = None;
        }
        // The title lives in the PR card: read the PR again.
        self.pr_refresh_now(cx);
        cx.notify();
    }

    fn pr_open_picker(&mut self, number: u64, kind: PickKind, window: &mut Window, cx: &mut Context<Self>) {
        let Some(gh) = self.pr_gh(number, cx) else { return };
        let exclude = self
            .pr_extras
            .details
            .get(&gh.target.key)
            .and_then(|entry| entry.data.as_ref())
            .map(|data| pr_edit_dialog::existing_values(kind, data))
            .unwrap_or_default();
        pr_edit_dialog::open_picker(gh.target, kind, exclude, window, cx);
    }

    fn pr_open_edit(&mut self, pr: &PullRequest, window: &mut Window, cx: &mut Context<Self>) {
        let Some(gh) = self.pr_gh(pr.number, cx) else { return };
        let body = self
            .pr_extras
            .details
            .get(&gh.target.key)
            .and_then(|entry| entry.data.as_ref())
            .map(|data| data.body.clone())
            .unwrap_or_default();
        pr_edit_dialog::open_edit(gh.target, pr.title.clone(), body, window, cx);
    }

    /// Section shown in the Pull request tab under the PR card; `None` hides it.
    pub(super) fn pr_details_section(
        &self,
        pr: &PullRequest,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let gh = self.pr_gh(pr.number, cx)?;
        let theme = cx.theme().clone();
        let key = gh.target.key.clone();
        let number = pr.number;
        let entry = self.pr_extras.details.get(&key);
        if entry.is_none_or(|entry| entry.needs_fetch(&pr.updated_at, Instant::now())) {
            let updated = pr.updated_at.clone();
            cx.spawn(async move |panel, cx| {
                let _ = panel.update(cx, |panel, cx| panel.pr_details_ensure(number, &updated, cx));
            })
            .detach();
        }
        let collapsed = self.pr_extras.details_collapsed;
        let header = h_flex()
            .id("pr-details-head")
            .h(px(26.))
            .gap_1()
            .px_2()
            .items_center()
            .cursor_pointer()
            .bg(theme.list_head)
            .hover(|style| style.bg(theme.list_hover))
            .on_click(cx.listener(|panel, _, _, cx| {
                panel.pr_extras.details_collapsed = !panel.pr_extras.details_collapsed;
                cx.notify();
            }))
            .child(chevron(!collapsed))
            .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).child(tr(lang, Tr::DetTitle)));
        let body = (!collapsed).then(|| {
            let content = match entry.and_then(|entry| entry.data.as_ref()) {
                Some(details) => self.pr_details_body(pr, details, entry, lang, cx),
                None => {
                    let text = match entry.and_then(|entry| entry.error.clone()) {
                        Some(error) => error.lines().take(3).collect::<Vec<_>>().join("\n"),
                        None => tr(lang, Tr::DetLoading).to_owned(),
                    };
                    let failed = entry.is_some_and(|entry| entry.error.is_some());
                    div()
                        .text_size(px(11.))
                        .text_color(if failed { theme.red } else { theme.muted_foreground })
                        .child(text)
                        .into_any_element()
                }
            };
            div().px_2().py_1().child(content)
        });
        Some(
            v_flex()
                .id("pr-details-section")
                .min_w_0()
                .child(header)
                .children(body)
                .into_any_element(),
        )
    }

    fn pr_details_body(
        &self,
        pr: &PullRequest,
        details: &PrDetails,
        entry: Option<&DetailsEntry>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let number = pr.number;
        let when = timestamp_ms(&details.created_at)
            .map(|ms| relative_time(lang, now_unix_ms(), ms))
            .unwrap_or_default();
        let opened = tr(lang, Tr::DetOpenedBy).replace("@{user}", &format!("@{}", details.author)).replace("{when}", &when);
        let long = body_is_long(&details.body);
        let expanded = self.pr_extras.body_expanded;
        let pr_for_edit = pr.clone();
        let muted = |text: String| div().text_size(px(11.)).text_color(theme.muted_foreground).child(text);
        let description = if details.body.trim().is_empty() {
            muted(tr(lang, Tr::DetNoDescription).to_owned()).into_any_element()
        } else {
            div()
                .min_w_0()
                .text_size(px(12.))
                .when(!expanded, |node| node.line_clamp(CLAMP_LINES))
                .child(details.body.trim().to_owned())
                .into_any_element()
        };
        let reviewers = h_flex().flex_wrap().gap_1().children(
            reviewer_rows(details).into_iter().enumerate().map(|(index, row)| {
                let (icon, color, tip) = match row.mark {
                    ReviewMark::Approved => (Ic::CircleCheck, theme.green, Tr::DetReviewApproved),
                    ReviewMark::ChangesRequested => (Ic::CircleX, theme.red, Tr::DetReviewChanges),
                    ReviewMark::Commented => (Ic::Minus, theme.muted_foreground, Tr::DetReviewCommented),
                    ReviewMark::Pending => (Ic::Circle, theme.yellow, Tr::DetReviewPending),
                };
                let login = row.login.clone();
                chip(
                    format!("pr-rev-{index}"),
                    row.login,
                    theme.muted_foreground,
                    Some((Icon::new(icon).size(px(11.)).text_color(color).flex_shrink_0(), tr(lang, tip))),
                    row.removable.then(|| {
                        let key = (self.worktree.clone().unwrap_or_default(), number);
                        remove_button(format!("pr-rev-x-{index}"), lang, cx.listener(move |panel, _, _, cx| {
                            panel.pr_apply_edit(key.clone(), DetailsEdit::RemoveReviewer(login.clone()), cx);
                        }))
                    }),
                    &theme,
                )
            }),
        );
        let labels = h_flex().flex_wrap().gap_1().children(details.labels.iter().enumerate().map(|(index, label)| {
            let name = label.name.clone();
            let key = (self.worktree.clone().unwrap_or_default(), number);
            chip(
                format!("pr-label-{index}"),
                label.name.clone(),
                hex_color(&label.color),
                None,
                Some(remove_button(format!("pr-label-x-{index}"), lang, cx.listener(move |panel, _, _, cx| {
                    panel.pr_apply_edit(key.clone(), DetailsEdit::RemoveLabel(name.clone()), cx);
                }))),
                &theme,
            )
        }));
        let assignees = h_flex().flex_wrap().gap_1().children(details.assignees.iter().enumerate().map(|(index, login)| {
            let name = login.clone();
            let key = (self.worktree.clone().unwrap_or_default(), number);
            chip(
                format!("pr-assignee-{index}"),
                login.clone(),
                theme.muted_foreground,
                None,
                Some(remove_button(format!("pr-assignee-x-{index}"), lang, cx.listener(move |panel, _, _, cx| {
                    panel.pr_apply_edit(key.clone(), DetailsEdit::RemoveAssignee(name.clone()), cx);
                }))),
                &theme,
            )
        }));
        let group = |title: Tr, kind: PickKind, id: &'static str, chips: gpui_kit::Div, empty: bool, cx: &mut Context<Self>| {
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(tr(lang, title)),
                        )
                        .child(tool_button(id, Ic::Plus, tr(lang, Tr::DetAdd)).on_click(cx.listener(
                            move |panel, _, window, cx| panel.pr_open_picker(number, kind, window, cx),
                        ))),
                )
                .child(chips)
                .when(empty, |node| node.child(muted(tr(lang, Tr::DetNone).to_owned())))
        };
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .child(div().flex_1().min_w_0().truncate().text_size(px(11.)).text_color(theme.muted_foreground).child(opened))
                    .child(
                        Button::new("pr-details-edit")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(Ic::Pencil))
                            .tooltip(tr(lang, Tr::DetEdit))
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.pr_open_edit(&pr_for_edit, window, cx)
                            })),
                    ),
            )
            .child(description)
            .when(long, |node| {
                node.child(h_flex().child(
                    Button::new("pr-details-more")
                        .ghost()
                        .xsmall()
                        .label(tr(lang, if expanded { Tr::DetShowLess } else { Tr::DetShowMore }))
                        .on_click(cx.listener(|panel, _, _, cx| {
                            panel.pr_extras.body_expanded = !panel.pr_extras.body_expanded;
                            cx.notify();
                        })),
                ))
            })
            .child(group(Tr::DetReviewers, PickKind::Reviewer, "pr-add-reviewer", reviewers, reviewer_rows(details).is_empty(), cx))
            .child(group(Tr::DetLabels, PickKind::Label, "pr-add-label", labels, details.labels.is_empty(), cx))
            .child(group(Tr::DetAssignees, PickKind::Assignee, "pr-add-assignee", assignees, details.assignees.is_empty(), cx))
            .children(entry.and_then(|entry| entry.edit_error.as_ref()).map(|error| {
                div()
                    .text_size(px(11.))
                    .text_color(theme.red)
                    .line_clamp(3)
                    .child(error.lines().take(3).collect::<Vec<_>>().join("\n"))
            }))
            .into_any_element()
    }
}

type ClickHandler = Box<dyn Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static>;

fn remove_button(id: String, lang: Language, handler: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static) -> (SharedString, ClickHandler, &'static str) {
    (SharedString::from(id), Box::new(handler), tr(lang, Tr::DetRemove))
}

/// A small tinted pill: optional leading icon, text, optional remove "x".
fn chip(
    id: String,
    text: String,
    color: Hsla,
    icon: Option<(Icon, &'static str)>,
    remove: Option<(SharedString, ClickHandler, &'static str)>,
    theme: &gpui_kit::component::Theme,
) -> AnyElement {
    h_flex()
        .id(SharedString::from(id))
        .max_w_full()
        .min_w_0()
        .px_1()
        .gap_1()
        .items_center()
        .rounded_sm()
        .bg(color.opacity(0.18))
        .text_size(px(11.))
        .text_color(theme.foreground)
        .children(icon.map(|(icon, _)| icon))
        .child(div().min_w_0().truncate().child(text))
        .children(remove.map(|(id, handler, tip)| {
            div()
                .id(id)
                .flex_shrink_0()
                .cursor_pointer()
                .text_color(theme.muted_foreground)
                .hover(|style| style.text_color(theme.red))
                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
                .on_click(move |event, window, cx| {
                    handler(event, window, cx);
                    cx.stop_propagation();
                })
                .child(Icon::new(IconName::Close).size(px(10.)))
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use sich_git::forge::{Label, Reviewer};

    use super::*;

    #[test]
    fn fetch_policy() {
        let now = Instant::now();
        let mut entry = DetailsEntry::default();
        assert!(entry.needs_fetch("t1", now));
        entry.fetched_at = Some(now);
        entry.pr_updated = "t1".into();
        assert!(!entry.needs_fetch("t1", now + Duration::from_secs(5)));
        assert!(entry.needs_fetch("t1", now + REFRESH_AFTER));
        assert!(entry.needs_fetch("t2", now + Duration::from_secs(1)));
        entry.loading = true;
        assert!(!entry.needs_fetch("t2", now + Duration::from_secs(60)));
        entry.loading = false;
        entry.pending = 1;
        assert!(!entry.needs_fetch("t2", now + Duration::from_secs(60)));
    }

    #[test]
    fn colours_and_timestamps() {
        assert_eq!(hex_color("ff0000"), Hsla::from(rgb(0xff0000)));
        assert_eq!(hex_color("#00ff00"), Hsla::from(rgb(0x00ff00)));
        assert_eq!(hex_color(""), Hsla::from(rgb(0x8b949e)));
        assert_eq!(hex_color("zzzzzz"), Hsla::from(rgb(0x8b949e)));
        assert_eq!(timestamp_ms("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(timestamp_ms("garbage"), None);
        assert_eq!(timestamp_ms(""), None);
    }

    #[test]
    fn long_body_detection() {
        assert!(!body_is_long("short"));
        assert!(body_is_long(&"line\n".repeat(9)));
        assert!(body_is_long(&"x".repeat(601)));
        assert!(!body_is_long(&"line\n".repeat(8)));
    }

    #[test]
    fn review_marks_and_reviewer_rows() {
        assert_eq!(ReviewMark::from_state(Some("APPROVED")), ReviewMark::Approved);
        assert_eq!(ReviewMark::from_state(Some("CHANGES_REQUESTED")), ReviewMark::ChangesRequested);
        assert_eq!(ReviewMark::from_state(Some("COMMENTED")), ReviewMark::Commented);
        assert_eq!(ReviewMark::from_state(Some("DISMISSED")), ReviewMark::Pending);
        assert_eq!(ReviewMark::from_state(None), ReviewMark::Pending);
        let details = PrDetails {
            requested_reviewers: vec![
                Reviewer { login: "carol".into(), is_team: false },
                Reviewer { login: "acme/core".into(), is_team: true },
            ],
            reviews: vec![("dave".into(), "APPROVED".into()), ("carol".into(), "CHANGES_REQUESTED".into())],
            labels: vec![Label { name: "x".into(), color: String::new() }],
            ..PrDetails::default()
        };
        let rows = reviewer_rows(&details);
        assert_eq!(
            rows,
            vec![
                ReviewerRow { login: "carol".into(), mark: ReviewMark::ChangesRequested, removable: true },
                ReviewerRow { login: "acme/core".into(), mark: ReviewMark::Pending, removable: true },
                ReviewerRow { login: "dave".into(), mark: ReviewMark::Approved, removable: false },
            ]
        );
    }
}
