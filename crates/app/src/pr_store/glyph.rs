//! What a pull request looks like as a small status glyph (sidebar rows, right panel tab and the
//! compact row of the Git tab): icon, tone and tooltip text. Pure, so it is unit-tested.

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::Theme;
use gpui_kit::{Hsla, rgb};
use sich_git::forge::{MergeState, Mergeable, PrState, PullRequest};

use super::model::PrView;
use crate::i18n::{Language, Tr, tr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Success,
    Warning,
    Danger,
    Muted,
    Merged,
}

pub(crate) fn tone_color(tone: Tone, theme: &Theme) -> Hsla {
    match tone {
        Tone::Success => theme.green,
        Tone::Warning => theme.yellow,
        Tone::Danger => theme.red,
        Tone::Muted => theme.muted_foreground,
        Tone::Merged => rgb(0x8250df).into(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GlyphIcon {
    Open,
    Draft,
    Merged,
    Closed,
}

impl GlyphIcon {
    pub(crate) fn icon(self) -> Ic {
        match self {
            Self::Open => Ic::GitPullRequest,
            Self::Draft => Ic::GitPullRequestDraft,
            Self::Merged => Ic::GitMerge,
            Self::Closed => Ic::GitPullRequestClosed,
        }
    }
}

/// The second line of the tooltip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GlyphStatus {
    Conflicts,
    ChecksFailing(u32),
    ChecksRunning,
    ChecksPassing,
    /// Open without any checks.
    Open,
    Draft,
    Merged,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Glyph {
    pub icon: GlyphIcon,
    pub tone: Tone,
    pub status: GlyphStatus,
    pub number: u64,
    pub title: String,
}

fn has_conflicts(pr: &PullRequest) -> bool {
    pr.merge_state == MergeState::Dirty || pr.mergeable == Mergeable::Conflicting
}

/// Glyph of a pull request, whatever its state.
pub(crate) fn glyph_of_pr(pr: &PullRequest) -> Glyph {
    let (icon, tone, status) = match pr.state {
        PrState::Merged => (GlyphIcon::Merged, Tone::Merged, GlyphStatus::Merged),
        PrState::Closed => (GlyphIcon::Closed, Tone::Muted, GlyphStatus::Closed),
        PrState::Draft => (GlyphIcon::Draft, Tone::Muted, GlyphStatus::Draft),
        PrState::Open => {
            let summary = pr.summary();
            if has_conflicts(pr) {
                (GlyphIcon::Open, Tone::Danger, GlyphStatus::Conflicts)
            } else if summary.failed > 0 {
                (GlyphIcon::Open, Tone::Danger, GlyphStatus::ChecksFailing(summary.failed))
            } else if summary.pending > 0 {
                (GlyphIcon::Open, Tone::Warning, GlyphStatus::ChecksRunning)
            } else if summary.passed > 0 {
                (GlyphIcon::Open, Tone::Success, GlyphStatus::ChecksPassing)
            } else {
                (GlyphIcon::Open, Tone::Success, GlyphStatus::Open)
            }
        }
    };
    Glyph { icon, tone, status, number: pr.number, title: pr.title.clone() }
}

/// `None` unless the worktree has a pull request: loading, no PR, hidden and error states show
/// nothing.
pub(crate) fn glyph(view: &PrView) -> Option<Glyph> {
    view.pr().map(glyph_of_pr)
}

pub(crate) fn status_text(lang: Language, status: GlyphStatus) -> String {
    match status {
        GlyphStatus::Conflicts => tr(lang, Tr::PrMergeConflicts).to_owned(),
        GlyphStatus::ChecksFailing(1) => tr(lang, Tr::PrTipCheckFailingOne).to_owned(),
        GlyphStatus::ChecksFailing(n) => tr(lang, Tr::PrTipChecksFailing).replace("{n}", &n.to_string()),
        GlyphStatus::ChecksRunning => tr(lang, Tr::PrTipChecksRunning).to_owned(),
        GlyphStatus::ChecksPassing => tr(lang, Tr::PrTipChecksPassing).to_owned(),
        GlyphStatus::Open => tr(lang, Tr::PrStateOpen).to_owned(),
        GlyphStatus::Draft => tr(lang, Tr::PrStateDraft).to_owned(),
        GlyphStatus::Merged => tr(lang, Tr::PrStateMerged).to_owned(),
        GlyphStatus::Closed => tr(lang, Tr::PrStateClosed).to_owned(),
    }
}

impl Glyph {
    /// `PR #123 · title`.
    pub(crate) fn heading(&self) -> String {
        format!("PR #{} \u{b7} {}", self.number, self.title)
    }

    /// Two lines: heading, then the status.
    pub(crate) fn tooltip(&self, lang: Language) -> String {
        format!("{}\n{}", self.heading(), status_text(lang, self.status))
    }
}

#[cfg(test)]
mod tests {
    use sich_git::forge::CheckState;

    use super::super::testutil::{check, pr};
    use super::*;

    fn open(states: &[CheckState]) -> PullRequest {
        let mut p = pr(PrState::Open);
        p.checks = states.iter().copied().map(check).collect();
        p
    }

    #[test]
    fn open_pr_is_colored_by_its_checks() {
        use CheckState::*;
        let g = glyph_of_pr(&open(&[Success, Success]));
        assert_eq!((g.icon, g.tone, g.status), (GlyphIcon::Open, Tone::Success, GlyphStatus::ChecksPassing));
        let g = glyph_of_pr(&open(&[Success, Pending]));
        assert_eq!((g.tone, g.status), (Tone::Warning, GlyphStatus::ChecksRunning));
        let g = glyph_of_pr(&open(&[Pending, Failure, Failure, Success]));
        assert_eq!((g.tone, g.status), (Tone::Danger, GlyphStatus::ChecksFailing(2)));
        let g = glyph_of_pr(&open(&[]));
        assert_eq!((g.tone, g.status), (Tone::Success, GlyphStatus::Open));
        let g = glyph_of_pr(&open(&[Skipped, Neutral]));
        assert_eq!((g.tone, g.status), (Tone::Success, GlyphStatus::Open));
    }

    #[test]
    fn conflicts_turn_an_open_pr_red_whatever_its_checks() {
        let mut p = open(&[CheckState::Success]);
        p.merge_state = MergeState::Dirty;
        let g = glyph_of_pr(&p);
        assert_eq!((g.icon, g.tone, g.status), (GlyphIcon::Open, Tone::Danger, GlyphStatus::Conflicts));
        let mut p = open(&[CheckState::Pending]);
        p.mergeable = Mergeable::Conflicting;
        assert_eq!(glyph_of_pr(&p).status, GlyphStatus::Conflicts);
    }

    #[test]
    fn draft_merged_and_closed_have_their_own_icons() {
        let g = glyph_of_pr(&pr(PrState::Draft));
        assert_eq!((g.icon, g.tone, g.status), (GlyphIcon::Draft, Tone::Muted, GlyphStatus::Draft));
        let g = glyph_of_pr(&pr(PrState::Merged));
        assert_eq!((g.icon, g.tone, g.status), (GlyphIcon::Merged, Tone::Merged, GlyphStatus::Merged));
        let g = glyph_of_pr(&pr(PrState::Closed));
        assert_eq!((g.icon, g.tone, g.status), (GlyphIcon::Closed, Tone::Muted, GlyphStatus::Closed));
        // Checks and conflicts do not matter once the PR is not open.
        let mut closed = open(&[CheckState::Failure]);
        closed.state = PrState::Closed;
        closed.merge_state = MergeState::Dirty;
        assert_eq!(glyph_of_pr(&closed).status, GlyphStatus::Closed);
    }

    #[test]
    fn only_a_found_pr_gets_a_glyph() {
        assert!(glyph(&PrView::Hidden).is_none());
        assert!(glyph(&PrView::Loading).is_none());
        assert!(glyph(&PrView::NoPr).is_none());
        assert!(glyph(&PrView::GhMissing).is_none());
        assert!(glyph(&PrView::NotAuthenticated).is_none());
        assert!(glyph(&PrView::Error("x".into())).is_none());
        assert!(glyph(&PrView::Pr(Box::new(pr(PrState::Open)))).is_some());
    }

    #[test]
    fn tooltip_has_the_heading_and_a_status_line() {
        let mut p = open(&[CheckState::Failure, CheckState::Failure]);
        let tip = glyph_of_pr(&p).tooltip(Language::En);
        assert_eq!(tip, "PR #7 \u{b7} Add thing\n2 checks failing");
        p.checks = vec![check(CheckState::Failure)];
        assert!(glyph_of_pr(&p).tooltip(Language::En).ends_with("1 check failing"));
        let cases = [
            (GlyphStatus::ChecksPassing, "Checks passing"),
            (GlyphStatus::ChecksRunning, "Checks running"),
            (GlyphStatus::Draft, "Draft"),
            (GlyphStatus::Merged, "Merged"),
            (GlyphStatus::Closed, "Closed"),
            (GlyphStatus::Conflicts, "Conflicts"),
            (GlyphStatus::Open, "Open"),
        ];
        for (status, text) in cases {
            assert_eq!(status_text(Language::En, status), text);
        }
    }

    #[test]
    fn status_texts_are_filled_in_for_every_language() {
        for lang in Language::ALL {
            for status in [
                GlyphStatus::Conflicts,
                GlyphStatus::ChecksFailing(1),
                GlyphStatus::ChecksFailing(5),
                GlyphStatus::ChecksRunning,
                GlyphStatus::ChecksPassing,
                GlyphStatus::Open,
                GlyphStatus::Draft,
                GlyphStatus::Merged,
                GlyphStatus::Closed,
            ] {
                let text = status_text(lang, status);
                assert!(!text.is_empty() && !text.contains('{'), "{lang:?} {status:?}: {text}");
            }
            assert!(status_text(lang, GlyphStatus::ChecksFailing(5)).contains('5'));
        }
    }

    #[test]
    fn glyph_icons_map_to_distinct_lucide_icons() {
        let icons = [GlyphIcon::Open, GlyphIcon::Draft, GlyphIcon::Merged, GlyphIcon::Closed].map(GlyphIcon::icon);
        for (i, a) in icons.iter().enumerate() {
            for b in &icons[i + 1..] {
                assert_ne!(format!("{a:?}"), format!("{b:?}"));
            }
        }
    }
}
