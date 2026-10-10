//! Eased value transitions for control states (hover, pressed, toggle knob, expansion).
//!
//! A [`Transition`] moves a value toward a target over a fixed duration with an [`Easing`]
//! curve. It is driven by `Event::AnimationFrame(now)`: the first frame after a new target
//! fixes the start time, so the host never needs to pass a clock into `Widget::event`. When the
//! user asked for reduced motion (`SPI_GETCLIENTAREAANIMATION` off) every transition jumps to
//! its target at once.

use std::time::Duration;

/// Duration of hover transitions (Fluent "control faster").
pub const HOVER_DURATION: Duration = Duration::from_millis(83);
/// Duration of pressed and toggle transitions (Fluent "control normal").
pub const PRESS_DURATION: Duration = Duration::from_millis(167);

/// The user's motion preference, shared by every animated widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Motion {
    reduce: bool,
}

impl Motion {
    /// Animations play.
    #[must_use]
    pub const fn full() -> Self {
        Self { reduce: false }
    }

    /// Animations are skipped (`reduce_motion`).
    #[must_use]
    pub const fn reduced() -> Self {
        Self { reduce: true }
    }

    /// `Motion::reduced()` when `reduce_motion` is set.
    #[must_use]
    pub const fn new(reduce_motion: bool) -> Self {
        Self {
            reduce: reduce_motion,
        }
    }

    #[must_use]
    pub const fn reduce_motion(self) -> bool {
        self.reduce
    }
}

/// A timing curve mapping linear progress `0..=1` to eased progress.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Easing {
    Linear,
    /// Fluent standard easing, `cubic-bezier(0.8, 0, 0.2, 1)`.
    Standard,
    /// Fluent decelerate (entrance), `cubic-bezier(0.1, 0.9, 0.2, 1)`.
    Decelerate,
    /// Fluent accelerate (exit), `cubic-bezier(0.7, 0, 1, 0.5)`.
    Accelerate,
    /// A CSS-style cubic Bézier through `(0,0)`, `(x1,y1)`, `(x2,y2)`, `(1,1)`.
    Bezier(f32, f32, f32, f32),
}

impl Easing {
    /// Eased progress for linear progress `t` (clamped to `0..=1`).
    #[must_use]
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Easing::Linear => t,
            Easing::Standard => bezier(0.8, 0.0, 0.2, 1.0, t),
            Easing::Decelerate => bezier(0.1, 0.9, 0.2, 1.0, t),
            Easing::Accelerate => bezier(0.7, 0.0, 1.0, 0.5, t),
            Easing::Bezier(x1, y1, x2, y2) => bezier(x1, y1, x2, y2, t),
        }
    }
}

/// Solves the Bézier's x for `t` by bisection and returns the matching y.
fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let curve = |a: f32, b: f32, s: f32| {
        let inverse = 1.0 - s;
        3.0 * inverse * inverse * s * a + 3.0 * inverse * s * s * b + s * s * s
    };
    let (mut low, mut high) = (0.0_f32, 1.0_f32);
    for _ in 0..24 {
        let mid = (low + high) / 2.0;
        if curve(x1, x2, mid) < t {
            low = mid;
        } else {
            high = mid;
        }
    }
    curve(y1, y2, (low + high) / 2.0)
}

/// A value easing toward a target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transition {
    from: f32,
    to: f32,
    value: f32,
    started: Option<Duration>,
    running: bool,
    duration: Duration,
    easing: Easing,
    motion: Motion,
}

impl Transition {
    /// A settled transition at `value`.
    #[must_use]
    pub fn new(value: f32, motion: Motion) -> Self {
        Self {
            from: value,
            to: value,
            value,
            started: None,
            running: false,
            duration: Duration::ZERO,
            easing: Easing::Linear,
            motion,
        }
    }

    #[must_use]
    pub fn value(&self) -> f32 {
        self.value
    }

    #[must_use]
    pub fn target(&self) -> f32 {
        self.to
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running
    }

    #[must_use]
    pub fn motion(&self) -> Motion {
        self.motion
    }

    pub fn set_motion(&mut self, motion: Motion) {
        self.motion = motion;
        if motion.reduce_motion() {
            self.snap();
        }
    }

    /// Eases toward `target`. Returns whether frames are needed (the transition is running).
    /// Re-targeting a running transition restarts from the current value; asking for the
    /// current target changes nothing.
    pub fn set_target(&mut self, target: f32, duration: Duration, easing: Easing) -> bool {
        if target == self.to {
            return self.running;
        }
        self.to = target;
        if self.motion.reduce_motion() || duration.is_zero() {
            self.snap();
            return false;
        }
        self.from = self.value;
        self.started = None;
        self.duration = duration;
        self.easing = easing;
        self.running = true;
        true
    }

    /// Jumps to the target.
    pub fn snap(&mut self) {
        self.value = self.to;
        self.from = self.to;
        self.running = false;
        self.started = None;
    }

    /// Advances to the frame time `now`. Returns whether the transition is still running.
    pub fn tick(&mut self, now: Duration) -> bool {
        if !self.running {
            return false;
        }
        let start = *self.started.get_or_insert(now);
        let elapsed = now.saturating_sub(start);
        let progress = (elapsed.as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0);
        if progress >= 1.0 {
            self.snap();
        } else {
            self.value = self.from + (self.to - self.from) * self.easing.apply(progress);
        }
        self.running
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_millis(16);

    #[test]
    fn kit_easing_endpoints_and_monotonic() {
        for easing in [
            Easing::Linear,
            Easing::Standard,
            Easing::Decelerate,
            Easing::Accelerate,
            Easing::Bezier(0.25, 0.1, 0.25, 1.0),
        ] {
            assert!(easing.apply(0.0).abs() < 1e-3, "{easing:?} start");
            assert!((easing.apply(1.0) - 1.0).abs() < 1e-3, "{easing:?} end");
            let mut previous = 0.0;
            for step in 0..=20 {
                let value = easing.apply(step as f32 / 20.0);
                assert!(
                    value + 1e-4 >= previous,
                    "{easing:?} not monotonic at {step}"
                );
                previous = value;
            }
        }
        assert_eq!(Easing::Linear.apply(0.5), 0.5);
        assert_eq!(Easing::Linear.apply(-1.0), 0.0);
        assert_eq!(Easing::Linear.apply(2.0), 1.0);
        // Decelerate is ahead of linear at the midpoint, accelerate behind.
        assert!(Easing::Decelerate.apply(0.5) > 0.5);
        assert!(Easing::Accelerate.apply(0.5) < 0.5);
    }

    #[test]
    fn kit_transition_runs_over_its_duration() {
        let mut transition = Transition::new(0.0, Motion::full());
        assert!(!transition.is_running());
        assert!(transition.set_target(1.0, HOVER_DURATION, Easing::Linear));
        assert!(transition.is_running());
        assert_eq!(transition.target(), 1.0);
        // The first frame fixes the start time and changes nothing.
        assert!(transition.tick(Duration::from_millis(1000)));
        assert_eq!(transition.value(), 0.0);
        // Half of 83 ms.
        assert!(transition.tick(Duration::from_micros(1_041_500)));
        assert!((transition.value() - 0.5).abs() < 1e-3);
        // Past the end it settles exactly on the target.
        assert!(!transition.tick(Duration::from_millis(1000) + HOVER_DURATION + FRAME));
        assert_eq!(transition.value(), 1.0);
        assert!(!transition.is_running());
        // A settled transition ignores ticks.
        assert!(!transition.tick(Duration::from_secs(5)));
    }

    #[test]
    fn kit_transition_retarget_restarts_from_current_value() {
        let mut transition = Transition::new(0.0, Motion::full());
        transition.set_target(1.0, PRESS_DURATION, Easing::Linear);
        transition.tick(Duration::ZERO);
        transition.tick(Duration::from_micros(83_500));
        let midway = transition.value();
        assert!((midway - 0.5).abs() < 1e-3);
        // Same target: nothing changes and the running state is reported.
        assert!(transition.set_target(1.0, PRESS_DURATION, Easing::Linear));
        transition.set_target(0.0, PRESS_DURATION, Easing::Linear);
        assert_eq!(transition.value(), midway);
        transition.tick(Duration::from_secs(1));
        transition.tick(Duration::from_secs(1) + PRESS_DURATION / 2);
        assert!((transition.value() - midway / 2.0).abs() < 1e-3);
        assert_eq!(transition.motion(), Motion::full());
    }

    #[test]
    fn kit_transition_reduced_motion_jumps() {
        let mut transition = Transition::new(0.0, Motion::new(true));
        assert!(Motion::reduced().reduce_motion());
        assert!(!transition.set_target(1.0, PRESS_DURATION, Easing::Standard));
        assert_eq!(transition.value(), 1.0);
        assert!(!transition.is_running());
        // Switching motion off mid-flight snaps to the target.
        let mut running = Transition::new(0.0, Motion::full());
        running.set_target(1.0, PRESS_DURATION, Easing::Linear);
        running.set_motion(Motion::reduced());
        assert_eq!(running.value(), 1.0);
        assert!(!running.is_running());
        // Enabling motion keeps the value.
        running.set_motion(Motion::full());
        assert_eq!(running.value(), 1.0);
        // A zero duration also jumps.
        assert!(!running.set_target(0.0, Duration::ZERO, Easing::Linear));
        assert_eq!(running.value(), 0.0);
    }
}
