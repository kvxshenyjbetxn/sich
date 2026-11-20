//! Background quota updates and the status-bar chips.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use chrono::TimeZone as _;
use futures::FutureExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div,
};
use sich_hooks::claude_install::{self, StatuslineState};
use sich_hooks::statusline::{self, SNAPSHOT_FILE, Snapshot};
use sich_hooks::usage::{self, CodexUnavailable, Window as Limit, Windows};
use sich_proto::AgentKind;

use crate::agent_badge::agent_icon_element;
use crate::app_view::AppView;
use crate::i18n::{self, Language, Tr, tr};
use crate::settings::{self, AppSettings};

const START_DELAY: Duration = Duration::from_secs(2);
const REFRESH: Duration = Duration::from_secs(15 * 60);
const FOCUS_AGE: Duration = Duration::from_secs(5 * 60);
const RETRY: Duration = Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_secs(5);
const STALE_SECS: i64 = 30 * 60;
const NARROW: f32 = 680.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Options {
    enabled: bool,
    wrap: bool,
}

impl Options {
    fn get(cx: &App) -> Self {
        let config = settings::get(cx);
        Self { enabled: config.usage_limits, wrap: config.claude_statusline_wrap }
    }
}

#[derive(Default)]
struct CodexPoll {
    enabled_at: Option<Instant>,
    running: bool,
    generation: u64,
    focused: bool,
    last_success: Option<Instant>,
    retry_at: Option<Instant>,
    backoff: Option<Duration>,
}

impl CodexPoll {
    fn enable(&mut self, enabled: bool, now: Instant) {
        if self.enabled_at.is_some() == enabled {
            return;
        }
        self.enabled_at = enabled.then_some(now);
        self.generation += 1;
        self.last_success = None;
        self.retry_at = None;
        self.backoff = None;
        self.focused = false;
    }

    fn begin(&mut self, visible: bool, now: Instant) -> Option<u64> {
        let enabled_at = self.enabled_at?;
        if !visible || self.running || now < enabled_at + START_DELAY {
            return None;
        }
        let due = match (self.retry_at, self.last_success) {
            (Some(retry_at), _) => now >= retry_at,
            (None, Some(last)) => {
                let elapsed = now.saturating_duration_since(last);
                elapsed >= REFRESH || (self.focused && elapsed > FOCUS_AGE)
            }
            (None, None) => true,
        };
        self.focused = false;
        if due {
            self.running = true;
            Some(self.generation)
        } else {
            None
        }
    }

    fn finish(&mut self, generation: u64, success: bool, now: Instant) -> bool {
        self.running = false;
        self.focused = false;
        if generation != self.generation {
            return false;
        }
        if success {
            self.last_success = Some(now);
            self.retry_at = None;
            self.backoff = None;
        } else {
            let backoff = self.backoff.map_or(RETRY, |last| (last * 2).min(REFRESH));
            self.retry_at = Some(now + backoff);
            self.backoff = Some(backoff);
        }
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Reading {
    windows: Windows,
    at: i64,
}

#[derive(Clone, Copy, Default, PartialEq)]
struct ClaudeUsage {
    reading: Option<Reading>,
    state: Option<StatuslineState>,
    failed: bool,
}

struct ClaudeWatch {
    path: Option<PathBuf>,
    config: Option<PathBuf>,
    exe: Option<PathBuf>,
    options: Option<Options>,
    config_mtime: Option<SystemTime>,
    snapshot_mtime: Option<SystemTime>,
    version: Option<String>,
    snapshot: Option<Snapshot>,
    update: ClaudeUsage,
}

impl ClaudeWatch {
    fn new() -> Self {
        let can_install = std::env::var_os("SICH_NO_HOOK_INSTALL").is_none();
        Self {
            path: usage::config_file(SNAPSHOT_FILE),
            config: can_install.then(claude_install::default_claude_config_dir).flatten(),
            exe: can_install.then(crate::daemon_host::launcher_exe).flatten(),
            options: None,
            config_mtime: None,
            snapshot_mtime: None,
            version: None,
            snapshot: None,
            update: ClaudeUsage::default(),
        }
    }

    fn check(&mut self, options: Options) -> ClaudeUsage {
        let changed = self.options != Some(options);
        self.options = Some(options);
        if !options.enabled && !changed {
            return self.update;
        }
        if let (Some(config), Some(exe), Some(path)) = (&self.config, &self.exe, &self.path) {
            let config_path = config.join("settings.json");
            let modified = mtime(&config_path);
            if changed || modified != self.config_mtime {
                if changed || self.version.is_none() {
                    self.version = claude_install::probe_claude_version();
                }
                // The snapshot lives below <config>/sich/usage.
                if let Some(sich_dir) = path.parent().and_then(Path::parent) {
                    match claude_install::update_statusline(
                        config,
                        sich_dir,
                        exe,
                        self.version.as_deref(),
                        options.enabled,
                        options.wrap,
                    ) {
                        Ok(state) => {
                            self.update.state = Some(state);
                            self.update.failed = false;
                        }
                        Err(error) => {
                            tracing::warn!("could not reconcile Claude statusLine: {error:#}");
                            self.update.state = None;
                            self.update.failed = true;
                        }
                    }
                }
                self.config_mtime = mtime(&config_path);
            }
        }
        if options.enabled {
            if let Some(path) = &self.path {
                let modified = mtime(path);
                if changed || modified != self.snapshot_mtime {
                    self.snapshot = statusline::read_snapshot(path);
                    self.snapshot_mtime = modified;
                }
            }
            self.update.reading = self
                .snapshot
                .as_ref()
                .filter(|_| self.update.state != Some(StatuslineState::Off))
                .map(|snapshot| Reading { windows: snapshot.windows, at: snapshot.recorded_at });
        } else {
            self.update.reading = None;
        }
        self.update
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

#[derive(Default)]
pub(crate) struct UsageBar {
    now: i64,
    poll: CodexPoll,
    codex: Option<Reading>,
    codex_error: Option<CodexUnavailable>,
    claude: ClaudeUsage,
}

impl UsageBar {
    pub(crate) fn watch(&mut self, window: &mut Window, cx: &mut Context<AppView>) {
        let (wake, rx) = async_channel::bounded::<()>(1);
        let mut options = Options::get(cx);
        self.enable(options.enabled);
        let settings_wake = wake.clone();
        cx.observe_global_in::<AppSettings>(window, move |view, _, cx| {
            let next = Options::get(cx);
            if next != options {
                options = next;
                view.usage_bar.enable(next.enabled);
                let _ = settings_wake.try_send(());
                cx.notify();
            }
        })
        .detach();
        cx.observe_window_activation(window, move |view, window, _| {
            if window.is_window_active() {
                view.usage_bar.poll.focused = true;
                let _ = wake.try_send(());
            }
        })
        .detach();

        cx.spawn_in(window, async move |view, cx| {
            let mut claude = ClaudeWatch::new();
            let mut interval = START_DELAY;
            loop {
                let timer = cx.background_executor().timer(interval).fuse();
                let wakeup = rx.recv().fuse();
                futures::pin_mut!(timer, wakeup);
                futures::select! {
                    _ = timer => {},
                    result = wakeup => if result.is_err() { break },
                }
                let Ok((options, next_interval)) = view.update_in(cx, |view, window, cx| {
                    let options = Options::get(cx);
                    view.usage_bar.enable(options.enabled);
                    view.usage_bar.tick(window, cx);
                    let until_probe = view
                        .usage_bar
                        .poll
                        .enabled_at
                        .and_then(|at| (at + START_DELAY).checked_duration_since(Instant::now()));
                    (options, until_probe.unwrap_or(CHECK_INTERVAL))
                }) else {
                    break;
                };
                let (watcher, update) = cx
                    .background_executor()
                    .spawn(async move {
                        let update = claude.check(options);
                        (claude, update)
                    })
                    .await;
                claude = watcher;
                if view
                    .update_in(cx, |view, _, cx| {
                        if options == Options::get(cx) && view.usage_bar.claude != update {
                            view.usage_bar.claude = update;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
                interval = next_interval;
            }
        })
        .detach();
    }

    fn enable(&mut self, enabled: bool) {
        // Startup and re-enable schedule a fresh, deferred probe.
        let was_enabled = self.poll.enabled_at.is_some();
        self.poll.enable(enabled, Instant::now());
        if was_enabled && !enabled {
            self.codex = None;
            self.codex_error = None;
            self.claude = ClaudeUsage::default();
        }
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<AppView>) {
        let now = usage::unix_seconds();
        if self.now != now {
            self.now = now;
            if self.poll.enabled_at.is_some() && (self.codex.is_some() || self.claude.reading.is_some()) {
                cx.notify();
            }
        }
        let Some(generation) = self.poll.begin(window.is_visible(), Instant::now()) else { return };
        cx.spawn(async move |view, cx| {
            let result = cx.background_executor().spawn(async { usage::read_codex() }).await;
            let _ = view.update(cx, |view, cx| {
                let bar = &mut view.usage_bar;
                if bar.poll.finish(generation, result.is_ok(), Instant::now()) {
                    bar.now = usage::unix_seconds();
                    match result {
                        Ok(windows) => {
                            bar.codex = Some(Reading { windows, at: bar.now });
                            bar.codex_error = None;
                        }
                        Err(error) => bar.codex_error = Some(error),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn render(&self, lang: Language, width: f32, cx: &App) -> Vec<AnyElement> {
        if !Options::get(cx).enabled {
            return Vec::new();
        }
        let claude_reason = match self.claude.state {
            Some(StatuslineState::NeedsWrap) => Some(Tr::UsageClaudeNeedsWrap),
            Some(StatuslineState::UserEdited) => Some(Tr::UsageClaudeEdited),
            Some(StatuslineState::Unavailable) => Some(Tr::UsageClaudeOld),
            _ if self.claude.failed => Some(Tr::UsageClaudeFailed),
            _ => None,
        };
        let codex_reason = self.codex_error.map(|error| match error {
            CodexUnavailable::MissingOrOld => Tr::UsageCodexOld,
            CodexUnavailable::Unsupported => Tr::UsageCodexUnsupported,
            CodexUnavailable::NoLimits => Tr::UsageCodexNoLimits,
            CodexUnavailable::ReadFailed => Tr::UsageCodexFailed,
        });

        [
            (AgentKind::Claude, self.claude.reading, claude_reason),
            (AgentKind::Codex, self.codex, codex_reason),
        ]
        .into_iter()
        .filter_map(|(kind, reading, reason)| {
            let chip = Chip::new(reading, reason, self.now);
            let has_hint = matches!(reason, Some(Tr::UsageClaudeNeedsWrap | Tr::UsageClaudeEdited));
            chip.visible(width, reading.is_some() || has_hint)
                .then(|| chip.render(kind, reading, reason, lang, cx))
        })
        .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Muted,
    Good,
    Warning,
    Danger,
}

fn tone(used: Option<u8>, stale: bool) -> Tone {
    match used.filter(|_| !stale) {
        Some(90..) => Tone::Danger,
        Some(80..=89) => Tone::Warning,
        Some(_) => Tone::Good,
        None => Tone::Muted,
    }
}

fn current(limit: Option<Limit>, now: i64) -> Option<u8> {
    limit
        .filter(|limit| limit.resets_at.is_none_or(|at| now < at))
        .map(|limit| limit.used_percent.round().clamp(0., 100.) as u8)
}

struct Chip {
    now: i64,
    stale: bool,
    percentages: [Option<u8>; 2],
}

impl Chip {
    fn new(reading: Option<Reading>, reason: Option<Tr>, now: i64) -> Self {
        let windows = reading.map(|reading| reading.windows).unwrap_or_default();
        Self {
            now,
            stale: reading.is_some_and(|reading| now.saturating_sub(reading.at) > STALE_SECS),
            percentages: if reason.is_some() {
                [None, None]
            } else {
                [current(windows.five_hour, now), current(windows.weekly, now)]
            },
        }
    }

    fn visible(&self, width: f32, has_data_or_hint: bool) -> bool {
        has_data_or_hint && (width >= NARROW || self.percentages.iter().flatten().any(|&used| used >= 80))
    }

    fn tooltip(
        &self,
        provider: &str,
        reading: Option<Reading>,
        reason: Option<Tr>,
        lang: Language,
    ) -> Vec<String> {
        let mut lines = vec![provider.to_owned()];
        let labels = [Tr::UsageFiveHour, Tr::UsageWeek];
        if let Some(reading) = reading {
            let windows = [reading.windows.five_hour, reading.windows.weekly];
            for ((label, window), used) in labels.into_iter().zip(windows).zip(self.percentages) {
                let details = if let Some(used) = used {
                    i18n::usage_window(lang, used, reset_detail(lang, window, self.now).as_deref())
                } else if window.is_some_and(|window| window.resets_at.is_some_and(|at| at <= self.now)) {
                    tr(lang, Tr::UsageResetPassed).into()
                } else {
                    "—".into()
                };
                lines.push(format!("{}  {details}", tr(lang, label)));
            }
            let minutes = self.now.saturating_sub(reading.at).max(0) / 60;
            let stale = if self.stale { tr(lang, Tr::UsageStale) } else { "" };
            lines.push(format!("{}{stale}", i18n::usage_updated(lang, minutes)));
        }
        if let Some(reason) = reason {
            lines.push(tr(lang, reason).into());
        }
        lines
    }

    fn render(
        &self,
        kind: AgentKind,
        reading: Option<Reading>,
        reason: Option<Tr>,
        lang: Language,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme();
        let (id, name) = match kind {
            AgentKind::Claude => ("usage-claude", "Claude Code"),
            _ => ("usage-codex", "Codex"),
        };
        let tooltip = self.tooltip(name, reading, reason, lang);
        let meter = |label: Tr, used| {
            let color = match tone(used, self.stale) {
                Tone::Muted => theme.muted_foreground,
                Tone::Good => theme.success,
                Tone::Warning => theme.warning,
                Tone::Danger => theme.danger,
            };
            let percent = used.map_or_else(|| "—".to_string(), |used| format!("{used}%"));
            h_flex().gap_1().child(tr(lang, label)).child(div().text_color(color).child(percent))
        };
        h_flex()
            .id(id)
            .flex_shrink_0()
            .gap_1()
            .items_center()
            .text_color(theme.muted_foreground)
            .child(agent_icon_element(kind, 12., theme.muted_foreground, theme.muted_foreground))
            .child(meter(Tr::UsageFiveHour, self.percentages[0]))
            .child("·")
            .child(meter(Tr::UsageWeek, self.percentages[1]))
            .tooltip(move |window, cx| {
                let lines = tooltip.clone();
                Tooltip::element(move |_, _| {
                    v_flex().children(lines.iter().cloned().map(|line| div().child(line)))
                })
                .build(window, cx)
            })
            .into_any_element()
    }
}

fn reset_detail(lang: Language, window: Option<Limit>, now: i64) -> Option<String> {
    let at = window?.resets_at?;
    let clock = chrono::Local.timestamp_opt(at, 0).single()?.format("%H:%M").to_string();
    let minutes = at.saturating_sub(now).max(0) / 60;
    Some(i18n::usage_reset(lang, minutes, &clock))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(at: i64, percent: f64, resets_at: Option<i64>) -> Reading {
        Reading {
            windows: Windows { five_hour: Some(Limit { used_percent: percent, resets_at }), weekly: None },
            at,
        }
    }

    #[test]
    fn tone_thresholds_and_staleness() {
        assert_eq!(tone(Some(0), false), Tone::Good);
        assert_eq!(tone(Some(79), false), Tone::Good);
        assert_eq!(tone(None, false), Tone::Muted);
        assert_eq!(tone(Some(80), false), Tone::Warning);
        assert_eq!(tone(Some(89), false), Tone::Warning);
        assert_eq!(tone(Some(90), false), Tone::Danger);
        let rounded = Chip::new(Some(reading(0, 79.5, None)), None, 0);
        assert_eq!(rounded.percentages[0], Some(80));
        assert_eq!(tone(rounded.percentages[0], false), Tone::Warning);
        let sample = reading(0, 99.0, None);
        assert!(!Chip::new(Some(sample), None, STALE_SECS).stale);
        let stale = Chip::new(Some(sample), None, STALE_SECS + 1);
        assert!(stale.stale);
        assert_eq!(tone(stale.percentages[0], stale.stale), Tone::Muted);
        let lines = stale.tooltip("Codex", Some(sample), None, Language::Uk);
        assert!(lines.contains(&"оновлено 30 хв тому (застаріло)".into()));
    }

    #[test]
    fn reset_passed_and_narrow_width() {
        let nearly_full = reading(0, 90.0, Some(200));
        let active = Chip::new(Some(nearly_full), None, 199);
        assert_eq!(active.percentages, [Some(90), None]);
        assert!(active.visible(NARROW - 1., true));

        let passed = Chip::new(Some(nearly_full), None, 200);
        assert_eq!(passed.percentages, [None, None]);
        assert!(!passed.visible(NARROW - 1., true));
        assert!(passed.visible(NARROW, true));
        assert_eq!(
            passed.tooltip("Codex", Some(nearly_full), None, Language::En)[1],
            "5h  Waiting for a new reading after reset"
        );
        let unavailable = Chip::new(Some(nearly_full), Some(Tr::UsageCodexFailed), 100);
        assert_eq!(unavailable.percentages, [None, None]);
        assert!(!unavailable.visible(NARROW - 1., true));
    }

    #[test]
    fn localized_usage_details() {
        let reset = i18n::usage_reset(Language::Uk, 134, "18:40");
        assert_eq!(
            i18n::usage_window(Language::Uk, 18, Some(&reset)),
            "використано 18% · скидання через 2 год 14 хв (о 18:40)"
        );
        assert_eq!(i18n::usage_updated(Language::Ru, 2), "обновлено 2 мин назад");
        assert_eq!(i18n::usage_reset(Language::En, 8, "18:40"), "resets in 8 min (at 18:40)");
    }

    #[test]
    fn codex_poll_defers_start_and_requires_a_visible_window() {
        let mut poll = CodexPoll::default();
        let start = Instant::now();
        assert!(poll.begin(true, start).is_none());
        poll.enable(true, start);
        assert!(poll.begin(true, start + Duration::from_secs(1)).is_none());
        assert!(poll.begin(false, start + START_DELAY).is_none());
        let generation = poll.begin(true, start + START_DELAY).unwrap();
        assert!(poll.begin(true, start + START_DELAY + REFRESH).is_none());
        assert!(poll.finish(generation, true, start + START_DELAY));

        let success = start + START_DELAY;
        assert!(poll.begin(true, success + REFRESH - Duration::from_secs(1)).is_none());
        poll.focused = true;
        assert!(poll.begin(true, success + FOCUS_AGE).is_none());
        poll.focused = true;
        let focus_refresh = success + FOCUS_AGE + Duration::from_secs(1);
        assert_eq!(poll.begin(true, focus_refresh), Some(generation));
        assert!(poll.finish(generation, true, focus_refresh));
        assert_eq!(poll.begin(true, focus_refresh + REFRESH), Some(generation));
    }

    #[test]
    fn codex_poll_backs_off_and_discards_results_from_previous_enable_cycle() {
        let mut poll = CodexPoll::default();
        let start = Instant::now();
        poll.enable(true, start);
        let mut now = start + START_DELAY;
        let generation = poll.begin(true, now).unwrap();

        for seconds in [30, 60, 120, 240, 480, 900, 900] {
            assert!(poll.finish(generation, false, now));
            poll.focused = true;
            assert!(poll.begin(true, now + Duration::from_secs(seconds - 1)).is_none());
            now += Duration::from_secs(seconds);
            assert_eq!(poll.begin(true, now), Some(generation));
        }
        poll.enable(false, now);
        poll.enable(true, now);
        assert!(poll.begin(true, now + START_DELAY).is_none());
        assert!(!poll.finish(generation, true, now + START_DELAY));
        assert_ne!(poll.begin(true, now + START_DELAY), Some(generation));
    }
}
