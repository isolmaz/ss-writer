//! Behavior of the smooth-scrolling model: animated and direct wheel input, programmatic
//! scrolling, fling inertia, clamping, typewriter overscroll and the reduced-motion switch.

use std::time::Duration;

use ssw_ui::geometry::Point;
use ssw_ui::input::{Modifiers, WheelEvent};
use ssw_ui::scroll_physics::{
    Extent, FLING_FRICTION, MIN_FLING_VELOCITY, Scroller, WHEEL_OMEGA, WheelLines,
};

const FRAME: Duration = Duration::from_millis(16);
const LINE: f64 = 22.0;
const VIEWPORT: f64 = 700.0;

fn wheel_event(y: f32, precise: bool) -> WheelEvent {
    WheelEvent {
        delta: Point::new(0.0, y),
        precise,
        position: Point::ZERO,
        mods: Modifiers::NONE,
    }
}

fn scroller(animations: bool) -> Scroller {
    let mut scroller = Scroller::new(animations);
    scroller.set_extent(Extent::new(VIEWPORT, 90_000.0));
    scroller
}

fn notch() -> f64 {
    WheelLines::DEFAULT.notch_distance(LINE, VIEWPORT)
}

/// Frame clock: every call to `advance` is one 16 ms frame.
struct Clock(Duration);

impl Clock {
    fn new() -> Self {
        Clock(Duration::ZERO)
    }

    fn advance(&mut self, scroller: &mut Scroller) -> bool {
        self.0 += FRAME;
        scroller.tick(self.0)
    }
}

/// Ticks until the motion ends and returns the offset after every frame (the start included).
fn run_to_rest(scroller: &mut Scroller, clock: &mut Clock, max_frames: usize) -> Vec<f64> {
    let mut offsets = vec![scroller.offset()];
    for _ in 0..max_frames {
        if !scroller.is_animating() {
            return offsets;
        }
        clock.advance(scroller);
        offsets.push(scroller.offset());
    }
    panic!("still animating after {max_frames} frames at {offsets:?}");
}

fn is_non_decreasing(values: &[f64]) -> bool {
    values.windows(2).all(|pair| pair[1] >= pair[0])
}

fn is_non_increasing(values: &[f64]) -> bool {
    values.windows(2).all(|pair| pair[1] <= pair[0])
}

fn approx(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{actual} is not within {tolerance} of {expected}"
    );
}

#[test]
fn wheel_lines_follow_the_system_setting() {
    assert_eq!(WheelLines::from_system(3), WheelLines::Lines(3));
    assert_eq!(WheelLines::from_system(0), WheelLines::Lines(0));
    assert_eq!(WheelLines::from_system(u32::MAX), WheelLines::Page);
    assert_eq!(WheelLines::DEFAULT, WheelLines::Lines(3));
    assert_eq!(notch(), 66.0);
    assert_eq!(WheelLines::Lines(0).notch_distance(LINE, VIEWPORT), 0.0);
    assert_eq!(WheelLines::Lines(1).notch_distance(17.5, VIEWPORT), 17.5);
    // A page keeps one line of overlap...
    approx(WheelLines::Page.notch_distance(LINE, VIEWPORT), 608.0, 1e-9);
    // ... and is never shorter than one line.
    assert_eq!(WheelLines::Page.notch_distance(LINE, 20.0), LINE);
}

#[test]
fn extent_limits_the_offset_and_reserves_typewriter_room() {
    let plain = Extent::new(600.0, 2000.0);
    assert_eq!(plain.max_offset(), 1400.0);
    assert_eq!(plain.clamp(-5.0), 0.0);
    assert_eq!(plain.clamp(700.0), 700.0);
    assert_eq!(plain.clamp(9999.0), 1400.0);
    // Content shorter than the viewport does not scroll.
    assert_eq!(Extent::new(600.0, 100.0).max_offset(), 0.0);
    // Negative lengths and negative extra room count as zero.
    let degenerate = Extent::new(-1.0, -1.0).with_end_overscroll(-50.0);
    assert_eq!(degenerate.viewport, 0.0);
    assert_eq!(degenerate.content, 0.0);
    assert_eq!(degenerate.end_overscroll, 0.0);
    assert_eq!(degenerate.max_offset(), 0.0);

    // With the caret line at the middle, the last line needs half a viewport of room.
    let centered = Extent::typewriter(600.0, 2000.0, 0.5);
    assert_eq!(centered.end_overscroll, 300.0);
    assert_eq!(centered.max_offset(), 1700.0);
    assert_eq!(Extent::typewriter(600.0, 2000.0, 0.0).max_offset(), 2000.0);
    assert_eq!(Extent::typewriter(600.0, 2000.0, 1.0).max_offset(), 1400.0);
    assert_eq!(Extent::typewriter(600.0, 2000.0, 2.0).max_offset(), 1400.0);
    assert_eq!(Extent::typewriter(600.0, 2000.0, -3.0).max_offset(), 2000.0);
    assert_eq!(
        Extent::typewriter(600.0, 2000.0, f64::NAN).max_offset(),
        1700.0
    );
    // A short document still scrolls its last line up to the anchor.
    assert_eq!(Extent::typewriter(600.0, 100.0, 0.5).max_offset(), 300.0);
}

#[test]
fn notched_wheel_accumulates_into_one_monotonic_animation() {
    let mut scroller = scroller(true);
    let mut offsets = Vec::new();
    let mut delivered = 0u32;
    for frame in 0..300u32 {
        let now = FRAME * frame;
        // Five notches, 60 ms apart.
        while delivered < 5 && Duration::from_millis(60) * delivered <= now {
            assert!(scroller.wheel(&wheel_event(-1.0, false), notch()));
            delivered += 1;
        }
        scroller.tick(now);
        offsets.push(scroller.offset());
        if delivered == 5 && !scroller.is_animating() {
            break;
        }
    }
    assert_eq!(delivered, 5);
    assert!(!scroller.is_animating());
    assert_eq!(scroller.offset(), 5.0 * 66.0);
    assert!(is_non_decreasing(&offsets), "{offsets:?}");
    assert!(offsets.iter().all(|&offset| offset <= 330.0), "{offsets:?}");
    // Animated: many intermediate frames, not a jump.
    assert!(
        offsets.iter().filter(|&&o| o > 0.0 && o < 330.0).count() > 15,
        "{offsets:?}"
    );
    // It settles in well under a second after the last notch (60 ms * 4 + ~0.45 s).
    assert!(offsets.len() < 60, "{} frames", offsets.len());
}

#[test]
fn first_frame_moves_a_notch_part_of_the_way() {
    let mut scroller = scroller(true);
    assert!(scroller.wheel(&wheel_event(-1.0, false), notch()));
    assert!(scroller.is_animating());
    assert_eq!(scroller.offset(), 0.0);
    assert_eq!(scroller.target(), 66.0);
    assert!(scroller.tick(Duration::ZERO));
    // One nominal 60 Hz frame of the critically damped spring: 66 * (1 - (1 + w dt) e^(-w dt)).
    let dt = 1.0 / 60.0;
    let expected = 66.0 * (1.0 - (1.0 + WHEEL_OMEGA * dt) * (-WHEEL_OMEGA * dt).exp());
    approx(scroller.offset(), expected, 0.01);
}

#[test]
fn wheel_up_moves_toward_the_start_and_clamps_there() {
    let mut scroller = scroller(true);
    scroller.jump_to(100.0);
    // Three notches up in a burst retarget the running animation.
    for _ in 0..3 {
        assert!(scroller.wheel(&wheel_event(1.0, false), notch()));
    }
    assert_eq!(scroller.target(), 0.0);
    let mut clock = Clock::new();
    let offsets = run_to_rest(&mut scroller, &mut clock, 200);
    assert_eq!(scroller.offset(), 0.0);
    assert!(is_non_increasing(&offsets), "{offsets:?}");
    assert!(offsets.iter().all(|&offset| offset >= 0.0));

    // Forty more notches against the start change nothing.
    for _ in 0..40 {
        assert!(!scroller.wheel(&wheel_event(1.0, false), notch()));
    }
    assert!(!scroller.is_animating());
    assert_eq!(scroller.offset(), 0.0);
}

#[test]
fn burst_in_the_opposite_direction_retargets_from_the_current_motion() {
    let mut scroller = scroller(true);
    scroller.jump_to(462.0);
    for _ in 0..3 {
        assert!(scroller.wheel(&wheel_event(1.0, false), notch()));
    }
    assert_eq!(scroller.target(), 462.0 - 3.0 * 66.0);
    let mut clock = Clock::new();
    // Start moving, then reverse half-way: the spring keeps its velocity and goes to the new
    // target without a jump.
    for _ in 0..4 {
        clock.advance(&mut scroller);
    }
    let before = scroller.offset();
    let velocity = scroller.velocity();
    assert!(velocity < 0.0);
    assert!(scroller.wheel(&wheel_event(-2.0, false), notch()));
    assert_eq!(scroller.offset(), before);
    assert_eq!(scroller.velocity(), velocity);
    assert_eq!(scroller.target(), 462.0 - 3.0 * 66.0 + 2.0 * 66.0);
    run_to_rest(&mut scroller, &mut clock, 300);
    assert_eq!(scroller.offset(), 462.0 - 66.0);
}

#[test]
fn wheel_stops_at_the_end_of_the_range() {
    let mut scroller = Scroller::new(true);
    scroller.set_extent(Extent::new(600.0, 2000.0));
    for _ in 0..40 {
        scroller.wheel(&wheel_event(-1.0, false), notch());
    }
    assert_eq!(scroller.target(), 1400.0);
    let mut clock = Clock::new();
    let offsets = run_to_rest(&mut scroller, &mut clock, 400);
    assert_eq!(scroller.offset(), 1400.0);
    assert!(offsets.iter().all(|&offset| offset <= 1400.0));
}

#[test]
fn high_resolution_wheel_moves_directly_without_animation() {
    let mut scroller = scroller(true);
    scroller.jump_to(500.0);
    // A hi-res wheel reports a tenth of a notch per message.
    for _ in 0..20 {
        assert!(scroller.wheel(&wheel_event(-0.1, false), notch()));
    }
    assert!(!scroller.is_animating());
    approx(scroller.offset(), 500.0 + 20.0 * 0.1 * 66.0, 1e-3);
    // Up again by the same total.
    for _ in 0..20 {
        scroller.wheel(&wheel_event(0.1, false), notch());
    }
    approx(scroller.offset(), 500.0, 1e-3);
    assert!(!scroller.is_animating());
}

#[test]
fn precise_events_are_pixels_not_notches() {
    let mut scroller = scroller(true);
    scroller.jump_to(100.0);
    assert!(scroller.wheel(&wheel_event(-12.0, true), notch()));
    assert_eq!(scroller.offset(), 112.0);
    assert!(scroller.wheel(&wheel_event(30.0, true), notch()));
    assert_eq!(scroller.offset(), 82.0);
    assert!(!scroller.is_animating());
    // Even a whole number of pixels is not a notch.
    assert!(scroller.wheel(&wheel_event(-1.0, true), notch()));
    assert_eq!(scroller.offset(), 83.0);
    assert!(!scroller.is_animating());
}

#[test]
fn events_without_a_vertical_part_are_ignored() {
    let mut scroller = scroller(true);
    scroller.jump_to(100.0);
    let horizontal = WheelEvent {
        delta: Point::new(3.0, 0.0),
        precise: false,
        position: Point::ZERO,
        mods: Modifiers::NONE,
    };
    assert!(!scroller.wheel(&horizontal, notch()));
    assert_eq!(scroller.offset(), 100.0);
    assert!(!scroller.is_animating());
}

#[test]
fn direct_moves_shift_a_running_spring_and_keep_it_consistent() {
    let mut scroller = scroller(true);
    scroller.jump_to(1000.0);
    assert!(scroller.scroll_by_animated(300.0));
    let mut clock = Clock::new();
    for _ in 0..5 {
        clock.advance(&mut scroller);
    }
    let before = scroller.offset();
    assert!(scroller.scroll_by(20.0));
    assert_eq!(scroller.offset(), before + 20.0);
    assert_eq!(scroller.target(), 1320.0);
    assert!(scroller.is_animating());
    run_to_rest(&mut scroller, &mut clock, 300);
    assert_eq!(scroller.offset(), 1320.0);

    // Clamped at both ends; a no-op reports no change.
    assert!(scroller.scroll_by(-1e9));
    assert_eq!(scroller.offset(), 0.0);
    assert!(!scroller.scroll_by(-10.0));
    // The running spring's target is clamped too (the offset itself is already at the start).
    assert!(scroller.scroll_by_animated(100.0));
    assert!(!scroller.scroll_by(-1e9));
    assert_eq!(scroller.target(), 0.0);
}

#[test]
fn programmatic_scroll_is_slower_than_the_wheel_and_lands_exactly() {
    let mut wheel = scroller(true);
    wheel.jump_to(1000.0);
    assert!(wheel.scroll_by_animated(2000.0));
    let wheel_frames = run_to_rest(&mut wheel, &mut Clock::new(), 600).len();
    assert_eq!(wheel.offset(), 3000.0);

    let mut jump = scroller(true);
    jump.jump_to(1000.0);
    assert!(jump.scroll_to(3000.0));
    assert_eq!(jump.target(), 3000.0);
    let offsets = run_to_rest(&mut jump, &mut Clock::new(), 600);
    assert_eq!(jump.offset(), 3000.0);
    assert!(is_non_decreasing(&offsets));
    assert!(offsets.iter().all(|&offset| offset <= 3000.0));
    assert!(
        offsets.len() > wheel_frames,
        "{} frames vs {wheel_frames}",
        offsets.len()
    );

    // Asking for where it already is does nothing.
    assert!(!jump.scroll_to(3000.0));
    assert!(!jump.scroll_to(3000.01));
    assert!(!jump.is_animating());
    // Targets are clamped.
    assert!(jump.scroll_to(1e12));
    assert_eq!(jump.target(), jump.extent().max_offset());
    assert!(jump.scroll_to(-1e12));
    assert_eq!(jump.target(), 0.0);
}

#[test]
fn typewriter_range_lets_the_last_line_reach_the_anchor() {
    let mut scroller = Scroller::new(true);
    scroller.set_extent(Extent::typewriter(600.0, 2000.0, 0.5));
    assert!(scroller.scroll_to(1e9));
    assert_eq!(scroller.target(), 1700.0);
    run_to_rest(&mut scroller, &mut Clock::new(), 600);
    assert_eq!(scroller.offset(), 1700.0);
    // Growing the content keeps the range consistent; shrinking pulls the offset back in.
    assert!(!scroller.set_extent(Extent::typewriter(600.0, 2400.0, 0.5)));
    assert_eq!(scroller.extent().max_offset(), 2100.0);
    assert!(scroller.set_extent(Extent::typewriter(600.0, 1000.0, 0.5)));
    assert_eq!(scroller.offset(), 700.0);
}

#[test]
fn motion_does_not_depend_on_the_frame_rate() {
    let mut coarse = scroller(true);
    let mut fine = scroller(true);
    coarse.scroll_by_animated(300.0);
    fine.scroll_by_animated(300.0);
    // The first frame of a motion assumes a nominal frame time; align both there.
    let start = Duration::from_millis(1000);
    coarse.tick(start);
    fine.tick(start);
    assert_eq!(coarse.offset(), fine.offset());

    coarse.tick(start + Duration::from_millis(100));
    for step in 1..=10 {
        fine.tick(start + Duration::from_millis(10 * step));
    }
    approx(coarse.offset(), fine.offset(), 1e-9);
    approx(coarse.velocity(), fine.velocity(), 1e-6);
    assert!(coarse.offset() > 0.0 && coarse.offset() < 300.0);
}

#[test]
fn a_stalled_frame_finishes_the_motion_and_a_backwards_clock_changes_nothing() {
    let mut scroller = scroller(true);
    scroller.scroll_by_animated(300.0);
    scroller.tick(Duration::from_millis(100));
    let before = scroller.offset();
    // A clock that went backwards advances by zero.
    assert!(!scroller.tick(Duration::from_millis(50)));
    assert_eq!(scroller.offset(), before);
    assert!(scroller.is_animating());
    // Ten seconds later the spring is at rest on its target.
    assert!(scroller.tick(Duration::from_secs(10)));
    assert_eq!(scroller.offset(), 300.0);
    assert!(!scroller.is_animating());
    assert_eq!(scroller.velocity(), 0.0);
    // Idle ticks report nothing.
    assert!(!scroller.tick(Duration::from_secs(11)));
}

#[test]
fn a_new_motion_after_idle_starts_from_a_nominal_frame_not_the_stale_clock() {
    let mut scroller = scroller(true);
    scroller.scroll_by_animated(300.0);
    scroller.tick(Duration::from_millis(0));
    scroller.tick(Duration::from_secs(10));
    assert!(!scroller.is_animating());
    // Much later: the first frame must not see a 90 s gap and finish the motion at once.
    scroller.scroll_by_animated(300.0);
    scroller.tick(Duration::from_secs(100));
    assert!(scroller.is_animating());
    assert!(scroller.offset() > 300.0 && scroller.offset() < 600.0);
}

#[test]
fn reduced_motion_makes_every_move_instant() {
    let mut scroller = scroller(false);
    assert!(!scroller.animations());
    assert!(scroller.wheel(&wheel_event(-1.0, false), notch()));
    assert_eq!(scroller.offset(), 66.0);
    assert!(!scroller.is_animating());
    assert!(scroller.scroll_to(1000.0));
    assert_eq!(scroller.offset(), 1000.0);
    assert!(scroller.scroll_by_animated(-100.0));
    assert_eq!(scroller.offset(), 900.0);
    assert!(!scroller.is_animating());
    // No inertia either.
    assert!(!scroller.fling(2000.0));
    assert!(!scroller.is_animating());
}

#[test]
fn switching_animations_off_finishes_the_running_motion() {
    let mut scroller = scroller(true);
    assert!(scroller.animations());
    // Nothing runs: no change either way.
    assert!(!scroller.set_animations(false));
    assert!(!scroller.set_animations(true));

    scroller.scroll_by_animated(330.0);
    scroller.tick(Duration::ZERO);
    assert!(scroller.is_animating());
    // Enabling is a no-op while running.
    assert!(!scroller.set_animations(true));
    assert!(scroller.is_animating());
    assert!(scroller.set_animations(false));
    assert_eq!(scroller.offset(), 330.0);
    assert!(!scroller.is_animating());
    assert!(!scroller.animations());

    // A fling jumps to its rest position.
    scroller.set_animations(true);
    scroller.jump_to(0.0);
    assert!(scroller.fling(900.0));
    assert!(scroller.set_animations(false));
    approx(scroller.offset(), 900.0 / FLING_FRICTION, 1e-9);
    assert!(!scroller.is_animating());
}

#[test]
fn tiny_animated_moves_are_not_animated() {
    let mut scroller = scroller(true);
    scroller.jump_to(100.0);
    assert!(!scroller.scroll_by_animated(0.01));
    assert!(!scroller.scroll_by_animated(0.0));
    assert!(!scroller.is_animating());
    assert_eq!(scroller.offset(), 100.0);
}

#[test]
fn fling_decelerates_to_its_projected_rest_position() {
    let mut scroller = scroller(true);
    scroller.jump_to(1000.0);
    assert!(scroller.fling(900.0));
    assert!(scroller.is_animating());
    assert_eq!(scroller.velocity(), 900.0);
    approx(scroller.target(), 1300.0, 1e-9);

    let mut clock = Clock::new();
    let mut offsets = vec![scroller.offset()];
    let mut velocities = vec![scroller.velocity()];
    while scroller.is_animating() {
        assert!(offsets.len() < 400, "fling never ended");
        clock.advance(&mut scroller);
        offsets.push(scroller.offset());
        velocities.push(scroller.velocity());
    }
    assert!(is_non_decreasing(&offsets), "{offsets:?}");
    // The speed falls every frame until the fling ends (and is then reset).
    let moving = &velocities[..velocities.len() - 1];
    assert!(is_non_increasing(moving), "{moving:?}");
    approx(scroller.offset(), 1300.0, 1e-6);
    assert_eq!(scroller.velocity(), 0.0);
    // v0 e^(-k t) < 5 after ln(180) / 3 = 1.73 s.
    let frames = offsets.len();
    assert!((100..120).contains(&frames), "{frames} frames");
}

#[test]
fn fling_toward_the_start_stops_at_zero_and_toward_the_end_at_the_maximum() {
    let mut scroller = Scroller::new(true);
    scroller.set_extent(Extent::new(600.0, 2000.0));
    scroller.jump_to(100.0);
    assert!(scroller.fling(-2000.0));
    assert_eq!(scroller.target(), 0.0);
    let offsets = run_to_rest(&mut scroller, &mut Clock::new(), 400);
    assert_eq!(scroller.offset(), 0.0);
    assert_eq!(scroller.velocity(), 0.0);
    assert!(offsets.iter().all(|&offset| offset >= 0.0));

    scroller.jump_to(1300.0);
    assert!(scroller.fling(2000.0));
    assert_eq!(scroller.target(), 1400.0);
    run_to_rest(&mut scroller, &mut Clock::new(), 400);
    assert_eq!(scroller.offset(), 1400.0);
    assert!(!scroller.is_animating());
}

#[test]
fn slow_releases_are_not_flings() {
    let mut scroller = scroller(true);
    assert!(!scroller.fling(MIN_FLING_VELOCITY - 1.0));
    assert!(!scroller.fling(-(MIN_FLING_VELOCITY - 1.0)));
    assert!(!scroller.is_animating());
    assert!(scroller.fling(MIN_FLING_VELOCITY));
    assert!(scroller.is_animating());
}

#[test]
fn a_wheel_notch_during_a_fling_continues_as_a_spring_with_the_same_velocity() {
    let mut scroller = scroller(true);
    scroller.jump_to(1000.0);
    scroller.fling(900.0);
    let mut clock = Clock::new();
    for _ in 0..5 {
        clock.advance(&mut scroller);
    }
    let offset = scroller.offset();
    let velocity = scroller.velocity();
    assert!(velocity > 0.0 && velocity < 900.0);
    assert!(scroller.wheel(&wheel_event(-1.0, false), notch()));
    assert_eq!(scroller.offset(), offset);
    assert_eq!(scroller.velocity(), velocity);
    assert_eq!(scroller.target(), offset + 66.0);
    run_to_rest(&mut scroller, &mut clock, 300);
    assert_eq!(scroller.offset(), offset + 66.0);
}

#[test]
fn direct_moves_do_not_disturb_a_fling_but_range_changes_can_end_it() {
    let mut scroller = scroller(true);
    scroller.jump_to(1000.0);
    scroller.fling(900.0);
    let mut clock = Clock::new();
    clock.advance(&mut scroller);
    let velocity = scroller.velocity();
    assert!(scroller.scroll_by(15.0));
    assert_eq!(scroller.velocity(), velocity);
    assert!(scroller.is_animating());

    // A range that still contains the offset leaves the fling running.
    assert!(!scroller.set_extent(Extent::new(VIEWPORT, 80_000.0)));
    assert!(scroller.is_animating());
    // One that excludes it clamps the offset and stops the fling.
    assert!(scroller.set_extent(Extent::new(VIEWPORT, 1100.0)));
    assert_eq!(scroller.offset(), 400.0);
    assert!(!scroller.is_animating());
    assert_eq!(scroller.velocity(), 0.0);
}

#[test]
fn range_changes_clamp_a_running_spring() {
    let mut scroller = scroller(true);
    scroller.jump_to(50.0);
    scroller.scroll_by_animated(280.0);
    assert_eq!(scroller.target(), 330.0);
    // The new maximum is 200: the target follows, the offset is already inside.
    assert!(!scroller.set_extent(Extent::new(VIEWPORT, 900.0)));
    assert_eq!(scroller.target(), 200.0);
    run_to_rest(&mut scroller, &mut Clock::new(), 300);
    assert_eq!(scroller.offset(), 200.0);
    // Idle scrollers just clamp.
    assert!(scroller.set_extent(Extent::new(VIEWPORT, 800.0)));
    assert_eq!(scroller.offset(), 100.0);
    assert_eq!(scroller.extent(), Extent::new(VIEWPORT, 800.0));
}

#[test]
fn adopted_offsets_may_overshoot_and_report_their_excess() {
    let mut scroller = scroller(true);
    assert_eq!(scroller.excess(), 0.0);
    assert!(scroller.adopt(-30.0));
    assert_eq!(scroller.offset(), -30.0);
    assert_eq!(scroller.excess(), -30.0);
    assert!(!scroller.adopt(-30.0));

    let max = scroller.extent().max_offset();
    assert!(scroller.adopt(max + 25.0));
    assert_eq!(scroller.excess(), 25.0);
    // Entering the range again (or a range change) clears the excess.
    assert!(scroller.set_extent(Extent::new(VIEWPORT, 90_000.0)));
    assert_eq!(scroller.offset(), max);
    assert_eq!(scroller.excess(), 0.0);

    // Adopting stops any motion of ours: the other driver is in charge now.
    scroller.jump_to(1000.0);
    assert!(scroller.scroll_by_animated(200.0));
    assert!(scroller.is_animating());
    assert!(scroller.adopt(10.0));
    assert!(!scroller.is_animating());
    assert_eq!(scroller.target(), 10.0);
    assert_eq!(scroller.velocity(), 0.0);
}

#[test]
fn stop_freezes_the_motion_where_it_is_and_jump_to_clamps() {
    let mut scroller = scroller(true);
    scroller.scroll_by_animated(330.0);
    let mut clock = Clock::new();
    for _ in 0..3 {
        clock.advance(&mut scroller);
    }
    let frozen = scroller.offset();
    assert!(frozen > 0.0 && frozen < 330.0);
    scroller.stop();
    assert!(!scroller.is_animating());
    assert_eq!(scroller.offset(), frozen);
    assert_eq!(scroller.target(), frozen);
    assert_eq!(scroller.velocity(), 0.0);
    assert!(!clock.advance(&mut scroller));

    assert!(scroller.jump_to(-50.0));
    assert_eq!(scroller.offset(), 0.0);
    assert!(!scroller.jump_to(-50.0));
    assert!(scroller.jump_to(1e12));
    assert_eq!(scroller.offset(), scroller.extent().max_offset());
}

#[test]
fn default_scroller_animates_over_an_empty_range() {
    let mut scroller = Scroller::default();
    assert!(scroller.animations());
    assert_eq!(scroller.extent(), Extent::new(0.0, 0.0));
    assert_eq!(scroller.offset(), 0.0);
    assert!(!scroller.is_animating());
    // Nothing to scroll: every request is clamped away.
    assert!(!scroller.wheel(&wheel_event(-1.0, false), notch()));
    assert!(!scroller.scroll_to(500.0));
    assert!(!scroller.is_animating());
}
