//! The caret's rhythm.
//!
//! A caret that snaps between on and off reads as a flicker: at two pixels wide
//! the eye catches the switch rather than the mark. This one holds solid while
//! you are working, then fades out and back — and, because the fade is the only
//! part that needs a fine clock, it also says when it next wants asking.

use std::time::Duration;

/// Solid, then a fade out, a beat of dark, and a fade back. In phase order from
/// the moment the caret last moved or typed.
const SOLID: Duration = Duration::from_millis(620);
const FADE: Duration = Duration::from_millis(280);
const DARK: Duration = Duration::from_millis(240);

/// How often opacity is resampled while it is actually moving.
const FADE_STEP: Duration = Duration::from_millis(16);

/// The whole cycle.
pub fn cycle() -> Duration {
    SOLID + FADE + DARK + FADE
}

/// The caret's opacity this far into its rhythm, and how long until it is worth
/// asking again.
///
/// Returning the delay is what keeps this cheap: during the solid and dark
/// phases the answer holds for the rest of the phase, so the caller sleeps
/// through them instead of waking sixty times a second to learn nothing.
pub fn phase(elapsed: Duration) -> (f32, Duration) {
    let solid = SOLID.as_secs_f32();
    let fade = FADE.as_secs_f32();
    let dark = DARK.as_secs_f32();
    // Smooth at both ends, so neither the departure nor the return has a corner.
    let ease = |x: f32| x * x * (3. - 2. * x);

    // Fold into the cycle in integer nanoseconds first. An `f32` of seconds
    // loses its millisecond resolution after a few days, and this app hides
    // rather than quits: left alone for a week, the fade would stop moving.
    let t = Duration::from_nanos((elapsed.as_nanos() % cycle().as_nanos()) as u64).as_secs_f32();
    let remaining = |until: f32| Duration::from_secs_f32((until - t).max(0.001));

    if t < solid {
        (1.0, remaining(solid))
    } else if t < solid + fade {
        (1.0 - ease((t - solid) / fade), FADE_STEP)
    } else if t < solid + fade + dark {
        (0.0, remaining(solid + fade + dark))
    } else {
        (ease((t - solid - fade - dark) / fade), FADE_STEP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(ms: u64) -> f32 {
        phase(Duration::from_millis(ms)).0
    }

    #[test]
    fn it_starts_solid_and_stays_solid_while_you_type() {
        assert_eq!(alpha(0), 1.0);
        assert_eq!(alpha(300), 1.0);
        assert_eq!(alpha(619), 1.0);
    }

    #[test]
    fn it_reaches_dark_and_comes_back() {
        assert_eq!(alpha(620 + 280), 0.0);
        assert_eq!(alpha(620 + 280 + 239), 0.0);
        // A full cycle later it is solid again.
        assert_eq!(alpha(cycle().as_millis() as u64), 1.0);
    }

    #[test]
    fn each_fade_is_monotone_and_never_leaves_its_bounds() {
        let mut last = 1.0;
        for ms in 620..=900 {
            let a = alpha(ms);
            assert!((0.0..=1.0).contains(&a), "alpha {a} at {ms}ms");
            assert!(a <= last + 1e-6, "fade out went back up at {ms}ms");
            last = a;
        }
        let mut last = 0.0;
        for ms in 1140..=1420 {
            let a = alpha(ms);
            assert!((0.0..=1.0).contains(&a), "alpha {a} at {ms}ms");
            assert!(a >= last - 1e-6, "fade in went back down at {ms}ms");
            last = a;
        }
    }

    /// The point of returning a delay: the still phases must not ask for a
    /// 16 ms wake-up, or the caret costs 60 Hz of CPU to sit there.
    #[test]
    fn the_still_phases_sleep_and_only_the_fades_step() {
        assert!(phase(Duration::from_millis(0)).1 >= Duration::from_millis(600));
        assert!(phase(Duration::from_millis(1000)).1 >= Duration::from_millis(100));
        assert_eq!(phase(Duration::from_millis(700)).1, FADE_STEP);
        assert_eq!(phase(Duration::from_millis(1300)).1, FADE_STEP);
    }

    /// This app hides rather than quits, so it can sit focused and untouched
    /// for days. The rhythm has to keep its millisecond resolution that far out
    /// — computing in `f32` seconds does not.
    #[test]
    fn the_rhythm_survives_a_week_of_uptime() {
        let week = Duration::from_secs(7 * 24 * 60 * 60);
        let cycle_ms = cycle().as_millis() as u64;
        // Aligned to a cycle boundary, a week later, it must still be solid...
        let aligned = Duration::from_millis((week.as_millis() as u64 / cycle_ms) * cycle_ms);
        assert_eq!(phase(aligned).0, 1.0);
        // ...and still be moving through the fade a fifth of a cycle later.
        let a = phase(aligned + Duration::from_millis(700)).0;
        let b = phase(aligned + Duration::from_millis(760)).0;
        assert!(a > b, "the fade stopped advancing after a week: {a} then {b}");
    }

    /// Whatever the phase, the next wake-up must land inside the cycle — a zero
    /// or negative delay would spin the loop.
    #[test]
    fn the_delay_is_always_positive_and_bounded() {
        for ms in 0..(cycle().as_millis() as u64 * 2) {
            let (_, delay) = phase(Duration::from_millis(ms));
            assert!(delay > Duration::ZERO, "zero delay at {ms}ms");
            assert!(delay <= cycle(), "delay {delay:?} past the cycle at {ms}ms");
        }
    }
}
