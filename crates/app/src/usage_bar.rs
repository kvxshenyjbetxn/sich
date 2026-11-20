//! Background quota updates, status-bar summary and provider details.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use chrono::{Datelike as _, Local, TimeZone, Timelike as _};
use futures::FutureExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, Theme, h_flex, v_flex};
use gpui_kit::{
    Anchor, App, Context, Div, Hsla, InteractiveElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, px, relative,
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

    pub(crate) fn render(&self, lang: Language, cx: &App) -> Option<Popover> {
        if !Options::get(cx).enabled {
            return None;
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

        let chips = [
            Chip::new(self.claude.reading, claude_reason, self.now),
            Chip::new(self.codex, codex_reason, self.now),
        ];
        if !chips.iter().any(Chip::shows_summary) {
            return None;
        }
        let providers = [AgentKind::Claude, AgentKind::Codex];
        let theme = cx.theme();
        let summary = h_flex()
            .text_xs()
            .font_normal()
            .gap_2()
            .child(chips[0].summary(providers[0], lang, theme))
            .child(div().h(px(12.)).w(px(1.)).bg(theme.border))
            .child(chips[1].summary(providers[1], lang, theme));
        Some(
            Popover::new("usage-limits-popover")
                .anchor(Anchor::BottomRight)
                .offset(px(6.))
                .trigger(
                    Button::new("usage-limits")
                        .ghost()
                        .xsmall()
                        .compact()
                        .accessibility_label(tr(lang, Tr::UsageTitle))
                        .child(summary),
                )
                .content(move |_, window, cx| {
                    let theme = cx.theme();
                    v_flex()
                        .id("usage-limits-details")
                        .max_h((window.viewport_size().height - px(56.)).max(px(80.)))
                        .overflow_y_scroll()
                        .w(px(312.))
                        .gap_3()
                        .child(div().text_sm().font_semibold().child(tr(lang, Tr::UsageTitle)))
                        .children(
                            providers.into_iter().zip(chips).map(|(kind, chip)| chip.details(kind, lang, theme)),
                        )
                }),
        )
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

#[derive(Clone, Copy)]
enum Period {
    FiveHour,
    Week,
}

impl Period {
    const ALL: [Self; 2] = [Self::FiveHour, Self::Week];

    fn label(self, detailed: bool) -> Tr {
        match (self, detailed) {
            (Self::FiveHour, false) => Tr::UsageFiveHour,
            (Self::Week, false) => Tr::UsageWeek,
            (Self::FiveHour, true) => Tr::UsageFiveHourDetail,
            (Self::Week, true) => Tr::UsageWeekDetail,
        }
    }

    fn reset_time(self, lang: Language, at: i64, zone: &impl TimeZone) -> Option<String> {
        let local = zone.timestamp_opt(at, 0).single()?;
        let (hour, minute) = (local.hour(), local.minute());
        Some(match self {
            Self::FiveHour => format!("{hour:02}:{minute:02}"),
            Self::Week => {
                let day = i18n::usage_weekday(lang, local.weekday());
                format!("{day}, {:02}.{:02}, {hour:02}:{minute:02}", local.day(), local.month())
            }
        })
    }
}

#[derive(Clone, Copy)]
struct Chip {
    now: i64,
    recorded_at: Option<i64>,
    reason: Option<Tr>,
    stale: bool,
    resets: [Option<i64>; 2],
    percentages: [Option<u8>; 2],
}

impl Chip {
    fn new(reading: Option<Reading>, reason: Option<Tr>, now: i64) -> Self {
        let windows = reading.map(|reading| reading.windows).unwrap_or_default();
        let limits = [windows.five_hour, windows.weekly];
        Self {
            now,
            recorded_at: reading.map(|reading| reading.at),
            reason,
            stale: reading.is_some_and(|reading| now.saturating_sub(reading.at) > STALE_SECS),
            resets: limits.map(|limit| limit.and_then(|limit| limit.resets_at)),
            percentages: if reason.is_some() { [None; 2] } else { limits.map(|limit| current(limit, now)) },
        }
    }

    fn shows_summary(&self) -> bool {
        self.recorded_at.is_some() || matches!(self.reason, Some(Tr::UsageClaudeNeedsWrap | Tr::UsageClaudeEdited))
    }

    fn updated(&self, lang: Language) -> Option<String> {
        let minutes = self.now.saturating_sub(self.recorded_at?).max(0) / 60;
        let mut label = i18n::usage_updated(lang, minutes);
        if self.stale {
            label.push_str(tr(lang, Tr::UsageStale));
        }
        Some(label)
    }

    fn reset_at(&self, period: Period) -> Option<i64> {
        self.resets[period as usize].filter(|&at| self.percentages[period as usize].is_some() && at > self.now)
    }

    fn countdown(&self, period: Period, lang: Language) -> Option<String> {
        let at = self.reset_at(period)?;
        chrono::DateTime::from_timestamp(at, 0)?;
        Some(i18n::usage_countdown(lang, at.saturating_sub(self.now) / 60))
    }

    fn missing(&self, period: Period) -> Tr {
        if self.resets[period as usize].is_some_and(|at| at <= self.now) {
            Tr::UsageResetPassed
        } else {
            Tr::UsageNoReading
        }
    }

    fn color(&self, used: Option<u8>, theme: &Theme) -> Hsla {
        match tone(used, self.stale) {
            Tone::Muted => theme.muted_foreground,
            Tone::Good => theme.success,
            Tone::Warning => theme.warning,
            Tone::Danger => theme.danger,
        }
    }

    fn summary(&self, kind: AgentKind, lang: Language, theme: &Theme) -> Div {
        h_flex()
            .gap_2()
            .text_color(theme.muted_foreground)
            .child(agent_icon_element(kind, 12., theme.muted_foreground, theme.muted_foreground))
            .children(Period::ALL.map(|period| {
                let used = self.percentages[period as usize];
                h_flex()
                    .gap_1()
                    .child(tr(lang, period.label(false)))
                    .child(
                        div()
                            .text_color(self.color(used, theme))
                            .child(used.map_or_else(|| "—".into(), |used| format!("{used}%"))),
                    )
                    .children(self.countdown(period, lang))
            }))
    }

    fn details(&self, kind: AgentKind, lang: Language, theme: &Theme) -> Div {
        let name = if kind == AgentKind::Claude { "Claude Code" } else { "Codex" };
        v_flex()
            .w_full()
            .gap_2()
            .pt_2()
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(agent_icon_element(kind, 14., theme.foreground, theme.foreground))
                            .child(div().text_sm().font_semibold().child(name)),
                    )
                    .children(self.updated(lang).map(|text| div().text_xs().text_color(theme.muted_foreground).child(text))),
            )
            .children(
                self.recorded_at
                    .filter(|_| self.reason.is_none())
                    .into_iter()
                    .flat_map(|_| Period::ALL)
                    .map(|period| self.meter(period, lang, theme)),
            )
            .children(
                self.reason
                    .or_else(|| self.recorded_at.is_none().then_some(Tr::UsageNoReading))
                    .map(|key| div().text_xs().text_color(theme.muted_foreground).child(tr(lang, key))),
            )
    }

    fn meter(&self, period: Period, lang: Language, theme: &Theme) -> Div {
        let used = self.percentages[period as usize];
        let color = self.color(used, theme);
        let reset = self.reset_at(period).and_then(|at| {
            let clock = period.reset_time(lang, at, &Local)?;
            Some(i18n::usage_reset(lang, at.saturating_sub(self.now) / 60, &clock))
        });
        v_flex()
            .w_full()
            .gap_1()
            .text_xs()
            .child(
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(tr(lang, period.label(true)))
                    .child(div().text_color(color).child(used.map_or_else(
                        || "—".into(),
                        |used| i18n::usage_window(lang, used),
                    ))),
            )
            .child(
                div().w_full().h(px(5.)).rounded_full().bg(theme.border).children(
                    used.filter(|&used| used > 0)
                        .map(|used| div().h_full().w(relative(f32::from(used) / 100.)).rounded_full().bg(color)),
                ),
            )
            .children(
                reset.or_else(|| used.is_none().then(|| tr(lang, self.missing(period)).into()))
                    .map(|label| div().text_color(theme.muted_foreground).child(label)),
            )
    }
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
        assert_eq!(stale.updated(Language::Uk).as_deref(), Some("оновлено 30 хв тому (застаріло)"));
    }

    #[test]
    fn expired_or_unavailable_samples_have_no_active_reset_or_progress() {
        let nearly_full = reading(0, 90.0, Some(200));
        let active = Chip::new(Some(nearly_full), None, 199);
        assert_eq!(active.percentages, [Some(90), None]);
        assert_eq!(active.reset_at(Period::FiveHour), Some(200));
        assert_eq!(active.reset_at(Period::Week), None);

        let passed = Chip::new(Some(nearly_full), None, 200);
        assert_eq!(passed.percentages, [None, None]);
        assert_eq!(passed.reset_at(Period::FiveHour), None);
        assert_eq!(tr(Language::En, passed.missing(Period::FiveHour)), "Waiting for a new reading after reset");
        assert_eq!(tr(Language::En, passed.missing(Period::Week)), "No usage data");
        let unavailable = Chip::new(Some(nearly_full), Some(Tr::UsageCodexFailed), 100);
        assert_eq!(unavailable.percentages, [None, None]);
        assert_eq!(unavailable.reset_at(Period::FiveHour), None);
    }

    #[test]
    fn status_bar_countdowns_are_bracketed_and_relative_for_both_windows() {
        let now = 1_800_000_000;
        let sample = Reading {
            windows: Windows {
                five_hour: Some(Limit { used_percent: 55., resets_at: Some(now + 80 * 60) }),
                weekly: Some(Limit { used_percent: 84., resets_at: Some(now + (4 * 24 + 15) * 3600) }),
            },
            at: now,
        };
        let chip = Chip::new(Some(sample), None, now);
        assert_eq!(chip.countdown(Period::FiveHour, Language::Uk).as_deref(), Some("[через 1г 20хв]"));
        assert_eq!(chip.countdown(Period::Week, Language::Uk).as_deref(), Some("[через 4д 15г]"));
        assert_eq!(chip.countdown(Period::FiveHour, Language::Ru).as_deref(), Some("[через 1ч 20мин]"));
        assert_eq!(chip.countdown(Period::FiveHour, Language::En).as_deref(), Some("[in 1h 20m]"));
    }

    #[test]
    fn status_bar_countdown_handles_the_last_minute_and_missing_timestamps() {
        let last_minute = Chip::new(Some(reading(30, 55., Some(60))), None, 30);
        assert_eq!(last_minute.countdown(Period::FiveHour, Language::Uk).as_deref(), Some("[через <1хв]"));
        let expired = Chip::new(Some(reading(30, 55., Some(60))), None, 60);
        assert_eq!(expired.countdown(Period::FiveHour, Language::Uk), None);
        for at in [None, Some(i64::MAX)] {
            let unknown = Chip::new(Some(reading(30, 55., at)), None, 30);
            assert_eq!(unknown.countdown(Period::FiveHour, Language::Uk), None);
        }
    }

    #[test]
    fn localized_usage_details() {
        assert_eq!(i18n::usage_window(Language::Uk, 18), "використано 18%");
        assert_eq!(i18n::usage_updated(Language::Ru, 2), "обновлено 2 мин назад");
        assert_eq!(i18n::usage_reset(Language::En, 8, "18:40"), "Resets: 18:40 · in 8 min");
        assert_eq!(i18n::usage_reset(Language::Uk, 1560, "сб, 21.11, 01:30"), "Скидання: сб, 21.11, 01:30 · через 1 д 2 год");
    }

    #[test]
    fn details_use_the_local_day_and_time_at_reset() {
        let utc = chrono::Utc.with_ymd_and_hms(2026, 11, 20, 23, 30, 0).unwrap();
        let zone = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        let at = utc.timestamp();
        assert_eq!(Period::FiveHour.reset_time(Language::Uk, at, &zone).as_deref(), Some("01:30"));
        assert_eq!(Period::Week.reset_time(Language::Uk, at, &zone).as_deref(), Some("сб, 21.11, 01:30"));
        assert_eq!(Period::Week.reset_time(Language::En, at, &zone).as_deref(), Some("Sat, 21.11, 01:30"));
        assert_eq!(Period::Week.reset_time(Language::Uk, i64::MAX, &zone), None);
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
