//! Smooth-scrolling math: animated wheel scrolling, direct high-resolution scrolling, programmatic
//! scroll-to, touch fling inertia, clamping and the extra scroll range typewriter mode needs.
//!
//! [`Scroller`] owns one axis. It never reads the clock or the OS: the host feeds it wheel events,
//! frame times (`Event::AnimationFrame`) and the user's motion preference
//! ([`Scroller::set_animations`], from `SPI_GETCLIENTAREAANIMATION`).
//!
//! # Motion model
//!
//! - **Notched wheel** (whole notches): the distance is added to the *pending target* of a
//!   critically damped spring ([`WHEEL_OMEGA`]); notches that arrive while it runs accumulate, the
//!   velocity is preserved on retarget and the position never overshoots the target.
//! - **High-resolution wheel / touchpad** (fractions of a notch, or pixel-precise events): applied
//!   directly, with no animation, so the content follows the fingers 1:1. A spring in flight is
//!   shifted by the same amount and stays consistent.
//! - **Programmatic scroll-to** uses the same spring with a softer stiffness
//!   ([`PROGRAMMATIC_OMEGA`]) so long jumps read as motion.
//! - **Fling** decays the velocity exponentially ([`FLING_FRICTION`]) and stops at the bounds.
//! - With animations disabled every move is instant.
//!
//! The spring and the fling are evaluated in closed form, so the result depends on the elapsed
//! time only, never on the frame rate.
//!
//! Offsets are DIPs from the start of the content; a larger offset shows content further down.

use std::time::Duration;

use crate::input::WheelEvent;

/// Stiffness (rad/s) of the wheel spring: about 60 % of a step in 60 ms and 99 % in 330 ms.
pub const WHEEL_OMEGA: f64 = 22.0;
/// Stiffness (rad/s) of programmatic scrolling: about 99 % in 550 ms.
pub const PROGRAMMATIC_OMEGA: f64 = 12.0;
/// Rate (1/s) at which a fling's velocity decays: it covers `velocity / FLING_FRICTION` in total.
pub const FLING_FRICTION: f64 = 3.0;
/// Slowest start velocity (DIPs/s) that still counts as a fling.
pub const MIN_FLING_VELOCITY: f64 = 50.0;

/// A spring closer than this (DIPs) to its target, and slower than [`SETTLE_SPEED`], has settled.
const SETTLE_DISTANCE: f64 = 0.05;
/// See [`SETTLE_DISTANCE`] (DIPs/s).
const SETTLE_SPEED: f64 = 2.0;
/// A fling slower than this (DIPs/s) ends.
const FLING_STOP_SPEED: f64 = 5.0;
/// The frame time assumed for the first frame of a motion.
const NOMINAL_FRAME: Duration = Duration::from_micros(16_667);
/// A wheel delta this close to a whole number of notches is a detented wheel.
const NOTCH_TOLERANCE: f64 = 1e-3;
/// Share of the viewport a page-mode wheel notch scrolls, before the one-line overlap.
const PAGE_SHARE: f64 = 0.9;

/// What one wheel notch scrolls (`SPI_GETWHEELSCROLLLINES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelLines {
    /// This many text lines per notch (the Windows default is 3).
    Lines(u32),
    /// One screen per notch (`WHEEL_PAGESCROLL`).
    Page,
}

impl WheelLines {
    /// The Windows default of three lines.
    pub const DEFAULT: WheelLines = WheelLines::Lines(3);

    /// The setting for the raw `SPI_GETWHEELSCROLLLINES` value (`u32::MAX` is page mode).
    #[must_use]
    pub const fn from_system(raw: u32) -> Self {
        if raw == u32::MAX {
            WheelLines::Page
        } else {
            WheelLines::Lines(raw)
        }
    }

    /// The distance of one notch in DIPs for text lines of `line_height` in a viewport of
    /// `viewport` DIPs. A page notch keeps one line of overlap and is never shorter than a line.
    #[must_use]
    pub fn notch_distance(self, line_height: f64, viewport: f64) -> f64 {
        match self {
            WheelLines::Lines(lines) => f64::from(lines) * line_height,
            WheelLines::Page => (viewport * PAGE_SHARE - line_height).max(line_height),
        }
    }
}

/// The scrollable range of an axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Extent {
    /// The visible length.
    pub viewport: f64,
    /// The length of the content.
    pub content: f64,
    /// Extra range past the end of the content (typewriter mode lets the last line reach the
    /// anchor position).
    pub end_overscroll: f64,
}

impl Extent {
    /// A range without extra space past the end. Negative or NaN lengths count as zero.
    #[must_use]
    pub fn new(viewport: f64, content: f64) -> Self {
        Self {
            viewport: viewport.max(0.0),
            content: content.max(0.0),
            end_overscroll: 0.0,
        }
    }

    /// The same range with `extra` DIPs of space past the end (negative counts as zero).
    #[must_use]
    pub fn with_end_overscroll(self, extra: f64) -> Self {
        Self {
            end_overscroll: extra.max(0.0),
            ..self
        }
    }

    /// The range of typewriter mode: the caret line sits at `anchor` (`0.0` top ... `1.0` bottom,
    /// clamped) of the viewport, so the last line needs `(1 - anchor) * viewport` of room past
    /// the end to get there.
    #[must_use]
    pub fn typewriter(viewport: f64, content: f64, anchor: f64) -> Self {
        let extent = Self::new(viewport, content);
        let anchor = if anchor.is_nan() {
            0.5
        } else {
            anchor.clamp(0.0, 1.0)
        };
        extent.with_end_overscroll(extent.viewport * (1.0 - anchor))
    }

    /// The largest offset.
    #[must_use]
    pub fn max_offset(&self) -> f64 {
        (self.content - self.viewport).max(0.0) + self.end_overscroll
    }

    /// `offset` limited to `0..=max_offset`.
    #[must_use]
    pub fn clamp(&self, offset: f64) -> f64 {
        offset.clamp(0.0, self.max_offset())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Idle,
    Spring,
    Fling,
}

/// The scroll offset of one axis and the motion that is carrying it.
#[derive(Debug, Clone, PartialEq)]
pub struct Scroller {
    extent: Extent,
    offset: f64,
    /// Where the spring ends (meaningful while `mode` is `Spring`).
    target: f64,
    /// DIPs per second.
    velocity: f64,
    omega: f64,
    mode: Mode,
    animations: bool,
    last_frame: Option<Duration>,
}

impl Default for Scroller {
    fn default() -> Self {
        Self::new(true)
    }
}

impl Scroller {
    /// A scroller at offset 0 over an empty range; `animations` is the user's motion preference
    /// (`SPI_GETCLIENTAREAANIMATION`).
    #[must_use]
    pub fn new(animations: bool) -> Self {
        Self {
            extent: Extent::new(0.0, 0.0),
            offset: 0.0,
            target: 0.0,
            velocity: 0.0,
            omega: WHEEL_OMEGA,
            mode: Mode::Idle,
            animations,
            last_frame: None,
        }
    }

    /// Whether animated motion is enabled.
    #[must_use]
    pub fn animations(&self) -> bool {
        self.animations
    }

    /// Changes the motion preference. Turning animations off finishes a running motion at once;
    /// returns whether the offset changed.
    pub fn set_animations(&mut self, enabled: bool) -> bool {
        self.animations = enabled;
        if enabled || self.mode == Mode::Idle {
            return false;
        }
        let end = self.target();
        self.jump_to(end)
    }

    /// The scrollable range.
    #[must_use]
    pub fn extent(&self) -> Extent {
        self.extent
    }

    /// Sets the scrollable range and pulls the offset (and a running spring's target) back into
    /// it. A fling that ends up outside stops. Returns whether the offset changed.
    pub fn set_extent(&mut self, extent: Extent) -> bool {
        self.extent = extent;
        let before = self.offset;
        self.offset = extent.clamp(self.offset);
        match self.mode {
            Mode::Spring => self.target = extent.clamp(self.target),
            Mode::Fling if self.offset != before => self.go_idle(),
            Mode::Fling | Mode::Idle => {}
        }
        self.offset != before
    }

    /// The offset to draw.
    #[must_use]
    pub fn offset(&self) -> f64 {
        self.offset
    }

    /// Where the current motion ends: the offset itself when idle, the spring's target, or the
    /// projected rest position of a fling.
    #[must_use]
    pub fn target(&self) -> f64 {
        match self.mode {
            Mode::Idle => self.offset,
            Mode::Spring => self.target,
            Mode::Fling => self
                .extent
                .clamp(self.offset + self.velocity / FLING_FRICTION),
        }
    }

    /// The current velocity in DIPs per second (0 when idle).
    #[must_use]
    pub fn velocity(&self) -> f64 {
        self.velocity
    }

    /// Whether [`tick`](Self::tick) has work to do, i.e. the host must keep requesting frames.
    #[must_use]
    pub fn is_animating(&self) -> bool {
        self.mode != Mode::Idle
    }

    /// How far the offset lies outside the range: negative above the start, positive past the
    /// end, 0 inside. Only an externally owned offset ([`adopt`](Self::adopt)) can be outside.
    #[must_use]
    pub fn excess(&self) -> f64 {
        self.offset - self.extent.clamp(self.offset)
    }

    /// Stops any motion and jumps to `offset` (clamped). Returns whether the offset changed.
    pub fn jump_to(&mut self, offset: f64) -> bool {
        let before = self.offset;
        self.offset = self.extent.clamp(offset);
        self.go_idle();
        self.offset != before
    }

    /// Takes over an offset another driver (DirectManipulation) computed: stops any motion and
    /// stores it as is, rubber-band overshoot included. Returns whether the offset changed.
    pub fn adopt(&mut self, offset: f64) -> bool {
        let before = self.offset;
        self.offset = offset;
        self.go_idle();
        self.offset != before
    }

    /// Freezes a running motion where it is.
    pub fn stop(&mut self) {
        self.go_idle();
    }

    /// Moves by `delta` at once (touchpad, precise wheel, keyboard). A running spring is shifted by
    /// the same amount so it stays consistent; a fling keeps its velocity. Returns whether the
    /// offset changed.
    pub fn scroll_by(&mut self, delta: f64) -> bool {
        let before = self.offset;
        self.offset = self.extent.clamp(self.offset + delta);
        if self.mode == Mode::Spring {
            self.target = self.extent.clamp(self.target + delta);
        }
        self.offset != before
    }

    /// Adds `delta` to the pending target of the wheel spring (notches accumulate). Instant with
    /// animations off. Returns whether the offset changed or an animation started.
    pub fn scroll_by_animated(&mut self, delta: f64) -> bool {
        let base = match self.mode {
            Mode::Spring => self.target,
            Mode::Idle | Mode::Fling => self.offset,
        };
        self.retarget(base + delta, WHEEL_OMEGA)
    }

    /// Animates to `offset` (clamped) with the programmatic spring; instant with animations off.
    /// Returns whether the offset changed or an animation started.
    pub fn scroll_to(&mut self, offset: f64) -> bool {
        self.retarget(offset, PROGRAMMATIC_OMEGA)
    }

    /// Applies the vertical part of a wheel event. A positive `delta.y` (wheel up) moves toward
    /// the start. Detented wheels (whole notches) animate by `notch` DIPs per notch; fractions of
    /// a notch and pixel-precise events move directly. The caller decides which axis an event
    /// belongs to (Shift, horizontal wheels). Returns whether the offset changed or an animation
    /// started.
    pub fn wheel(&mut self, event: &WheelEvent, notch: f64) -> bool {
        let up = f64::from(event.delta.y);
        if up == 0.0 {
            false
        } else if event.precise {
            self.scroll_by(-up)
        } else if (up - up.round()).abs() < NOTCH_TOLERANCE {
            self.scroll_by_animated(-up * notch)
        } else {
            self.scroll_by(-up * notch)
        }
    }

    /// Starts a fling with `velocity` DIPs/s (positive moves toward the end), carrying a touch
    /// release. Ignored with animations off or below [`MIN_FLING_VELOCITY`]. Returns whether a
    /// fling started.
    pub fn fling(&mut self, velocity: f64) -> bool {
        if !self.animations || velocity.abs() < MIN_FLING_VELOCITY {
            return false;
        }
        self.velocity = velocity;
        self.mode = Mode::Fling;
        self.last_frame = None;
        true
    }

    /// Advances the motion to the frame time `now`. Returns whether the offset changed; keep
    /// requesting frames while [`is_animating`](Self::is_animating).
    pub fn tick(&mut self, now: Duration) -> bool {
        if self.mode == Mode::Idle {
            return false;
        }
        let dt = match self.last_frame.replace(now) {
            Some(previous) => now.saturating_sub(previous),
            None => NOMINAL_FRAME,
        }
        .as_secs_f64();
        if dt == 0.0 {
            return false;
        }
        let before = self.offset;
        if self.mode == Mode::Spring {
            self.step_spring(dt);
        } else {
            self.step_fling(dt);
        }
        self.offset != before
    }

    fn retarget(&mut self, target: f64, omega: f64) -> bool {
        let target = self.extent.clamp(target);
        if !self.animations {
            return self.jump_to(target);
        }
        if self.mode == Mode::Idle {
            if (target - self.offset).abs() < SETTLE_DISTANCE {
                return false;
            }
            self.velocity = 0.0;
            self.last_frame = None;
        }
        self.target = target;
        self.omega = omega;
        self.mode = Mode::Spring;
        true
    }

    /// Critically damped spring in closed form: `x(t) = (x0 + (v0 + w x0) t) e^(-w t)`.
    fn step_spring(&mut self, dt: f64) {
        let w = self.omega;
        let from_target = self.offset - self.target;
        let slope = self.velocity + w * from_target;
        let decay = (-w * dt).exp();
        let position = self.target + (from_target + slope * dt) * decay;
        self.velocity = (self.velocity - w * slope * dt) * decay;
        self.offset = self.extent.clamp(position);
        if self.offset != position {
            self.velocity = 0.0;
        }
        if (self.offset - self.target).abs() < SETTLE_DISTANCE && self.velocity.abs() < SETTLE_SPEED
        {
            self.offset = self.target;
            self.go_idle();
        }
    }

    /// Exponential velocity decay in closed form: `v(t) = v0 e^(-k t)`, `x(t) = x0 + v0/k (1 - e^(-k t))`.
    fn step_fling(&mut self, dt: f64) {
        let decay = (-FLING_FRICTION * dt).exp();
        let travelled = self.velocity / FLING_FRICTION * (1.0 - decay);
        self.velocity *= decay;
        let position = self.offset + travelled;
        self.offset = self.extent.clamp(position);
        if self.offset != position {
            self.go_idle();
        } else if self.velocity.abs() < FLING_STOP_SPEED {
            // Land on the rest position instead of stopping short of it.
            self.offset = self
                .extent
                .clamp(self.offset + self.velocity / FLING_FRICTION);
            self.go_idle();
        }
    }

    fn go_idle(&mut self) {
        self.mode = Mode::Idle;
        self.velocity = 0.0;
        self.target = self.offset;
        self.last_frame = None;
    }
}
