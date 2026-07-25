//! Single one-shot timer scheduling.
//!
//! Zellij reports how late a timeout fired, not which requested duration it
//! belonged to. Keeping exactly one timeout outstanding makes identity
//! explicit: the cadence stored here is the cadence of the next callback.

/// How urgently the one-shot timer should re-fire. `Fast` is the 8 Hz visual
/// frame clock; every eighth Fast fire advances the 1 Hz domain clock. `Idle`
/// keeps session liveness on that 1 Hz domain clock without visual frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cadence {
    Fast,
    Idle,
}

impl Cadence {
    pub(crate) fn seconds(self) -> f64 {
        match self {
            Cadence::Fast => 0.125,
            Cadence::Idle => 1.0,
        }
    }
}

/// Visual frames per 1 Hz domain tick. Keeping this conversion explicit lets
/// the rail animate smoothly without accelerating TTLs, debounce, or host I/O.
pub(crate) const FAST_FRAMES_PER_DOMAIN_TICK: u8 = 8;

#[derive(Default)]
pub(super) struct TimerChain {
    armed: Option<Cadence>,
}

impl TimerChain {
    /// Arm one timeout only when none is already outstanding. A transition
    /// from Idle to Fast waits for the current Idle callback (at most one
    /// second), then re-arms at Fast; this keeps callback identity explicit.
    pub(super) fn arm(&mut self, desired: Option<Cadence>) -> Option<Cadence> {
        if self.armed.is_some() {
            return None;
        }
        self.armed = desired;
        desired
    }

    /// Retire the sole outstanding timeout and return its scheduled cadence.
    /// The host-reported elapsed duration is intentionally irrelevant.
    pub(super) fn on_fire(&mut self) -> Option<Cadence> {
        self.armed.take()
    }

    #[cfg(test)]
    pub(super) fn armed(&self) -> Option<Cadence> {
        self.armed
    }

    #[cfg(test)]
    pub(super) fn disarm_for_test(&mut self) {
        self.armed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_seconds_maps_fast_and_idle() {
        assert_eq!(Cadence::Fast.seconds(), 0.125);
        assert_eq!(Cadence::Idle.seconds(), 1.0);
    }

    #[test]
    fn only_one_timeout_can_be_outstanding() {
        let mut chain = TimerChain::default();
        assert_eq!(chain.arm(Some(Cadence::Idle)), Some(Cadence::Idle));
        assert_eq!(chain.arm(Some(Cadence::Fast)), None);
        assert_eq!(chain.armed(), Some(Cadence::Idle));
        assert_eq!(chain.on_fire(), Some(Cadence::Idle));
        assert_eq!(chain.arm(Some(Cadence::Fast)), Some(Cadence::Fast));
    }
}
