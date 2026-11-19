//! Coalesce refresh requests. The caller supplies time and executes the work.

use std::time::{Duration, Instant};

use crate::watch::WatchSignal;

const DEBOUNCE: Duration = Duration::from_millis(80);
const MAX_WAIT: Duration = Duration::from_millis(300);
const MIN_GAP: Duration = Duration::from_millis(150);
const SAFETY_REFRESH: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
struct Pending {
    first: Instant,
    last: Instant,
    immediate: bool,
    history: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refresh {
    pub history: bool,
}

#[derive(Default)]
pub struct Scheduler {
    pending: Option<Pending>,
    last_start: Option<Instant>,
    safety_due: Option<Instant>,
    running: bool,
}

impl Scheduler {
    pub fn event(&mut self, now: Instant, signal: WatchSignal) {
        if signal.files_changed || signal.refs_changed {
            self.request(now, signal.refs_changed, false);
        }
    }

    pub fn immediate(&mut self, now: Instant, history: bool) {
        self.request(now, history, true);
    }

    fn request(&mut self, now: Instant, history: bool, immediate: bool) {
        let pending = self.pending.get_or_insert(Pending {
            first: now,
            last: now,
            immediate,
            history,
        });
        pending.first = pending.first.min(now);
        pending.last = pending.last.max(now);
        pending.immediate |= immediate;
        pending.history |= history;
    }

    /// The caller drives this while the panel is visible.
    pub fn deadline(&self) -> Option<Instant> {
        if self.running {
            return None;
        }
        let requested = self.pending.map(|pending| {
            if pending.immediate {
                pending.first
            } else {
                (pending.last + DEBOUNCE).min(pending.first + MAX_WAIT)
            }
        });
        let deadline = match (requested, self.safety_due) {
            (Some(requested), Some(safety)) => requested.min(safety),
            (Some(deadline), None) | (None, Some(deadline)) => deadline,
            (None, None) => return None,
        };
        Some(
            self.last_start
                .map_or(deadline, |last| deadline.max(last + MIN_GAP)),
        )
    }

    pub fn start(&mut self, now: Instant) -> Option<Refresh> {
        if now < self.deadline()? {
            return None;
        }
        let safety = self.safety_due.is_some_and(|due| now >= due);
        if self.safety_due.is_none() || safety {
            self.safety_due = Some(now + SAFETY_REFRESH);
        }
        let history = self.pending.take().is_some_and(|pending| pending.history);
        self.running = true;
        self.last_start = Some(now);
        Some(Refresh {
            history: history || safety,
        })
    }

    pub fn finished(&mut self) {
        self.running = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(time: Instant, value: u64) -> Instant {
        time + Duration::from_millis(value)
    }

    #[test]
    fn waits_after_last_event_but_never_more_than_300_ms() {
        let t = Instant::now();
        let mut scheduler = Scheduler::default();
        scheduler.event(t, WatchSignal::FILES);
        scheduler.event(ms(t, 60), WatchSignal::REFS);
        assert_eq!(scheduler.deadline(), Some(ms(t, 140)));
        for i in [120, 180, 240, 299] {
            scheduler.event(ms(t, i), WatchSignal::FILES);
        }
        assert_eq!(scheduler.start(ms(t, 299)), None);
        assert_eq!(scheduler.start(ms(t, 300)), Some(Refresh { history: true }));
    }

    #[test]
    fn runs_one_trailing_refresh_with_min_gap_and_safety_refresh() {
        let t = Instant::now();
        let mut scheduler = Scheduler::default();
        scheduler.immediate(t, true);
        assert_eq!(scheduler.start(t), Some(Refresh { history: true }));
        scheduler.event(ms(t, 20), WatchSignal::FILES);
        scheduler.immediate(ms(t, 40), false);
        assert_eq!(scheduler.deadline(), None);
        scheduler.finished();
        assert_eq!(scheduler.deadline(), Some(ms(t, 150)));
        assert_eq!(
            scheduler.start(ms(t, 150)),
            Some(Refresh { history: false })
        );
        scheduler.finished();
        assert_eq!(scheduler.deadline(), Some(ms(t, 10_000)));
        assert_eq!(
            scheduler.start(ms(t, 10_000)),
            Some(Refresh { history: true })
        );
    }
}
