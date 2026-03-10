use std::time::Duration;
use std::time::Instant;

use crate::state::ZenState;

/// Fade-out transition on the newly active output.
pub(crate) struct Transition {
    pub(crate) start: Instant,
    /// Wait for the window to settle before fading
    pub(crate) delay: Duration,
    pub(crate) duration: Duration,
    /// Output being revealed (becoming active, overlay fading OUT)
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

pub(crate) fn ease_out_quad(t: f64) -> f64 {
    1.0 - (1.0 - t) * (1.0 - t)
}

impl Transition {
    /// Pure computation: given elapsed time and target opacity, return what
    /// the caller should do (wait, draw at alpha, or finish).
    pub(crate) fn tick(&self, elapsed: Duration, target_opacity: f64) -> TransitionTick {
        if elapsed < self.delay {
            return TransitionTick::Waiting;
        }

        let fade_elapsed = elapsed - self.delay;
        let t = (fade_elapsed.as_secs_f64() / self.duration.as_secs_f64()).min(1.0);
        let eased = ease_out_quad(t);
        let alpha = ((1.0 - eased) * target_opacity * 255.0) as u8;

        if t >= 1.0 {
            TransitionTick::Done { alpha }
        } else {
            TransitionTick::Fading { alpha }
        }
    }
}

impl ZenState {
    /// Returns true if a cross-fade transition is in progress.
    pub(crate) fn is_transitioning(&self) -> bool {
        self.transition.is_some()
    }

    /// Tick the cross-fade transition. Returns true if still animating.
    pub(crate) fn tick_transition(&mut self) -> bool {
        let transition = match &self.transition {
            Some(t) => t,
            None => return false,
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
        let steps: Vec<f64> = (0..=100).map(|i| i as f64 / 100.0).collect();
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
        // eased = 0.75, alpha = (1-0.75) * 1.0 * 255 = 63
        let tick = t.tick(Duration::from_millis(200), 1.0);
        match tick {
            TransitionTick::Fading { alpha } => assert_eq!(alpha, 63),
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
        // At start of fade (t=0), alpha = (1-0) * 0.5 * 255 = 127
        let tick = t.tick(Duration::from_millis(100), 0.5);
        match tick {
            TransitionTick::Fading { alpha } => assert_eq!(alpha, 127),
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
}
