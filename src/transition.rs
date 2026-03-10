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

pub(crate) fn ease_out_quad(t: f64) -> f64 {
    1.0 - (1.0 - t) * (1.0 - t)
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

        // Hold overlay opaque while the window settles
        if elapsed < transition.delay {
            return true;
        }

        let fade_elapsed = elapsed - transition.delay;
        let t = (fade_elapsed.as_secs_f64() / transition.duration.as_secs_f64()).min(1.0);
        let eased = ease_out_quad(t);
        let target_alpha = self.target_opacity * 255.0;

        let revealing = self.transition.as_ref().unwrap().revealing.clone();
        let done = t >= 1.0;

        // Only the newly active monitor's overlay fades — opaque → transparent
        for idx in 0..self.surfaces.len() {
            if self.surfaces[idx].is_backdrop() {
                continue;
            }
            let name = self.surfaces[idx].output_name.as_deref();
            if name.is_some() && name == revealing.as_deref() {
                let alpha = ((1.0 - eased) * target_alpha) as u8;
                self.draw_surface_alpha(idx, alpha);
                break;
            }
        }

        if done {
            self.transition = None;
        }

        !done
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
}
