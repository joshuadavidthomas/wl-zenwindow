use std::time::Duration;
use std::time::Instant;

use crate::state::opacity_to_alpha;
use crate::state::ZenState;

/// Convert an eased opacity fraction (0.0–1.0) to a `u32` alpha multiplier
/// for the `wp_alpha_modifier_v1` protocol (0–`u32::MAX`).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn opacity_to_multiplier(opacity: f64) -> u32 {
    // After clamping to [0, u32::MAX], the cast is lossless.
    (opacity * f64::from(u32::MAX))
        .round()
        .clamp(0.0, f64::from(u32::MAX)) as u32
}

/// Fade-out transition on the newly active output.
pub(crate) struct Transition {
    /// When this transition started.
    pub(crate) start: Instant,
    /// Wait for the window to settle before fading.
    pub(crate) delay: Duration,
    /// How long the fade animation takes.
    pub(crate) duration: Duration,
    /// Output being revealed (becoming active, overlay fading OUT).
    pub(crate) revealing: Option<String>,
}

/// Result of a single transition tick.
#[derive(Debug)]
pub(crate) enum TransitionTick {
    /// Still waiting for the window to settle before fading.
    Waiting,
    /// Actively fading — the overlay on the revealing output should be
    /// drawn at this alpha value.
    Fading { alpha: u8 },
    /// The fade completed this tick — draw final alpha and clean up.
    Done { alpha: u8 },
}

/// Quadratic ease-out: fast start, decelerating finish.
///
/// Maps `t` in `[0.0, 1.0]` to an eased value in the same range.
/// Formula: `1 - (1 - t)²`.
pub(crate) fn ease_out_quad(t: f64) -> f64 {
    1.0 - (1.0 - t) * (1.0 - t)
}

/// Initial fade-in animation parameters.
///
/// Computes per-frame alpha and brightness values from elapsed time,
/// without touching any Wayland state.
pub(crate) struct FadeIn {
    /// Total duration of the fade-in animation.
    pub(crate) duration: Duration,
    /// Target overlay opacity (0.0–1.0).
    pub(crate) target_opacity: f64,
    /// Target monitor brightness (1.0 = full, lower = dimmer).
    pub(crate) target_brightness: f64,
}

/// Values for a single frame of the fade-in animation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FadeFrame {
    /// Alpha as a u8 (0–255) for the software rendering path.
    pub(crate) alpha: u8,
    /// Alpha as a u32 multiplier (`0–u32::MAX`) for the alpha-modifier protocol.
    pub(crate) multiplier: u32,
    /// Current brightness (1.0 = full, interpolated toward target).
    pub(crate) brightness: f64,
    /// Whether the animation is complete (t ≥ 1.0).
    pub(crate) done: bool,
}

/// Frame computation.
impl FadeIn {
    /// Compute the frame values at a given elapsed time.
    pub(crate) fn frame_at(&self, elapsed: Duration) -> FadeFrame {
        let t = (elapsed.as_secs_f64() / self.duration.as_secs_f64()).min(1.0);
        let eased = ease_out_quad(t);

        let alpha = opacity_to_alpha(eased * self.target_opacity);
        let multiplier = opacity_to_multiplier(eased * self.target_opacity);
        let brightness = 1.0 - eased * (1.0 - self.target_brightness);

        FadeFrame {
            alpha,
            multiplier,
            brightness,
            done: t >= 1.0,
        }
    }
}

/// Tick computation.
impl Transition {
    /// Pure computation: given elapsed time and target opacity, return what
    /// the caller should do (wait, draw at alpha, or finish).
    pub(crate) fn tick(&self, elapsed: Duration, target_opacity: f64) -> TransitionTick {
        if elapsed < self.delay {
            return TransitionTick::Waiting;
        }

        let fade_elapsed = elapsed.checked_sub(self.delay).unwrap();
        let t = (fade_elapsed.as_secs_f64() / self.duration.as_secs_f64()).min(1.0);
        let eased = ease_out_quad(t);
        let alpha = opacity_to_alpha((1.0 - eased) * target_opacity);

        if t >= 1.0 {
            TransitionTick::Done { alpha }
        } else {
            TransitionTick::Fading { alpha }
        }
    }
}

/// Cross-fade transition management.
impl ZenState {
    /// Returns true if a cross-fade transition is in progress.
    pub(crate) fn is_transitioning(&self) -> bool {
        self.transition.is_some()
    }

    /// Tick the cross-fade transition. Returns true if still animating.
    pub(crate) fn tick_transition(&mut self) -> bool {
        let Some(transition) = &self.transition else {
            return false;
        };

        let elapsed = transition.start.elapsed();
        let tick = transition.tick(elapsed, self.target_opacity);
        let revealing = transition.revealing.clone();

        match tick {
            TransitionTick::Waiting => true,
            TransitionTick::Fading { alpha } | TransitionTick::Done { alpha } => {
                // Only the newly active monitor's overlay fades — opaque → transparent
                for idx in 0..self.surfaces.len() {
                    if self.surfaces[idx].is_backdrop() {
                        continue;
                    }
                    let name = self.surfaces[idx].output_name.as_deref();
                    if name.is_some() && name == revealing.as_deref() {
                        self.draw_surface_alpha(idx, alpha);
                        break;
                    }
                }

                let done = matches!(tick, TransitionTick::Done { .. });
                if done {
                    self.transition = None;
                }
                !done
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_quad_at_zero() {
        assert!((ease_out_quad(0.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn ease_out_quad_at_one() {
        assert!((ease_out_quad(1.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn ease_out_quad_at_half() {
        // 1 - (1 - 0.5)^2 = 1 - 0.25 = 0.75
        assert!((ease_out_quad(0.5) - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn ease_out_quad_at_quarter() {
        // 1 - (1 - 0.25)^2 = 1 - 0.5625 = 0.4375
        assert!((ease_out_quad(0.25) - 0.4375).abs() < f64::EPSILON);
    }

    #[test]
    fn ease_out_quad_is_monotonic() {
        let steps: Vec<f64> = (0..=100).map(|i| f64::from(i) / 100.0).collect();
        for pair in steps.windows(2) {
            assert!(
                ease_out_quad(pair[1]) >= ease_out_quad(pair[0]),
                "ease_out_quad({}) < ease_out_quad({})",
                pair[1],
                pair[0],
            );
        }
    }

    fn test_transition() -> Transition {
        Transition {
            start: Instant::now(),
            delay: Duration::from_millis(100),
            duration: Duration::from_millis(200),
            revealing: Some("DP-1".to_string()),
        }
    }

    #[test]
    fn tick_during_delay_returns_waiting() {
        let t = test_transition();
        let tick = t.tick(Duration::from_millis(50), 1.0);
        assert!(matches!(tick, TransitionTick::Waiting));
    }

    #[test]
    fn tick_at_exact_delay_boundary_returns_fading() {
        let t = test_transition();
        // elapsed == delay means fade_elapsed == 0, t == 0, alpha == 255
        let tick = t.tick(Duration::from_millis(100), 1.0);
        assert!(matches!(tick, TransitionTick::Fading { alpha: 255 }));
    }

    #[test]
    fn tick_midway_through_fade() {
        let t = test_transition();
        // delay=100ms, duration=200ms, elapsed=200ms → fade_elapsed=100ms → t=0.5
        // eased = 0.75, alpha = round((1-0.75) * 1.0 * 255) = round(63.75) = 64
        let tick = t.tick(Duration::from_millis(200), 1.0);
        match tick {
            TransitionTick::Fading { alpha } => assert_eq!(alpha, 64),
            other => panic!("expected Fading, got {other:?}"),
        }
    }

    #[test]
    fn tick_at_end_returns_done() {
        let t = test_transition();
        // delay=100ms, duration=200ms, elapsed=300ms → fade_elapsed=200ms → t=1.0
        // eased = 1.0, alpha = (1-1) * 255 = 0
        let tick = t.tick(Duration::from_millis(300), 1.0);
        assert!(matches!(tick, TransitionTick::Done { alpha: 0 }));
    }

    #[test]
    fn tick_past_end_returns_done() {
        let t = test_transition();
        let tick = t.tick(Duration::from_millis(500), 1.0);
        assert!(matches!(tick, TransitionTick::Done { alpha: 0 }));
    }

    #[test]
    fn tick_respects_target_opacity() {
        let t = test_transition();
        // At start of fade (t=0), alpha = round((1-0) * 0.5 * 255) = round(127.5) = 128
        let tick = t.tick(Duration::from_millis(100), 0.5);
        match tick {
            TransitionTick::Fading { alpha } => assert_eq!(alpha, 128),
            other => panic!("expected Fading, got {other:?}"),
        }
    }

    #[test]
    fn tick_done_alpha_is_zero_regardless_of_opacity() {
        let t = test_transition();
        let tick = t.tick(Duration::from_millis(300), 0.5);
        assert!(matches!(tick, TransitionTick::Done { alpha: 0 }));
    }

    #[test]
    fn tick_alpha_decreases_over_time() {
        let t = test_transition();
        let mut prev_alpha = 255u8;
        // Sample at 10ms intervals through the fade (100ms..300ms)
        for ms in (100..=300).step_by(10) {
            let tick = t.tick(Duration::from_millis(ms), 1.0);
            let alpha = match tick {
                TransitionTick::Fading { alpha } | TransitionTick::Done { alpha } => alpha,
                TransitionTick::Waiting => panic!("unexpected Waiting at {ms}ms"),
            };
            assert!(
                alpha <= prev_alpha,
                "alpha increased at {ms}ms: {alpha} > {prev_alpha}"
            );
            prev_alpha = alpha;
        }
        assert_eq!(prev_alpha, 0);
    }

    #[test]
    fn tick_zero_delay_starts_fading_immediately() {
        let t = Transition {
            start: Instant::now(),
            delay: Duration::ZERO,
            duration: Duration::from_millis(200),
            revealing: Some("eDP-1".to_string()),
        };
        let tick = t.tick(Duration::ZERO, 1.0);
        assert!(matches!(tick, TransitionTick::Fading { alpha: 255 }));
    }

    fn test_fade_in() -> FadeIn {
        FadeIn {
            duration: Duration::from_millis(500),
            target_opacity: 1.0,
            target_brightness: 1.0,
        }
    }

    #[test]
    fn fade_in_at_zero_all_transparent() {
        let fade = test_fade_in();
        let frame = fade.frame_at(Duration::ZERO);
        assert_eq!(frame.alpha, 0);
        assert_eq!(frame.multiplier, 0);
        assert!((frame.brightness - 1.0).abs() < f64::EPSILON);
        assert!(!frame.done);
    }

    #[test]
    fn fade_in_at_end_fully_opaque() {
        let fade = test_fade_in();
        let frame = fade.frame_at(Duration::from_millis(500));
        assert_eq!(frame.alpha, 255);
        assert_eq!(frame.multiplier, u32::MAX);
        assert!((frame.brightness - 1.0).abs() < f64::EPSILON);
        assert!(frame.done);
    }

    #[test]
    fn fade_in_past_end_clamps() {
        let fade = test_fade_in();
        let frame = fade.frame_at(Duration::from_secs(5));
        assert_eq!(frame.alpha, 255);
        assert!(frame.done);
    }

    #[test]
    fn fade_in_midway() {
        let fade = test_fade_in();
        // t=0.5, eased=0.75
        let frame = fade.frame_at(Duration::from_millis(250));
        // alpha = 0.75 * 1.0 * 255 = 191
        assert_eq!(frame.alpha, 191);
        assert!(!frame.done);
    }

    #[test]
    fn fade_in_respects_target_opacity() {
        let fade = FadeIn {
            duration: Duration::from_millis(500),
            target_opacity: 0.5,
            target_brightness: 1.0,
        };
        // At end: alpha = round(1.0 * 0.5 * 255) = round(127.5) = 128
        let frame = fade.frame_at(Duration::from_millis(500));
        assert_eq!(frame.alpha, 128);
        assert!(frame.done);
    }

    #[test]
    fn fade_in_multiplier_respects_opacity() {
        let fade = FadeIn {
            duration: Duration::from_millis(500),
            target_opacity: 0.5,
            target_brightness: 1.0,
        };
        // At end: multiplier = 1.0 * 0.5 * u32::MAX
        let frame = fade.frame_at(Duration::from_millis(500));
        let expected = opacity_to_multiplier(0.5);
        assert_eq!(frame.multiplier, expected);
    }

    #[test]
    fn fade_in_brightness_interpolation() {
        let fade = FadeIn {
            duration: Duration::from_millis(500),
            target_opacity: 1.0,
            target_brightness: 0.4,
        };
        // At t=0: brightness = 1.0 (no dimming yet)
        let frame0 = fade.frame_at(Duration::ZERO);
        assert!((frame0.brightness - 1.0).abs() < f64::EPSILON);

        // At t=1: brightness = 1.0 - 1.0 * (1.0 - 0.4) = 0.4
        let frame_end = fade.frame_at(Duration::from_millis(500));
        assert!((frame_end.brightness - 0.4).abs() < f64::EPSILON);

        // At t=0.5 (eased=0.75): brightness = 1.0 - 0.75 * 0.6 = 0.55
        let frame_mid = fade.frame_at(Duration::from_millis(250));
        assert!((frame_mid.brightness - 0.55).abs() < f64::EPSILON);
    }

    #[test]
    fn fade_in_alpha_increases_over_time() {
        let fade = test_fade_in();
        let mut prev_alpha = 0u8;
        for ms in (0..=500).step_by(10) {
            let frame = fade.frame_at(Duration::from_millis(ms));
            assert!(
                frame.alpha >= prev_alpha,
                "alpha decreased at {ms}ms: {} < {prev_alpha}",
                frame.alpha,
            );
            prev_alpha = frame.alpha;
        }
        assert_eq!(prev_alpha, 255);
    }

    #[test]
    fn fade_in_brightness_decreases_toward_target() {
        let fade = FadeIn {
            duration: Duration::from_millis(500),
            target_opacity: 1.0,
            target_brightness: 0.3,
        };
        let mut prev_brightness = 1.0f64;
        for ms in (0..=500).step_by(10) {
            let frame = fade.frame_at(Duration::from_millis(ms));
            assert!(
                frame.brightness <= prev_brightness + f64::EPSILON,
                "brightness increased at {ms}ms: {} > {prev_brightness}",
                frame.brightness,
            );
            prev_brightness = frame.brightness;
        }
        assert!((prev_brightness - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn fade_in_full_brightness_stays_at_one() {
        let fade = test_fade_in();
        for ms in (0..=500).step_by(50) {
            let frame = fade.frame_at(Duration::from_millis(ms));
            assert!(
                (frame.brightness - 1.0).abs() < f64::EPSILON,
                "brightness != 1.0 at {ms}ms: {}",
                frame.brightness,
            );
        }
    }
}
