//! CPU/RAM indicator in the status bar with a per-terminal breakdown popover.
//!
//! The Sich GUI process, every connected daemon and all their descendants are sampled with
//! `sysinfo` on a background thread. A daemon's direct children are attributed to terminals via
//! the `SICH_TERM_ID` / `SICH_SOCKET` variables the daemon puts in each shell's environment.
//! Everything except [`Sampler`] and the view code is pure and unit-tested.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    Anchor, ClickEvent, Context, Div, InteractiveElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use sich_proto::{ProjectInfo, TermId, TerminalInfo};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::app_view::AppView;
use crate::daemon_client::Snapshot;
use crate::i18n::{Language, Tr, tr};
use crate::workspace::same_path;

pub type Pid = u32;

const ACTIVE_INTERVAL: Duration = Duration::from_secs(2);
const INACTIVE_INTERVAL: Duration = Duration::from_secs(10);
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

// ---- pure model -------------------------------------------------------------------------------

/// One process as seen by the sampler. `cpu` is a percentage of ONE core.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProcSample {
    pub pid: Pid,
    pub parent: Option<Pid>,
    pub start_time: u64,
    pub cpu: f32,
    pub mem: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub cpu: f32,
    pub mem: u64,
}

impl Usage {
    fn add_sample(&mut self, sample: &ProcSample) {
        self.cpu += sample.cpu;
        self.mem = self.mem.saturating_add(sample.mem);
    }

    fn merge(&mut self, other: Usage) {
        self.cpu += other.cpu;
        self.mem = self.mem.saturating_add(other.mem);
    }
}

/// What a daemon's child process says about itself (from its environment).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribution {
    pub socket: Option<String>,
    pub term_id: u64,
}

/// A live terminal of this window, as known by the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermRef {
    /// The UI's terminal key.
    pub key: TermId,
    pub socket: String,
    pub daemon_pid: Pid,
    /// The terminal id on its daemon (`SICH_TERM_ID`).
    pub term_id: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Aggregate {
    pub total: Usage,
    /// GUI + daemons + descendants not attributed to a terminal.
    pub sich: Usage,
    pub terminals: HashMap<TermId, Usage>,
}

/// Parents to children. A child that started before its parent is a reused pid, not a child.
fn children_map(samples: &[ProcSample]) -> HashMap<Pid, Vec<Pid>> {
    let start: HashMap<Pid, u64> = samples.iter().map(|s| (s.pid, s.start_time)).collect();
    let mut map: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for sample in samples {
        let Some(parent) = sample.parent.filter(|&parent| parent != sample.pid) else { continue };
        let Some(&parent_start) = start.get(&parent) else { continue };
        if sample.start_time < parent_start {
            continue;
        }
        map.entry(parent).or_default().push(sample.pid);
    }
    map
}

/// Direct children of the given daemons as `(pid, start_time)`.
pub fn daemon_children(samples: &[ProcSample], daemons: &[Pid]) -> Vec<(Pid, u64)> {
    let children = children_map(samples);
    let start: HashMap<Pid, u64> = samples.iter().map(|s| (s.pid, s.start_time)).collect();
    let mut seen = HashSet::new();
    daemons
        .iter()
        .filter_map(|daemon| children.get(daemon))
        .flatten()
        .filter(|pid| seen.insert(**pid))
        .map(|pid| (*pid, start[pid]))
        .collect()
}

fn resolve(attribution: &Attribution, daemon: Pid, terms: &[TermRef]) -> Option<TermId> {
    terms
        .iter()
        .find(|term| {
            term.term_id == attribution.term_id
                && match &attribution.socket {
                    Some(socket) => term.socket == *socket,
                    None => term.daemon_pid == daemon,
                }
        })
        .map(|term| term.key)
}

/// Sums `root` and its not yet claimed descendants, claiming them.
fn walk(
    root: Pid,
    index: &HashMap<Pid, &ProcSample>,
    children: &HashMap<Pid, Vec<Pid>>,
    claimed: &mut HashSet<Pid>,
) -> Usage {
    let mut usage = Usage::default();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if !claimed.insert(pid) {
            continue;
        }
        if let Some(sample) = index.get(&pid) {
            usage.add_sample(sample);
        }
        stack.extend(children.get(&pid).into_iter().flatten().copied());
    }
    usage
}

/// Totals for the Sich GUI, the daemons and everything below them. `attribution` maps a direct
/// child of a daemon to what its environment says; matching children (with their whole subtree)
/// are credited to that terminal, the rest lands in the Sich bucket. Processes outside the Sich
/// tree are ignored.
pub fn aggregate(
    samples: &[ProcSample],
    gui: Pid,
    daemons: &[Pid],
    attribution: &HashMap<Pid, Attribution>,
    terms: &[TermRef],
) -> Aggregate {
    let index: HashMap<Pid, &ProcSample> = samples.iter().map(|s| (s.pid, s)).collect();
    let children = children_map(samples);
    let mut claimed = HashSet::new();
    let mut out = Aggregate::default();
    for &daemon in daemons {
        if !index.contains_key(&daemon) {
            continue;
        }
        for &child in children.get(&daemon).into_iter().flatten() {
            let Some(key) = attribution.get(&child).and_then(|attr| resolve(attr, daemon, terms)) else { continue };
            if claimed.contains(&child) {
                continue;
            }
            let usage = walk(child, &index, &children, &mut claimed);
            out.terminals.entry(key).or_default().merge(usage);
            out.total.merge(usage);
        }
    }
    for &root in std::iter::once(&gui).chain(daemons) {
        if index.contains_key(&root) {
            let usage = walk(root, &index, &children, &mut claimed);
            out.sich.merge(usage);
            out.total.merge(usage);
        }
    }
    out
}

/// Parses `KEY=VALUE` environment entries. No `SICH_TERM_ID` means no attribution.
pub fn parse_attribution<S: AsRef<str>>(vars: impl IntoIterator<Item = S>) -> Option<Attribution> {
    let (mut term_id, mut socket) = (None, None);
    for var in vars {
        let Some((key, value)) = var.as_ref().split_once('=') else { continue };
        if key.eq_ignore_ascii_case("SICH_TERM_ID") {
            term_id = value.trim().parse().ok();
        } else if key.eq_ignore_ascii_case("SICH_SOCKET") && !value.is_empty() {
            socket = Some(value.to_string());
        }
    }
    term_id.map(|term_id| Attribution { socket, term_id })
}

/// `(pid, start_time)` to attribution, so a process environment is read once.
#[derive(Default)]
pub struct AttrCache {
    map: HashMap<(Pid, u64), Option<Attribution>>,
}

impl AttrCache {
    /// `read` returns `None` when the environment could not be read; that is not cached.
    pub fn resolve(
        &mut self,
        pid: Pid,
        start_time: u64,
        read: impl FnOnce() -> Option<Option<Attribution>>,
    ) -> Option<Attribution> {
        if let Some(cached) = self.map.get(&(pid, start_time)) {
            return cached.clone();
        }
        let result = read()?;
        self.map.insert((pid, start_time), result.clone());
        result
    }

    /// Forgets entries of processes that are gone.
    pub fn prune(&mut self, live: impl Fn(Pid, u64) -> bool) {
        self.map.retain(|&(pid, start), _| live(pid, start));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TermRow {
    pub key: TermId,
    pub title: String,
    pub usage: Usage,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectGroup {
    pub name: String,
    pub usage: Usage,
    pub rows: Vec<TermRow>,
}

/// `(terminal key, display title, project name)` of every live terminal, grouped by project.
/// Terminals without a live process are omitted. Projects and rows are sorted by RAM, descending.
pub fn build_groups(terminals: Vec<(TermId, String, String)>, aggregate: &Aggregate) -> Vec<ProjectGroup> {
    let mut groups: Vec<ProjectGroup> = Vec::new();
    for (key, title, project) in terminals {
        let Some(&usage) = aggregate.terminals.get(&key) else { continue };
        let index = match groups.iter().position(|group| group.name == project) {
            Some(index) => index,
            None => {
                groups.push(ProjectGroup { name: project, usage: Usage::default(), rows: Vec::new() });
                groups.len() - 1
            }
        };
        groups[index].usage.merge(usage);
        groups[index].rows.push(TermRow { key, title, usage });
    }
    for group in &mut groups {
        group.rows.sort_by(|a, b| b.usage.mem.cmp(&a.usage.mem).then_with(|| a.title.cmp(&b.title)));
    }
    groups.sort_by(|a, b| b.usage.mem.cmp(&a.usage.mem).then_with(|| a.name.cmp(&b.name)));
    groups
}

// ---- formatting -------------------------------------------------------------------------------

/// CPU share of the whole machine, 1 decimal; "—" until a second sample exists.
pub fn format_cpu(raw_cpu: f32, cores: usize, ready: bool) -> String {
    if !ready {
        return "—".into();
    }
    format!("{:.1}%", raw_cpu / cores.max(1) as f32)
}

/// `0 MB`, `850 MB`, `1.4 GB` (1024-based).
pub fn format_mem(bytes: u64) -> String {
    let mb = (bytes + MIB / 2) / MIB;
    if bytes >= GIB || mb >= 1024 {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    } else {
        format!("{mb} MB")
    }
}

// ---- sysinfo adapter --------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResourceSnapshot {
    pub aggregate: Aggregate,
    pub cores: usize,
    /// CPU needs two samples; before that CPU shows "—".
    pub cpu_ready: bool,
    pub sampled: bool,
}

pub struct Sampler {
    sys: System,
    cache: AttrCache,
    taken: u32,
}

impl Sampler {
    pub fn new() -> Self {
        Self { sys: System::new(), cache: AttrCache::default(), taken: 0 }
    }

    pub fn sample(&mut self, gui: Pid, daemons: &[Pid], terms: &[TermRef]) -> ResourceSnapshot {
        let Self { sys, cache, taken } = self;
        sys.refresh_processes_specifics(ProcessesToUpdate::All, ProcessRefreshKind::new().with_cpu().with_memory());
        *taken = taken.saturating_add(1);
        let samples: Vec<ProcSample> = sys
            .processes()
            .iter()
            .map(|(pid, process)| ProcSample {
                pid: pid.as_u32(),
                parent: process.parent().map(sysinfo::Pid::as_u32),
                start_time: process.start_time(),
                cpu: process.cpu_usage(),
                mem: process.memory(),
            })
            .collect();

        let mut attribution = HashMap::new();
        for (pid, start) in daemon_children(&samples, daemons) {
            let found = cache.resolve(pid, start, || {
                let target = sysinfo::Pid::from_u32(pid);
                sys.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[target]),
                    ProcessRefreshKind::new().with_environ(UpdateKind::OnlyIfNotSet),
                );
                let environ = sys.process(target)?.environ();
                if environ.is_empty() {
                    return None;
                }
                Some(parse_attribution(environ.iter().map(|var| var.to_string_lossy())))
            });
            if let Some(found) = found {
                attribution.insert(pid, found);
            }
        }
        let live: HashSet<(Pid, u64)> = samples.iter().map(|s| (s.pid, s.start_time)).collect();
        cache.prune(|pid, start| live.contains(&(pid, start)));

        ResourceSnapshot {
            aggregate: aggregate(&samples, gui, daemons, &attribution, terms),
            cores: std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
            cpu_ready: *taken >= 2,
            sampled: true,
        }
    }
}

// ---- view -------------------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct ResourceState {
    snapshot: ResourceSnapshot,
    open: bool,
}

/// Starts the background polling: every 2 s while the window is active, every 10 s otherwise.
pub(crate) fn watch(window: &mut Window, cx: &mut Context<AppView>) {
    cx.spawn_in(window, async move |view, cx| {
        let mut sampler = Sampler::new();
        loop {
            let Ok((daemons, terms, interval)) = view.update_in(cx, |view, window, _| {
                let daemons: Vec<Pid> = view.client.connected_peers().into_iter().map(|(_, pid)| pid).collect();
                let terms: Vec<TermRef> = view
                    .client
                    .snapshot()
                    .terminals
                    .iter()
                    .filter(|entry| entry.info.exited.is_none())
                    .filter_map(|entry| {
                        let owner = view.client.owner(entry.info.id)?;
                        Some(TermRef { key: entry.info.id, socket: owner.socket, daemon_pid: owner.pid, term_id: owner.id })
                    })
                    .collect();
                let interval = if window.is_window_active() { ACTIVE_INTERVAL } else { INACTIVE_INTERVAL };
                (daemons, terms, interval)
            }) else {
                break;
            };
            let (next, snapshot) = cx
                .background_executor()
                .spawn(async move {
                    let snapshot = sampler.sample(std::process::id(), &daemons, &terms);
                    (sampler, snapshot)
                })
                .await;
            sampler = next;
            let updated = view.update(cx, |view, cx| {
                if view.resources.snapshot != snapshot {
                    view.resources.snapshot = snapshot;
                    cx.notify();
                }
            });
            if updated.is_err() {
                break;
            }
            cx.background_executor().timer(interval).fuse().await;
        }
    })
    .detach();
}

fn project_name(info: &TerminalInfo, projects: &[ProjectInfo]) -> String {
    info.worktree
        .as_deref()
        .and_then(|path| {
            projects
                .iter()
                .find(|project| project.worktrees.iter().any(|w| same_path(&w.path, path)))
                .map(|project| project.name.clone())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| crate::notify::project_label(info, projects))
}

/// Pre-formatted popover content, so the popover closure holds plain data.
struct Model {
    title: &'static str,
    cpu_label: &'static str,
    ram_label: &'static str,
    total: (String, String),
    sich_label: &'static str,
    sich: (String, String),
    groups: Vec<(String, (String, String), Vec<(TermId, String, (String, String))>)>,
}

impl Model {
    fn build(view: &AppView, snap: &Snapshot, lang: Language) -> Self {
        let state = &view.resources.snapshot;
        let fmt = |usage: Usage| -> (String, String) {
            if state.sampled {
                (format_cpu(usage.cpu, state.cores, state.cpu_ready), format_mem(usage.mem))
            } else {
                ("—".into(), "—".into())
            }
        };
        let terminals = snap
            .terminals
            .iter()
            .filter(|entry| entry.info.exited.is_none())
            .map(|entry| {
                (entry.info.id, crate::tab_bar::entry_title(entry), project_name(&entry.info, &view.projects))
            })
            .collect();
        let groups = build_groups(terminals, &state.aggregate)
            .into_iter()
            .map(|group| {
                let rows = group.rows.into_iter().map(|row| (row.key, row.title, fmt(row.usage))).collect();
                (group.name, fmt(group.usage), rows)
            })
            .collect();
        Self {
            title: tr(lang, Tr::ResourcesTitle),
            cpu_label: tr(lang, Tr::ResourcesCpu),
            ram_label: tr(lang, Tr::ResourcesRam),
            total: fmt(state.aggregate.total),
            sich_label: tr(lang, Tr::ResourcesSich),
            sich: fmt(state.aggregate.sich),
            groups,
        }
    }
}

const CPU_W: f32 = 56.;
const RAM_W: f32 = 72.;

fn numbers(cpu: String, ram: String) -> impl IntoIterator<Item = Div> {
    [
        div().w(px(CPU_W)).flex_shrink_0().text_right().child(cpu),
        div().w(px(RAM_W)).flex_shrink_0().text_right().child(ram),
    ]
}

fn row(label: impl IntoIterator<Item = Div>, cpu: String, ram: String) -> Div {
    h_flex()
        .w_full()
        .gap_2()
        .items_center()
        .children(label)
        .children(numbers(cpu, ram))
}

fn name_cell(text: impl Into<SharedString>) -> Div {
    div().flex_1().min_w_0().truncate().child(text.into())
}

/// The status-bar item and its popover.
pub(crate) fn status_item(view: &AppView, snap: &Snapshot, lang: Language, cx: &mut Context<AppView>) -> Popover {
    let model = Arc::new(Model::build(view, snap, lang));
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let compact = h_flex()
        .gap_1()
        .text_color(muted)
        .child(Icon::new(Ic::Cpu).size(px(12.)))
        .child(format!(
            "{} {}  {} {}",
            model.cpu_label, model.total.0, model.ram_label, model.total.1
        ));
    let entity = cx.entity();
    let open_entity = entity.clone();
    let content_model = model;
    Popover::new("resource-usage-popover")
        .anchor(Anchor::BottomRight)
        .offset(px(6.))
        .trigger(
            Button::new("resource-usage")
                .ghost()
                .xsmall()
                .compact()
                .tooltip(tr(lang, Tr::ResourcesTooltip))
                .accessibility_label(tr(lang, Tr::ResourcesTooltip))
                .child(compact),
        )
        .open(view.resources.open)
        .on_open_change(move |open, _, cx| {
            let open = *open;
            open_entity.update(cx, |view, cx| {
                view.resources.open = open;
                cx.notify();
            });
        })
        .content(move |_, window, cx| {
            let theme = cx.theme().clone();
            let model = &content_model;
            let muted = theme.muted_foreground;
            let max_h = px(420.).min((window.viewport_size().height - px(56.)).max(px(80.)));
            let mut list = v_flex().w_full().gap_1();
            list = list.child(
                row(
                    [name_cell("")],
                    model.cpu_label.to_string(),
                    model.ram_label.to_string(),
                )
                .text_xs()
                .text_color(muted),
            );
            list = list.child(
                row([name_cell(model.sich_label)], model.sich.0.clone(), model.sich.1.clone()).text_xs(),
            );
            for (name, (cpu, ram), rows) in &model.groups {
                list = list.child(
                    row([name_cell(name.clone()).font_semibold()], cpu.clone(), ram.clone())
                        .text_xs()
                        .mt_2()
                        .pt_1()
                        .border_t_1()
                        .border_color(theme.border),
                );
                for (key, title, (cpu, ram)) in rows {
                    let key = *key;
                    let entity = entity.clone();
                    list = list.child(
                        row([name_cell(title.clone()).pl_3()], cpu.clone(), ram.clone())
                            .id(SharedString::from(format!("resource-term-{key}")))
                            .text_xs()
                            .text_color(muted)
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(theme.muted.opacity(0.5)))
                            .on_click(move |_: &ClickEvent, window, cx| {
                                entity.update(cx, |view, cx| {
                                    view.resources.open = false;
                                    view.activate_terminal(key, window, cx);
                                    cx.notify();
                                });
                            }),
                    );
                }
            }
            v_flex()
                .w(px(360.))
                .gap_2()
                .child(
                    row(
                        [name_cell(model.title).text_sm().font_semibold()],
                        model.total.0.clone(),
                        model.total.1.clone(),
                    )
                    .text_sm(),
                )
                .child(div().id("resource-usage-list").max_h(max_h).overflow_y_scroll().child(list))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: Pid, parent: Option<Pid>, start: u64, cpu: f32, mem: u64) -> ProcSample {
        ProcSample { pid, parent, start_time: start, cpu, mem }
    }

    fn attr(socket: Option<&str>, term_id: u64) -> Attribution {
        Attribution { socket: socket.map(Into::into), term_id }
    }

    fn term(key: TermId, socket: &str, daemon_pid: Pid, term_id: u64) -> TermRef {
        TermRef { key, socket: socket.into(), daemon_pid, term_id }
    }

    #[test]
    fn subtree_is_summed_across_levels() {
        // gui 1; daemon 2 (child of gui); shell 10 -> node 11 -> worker 12.
        let samples = [
            p(1, None, 10, 1.0, 100),
            p(2, Some(1), 11, 0.5, 50),
            p(10, Some(2), 12, 2.0, 10),
            p(11, Some(10), 13, 3.0, 20),
            p(12, Some(11), 14, 4.0, 30),
        ];
        let attribution = HashMap::from([(10, attr(Some("s"), 7))]);
        let out = aggregate(&samples, 1, &[2], &attribution, &[term(5, "s", 2, 7)]);
        assert_eq!(out.terminals[&5], Usage { cpu: 9.0, mem: 60 });
        assert_eq!(out.sich, Usage { cpu: 1.5, mem: 150 });
        assert_eq!(out.total, Usage { cpu: 10.5, mem: 210 });
    }

    #[test]
    fn unattributed_children_go_to_the_sich_bucket() {
        let samples = [
            p(1, None, 10, 0.0, 1),
            p(2, Some(1), 11, 0.0, 2),
            p(10, Some(2), 12, 1.0, 10), // unknown environment
            p(11, Some(10), 13, 1.0, 20),
            p(12, Some(2), 12, 1.0, 40), // attributed to a terminal we do not know
        ];
        let attribution = HashMap::from([(12, attr(Some("s"), 99))]);
        let out = aggregate(&samples, 1, &[2], &attribution, &[term(5, "s", 2, 7)]);
        assert!(out.terminals.is_empty());
        assert_eq!(out.sich.mem, 73);
        assert_eq!(out.total, out.sich);
    }

    #[test]
    fn child_older_than_its_parent_is_a_reused_pid() {
        let samples = [
            p(1, None, 100, 0.0, 1),
            p(2, Some(1), 100, 0.0, 2),
            p(10, Some(2), 101, 0.0, 10),
            p(11, Some(10), 50, 5.0, 999), // claims parent 10 but started before it
        ];
        let attribution = HashMap::from([(10, attr(None, 3))]);
        let out = aggregate(&samples, 1, &[2], &attribution, &[term(1, "s", 2, 3)]);
        assert_eq!(out.terminals[&1].mem, 10);
        assert_eq!(out.total.mem, 13);
    }

    #[test]
    fn processes_outside_the_sich_tree_are_ignored() {
        let samples = [
            p(1, None, 10, 0.0, 1),
            p(2, Some(1), 11, 0.0, 2),
            p(50, None, 10, 90.0, 5000),
            p(51, Some(50), 11, 90.0, 5000),
        ];
        let out = aggregate(&samples, 1, &[2], &HashMap::new(), &[]);
        assert_eq!(out.total, Usage { cpu: 0.0, mem: 3 });
        // A daemon that is not running contributes nothing (pid 2 is still below the GUI).
        assert_eq!(aggregate(&samples, 1, &[777], &HashMap::new(), &[]).total.mem, 3);
        assert_eq!(aggregate(&samples, 99, &[777], &HashMap::new(), &[]).total, Usage::default());
    }

    #[test]
    fn daemon_inside_the_gui_tree_is_not_counted_twice() {
        let samples = [p(1, None, 1, 1.0, 10), p(2, Some(1), 2, 1.0, 20), p(3, Some(2), 3, 1.0, 30)];
        let attribution = HashMap::from([(3, attr(None, 1))]);
        let out = aggregate(&samples, 1, &[2], &attribution, &[term(9, "s", 2, 1)]);
        assert_eq!(out.total, Usage { cpu: 3.0, mem: 60 });
        assert_eq!(out.terminals[&9].mem, 30);
        assert_eq!(out.sich.mem, 30);
    }

    #[test]
    fn socket_and_daemon_pid_matching() {
        let terms = [term(1, "a", 2, 7), term(2, "b", 3, 7)];
        assert_eq!(resolve(&attr(Some("b"), 7), 3, &terms), Some(2));
        assert_eq!(resolve(&attr(Some("c"), 7), 3, &terms), None);
        assert_eq!(resolve(&attr(None, 7), 2, &terms), Some(1));
        assert_eq!(resolve(&attr(None, 8), 2, &terms), None);
    }

    #[test]
    fn parses_environment_entries() {
        let found = parse_attribution(["PATH=x", "SICH_TERM_ID=12", "SICH_SOCKET=sich-abc"]).unwrap();
        assert_eq!(found, attr(Some("sich-abc"), 12));
        assert_eq!(parse_attribution(["sich_term_id=3"]), Some(attr(None, 3)));
        assert_eq!(parse_attribution(["SICH_TERM_ID=nope"]), None);
        assert_eq!(parse_attribution(["SICH_SOCKET=x"]), None);
        assert_eq!(parse_attribution(Vec::<String>::new()), None);
    }

    #[test]
    fn groups_are_sorted_by_ram_and_skip_terminals_without_processes() {
        let mut aggregate = Aggregate::default();
        for (key, mem) in [(1, 100), (2, 300), (3, 50), (4, 200)] {
            aggregate.terminals.insert(key, Usage { cpu: 1.0, mem });
        }
        let terminals = vec![
            (1, "a".to_string(), "small".to_string()),
            (2, "b".to_string(), "big".to_string()),
            (3, "c".to_string(), "small".to_string()),
            (4, "d".to_string(), "big".to_string()),
            (5, "exited".to_string(), "big".to_string()),
            (6, "lonely".to_string(), "none".to_string()),
        ];
        let groups = build_groups(terminals, &aggregate);
        let names: Vec<_> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["big", "small"]);
        assert_eq!(groups[0].usage.mem, 500);
        assert_eq!(groups[0].rows.iter().map(|r| r.key).collect::<Vec<_>>(), [2, 4]);
        assert_eq!(groups[1].rows.iter().map(|r| r.key).collect::<Vec<_>>(), [1, 3]);
    }

    #[test]
    fn cpu_is_normalized_by_core_count_and_needs_two_samples() {
        assert_eq!(format_cpu(80.0, 8, true), "10.0%");
        assert_eq!(format_cpu(25.0, 0, true), "25.0%");
        assert_eq!(format_cpu(3.2, 1, true), "3.2%");
        assert_eq!(format_cpu(80.0, 8, false), "—");
    }

    #[test]
    fn bytes_are_formatted_in_mb_and_gb() {
        assert_eq!(format_mem(0), "0 MB");
        assert_eq!(format_mem(850 * MIB), "850 MB");
        assert_eq!(format_mem(GIB - MIB), "1023 MB");
        assert_eq!(format_mem(GIB - 1), "1.0 GB");
        assert_eq!(format_mem(GIB), "1.0 GB");
        assert_eq!(format_mem(GIB + GIB * 4 / 10), "1.4 GB");
    }

    #[test]
    fn attribution_cache_reads_once_and_prunes_dead_processes() {
        let mut cache = AttrCache::default();
        let mut reads = 0;
        for _ in 0..3 {
            let got = cache.resolve(10, 5, || {
                reads += 1;
                Some(Some(attr(None, 1)))
            });
            assert_eq!(got, Some(attr(None, 1)));
        }
        assert_eq!(reads, 1);
        // A failed read is not cached and is retried.
        assert_eq!(cache.resolve(11, 5, || None), None);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.resolve(11, 5, || Some(None)), None);
        assert_eq!(cache.len(), 2, "processes without SICH vars are cached too");
        // Same pid with another start time is a different process.
        cache.resolve(10, 6, || Some(Some(attr(None, 2))));
        assert_eq!(cache.len(), 3);
        cache.prune(|pid, start| (pid, start) == (10, 6));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn daemon_children_respect_the_pid_reuse_guard() {
        let samples = [p(2, None, 10, 0.0, 0), p(3, Some(2), 11, 0.0, 0), p(4, Some(2), 5, 0.0, 0)];
        assert_eq!(daemon_children(&samples, &[2, 2]), [(3, 11)]);
    }

    #[test]
    fn resource_keys_are_translated() {
        for key in [Tr::ResourcesTooltip, Tr::ResourcesTitle, Tr::ResourcesSich, Tr::ResourcesCpu, Tr::ResourcesRam] {
            for lang in Language::ALL {
                assert!(!tr(lang, key).trim().is_empty(), "{key:?} {lang:?}");
            }
        }
    }
}
