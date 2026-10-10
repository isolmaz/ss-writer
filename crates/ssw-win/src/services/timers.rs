//! `SetTimer` timers and animation-frame scheduling.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use ssw_ui::services::{TimerId, Timers};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWM_TIMING_INFO, DwmGetCompositionTimingInfo};
use windows::Win32::UI::WindowsAndMessaging::{
    KillTimer, SetTimer, USER_TIMER_MAXIMUM, USER_TIMER_MINIMUM,
};

/// What a `WM_TIMER` message resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fired {
    pub id: TimerId,
    /// The timer does not repeat; the caller must `KillTimer` it.
    pub one_shot: bool,
}

/// Maps `SetTimer` ids to [`TimerId`]s and remembers a pending animation-frame request.
#[derive(Debug, Default)]
pub struct TimerTable {
    by_os_id: HashMap<usize, (TimerId, bool)>,
    by_id: HashMap<TimerId, usize>,
    next_os_id: usize,
    frame_requested: bool,
}

impl TimerTable {
    /// The OS timer id to use for `id` (stable while the timer exists).
    pub fn register(&mut self, id: TimerId, repeat: bool) -> usize {
        let os_id = *self.by_id.entry(id).or_insert_with(|| {
            self.next_os_id += 1;
            self.next_os_id
        });
        self.by_os_id.insert(os_id, (id, repeat));
        os_id
    }

    /// Forgets `id`; returns its OS timer id.
    pub fn unregister(&mut self, id: TimerId) -> Option<usize> {
        let os_id = self.by_id.remove(&id)?;
        self.by_os_id.remove(&os_id);
        Some(os_id)
    }

    /// Resolves a `WM_TIMER` id; one-shot timers are forgotten.
    pub fn fire(&mut self, os_id: usize) -> Option<Fired> {
        let (id, repeat) = *self.by_os_id.get(&os_id)?;
        if !repeat {
            self.by_os_id.remove(&os_id);
            self.by_id.remove(&id);
        }
        Some(Fired {
            id,
            one_shot: !repeat,
        })
    }

    /// Takes the pending animation-frame request.
    pub fn take_frame_request(&mut self) -> bool {
        std::mem::take(&mut self.frame_requested)
    }

    /// Number of live timers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_os_id.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_os_id.is_empty()
    }
}

/// The [`Timers`] implementation of one window.
#[derive(Debug, Clone)]
pub struct WindowTimers {
    hwnd: HWND,
    table: Rc<RefCell<TimerTable>>,
}

impl WindowTimers {
    #[must_use]
    pub fn new(hwnd: HWND, table: Rc<RefCell<TimerTable>>) -> Self {
        Self { hwnd, table }
    }

    /// The shared table (the window resolves `WM_TIMER` through it).
    #[must_use]
    pub fn table(&self) -> &Rc<RefCell<TimerTable>> {
        &self.table
    }
}

/// An interval clamped to what `SetTimer` accepts, in milliseconds.
#[must_use]
pub fn clamp_interval_ms(interval: Duration) -> u32 {
    u32::try_from(interval.as_millis())
        .unwrap_or(USER_TIMER_MAXIMUM)
        .clamp(USER_TIMER_MINIMUM, USER_TIMER_MAXIMUM)
}

impl Timers for WindowTimers {
    fn set(&mut self, id: TimerId, interval: Duration, repeat: bool) {
        let os_id = self.table.borrow_mut().register(id, repeat);
        // SAFETY: plain timer registration on our own window; no callback.
        unsafe {
            SetTimer(Some(self.hwnd), os_id, clamp_interval_ms(interval), None);
        }
    }

    fn kill(&mut self, id: TimerId) {
        if let Some(os_id) = self.table.borrow_mut().unregister(id) {
            // SAFETY: kills a timer registered on our own window; an already-expired timer
            // makes this fail harmlessly.
            unsafe {
                let _ = KillTimer(Some(self.hwnd), os_id);
            }
        }
    }

    fn request_animation_frame(&mut self) {
        self.table.borrow_mut().frame_requested = true;
    }
}

/// The display refresh interval from DWM (60 Hz when unavailable).
#[must_use]
pub fn refresh_interval() -> Duration {
    let mut info = DWM_TIMING_INFO {
        cbSize: size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    // SAFETY: valid out structure with its size set; a null window asks for the composition
    // timing of the primary display.
    let ok = unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut info) }.is_ok();
    let ratio = info.rateRefresh;
    interval_from_rate(ok, ratio.uiNumerator, ratio.uiDenominator)
}

/// The frame interval of a refresh rate of `numerator / denominator` Hz that DWM reported
/// (`known`); 60 Hz when DWM failed or reported no usable rate.
fn interval_from_rate(known: bool, numerator: u32, denominator: u32) -> Duration {
    if known && numerator > 0 && denominator > 0 {
        Duration::from_secs_f64(f64::from(denominator) / f64::from(numerator))
    } else {
        Duration::from_nanos(16_666_667)
    }
}

/// How long the loop may sleep before the next frame: the time left of `interval` since the
/// last frame (zero when a frame is overdue).
#[must_use]
pub fn time_until_next_frame(now: Duration, last_frame: Duration, interval: Duration) -> Duration {
    (last_frame + interval).saturating_sub(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::MessageWindow;
    use windows::Win32::UI::WindowsAndMessaging::{MSG, PM_REMOVE, PeekMessageW, WM_TIMER};

    fn next_timer_message(owner: HWND, timeout: Duration) -> Option<usize> {
        let deadline = std::time::Instant::now() + timeout;
        let mut message = MSG::default();
        while std::time::Instant::now() < deadline {
            // SAFETY: peeks this thread's queue for WM_TIMER messages of the owner window.
            let found =
                unsafe { PeekMessageW(&mut message, Some(owner), WM_TIMER, WM_TIMER, PM_REMOVE) };
            if found.as_bool() {
                return Some(message.wParam.0);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    #[test]
    fn table_maps_ids_and_forgets_one_shots() {
        let mut table = TimerTable::default();
        let (a, b) = (TimerId::new(1), TimerId::new(2));
        let os_a = table.register(a, true);
        let os_b = table.register(b, false);
        assert_ne!(os_a, os_b);
        assert_eq!(table.register(a, true), os_a, "restarting keeps the OS id");
        assert_eq!(table.len(), 2);
        assert_eq!(
            table.fire(os_a),
            Some(Fired {
                id: a,
                one_shot: false
            })
        );
        assert_eq!(
            table.fire(os_a),
            Some(Fired {
                id: a,
                one_shot: false
            })
        );
        assert_eq!(
            table.fire(os_b),
            Some(Fired {
                id: b,
                one_shot: true
            })
        );
        assert_eq!(table.fire(os_b), None, "one-shot timers fire once");
        assert_eq!(table.fire(999), None);
        assert_eq!(table.unregister(a), Some(os_a));
        assert_eq!(table.unregister(a), None);
        assert!(table.is_empty());
        assert!(!table.take_frame_request());
    }

    #[test]
    fn intervals_are_clamped_to_the_system_range() {
        assert_eq!(
            clamp_interval_ms(Duration::from_millis(1)),
            USER_TIMER_MINIMUM
        );
        assert_eq!(clamp_interval_ms(Duration::from_millis(250)), 250);
        assert_eq!(
            clamp_interval_ms(Duration::from_secs(u64::MAX / 2000)),
            USER_TIMER_MAXIMUM
        );
    }

    #[test]
    fn timers_fire_repeat_and_stop() {
        let owner = MessageWindow::new().unwrap();
        let mut timers =
            WindowTimers::new(owner.hwnd(), Rc::new(RefCell::new(TimerTable::default())));
        let (repeating, once) = (TimerId::allocate(), TimerId::allocate());
        timers.set(repeating, Duration::from_millis(20), true);
        let os_id = next_timer_message(owner.hwnd(), Duration::from_secs(2)).expect("first tick");
        assert_eq!(
            timers.table().borrow_mut().fire(os_id).map(|f| f.id),
            Some(repeating)
        );
        assert!(
            next_timer_message(owner.hwnd(), Duration::from_secs(2)).is_some(),
            "repeats"
        );
        timers.kill(repeating);
        timers.kill(repeating);
        assert!(timers.table().borrow().is_empty());
        // Drain a tick that was already queued.
        let _ = next_timer_message(owner.hwnd(), Duration::from_millis(50));
        timers.set(once, Duration::from_millis(20), false);
        let os_id =
            next_timer_message(owner.hwnd(), Duration::from_secs(2)).expect("one-shot tick");
        let fired = timers.table().borrow_mut().fire(os_id).unwrap();
        assert!(fired.one_shot && fired.id == once);
        timers.request_animation_frame();
        assert!(timers.table().borrow_mut().take_frame_request());
        assert!(!timers.table().borrow_mut().take_frame_request());
    }

    #[test]
    fn frame_timing() {
        let interval = refresh_interval();
        assert!(interval > Duration::from_millis(2) && interval < Duration::from_millis(100));
        let ms = Duration::from_millis;
        assert_eq!(time_until_next_frame(ms(10), ms(0), ms(16)), ms(6));
        assert_eq!(time_until_next_frame(ms(20), ms(0), ms(16)), Duration::ZERO);
    }

    #[test]
    fn a_missing_or_unusable_refresh_rate_means_sixty_hertz() {
        let sixty = Duration::from_nanos(16_666_667);
        assert_eq!(
            interval_from_rate(true, 120, 1),
            Duration::from_secs_f64(1.0 / 120.0)
        );
        assert_eq!(
            interval_from_rate(true, 144_000, 1001),
            Duration::from_secs_f64(1001.0 / 144_000.0)
        );
        assert_eq!(interval_from_rate(false, 120, 1), sixty, "DWM failed");
        assert_eq!(interval_from_rate(true, 0, 1), sixty, "no numerator");
        assert_eq!(interval_from_rate(true, 60, 0), sixty, "no denominator");
    }
}
