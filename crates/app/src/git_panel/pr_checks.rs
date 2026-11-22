//! Checks list, check details and the conflicts block of the "Pull request" tab. The data and the
//! `gh` work live in `crate::pr_store`; the text helpers here are pure and unit-tested.

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, ClipboardItem, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use sich_git::forge::{
    CheckDetails, CheckRow, ConflictSummary, FailedJob, PullRequest, RerunScope, RowCounts, RowState,
    count_rows, parse_rfc3339, plan_rerun, row_duration,
};

use super::pr_card::{open_url, tone_color};
use super::render::{chevron, tiny, tool_button};
use super::*;
use crate::i18n::relative_time;
use crate::pr_store::{DetailState, PrAction, PrEntry, Tone};
use crate::sidebar::now_unix_ms;

const LOG_MAX_H: f32 = 220.;
const MONO: &str = "Cascadia Mono";
/// Long check output is clipped to this many characters.
const OUTPUT_CLIP: usize = 1500;
const MAX_CONFLICT_FILES: usize = 30;

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// "5 passing · 1 failing · 2 pending · 3 skipped" without the zero groups.
pub(super) fn summary_text(lang: Language, counts: RowCounts) -> String {
    let part = |key: Tr, n: u32| (n > 0).then(|| tr(lang, key).replace("{n}", &n.to_string()));
    let parts: Vec<String> = [
        part(Tr::PrCheckPassing, counts.passing),
        part(Tr::PrCheckFailing, counts.failing),
        part(Tr::PrCheckPendingN, counts.pending),
        part(Tr::PrCheckSkipped, counts.skipped),
    ]
    .into_iter()
    .flatten()
    .collect();
    parts.join(" \u{b7} ")
}

/// Icon and tone of a check row.
pub(super) fn row_icon(row: &CheckRow) -> (Ic, Tone) {
    if row.pending_approval {
        return (Ic::CircleAlert, Tone::Warning);
    }
    match row.state {
        RowState::Success => (Ic::CircleCheck, Tone::Success),
        RowState::Failure | RowState::TimedOut => (Ic::CircleX, Tone::Danger),
        RowState::ActionRequired => (Ic::CircleAlert, Tone::Warning),
        RowState::Cancelled => (Ic::CircleX, Tone::Muted),
        RowState::Pending => (Ic::Circle, Tone::Warning),
        RowState::Neutral | RowState::Skipped => (Ic::Minus, Tone::Muted),
    }
}

/// "5m \u{b7} 2m 10s": how long ago it finished (or started) and how long it ran.
pub(super) fn timing_text(lang: Language, row: &CheckRow, now_ms: u64) -> Option<String> {
    let at = row.completed_at.as_deref().or(row.started_at.as_deref()).and_then(parse_rfc3339);
    let age = at.map(|secs| relative_time(lang, now_ms, secs.max(0) as u64 * 1000));
    let duration = row_duration(row, (now_ms / 1000) as i64);
    match (age, duration) {
        (Some(age), Some(duration)) => Some(format!("{age} \u{b7} {duration}")),
        (Some(text), None) | (None, Some(text)) => Some(text),
        (None, None) => None,
    }
}

/// "2026-11-22 12:00:05" of an RFC 3339 timestamp in `timezone`.
pub(super) fn format_stamp<Tz: chrono::TimeZone>(stamp: &str, timezone: &Tz) -> Option<String>
where
    Tz::Offset: std::fmt::Display,
{
    let secs = parse_rfc3339(stamp)?;
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|date| date.with_timezone(timezone).format("%Y-%m-%d %H:%M:%S").to_string())
}

/// Tooltip of the timing text: "Started: ...\nFinished: ...".
pub(super) fn timing_tooltip<Tz: chrono::TimeZone>(lang: Language, row: &CheckRow, timezone: &Tz) -> Option<String>
where
    Tz::Offset: std::fmt::Display,
{
    let line = |key: Tr, stamp: &Option<String>| {
        let text = format_stamp(stamp.as_deref()?, timezone)?;
        Some(format!("{}: {text}", tr(lang, key)))
    };
    let lines: Vec<String> = [
        line(Tr::PrCheckStarted, &row.started_at),
        line(Tr::PrCheckFinished, &row.completed_at),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The row as a job to describe to an agent. The check run id is the job id for Actions checks.
pub(super) fn failed_job_of(row: &CheckRow) -> FailedJob {
    FailedJob {
        name: row.label(),
        url: row.url.clone(),
        job_id: row.workflow_run_id.and(row.check_run_id),
    }
}

/// Rows that "Send to agent" can describe: failed, timed out or cancelled checks.
pub(super) fn failed_jobs_of(rows: &[CheckRow]) -> Vec<FailedJob> {
    rows.iter()
        .filter(|row| row.state.is_rerunnable_failure())
        .map(failed_job_of)
        .collect()
}

/// At most `max` characters, with an ellipsis when cut.
pub(super) fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}\u{2026}", text[..end].trim_end()),
        None => text.to_owned(),
    }
}

/// "3 commits behind main (base commit abc1234)", `None` when not behind.
pub(super) fn behind_text(lang: Language, summary: &ConflictSummary, base: &str) -> Option<String> {
    let key = match summary.behind {
        0 => return None,
        1 => Tr::PrConflictsBehindOne,
        _ => Tr::PrConflictsBehind,
    };
    Some(
        tr(lang, key)
            .replace("{n}", &summary.behind.to_string())
            .replace("{base}", base)
            .replace("{sha}", summary.base_short()),
    )
}

/// The conflict block's body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ConflictBody {
    /// Not computed (yet).
    Nothing,
    Error(String),
    /// GitHub says conflicts, local Git found none.
    NotReproduced,
    Files { behind: Option<String>, files: Vec<String>, more: usize },
}

pub(super) fn conflict_body(
    lang: Language,
    result: Option<&Result<ConflictSummary, String>>,
    base: &str,
) -> ConflictBody {
    match result {
        None => ConflictBody::Nothing,
        Some(Err(message)) => ConflictBody::Error(message.clone()),
        Some(Ok(summary)) if summary.files.is_empty() => ConflictBody::NotReproduced,
        Some(Ok(summary)) => ConflictBody::Files {
            behind: behind_text(lang, summary, base),
            files: summary.files.iter().take(MAX_CONFLICT_FILES).cloned().collect(),
            more: summary.files.len().saturating_sub(MAX_CONFLICT_FILES),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Panel wrappers
// ---------------------------------------------------------------------------------------------

impl GitPanel {
    pub(super) fn pr_toggle_check(&mut self, row: &CheckRow, cx: &mut Context<Self>) {
        if let Some(path) = self.worktree.clone() {
            self.store.update(cx, |store, cx| store.toggle_check(&path, row, cx));
        }
    }

    fn pr_toggle_checks_list(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.worktree.clone() {
            self.store.update(cx, |store, cx| store.toggle_checks_list(&path, cx));
        }
    }

    fn pr_rerun(&mut self, rows: &[CheckRow], scope: RerunScope, cx: &mut Context<Self>) {
        let calls = plan_rerun(rows, scope);
        if !calls.is_empty() {
            self.pr_run_action(PrAction::Rerun(calls), cx);
        }
    }

    fn pr_send_conflicts(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.worktree.clone() {
            self.store.update(cx, |store, cx| store.send_conflicts_to_agent(&path, cx));
        }
    }

    /// The PR tab is on screen: the store fetches and polls the detailed checks for it.
    pub(super) fn pr_tab_visible(&self, cx: &mut Context<Self>) {
        if let Some(path) = self.worktree.clone() {
            self.store.update(cx, |store, cx| store.pr_tab_shown(&path, cx));
        }
    }

    /// The PR tab was left: the store stops polling.
    pub(super) fn pr_tab_left(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, _| store.pr_tab_hidden());
    }

    // ---- checks list -----------------------------------------------------------------------

    pub(super) fn pr_checks_block(
        &self,
        pr: &PullRequest,
        state: &PrEntry,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let checks = &state.checks;
        if !checks.loaded {
            return match (&checks.error, pr.checks.is_empty()) {
                (Some(error), _) => Some(self.checks_error_line(error, lang, cx)),
                (None, true) => None,
                (None, false) => Some(tiny(tr(lang, Tr::PrChecking), cx)),
            };
        }
        if checks.rows.is_empty() {
            return Some(tiny(tr(lang, Tr::PrChecksNone), cx));
        }
        let rows = checks.rows.clone();
        let counts = count_rows(&rows);
        let busy = state.busy;
        let failed_plan = !plan_rerun(&rows, RerunScope::Failed).is_empty();
        let all_plan = !plan_rerun(&rows, RerunScope::All).is_empty();
        let failed_jobs = failed_jobs_of(&rows);

        let summary_segments = [
            (counts.passing, Ic::CircleCheck, Tone::Success, Tr::PrCheckPassing),
            (counts.failing, Ic::CircleX, Tone::Danger, Tr::PrCheckFailing),
            (counts.pending, Ic::Circle, Tone::Warning, Tr::PrCheckPendingN),
            (counts.skipped, Ic::Minus, Tone::Muted, Tr::PrCheckSkipped),
        ];
        let summary = h_flex()
            .id("pr-checks-summary")
            .h(px(22.))
            .gap_2()
            .items_center()
            .min_w_0()
            .cursor_pointer()
            .hover(|style| style.bg(theme.list_hover))
            .tooltip({
                let text: SharedString = summary_text(lang, counts).into();
                move |window, cx| Tooltip::new(text.clone()).build(window, cx)
            })
            .on_click(cx.listener(|panel, _, _, cx| panel.pr_toggle_checks_list(cx)))
            .child(chevron(!checks.list_collapsed))
            .children(summary_segments.into_iter().filter(|(n, ..)| *n > 0).map(|(n, icon, tone, key)| {
                h_flex()
                    .gap_1()
                    .items_center()
                    .flex_shrink_0()
                    .child(Icon::new(icon).size(px(12.)).text_color(tone_color(tone, cx)))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(tr(lang, key).replace("{n}", &n.to_string())),
                    )
            }));

        let buttons = h_flex()
            .gap_1()
            .items_center()
            .child({
                let rows = rows.clone();
                Button::new("pr-rerun-failed")
                    .ghost()
                    .xsmall()
                    .label(tr(lang, Tr::PrRerunFailed))
                    .disabled(busy || !failed_plan)
                    .on_click(cx.listener(move |panel, _, _, cx| panel.pr_rerun(&rows, RerunScope::Failed, cx)))
            })
            .child({
                let rows = rows.clone();
                Button::new("pr-rerun-all")
                    .ghost()
                    .xsmall()
                    .label(tr(lang, Tr::PrRerunAll))
                    .disabled(busy || !all_plan)
                    .on_click(cx.listener(move |panel, _, _, cx| panel.pr_rerun(&rows, RerunScope::All, cx)))
            })
            .children((!failed_jobs.is_empty()).then(|| {
                Button::new("pr-send-all")
                    .ghost()
                    .xsmall()
                    .label(tr(lang, Tr::PrSendFailuresToAgent))
                    .disabled(busy)
                    .loading(busy)
                    .on_click(cx.listener(move |panel, _, _, cx| panel.pr_send_to_agent(failed_jobs.clone(), cx)))
            }));

        let list = (!checks.list_collapsed).then(|| {
            v_flex().children(
                rows.iter()
                    .enumerate()
                    .map(|(index, row)| self.pr_check_row(row, index, state, lang, cx)),
            )
        });
        Some(
            v_flex()
                .gap_1()
                .child(buttons)
                .child(summary)
                .children(checks.error.as_ref().map(|error| self.checks_error_line(error, lang, cx)))
                .children(list)
                .into_any_element(),
        )
    }

    fn checks_error_line(&self, error: &str, lang: Language, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let full: SharedString = error.to_owned().into();
        h_flex()
            .gap_1()
            .items_center()
            .child(
                div()
                    .id("pr-checks-error")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(theme.warning)
                    .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
                    .child(error.lines().next().unwrap_or_default().to_owned()),
            )
            .child(
                Button::new("pr-checks-retry")
                    .ghost()
                    .xsmall()
                    .label(tr(lang, Tr::PrRetry))
                    .on_click(cx.listener(|panel, _, _, cx| panel.pr_refresh_now(cx))),
            )
            .into_any_element()
    }

    fn pr_check_row(
        &self,
        row: &CheckRow,
        index: usize,
        state: &PrEntry,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let expanded = state.checks.is_expanded(row);
        let (icon, tone) = row_icon(row);
        let mut label = row.label();
        if row.pending_approval {
            label = format!("{label} ({})", tr(lang, Tr::PrCheckApproval));
        }
        let label: SharedString = label.into();
        let tooltip = label.clone();
        let timing = timing_text(lang, row, now_unix_ms());
        let timing_tip = timing_tooltip(lang, row, &chrono::Local);
        let url = row.url.clone();
        let send_job = row.state.is_rerunnable_failure().then(|| failed_job_of(row));
        let busy = state.busy;
        let toggle_row = row.clone();

        let head = h_flex()
            .id(("pr-check", index))
            .h(px(22.))
            .gap_1()
            .items_center()
            .min_w_0()
            .cursor_pointer()
            .hover(|style| style.bg(theme.list_hover))
            .on_click(cx.listener(move |panel, _, _, cx| panel.pr_toggle_check(&toggle_row, cx)))
            .child(chevron(expanded))
            .child(Icon::new(icon).size(px(12.)).text_color(tone_color(tone, cx)).flex_shrink_0())
            .child(
                div()
                    .id(("pr-check-name", index))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .child(label),
            )
            .children(timing.map(|text| {
                let tip = timing_tip.clone();
                div()
                    .id(("pr-check-time", index))
                    .flex_shrink_0()
                    .text_size(px(10.5))
                    .text_color(theme.muted_foreground)
                    .when_some(tip, |node, tip| {
                        node.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    })
                    .child(text)
            }))
            .children(url.map(|url| {
                tool_button(("pr-check-open", index), Ic::ExternalLink, tr(lang, Tr::PrOpenInBrowser))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        open_url(&url, cx);
                    })
            }))
            .children(send_job.map(|job| {
                tool_button(("pr-send", index), Ic::Bot, tr(lang, Tr::PrSendToAgent))
                    .disabled(busy)
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        cx.stop_propagation();
                        panel.pr_send_to_agent(vec![job.clone()], cx)
                    }))
            }));

        let details = expanded.then(|| self.pr_check_details(row, index, state, lang, cx));
        v_flex().child(head).children(details).into_any_element()
    }

    fn pr_check_details(
        &self,
        row: &CheckRow,
        index: usize,
        state: &PrEntry,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let body = match state.checks.details.get(&row.key()) {
            None | Some(DetailState::Loading) => h_flex()
                .gap_1()
                .items_center()
                .child(Icon::new(Ic::LoaderCircle).size(px(12.)).text_color(theme.muted_foreground))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child(tr(lang, Tr::PrCheckLoading)),
                )
                .into_any_element(),
            Some(DetailState::Failed(message)) => div()
                .text_size(px(11.))
                .text_color(theme.red)
                .line_clamp(3)
                .child(message.lines().take(3).collect::<Vec<_>>().join("\n"))
                .into_any_element(),
            Some(DetailState::Loaded(details)) if details.is_empty() => div()
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .child(tr(lang, Tr::PrCheckNoDetails))
                .into_any_element(),
            Some(DetailState::Loaded(details)) => self.pr_details_body(details, index, lang, cx),
        };
        div().pl_5().pr_1().pb_1().child(body).into_any_element()
    }

    fn pr_details_body(&self, details: &CheckDetails, index: usize, lang: Language, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let heading = |text: &'static str| {
            div().text_size(px(10.5)).text_color(theme.muted_foreground).child(text)
        };
        let mut sections: Vec<AnyElement> = Vec::new();

        if !details.jobs.is_empty() {
            let mut jobs = v_flex().gap_1().child(heading(tr(lang, Tr::PrCheckFailedJobs)));
            for (job_index, job) in details.jobs.iter().enumerate() {
                let mut block = v_flex().gap_px().child(
                    div()
                        .text_size(px(11.))
                        .child(format!("{} ({})", job.name, job.conclusion.replace('_', " "))),
                );
                if !job.failed_steps.is_empty() {
                    block = block.child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{}: {}",
                                tr(lang, Tr::PrCheckFailedSteps),
                                job.failed_steps
                                    .iter()
                                    .map(|step| format!("{}. {}", step.number, step.name))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )),
                    );
                }
                let log_id = (index * 16 + job_index) as u64;
                block = block.child(match &job.log_tail {
                    Some(log) => {
                        let copy = log.clone();
                        div()
                            .relative()
                            .child(
                                div()
                                    .id(("pr-check-log", log_id))
                                    .max_h(px(LOG_MAX_H))
                                    .overflow_y_scroll()
                                    .p_1()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.background)
                                    .font_family(MONO)
                                    .text_size(px(10.5))
                                    .child(log.clone()),
                            )
                            .child(
                                div().absolute().top_0().right_0().child(
                                    tool_button(("pr-check-log-copy", log_id), Ic::Copy, tr(lang, Tr::PrCheckCopyLog))
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                                        }),
                                ),
                            )
                            .into_any_element()
                    }
                    None => div()
                        .text_size(px(10.5))
                        .text_color(theme.muted_foreground)
                        .child(tr(lang, Tr::PrCheckNoLog))
                        .into_any_element(),
                });
                jobs = jobs.child(block);
            }
            sections.push(jobs.into_any_element());
        }

        if let Some(output) = &details.output {
            let mut block = v_flex().gap_px().child(heading(tr(lang, Tr::PrCheckOutput)));
            for text in [&output.title, &output.summary, &output.text] {
                if !text.is_empty() {
                    block = block.child(div().text_size(px(11.)).child(clip(text, OUTPUT_CLIP)));
                }
            }
            sections.push(block.into_any_element());
        }

        if !details.annotations.is_empty() {
            let mut block = v_flex().gap_px().child(heading(tr(lang, Tr::PrCheckAnnotations)));
            for annotation in &details.annotations {
                let place = if annotation.line > 0 {
                    format!("{}:{}", annotation.path, annotation.line)
                } else {
                    annotation.path.clone()
                };
                block = block.child(
                    v_flex()
                        .child(div().font_family(MONO).text_size(px(10.5)).text_color(theme.muted_foreground).child(place))
                        .child(div().text_size(px(11.)).child(clip(&annotation.message, 400))),
                );
            }
            sections.push(block.into_any_element());
        }
        v_flex().gap_2().children(sections).into_any_element()
    }

    // ---- conflicts -------------------------------------------------------------------------

    /// The warning card for a conflicting PR; `None` when the PR does not conflict.
    pub(super) fn pr_conflicts_block(
        &self,
        pr: &PullRequest,
        state: &PrEntry,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !crate::pr_store::is_conflicting(pr) {
            return None;
        }
        let theme = cx.theme().clone();
        let warn = theme.yellow;
        let body = conflict_body(lang, state.conflicts.as_ref(), &pr.base);
        let muted = |text: String| div().text_size(px(11.)).text_color(theme.muted_foreground).child(text);
        let content: Vec<AnyElement> = match body {
            ConflictBody::Nothing => Vec::new(),
            ConflictBody::Error(message) => vec![muted(message.lines().next().unwrap_or_default().to_owned()).into_any_element()],
            ConflictBody::NotReproduced => vec![muted(tr(lang, Tr::PrConflictsNotReproduced).to_owned()).into_any_element()],
            ConflictBody::Files { behind, files, more } => {
                let mut out = Vec::new();
                out.extend(behind.map(|text| muted(text).into_any_element()));
                let mut list = v_flex().gap_px();
                for file in files {
                    list = list.child(div().font_family(MONO).text_size(px(11.)).truncate().child(file));
                }
                if more > 0 {
                    list = list.child(muted(format!("+{more}")));
                }
                out.push(list.into_any_element());
                out.push(
                    h_flex()
                        .child(
                            Button::new("pr-conflicts-send")
                                .xsmall()
                                .icon(Icon::new(Ic::Bot))
                                .label(tr(lang, Tr::PrConflictsSend))
                                .disabled(state.busy)
                                .on_click(cx.listener(|panel, _, _, cx| panel.pr_send_conflicts(cx))),
                        )
                        .into_any_element(),
                );
                out
            }
        };
        Some(
            v_flex()
                .gap_1()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(warn.opacity(0.5))
                .bg(warn.opacity(0.1))
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(Icon::new(Ic::CircleAlert).size(px(13.)).text_color(warn).flex_shrink_0())
                        .child(div().text_size(px(11.5)).child(tr(lang, Tr::PrConflictsTitle))),
                )
                .children(content)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: RowState) -> CheckRow {
        CheckRow {
            name: "test".into(),
            workflow: Some("CI".into()),
            state,
            check_run_id: Some(5),
            workflow_run_id: Some(9),
            url: Some("https://github.com/o/r/actions/runs/9/job/5".into()),
            started_at: Some("2026-11-22T10:00:00Z".into()),
            completed_at: Some("2026-11-22T10:02:10Z".into()),
            pending_approval: false,
        }
    }

    fn summary(behind: u32, files: &[&str]) -> ConflictSummary {
        ConflictSummary {
            head_oid: "h".into(),
            base_oid: "abcdef0123".into(),
            behind,
            files: files.iter().map(|f| (*f).to_owned()).collect(),
        }
    }

    #[test]
    fn summary_text_skips_empty_groups() {
        let counts = RowCounts { passing: 5, failing: 1, pending: 0, skipped: 3 };
        assert_eq!(summary_text(Language::En, counts), "5 passing \u{b7} 1 failing \u{b7} 3 skipped");
        assert_eq!(summary_text(Language::En, RowCounts::default()), "");
        for lang in Language::ALL {
            let text = summary_text(lang, RowCounts { passing: 1, failing: 2, pending: 3, skipped: 4 });
            assert!(!text.contains('{') && text.contains('4'), "{lang:?}: {text}");
        }
    }

    #[test]
    fn icons_follow_state() {
        assert_eq!(row_icon(&row(RowState::Success)), (Ic::CircleCheck, Tone::Success));
        assert_eq!(row_icon(&row(RowState::Failure)).1, Tone::Danger);
        assert_eq!(row_icon(&row(RowState::TimedOut)).1, Tone::Danger);
        assert_eq!(row_icon(&row(RowState::Pending)).1, Tone::Warning);
        assert_eq!(row_icon(&row(RowState::Skipped)).1, Tone::Muted);
        let mut approval = row(RowState::ActionRequired);
        approval.pending_approval = true;
        assert_eq!(row_icon(&approval), (Ic::CircleAlert, Tone::Warning));
    }

    #[test]
    fn timing_combines_age_and_duration() {
        let now_ms = parse_rfc3339("2026-11-22T10:07:10Z").unwrap() as u64 * 1000;
        assert_eq!(timing_text(Language::En, &row(RowState::Success), now_ms).as_deref(), Some("5m \u{b7} 2m 10s"));
        let mut bare = row(RowState::Success);
        bare.started_at = None;
        bare.completed_at = None;
        assert_eq!(timing_text(Language::En, &bare, now_ms), None);
        bare.started_at = Some("2026-11-22T10:06:00Z".into());
        bare.state = RowState::Pending;
        assert_eq!(timing_text(Language::En, &bare, now_ms).as_deref(), Some("1m \u{b7} 1m 10s"));
    }

    #[test]
    fn tooltip_lists_started_and_finished_in_the_display_timezone() {
        let plus_two = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(
            timing_tooltip(Language::En, &row(RowState::Success), &plus_two).as_deref(),
            Some("Started: 2026-11-22 12:00:00\nFinished: 2026-11-22 12:02:10")
        );
        let mut running = row(RowState::Pending);
        running.completed_at = None;
        assert_eq!(
            timing_tooltip(Language::En, &running, &plus_two).as_deref(),
            Some("Started: 2026-11-22 12:00:00")
        );
        running.started_at = None;
        assert_eq!(timing_tooltip(Language::En, &running, &plus_two), None);
    }

    #[test]
    fn failed_jobs_for_the_agent() {
        let job = failed_job_of(&row(RowState::Failure));
        assert_eq!((job.name.as_str(), job.job_id), ("CI / test", Some(5)));
        let mut external = row(RowState::Failure);
        external.workflow_run_id = None;
        external.workflow = None;
        assert_eq!(failed_job_of(&external).job_id, None, "not an Actions job: no log by job id");
        let rows = vec![row(RowState::Success), row(RowState::Failure), row(RowState::Cancelled), row(RowState::Pending)];
        assert_eq!(failed_jobs_of(&rows).len(), 2);
    }

    #[test]
    fn clip_cuts_on_characters() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("abcdef", 3), "abc\u{2026}");
        assert_eq!(clip("\u{43f}\u{440}\u{438}\u{432}\u{456}\u{442}", 3), "\u{43f}\u{440}\u{438}\u{2026}");
        assert_eq!(clip("", 3), "");
    }

    #[test]
    fn behind_text_and_conflict_body() {
        assert_eq!(
            behind_text(Language::En, &summary(3, &["a"]), "main").as_deref(),
            Some("3 commits behind main (base commit abcdef0)")
        );
        assert_eq!(
            behind_text(Language::En, &summary(1, &["a"]), "main").as_deref(),
            Some("1 commit behind main (base commit abcdef0)")
        );
        assert_eq!(behind_text(Language::En, &summary(0, &["a"]), "main"), None);
        assert_eq!(conflict_body(Language::En, None, "main"), ConflictBody::Nothing);
        assert_eq!(
            conflict_body(Language::En, Some(&Err("boom".into())), "main"),
            ConflictBody::Error("boom".into())
        );
        assert_eq!(conflict_body(Language::En, Some(&Ok(summary(2, &[]))), "main"), ConflictBody::NotReproduced);
        let many: Vec<String> = (0..35).map(|i| format!("f{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        match conflict_body(Language::En, Some(&Ok(summary(0, &refs))), "main") {
            ConflictBody::Files { behind, files, more } => {
                assert_eq!(behind, None);
                assert_eq!((files.len(), more), (MAX_CONFLICT_FILES, 5));
            }
            other => panic!("unexpected {other:?}"),
        }
        for lang in Language::ALL {
            assert!(!behind_text(lang, &summary(5, &["a"]), "dev").unwrap().contains('{'));
        }
    }
}
