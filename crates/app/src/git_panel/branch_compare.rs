//! Branch compare against a base ref: the compact row under the PR row, the base picker and the
//! collapsible "Branch changes" list (merge-base..HEAD).

use std::collections::BTreeMap;
use std::time::Duration;

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, StyleRefinement, Styled as _, div, px,
};
use serde::{Deserialize, Serialize};
use sich_git::compare::CompareSource;
use sich_git::{BranchCompare, CompareStatus};

use super::render::chevron;
use super::*;
use crate::pr_store::PrEntry;
use crate::workspace::normalize_path;

const MONO: &str = "Cascadia Mono";
/// A finished comparison is recomputed at least this often, even if nothing we can see changed
/// (the base branch may have moved after a fetch from a terminal).
const STALE_AFTER: Duration = Duration::from_secs(30);
const FILTER_THRESHOLD: usize = 12;

/// Per-worktree pinned base refs, stored in `<config>/sich/branch_bases.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct BranchBases(BTreeMap<String, String>);

impl BranchBases {
    fn parse(text: &str) -> Self {
        serde_json::from_str(text).unwrap_or_default()
    }

    fn load() -> Self {
        crate::settings::config_file("branch_bases.json")
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    fn save(&self) {
        let Some(path) = crate::settings::config_file("branch_bases.json") else { return };
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
            std::fs::rename(&tmp, &path)
        };
        if let Err(error) = write() {
            tracing::warn!("could not save {}: {error}", path.display());
        }
    }

    pub(super) fn get(&self, worktree: &Path) -> Option<&str> {
        self.0.get(&normalize_path(worktree)).map(String::as_str).filter(|base| !base.is_empty())
    }

    pub(super) fn set(&mut self, worktree: &Path, base: Option<String>) {
        let key = normalize_path(worktree);
        match base.map(|base| base.trim().to_owned()).filter(|base| !base.is_empty()) {
            Some(base) => self.0.insert(key, base),
            None => self.0.remove(&key),
        };
    }
}

/// `<base remote>/<pr base>`: the remote-tracking ref of the pull request's target branch.
pub(super) fn pr_base_ref(entry: &PrEntry) -> Option<String> {
    let pr = entry.view.pr()?;
    let remote = entry.target.as_ref()?.base_remote.as_deref()?;
    (!pr.base.is_empty()).then(|| format!("{remote}/{}", pr.base))
}

/// Everything the comparison depends on that the status shows us. When it is unchanged (and the
/// result is fresh) the comparison is not recomputed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Fingerprint {
    pinned: Option<String>,
    pr_base: Option<String>,
    head: Option<String>,
    staged: usize,
    unstaged: usize,
    lines: u64,
}

pub(super) fn fingerprint(
    pinned: Option<&str>,
    pr_base: Option<&str>,
    status: &RepoStatus,
) -> Fingerprint {
    let lines = status
        .staged
        .iter()
        .chain(&status.unstaged)
        .map(|file| u64::from(file.added.unwrap_or(0)) + u64::from(file.removed.unwrap_or(0)))
        .sum();
    Fingerprint {
        pinned: pinned.map(str::to_owned),
        pr_base: pr_base.map(str::to_owned),
        head: status.head_oid.clone(),
        staged: status.staged.len(),
        unstaged: status.unstaged.len(),
        lines,
    }
}

/// Whether the current branch is the base itself (`main` vs `origin/main`).
pub(super) fn is_base_branch(branch: &str, base_ref: &str) -> bool {
    base_ref == branch
        || base_ref
            .strip_suffix(branch)
            .is_some_and(|prefix| prefix.ends_with('/'))
}

/// The row has nothing to say on the base branch itself with no commits and no changes.
pub(super) fn hide_row(branch: Option<&str>, compare: &BranchCompare) -> bool {
    compare.status == CompareStatus::Ready
        && compare.is_empty()
        && branch.is_some_and(|branch| is_base_branch(branch, &compare.base_ref))
}

pub(super) fn ahead_behind_tooltip(lang: Language, ahead: u32, behind: u32, base: &str) -> String {
    let mut lines = Vec::new();
    if ahead > 0 {
        lines.push(format!("{ahead} {} {base}", tr(lang, Tr::CmpAheadOf)));
    }
    if behind > 0 {
        lines.push(format!("{behind} {} {base}", tr(lang, Tr::CmpBehindOf)));
    }
    if lines.is_empty() {
        lines.push(format!("0 {} {base}", tr(lang, Tr::CmpAheadOf)));
    }
    lines.join("\n")
}

pub(super) enum Loaded {
    /// No base ref could be determined (no remote, no `main`/`master`).
    NoBase,
    Failed(String),
    Done(Box<BranchCompare>),
}

pub(super) struct CompareState {
    bases: BranchBases,
    loaded: Option<Loaded>,
    loading: bool,
    /// Something changed while a comparison was running: run again when it finishes.
    dirty: bool,
    fingerprint: Option<Fingerprint>,
    computed_at: Option<Instant>,
    request: u64,
    changes_open: bool,
    picker: Option<Entity<BasePicker>>,
}

impl CompareState {
    pub(super) fn new() -> Self {
        Self {
            bases: BranchBases::load(),
            loaded: None,
            loading: false,
            dirty: false,
            fingerprint: None,
            computed_at: None,
            request: 0,
            changes_open: false,
            picker: None,
        }
    }

    /// A different worktree was selected.
    pub(super) fn reset(&mut self) {
        self.loaded = None;
        self.loading = false;
        self.dirty = false;
        self.fingerprint = None;
        self.computed_at = None;
        self.request += 1;
        self.changes_open = false;
        self.picker = None;
    }

    /// The refs moved (fetch, pull, commit...): recompute on the next status.
    pub(super) fn invalidate(&mut self) {
        self.fingerprint = None;
    }

    fn fresh(&self, fingerprint: &Fingerprint) -> bool {
        self.loaded.is_some()
            && self.fingerprint.as_ref() == Some(fingerprint)
            && self.computed_at.is_some_and(|at| at.elapsed() < STALE_AFTER)
    }
}

impl GitPanel {
    /// Called after every status refresh. Cheap: it only starts a comparison when something it
    /// depends on changed.
    pub(super) fn compare_sync(&mut self, cx: &mut Context<Self>) {
        let (Some(repo), Some(path), Some(status)) =
            (self.repo.clone(), self.worktree.clone(), self.status.as_ref())
        else {
            return;
        };
        if status.head_oid.is_none() {
            self.compare.loaded = None;
            return;
        }
        let pinned = self.compare.bases.get(&path).map(str::to_owned);
        let pr_base = self.store.read(cx).entry(&path).and_then(pr_base_ref);
        let print = fingerprint(pinned.as_deref(), pr_base.as_deref(), status);
        if self.compare.fresh(&print) {
            return;
        }
        if self.compare.loading {
            self.compare.dirty = true;
            return;
        }
        self.compare.loading = true;
        self.compare.dirty = false;
        self.compare.request += 1;
        let request = self.compare.request;
        let generation = self.generation;
        let job = cx.background_spawn(async move {
            let known = sich_git::choose_base(pinned.as_deref(), pr_base.as_deref(), None);
            let default = if known.is_none() {
                repo.default_compare_base().map_err(|error| error.to_string())?
            } else {
                None
            };
            match sich_git::choose_base(pinned.as_deref(), pr_base.as_deref(), default.as_deref()) {
                None => Ok(Loaded::NoBase),
                Some(base) => repo
                    .branch_compare(&base)
                    .map(|compare| Loaded::Done(Box::new(compare)))
                    .map_err(|error| error.to_string()),
            }
        });
        cx.spawn(async move |panel, cx| {
            let result = job.await;
            let _ = panel.update(cx, |panel, cx| {
                panel.generation.apply(generation, || {
                    if panel.compare.request != request {
                        return;
                    }
                    panel.compare.loading = false;
                    panel.compare.loaded = Some(result.unwrap_or_else(Loaded::Failed));
                    panel.compare.fingerprint = Some(print);
                    panel.compare.computed_at = Some(Instant::now());
                    if panel.compare.dirty {
                        panel.compare.dirty = false;
                        panel.compare_sync(cx);
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// Pins `base` for this worktree, or goes back to the default with `None`.
    fn set_base(&mut self, base: Option<String>, cx: &mut Context<Self>) {
        let Some(path) = self.worktree.clone() else { return };
        self.compare.bases.set(&path, base);
        self.compare.bases.save();
        self.compare.invalidate();
        self.compare_sync(cx);
        cx.notify();
    }

    fn base_popover(
        &self,
        id: &'static str,
        trigger: Button,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let panel = cx.weak_entity();
        let picker = self.compare.picker.clone();
        Popover::new(id)
            .trigger_style(StyleRefinement::default().min_w_0())
            .trigger(trigger.disabled(self.repo.is_none()))
            .open(picker.is_some())
            .content(move |_, _, _| div().children(picker.clone()))
            .on_open_change(move |open, window, cx| {
                let _ = panel.update(cx, |this, cx| {
                    if *open {
                        if this.compare.picker.is_none()
                            && let (Some(repo), Some(path)) = (this.repo.clone(), this.worktree.clone())
                        {
                            let current = this.compare.bases.get(&path).map(str::to_owned);
                            let weak = cx.weak_entity();
                            let generation = this.generation;
                            this.compare.picker = Some(cx.new(|cx| {
                                BasePicker::new(repo, weak, generation, current, lang, window, cx)
                            }));
                        }
                    } else {
                        this.compare.picker = None;
                    }
                    cx.notify();
                });
            })
    }

    /// Compact row under the PR row: base ref, ahead/behind, line totals.
    pub(super) fn branch_compare_row(
        &self,
        status: &RepoStatus,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let row = || h_flex().h(px(24.)).flex_shrink_0().min_w_0().px_2().gap_1().items_center();
        let compare = match self.compare.loaded.as_ref() {
            None if self.compare.loading => {
                return Some(
                    row()
                        .child(Icon::new(Ic::LoaderCircle).size(px(12.)).text_color(theme.muted_foreground))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(format!("{} …", tr(lang, Tr::CmpVs))),
                        )
                        .into_any_element(),
                );
            }
            None | Some(Loaded::NoBase) => return None,
            Some(Loaded::Failed(error)) => {
                return Some(self.compare_warning(error.clone(), lang, cx));
            }
            Some(Loaded::Done(compare)) => compare,
        };
        match compare.status {
            CompareStatus::UnbornHead => return None,
            CompareStatus::InvalidBase => {
                let text = format!("{}: {}", tr(lang, Tr::CmpBaseMissing), compare.base_ref);
                return Some(self.compare_warning(text, lang, cx));
            }
            CompareStatus::NoMergeBase => {
                let text = format!("{} {}", tr(lang, Tr::CmpNoCommonHistory), compare.base_ref);
                return Some(self.compare_warning(text, lang, cx));
            }
            CompareStatus::Ready => {}
        }
        if hide_row(status.branch.as_deref(), compare) {
            return None;
        }
        let base_button = Button::new("cmp-base")
            .ghost()
            .xsmall()
            .compact()
            .accessibility_label(compare.base_ref.clone())
            .tooltip(tr(lang, Tr::CmpBaseTooltip))
            .child(
                div()
                    .max_w(px(150.))
                    .truncate()
                    .font_family(MONO)
                    .text_size(px(11.))
                    .text_color(theme.link)
                    .child(compare.base_ref.clone()),
            );
        let tooltip: SharedString =
            ahead_behind_tooltip(lang, compare.ahead, compare.behind, &compare.base_ref).into();
        let (added, removed) = compare.totals;
        Some(
            row()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child(tr(lang, Tr::CmpVs)),
                )
                .child(self.base_popover("cmp-base-picker", base_button, lang, cx))
                .child(
                    div()
                        .id("cmp-ahead-behind")
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                        .child(format!("↑{} ↓{}", compare.ahead, compare.behind)),
                )
                .child(div().flex_1())
                .children((added > 0 || removed > 0).then(|| {
                    h_flex()
                        .flex_shrink_0()
                        .gap_1()
                        .text_size(px(11.))
                        .child(div().text_color(theme.green).child(format!("+{added}")))
                        .child(div().text_color(theme.red).child(format!("−{removed}")))
                }))
                .into_any_element(),
        )
    }

    fn compare_warning(&self, text: String, lang: Language, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let change = Button::new("cmp-change-base").ghost().xsmall().compact().label(tr(lang, Tr::CmpChangeBase));
        h_flex()
            .h(px(24.))
            .flex_shrink_0()
            .min_w_0()
            .px_2()
            .gap_1()
            .items_center()
            .child(Icon::new(Ic::CircleAlert).size(px(12.)).text_color(theme.warning).flex_shrink_0())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(theme.warning)
                    .child(text),
            )
            .child(self.base_popover("cmp-warning-picker", change, lang, cx))
            .into_any_element()
    }

    /// Collapsible "Branch changes" file list (merge-base..HEAD).
    pub(super) fn branch_changes_section(
        &self,
        _status: &RepoStatus,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let Some(Loaded::Done(compare)) = self.compare.loaded.as_ref() else { return None };
        if compare.status != CompareStatus::Ready || compare.files.is_empty() {
            return None;
        }
        let (merge_base, head) = (compare.merge_base.clone()?, compare.head_oid.clone()?);
        let theme = cx.theme().clone();
        let open = self.compare.changes_open;
        Some(
            v_flex()
                .id("git-branch-changes-section")
                .child(
                    h_flex()
                        .id("git-branch-changes")
                        .h(px(26.))
                        .gap_1()
                        .px_2()
                        .items_center()
                        .cursor_pointer()
                        .bg(theme.list_head)
                        .hover(|style| style.bg(theme.list_hover))
                        .on_click(cx.listener(|panel, _, _, cx| {
                            panel.compare.changes_open = !panel.compare.changes_open;
                            cx.notify();
                        }))
                        .child(chevron(open))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_size(px(12.))
                                .child(format!(
                                    "{} ({})",
                                    tr(lang, Tr::CmpBranchChanges),
                                    compare.files.len()
                                )),
                        ),
                )
                .children(compare.files.iter().filter(|_| open).map(|file| {
                    let source = CompareSource::Range { base: merge_base.clone(), head: head.clone() };
                    let group = SharedString::from(format!("git-branch-changes-{}", file.path));
                    self.file_row(file, source, group, None, cx)
                }))
                .into_any_element(),
        )
    }
}

/// Filterable list of local and remote branches to compare against.
pub(super) struct BasePicker {
    panel: WeakEntity<GitPanel>,
    generation: Generation,
    lang: Language,
    /// The pinned base, if any: offers "Reset to default".
    pinned: Option<String>,
    branches: Option<Result<Vec<Branch>, GitError>>,
    filter: Entity<InputState>,
    _filter_events: Subscription,
}

impl BasePicker {
    fn new(
        repo: Repo,
        panel: WeakEntity<GitPanel>,
        generation: Generation,
        pinned: Option<String>,
        lang: Language,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr(lang, Tr::GitFilterBranches)));
        let filter_events = cx.subscribe(&filter, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let load = cx.background_spawn(async move { repo.branches() });
        cx.spawn(async move |picker, cx| {
            let branches = load.await;
            let _ = picker.update(cx, |picker, cx| {
                picker.branches = Some(branches);
                cx.notify();
            });
        })
        .detach();
        Self { panel, generation, lang, pinned, branches: None, filter, _filter_events: filter_events }
    }

    fn choose(&self, base: Option<String>, cx: &mut Context<Self>) {
        let _ = self.panel.update(cx, |panel, cx| {
            panel.generation.apply(self.generation, || {
                panel.compare.picker = None;
                panel.set_base(base, cx);
            });
        });
    }
}

impl Render for BasePicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = self.lang;
        let muted = cx.theme().muted_foreground;
        let body = v_flex().w(px(248.)).gap_1();
        let mut body = body.children(self.pinned.is_some().then(|| {
            Button::new("cmp-reset-base")
                .ghost()
                .xsmall()
                .w_full()
                .label(tr(lang, Tr::CmpResetBase))
                .on_click(cx.listener(|this, _, _, cx| this.choose(None, cx)))
        }));
        body = match &self.branches {
            None => body.child(
                div().p_1().text_xs().text_color(muted).child(tr(lang, Tr::GitLoading)),
            ),
            Some(Err(error)) => body.child(
                div().p_1().text_xs().text_color(cx.theme().danger).child(error.to_string()),
            ),
            Some(Ok(branches)) => {
                let query = self.filter.read(cx).value().trim().to_lowercase();
                let mut rows = v_flex().id("cmp-base-list").max_h(px(286.)).overflow_y_scroll();
                let mut remote_heading = false;
                for (index, branch) in branches.iter().enumerate() {
                    if !branch.name().to_lowercase().contains(&query) {
                        continue;
                    }
                    if branch.is_remote() && !remote_heading {
                        remote_heading = true;
                        rows = rows.child(
                            div()
                                .flex_shrink_0()
                                .px_2()
                                .py_1()
                                .text_xs()
                                .text_color(muted)
                                .child(tr(lang, Tr::GitRemoteBranches)),
                        );
                    }
                    let name = branch.name().to_owned();
                    let selected = self.pinned.as_deref() == Some(name.as_str());
                    let value = name.clone();
                    rows = rows.child(
                        Button::new(("cmp-base-branch", index))
                            .ghost()
                            .xsmall()
                            .w_full()
                            .flex_shrink_0()
                            .accessibility_label(name.clone())
                            .tooltip(name.clone())
                            .child(
                                h_flex()
                                    .w_full()
                                    .min_w_0()
                                    .gap_1()
                                    .child(div().flex_1().min_w_0().truncate().child(name))
                                    .children(selected.then(|| Icon::new(Ic::Check).size(px(12.)))),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.choose(Some(value.clone()), cx)
                            })),
                    );
                }
                body.children((branches.len() > FILTER_THRESHOLD).then(|| {
                    Input::new(&self.filter).small().aria_label(tr(lang, Tr::GitFilterBranches))
                }))
                .child(rows)
            }
        };
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compare(base: &str, ahead: u32, behind: u32, totals: (u32, u32)) -> BranchCompare {
        BranchCompare {
            base_ref: base.into(),
            base_oid: Some("a".into()),
            head_oid: Some("b".into()),
            merge_base: Some("a".into()),
            ahead,
            behind,
            files: Vec::new(),
            totals,
            status: CompareStatus::Ready,
        }
    }

    #[test]
    fn pinned_bases_are_keyed_by_normalized_worktree_path() {
        let mut bases = BranchBases::default();
        let path = Path::new("C:/work/Repo");
        assert_eq!(bases.get(path), None);
        bases.set(path, Some(" origin/dev ".into()));
        assert_eq!(bases.get(path), Some("origin/dev"));
        assert_eq!(bases.get(Path::new("C:/work/Other")), None);
        bases.set(path, Some("  ".into()));
        assert_eq!(bases.get(path), None);
        bases.set(path, Some("main".into()));
        bases.set(path, None);
        assert_eq!(bases, BranchBases::default());
    }

    #[test]
    fn pinned_bases_survive_a_json_round_trip_and_ignore_garbage() {
        let mut bases = BranchBases::default();
        bases.set(Path::new("C:/a"), Some("origin/main".into()));
        let json = serde_json::to_string(&bases).unwrap();
        assert_eq!(BranchBases::parse(&json), bases);
        assert_eq!(BranchBases::parse("not json"), BranchBases::default());
        assert_eq!(BranchBases::parse("[1,2]"), BranchBases::default());
        assert_eq!(BranchBases::parse(""), BranchBases::default());
    }

    #[test]
    fn fingerprint_tracks_head_pins_and_working_tree_size() {
        let mut status = RepoStatus { head_oid: Some("h1".into()), ..RepoStatus::default() };
        let base = fingerprint(None, None, &status);
        assert_eq!(base, fingerprint(None, None, &status));
        assert_ne!(base, fingerprint(Some("dev"), None, &status));
        assert_ne!(base, fingerprint(None, Some("origin/main"), &status));
        let mut moved = status.clone();
        moved.head_oid = Some("h2".into());
        assert_ne!(base, fingerprint(None, None, &moved));
        let mut file = FileChange {
            path: "a".into(),
            orig_path: None,
            status: ChangeKind::Modified,
            added: Some(1),
            removed: None,
            conflict: None,
        };
        status.unstaged.push(file.clone());
        let one = fingerprint(None, None, &status);
        assert_ne!(base, one);
        file.added = Some(5);
        status.unstaged[0] = file;
        assert_ne!(one, fingerprint(None, None, &status));
    }

    #[test]
    fn base_branch_detection_handles_remote_prefixes() {
        assert!(is_base_branch("main", "main"));
        assert!(is_base_branch("main", "origin/main"));
        assert!(is_base_branch("feature/x", "origin/feature/x"));
        assert!(!is_base_branch("main", "origin/domain"));
        assert!(!is_base_branch("main", "origin/dev"));
        assert!(!is_base_branch("x", "origin/feature/ax"));
    }

    #[test]
    fn row_hides_only_on_the_base_branch_without_anything_to_show() {
        let empty = compare("origin/main", 0, 0, (0, 0));
        assert!(hide_row(Some("main"), &empty));
        assert!(!hide_row(Some("feature"), &empty));
        assert!(!hide_row(None, &empty));
        assert!(!hide_row(Some("main"), &compare("origin/main", 0, 1, (0, 0))));
        assert!(!hide_row(Some("main"), &compare("origin/main", 0, 0, (3, 0))));
        let mut invalid = compare("origin/main", 0, 0, (0, 0));
        invalid.status = CompareStatus::InvalidBase;
        assert!(!hide_row(Some("main"), &invalid));
    }

    #[test]
    fn ahead_behind_tooltip_names_the_base() {
        let both = ahead_behind_tooltip(Language::En, 3, 2, "origin/main");
        assert_eq!(both, "3 commits ahead of origin/main\n2 commits behind origin/main");
        assert_eq!(
            ahead_behind_tooltip(Language::En, 1, 0, "main"),
            "1 commits ahead of main"
        );
        assert_eq!(
            ahead_behind_tooltip(Language::En, 0, 0, "main"),
            "0 commits ahead of main"
        );
    }
}
