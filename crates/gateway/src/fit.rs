//! Fit-to-phone resize rules (pure logic; the hub applies the effects).
//!
//! * the first phone that fits a terminal remembers the desktop size and resizes the PTY;
//! * with several fitting phones the most recent one wins;
//! * when the last fitting phone leaves, the desktop size is restored, but only if the PTY is
//!   still at the size we set;
//! * if somebody else (the GUI) resizes the PTY while we fit, we stop fitting instead of fighting.

use std::collections::HashMap;

use sich_remote_proto::TermRef;

pub type Size = (u16, u16);
pub type PhoneId = u64;

/// Consecutive mismatching observations before a foreign resize is declared (a poll of the
/// daemon's terminal list can race with our own resize).
const CONFLICT_OBSERVATIONS: u32 = 2;

struct TermFit {
    desktop: Size,
    /// The size we last asked the PTY for.
    applied: Size,
    /// Fitting phones in the order they started; the last one wins.
    fitters: Vec<(PhoneId, Size)>,
    mismatches: u32,
}

#[derive(Default)]
pub struct FitTracker {
    terms: HashMap<TermRef, TermFit>,
}

fn norm(size: Size) -> Size {
    (size.0.max(1), size.1.max(1))
}

impl FitTracker {
    pub fn is_fitting(&self, term: &TermRef, phone: PhoneId) -> bool {
        self.terms.get(term).is_some_and(|t| t.fitters.iter().any(|(p, _)| *p == phone))
    }

    /// The size we last asked the PTY for while fitting.
    pub fn applied(&self, term: &TermRef) -> Option<Size> {
        self.terms.get(term).map(|t| t.applied)
    }

    pub fn terms(&self) -> Vec<TermRef> {
        self.terms.keys().cloned().collect()
    }

    /// `current` is the PTY size right now. Returns the size to set, if it changes.
    pub fn enable(&mut self, term: &TermRef, phone: PhoneId, size: Size, current: Size) -> Option<Size> {
        let size = norm(size);
        let entry = self.terms.entry(term.clone()).or_insert_with(|| TermFit {
            desktop: current,
            applied: current,
            fitters: Vec::new(),
            mismatches: 0,
        });
        entry.fitters.retain(|(p, _)| *p != phone);
        entry.fitters.push((phone, size));
        entry.mismatches = 0;
        let changed = entry.applied != size;
        entry.applied = size;
        (changed || current != size).then_some(size)
    }

    /// The phone's viewport changed. Applies only while it fits and is the active fitter.
    pub fn resize(&mut self, term: &TermRef, phone: PhoneId, size: Size) -> Option<Size> {
        let size = norm(size);
        let entry = self.terms.get_mut(term)?;
        let slot = entry.fitters.iter_mut().find(|(p, _)| *p == phone)?;
        slot.1 = size;
        if entry.fitters.last().map(|(p, _)| *p) != Some(phone) || entry.applied == size {
            return None;
        }
        entry.applied = size;
        entry.mismatches = 0;
        Some(size)
    }

    /// The phone stops fitting (unsubscribe, disconnect, fit off). Returns the size to set.
    pub fn disable(&mut self, term: &TermRef, phone: PhoneId, current: Size) -> Option<Size> {
        let entry = self.terms.get_mut(term)?;
        let before = entry.fitters.len();
        entry.fitters.retain(|(p, _)| *p != phone);
        if entry.fitters.len() == before {
            return None;
        }
        entry.mismatches = 0;
        if let Some(&(_, size)) = entry.fitters.last() {
            let changed = entry.applied != size;
            entry.applied = size;
            return changed.then_some(size);
        }
        let entry = self.terms.remove(term)?;
        (current == entry.applied && current != entry.desktop).then_some(entry.desktop)
    }

    /// Compares the PTY's real size with ours. On a (confirmed) foreign resize all fitting is
    /// dropped and the affected phones are returned.
    pub fn observe(&mut self, term: &TermRef, current: Size) -> Vec<PhoneId> {
        let Some(entry) = self.terms.get_mut(term) else { return Vec::new() };
        if current == entry.applied {
            entry.mismatches = 0;
            return Vec::new();
        }
        entry.mismatches += 1;
        if entry.mismatches < CONFLICT_OBSERVATIONS {
            return Vec::new();
        }
        self.terms.remove(term).map(|e| e.fitters.into_iter().map(|(p, _)| p).collect()).unwrap_or_default()
    }

    /// The terminal is gone.
    pub fn forget(&mut self, term: &TermRef) -> Vec<PhoneId> {
        self.terms.remove(term).map(|e| e.fitters.into_iter().map(|(p, _)| p).collect()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> TermRef {
        TermRef { daemon: "d".into(), id: 1 }
    }

    #[test]
    fn enable_remembers_desktop_size_and_restores_on_disable() {
        let mut f = FitTracker::default();
        assert_eq!(f.enable(&t(), 1, (40, 20), (120, 30)), Some((40, 20)));
        assert!(f.is_fitting(&t(), 1));
        assert_eq!(f.disable(&t(), 1, (40, 20)), Some((120, 30)));
        assert!(!f.is_fitting(&t(), 1));
        assert!(f.terms().is_empty());
    }

    #[test]
    fn restore_only_if_pty_is_still_at_phone_size() {
        let mut f = FitTracker::default();
        f.enable(&t(), 1, (40, 20), (120, 30));
        assert_eq!(f.disable(&t(), 1, (100, 25)), None);
    }

    #[test]
    fn no_resize_when_already_at_the_phone_size() {
        let mut f = FitTracker::default();
        assert_eq!(f.enable(&t(), 1, (80, 24), (80, 24)), None);
        assert_eq!(f.disable(&t(), 1, (80, 24)), None);
    }

    #[test]
    fn last_fitter_wins_and_desktop_size_survives_handover() {
        let mut f = FitTracker::default();
        assert_eq!(f.enable(&t(), 1, (40, 20), (120, 30)), Some((40, 20)));
        assert_eq!(f.enable(&t(), 2, (60, 30), (40, 20)), Some((60, 30)));
        // The inactive fitter's viewport changes do not touch the PTY.
        assert_eq!(f.resize(&t(), 1, (50, 22)), None);
        // Phone 2 leaves: phone 1 takes over with its latest size.
        assert_eq!(f.disable(&t(), 2, (60, 30)), Some((50, 22)));
        // The last one leaves: back to the desktop size from before the first fit.
        assert_eq!(f.disable(&t(), 1, (50, 22)), Some((120, 30)));
    }

    #[test]
    fn resize_applies_only_for_the_active_fitter() {
        let mut f = FitTracker::default();
        assert_eq!(f.resize(&t(), 1, (10, 10)), None, "not fitting");
        f.enable(&t(), 1, (40, 20), (120, 30));
        assert_eq!(f.resize(&t(), 1, (41, 21)), Some((41, 21)));
        assert_eq!(f.resize(&t(), 1, (41, 21)), None, "unchanged");
    }

    #[test]
    fn sizes_are_clamped_to_at_least_one() {
        let mut f = FitTracker::default();
        assert_eq!(f.enable(&t(), 1, (0, 0), (80, 24)), Some((1, 1)));
    }

    #[test]
    fn disabling_a_phone_that_does_not_fit_is_a_noop() {
        let mut f = FitTracker::default();
        assert_eq!(f.disable(&t(), 9, (80, 24)), None);
        f.enable(&t(), 1, (40, 20), (120, 30));
        assert_eq!(f.disable(&t(), 9, (40, 20)), None);
        assert!(f.is_fitting(&t(), 1));
    }

    #[test]
    fn foreign_resize_drops_fit_after_two_observations() {
        let mut f = FitTracker::default();
        f.enable(&t(), 1, (40, 20), (120, 30));
        f.enable(&t(), 2, (60, 30), (40, 20));
        assert!(f.observe(&t(), (40, 20)).is_empty(), "ours is stale: first mismatch is tolerated");
        assert!(f.observe(&t(), (60, 30)).is_empty(), "matches again, counter resets");
        assert!(f.observe(&t(), (100, 40)).is_empty());
        let mut dropped = f.observe(&t(), (100, 40));
        dropped.sort();
        assert_eq!(dropped, vec![1, 2]);
        assert!(f.terms().is_empty());
        // Nothing is restored afterwards: the foreign size is respected.
        assert_eq!(f.disable(&t(), 1, (100, 40)), None);
    }

    #[test]
    fn forget_returns_the_fitters() {
        let mut f = FitTracker::default();
        f.enable(&t(), 7, (40, 20), (120, 30));
        assert_eq!(f.forget(&t()), vec![7]);
        assert!(f.forget(&t()).is_empty());
    }
}
