//! Dimming state and transition logic.
//!
//! [`DimController`] owns all per-output dimming state and computes updates
//! atomically. Every update includes both alpha and brightness so they never
//! drift out of sync.
//!
//! # State model
//!
//! Each output has an [`OutputDimState`] tracking current alpha, brightness,
//! and whether it's skipped (has focus). The controller never touches
//! Wayland directly; it returns [`DimUpdates`] for the caller to apply.
//!
//! # Focus transitions
//!
//! When focus changes ([`focus_changed`]):
//!
//! 1. Old active output snaps immediately to dimmed (no delay)
//! 2. New active output starts a cross-fade after 325ms
//!
//! The delay lets the window settle on the new output before revealing it.
//! Without this pause, fast window movement causes distracting flicker as
//! the overlay races to catch up.
//!
//! # Animation
//!
//! - [`fade_in_frame`] — startup animation, all outputs together
//! - [`tick`] — cross-fade on the revealing output only
//! - [`reveal_output`] — choreographed reveal after `spawn_with` callback
//! - [`snap_to_target`] — instant jump, no animation
//! - [`snap_all_to_dimmed`] — emergency snap during window drag

use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use crate::render::Brightness;
use crate::render::Opacity;

/// Per-output dimming state.
///
/// This is the source of truth for "what should this output look like."
#[derive(Debug, Clone)]
pub struct OutputDimState {
    /// Current overlay alpha (0.0 = transparent, `target_opacity` = fully dimmed)
    pub alpha: f64,
    /// Current gamma brightness (1.0 = normal, `target_brightness` = dimmed)
    pub brightness: f64,
    /// Whether this output is skipped (has focus, should be undimmed)
    pub skipped: bool,
}

/// Updates to apply to surfaces after a dim state change.
#[derive(Debug, Default)]
pub struct DimUpdates(Vec<OutputUpdate>);

impl std::ops::Deref for DimUpdates {
    type Target = [OutputUpdate];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A single output's new dimming values.
#[derive(Debug, Clone)]
pub struct OutputUpdate {
    pub name: String,
    pub opacity: Opacity,
    pub brightness: Brightness,
}

/// Owns all dimming logic. Computes alpha AND gamma together.
pub struct DimController {
    target_opacity: f64,
    target_brightness: f64,

    /// Per-output dimming state
    outputs: HashMap<String, OutputDimState>,

    /// Currently active (focused) output name
    active: Option<String>,

    /// Ongoing cross-fade transition
    transition: Option<Transition>,
}

struct Transition {
    start: Instant,
    delay: Duration,
    duration: Duration,
    /// Output being revealed (overlay fading out, gamma returning to normal)
    revealing: String,
}

impl DimController {
    pub fn new(target_opacity: f64, target_brightness: Option<f64>) -> Self {
        Self {
            target_opacity,
            target_brightness: target_brightness.unwrap_or(1.0),
            outputs: HashMap::new(),
            active: None,
            transition: None,
        }
    }

    /// Register an output. Active outputs are skipped (left undimmed).
    pub fn add_output(&mut self, name: String, is_active: bool) {
        if is_active {
            self.active = Some(name.clone());
        }
        self.outputs.insert(
            name,
            OutputDimState {
                // During initial setup, all outputs start at 0 alpha (transparent overlay)
                // and 1.0 brightness (normal gamma). The fade-in will animate them.
                alpha: 0.0,
                brightness: 1.0,
                skipped: is_active,
            },
        );
    }

    /// Remove an output (e.g., when unplugged or surface closed).
    pub fn remove_output(&mut self, name: &str) {
        self.outputs.remove(name);
        if self.active.as_deref() == Some(name) {
            self.active = None;
        }
        if self
            .transition
            .as_ref()
            .map(|t| t.revealing.as_str())
            .is_some_and(|r| r == name)
        {
            self.transition = None;
        }
    }

    /// Get current state for an output.
    #[cfg(test)]
    pub fn get(&self, name: &str) -> Option<&OutputDimState> {
        self.outputs.get(name)
    }

    /// Check if an output is currently skipped.
    pub fn is_output_skipped(&self, name: &str) -> bool {
        self.outputs.get(name).is_some_and(|s| s.skipped)
    }

    /// Get the currently active output name.
    pub fn active_output(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Focus changed to a new output. Returns immediate updates and starts transition.
    pub fn focus_changed(&mut self, new_active: Option<String>) -> DimUpdates {
        if self.active == new_active {
            return DimUpdates::default();
        }

        let old_active = std::mem::replace(&mut self.active, new_active);

        let mut updates = Vec::new();

        // Immediately dim the old active output (snap to target)
        if let Some(ref name) = old_active {
            if let Some(state) = self.outputs.get_mut(name) {
                state.skipped = false;
                state.alpha = self.target_opacity;
                state.brightness = self.target_brightness;
                updates.push(OutputUpdate {
                    name: name.clone(),
                    opacity: Opacity::new(state.alpha),
                    brightness: Brightness::new(state.brightness),
                });
            }
        }

        // Mark the new active output as skipped and start transition
        if let Some(ref name) = self.active {
            if let Some(state) = self.outputs.get_mut(name) {
                state.skipped = true;
                // Don't update alpha/brightness yet — transition will animate them
            }
            self.transition = Some(Transition {
                start: Instant::now(),
                delay: Duration::from_millis(325),
                duration: Duration::from_millis(150),
                revealing: name.clone(),
            });
        }

        DimUpdates(updates)
    }

    /// Whether a transition is in progress.
    pub fn is_animating(&self) -> bool {
        self.transition.is_some()
    }

    /// Tick the cross-fade transition. Returns updates to apply.
    pub fn tick(&mut self) -> DimUpdates {
        let Some(transition) = &self.transition else {
            return DimUpdates::default();
        };

        let elapsed = transition.start.elapsed();
        if elapsed < transition.delay {
            return DimUpdates::default();
        }

        let fade_elapsed = elapsed.checked_sub(transition.delay).unwrap();
        let t = (fade_elapsed.as_secs_f64() / transition.duration.as_secs_f64()).min(1.0);
        let eased = ease_out_quad(t);

        let mut updates = Vec::new();

        // Animate the revealing output: alpha goes target→0, brightness goes target→1
        if let Some(state) = self.outputs.get_mut(&transition.revealing) {
            state.alpha = self.target_opacity * (1.0 - eased);
            state.brightness = self.target_brightness + (1.0 - self.target_brightness) * eased;
            updates.push(OutputUpdate {
                name: transition.revealing.clone(),
                opacity: Opacity::new(state.alpha),
                brightness: Brightness::new(state.brightness),
            });
        }

        if t >= 1.0 {
            self.transition = None;
        }

        DimUpdates(updates)
    }

    /// Compute a fade-in frame for the initial animation.
    ///
    /// Skipped outputs (active monitor) stay transparent.
    /// Non-skipped outputs fade from transparent to target opacity.
    pub fn fade_in_frame(&mut self, elapsed: Duration, duration: Duration) -> DimUpdates {
        let t = if duration.is_zero() {
            1.0
        } else {
            (elapsed.as_secs_f64() / duration.as_secs_f64()).min(1.0)
        };
        let eased = ease_out_quad(t);

        let mut updates = Vec::new();

        for (name, state) in &mut self.outputs {
            if state.skipped {
                // Active monitor stays transparent
                state.alpha = 0.0;
                state.brightness = 1.0;
            } else {
                // Other monitors fade to dimmed
                state.alpha = self.target_opacity * eased;
                state.brightness = 1.0 - (1.0 - self.target_brightness) * eased;
            }
            updates.push(OutputUpdate {
                name: name.clone(),
                opacity: Opacity::new(state.alpha),
                brightness: Brightness::new(state.brightness),
            });
        }

        DimUpdates(updates)
    }

    /// Snap all outputs to their final dimmed state (no animation).
    pub fn snap_to_target(&mut self) -> DimUpdates {
        let mut updates = Vec::new();

        for (name, state) in &mut self.outputs {
            if state.skipped {
                state.alpha = 0.0;
                state.brightness = 1.0;
            } else {
                state.alpha = self.target_opacity;
                state.brightness = self.target_brightness;
            }
            updates.push(OutputUpdate {
                name: name.clone(),
                opacity: Opacity::new(state.alpha),
                brightness: Brightness::new(state.brightness),
            });
        }

        DimUpdates(updates)
    }

    /// Get updates for a single output at its current state.
    pub fn current_update(&self, name: &str) -> Option<OutputUpdate> {
        self.outputs.get(name).map(|state| OutputUpdate {
            name: name.to_string(),
            opacity: Opacity::new(state.alpha),
            brightness: Brightness::new(state.brightness),
        })
    }

    /// Cancel the current transition.
    pub fn cancel_transition(&mut self) {
        self.transition = None;
    }

    /// Reveal a specific output after a choreographed launch.
    ///
    /// Marks the output as the active (skipped) output and starts a
    /// transition to fade its overlay from target opacity to transparent.
    /// After the reveal animation completes, normal focus tracking takes over.
    pub fn reveal_output(&mut self, name: &str) {
        self.active = Some(name.to_string());
        if let Some(state) = self.outputs.get_mut(name) {
            state.skipped = true;
        }
        self.transition = Some(Transition {
            start: Instant::now(),
            delay: Duration::from_millis(50),
            duration: Duration::from_millis(200),
            revealing: name.to_string(),
        });
    }

    /// Snap ALL outputs to dimmed state (overlays opaque).
    /// Used during window movement to prevent flash.
    pub fn snap_all_to_dimmed(&mut self) -> DimUpdates {
        let mut updates = Vec::new();

        for (name, state) in &mut self.outputs {
            state.skipped = false;
            state.alpha = self.target_opacity;
            state.brightness = self.target_brightness;
            updates.push(OutputUpdate {
                name: name.clone(),
                opacity: Opacity::new(state.alpha),
                brightness: Brightness::new(state.brightness),
            });
        }

        DimUpdates(updates)
    }
}

fn ease_out_quad(t: f64) -> f64 {
    1.0 - (1.0 - t) * (1.0 - t)
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // Exact equality is intentional in these tests
mod tests {
    use super::*;

    #[test]
    fn new_output_starts_at_zero_alpha() {
        let mut ctrl = DimController::new(0.8, Some(0.5));
        ctrl.add_output("DP-1".into(), false);

        let state = ctrl.get("DP-1").unwrap();
        assert_eq!(state.alpha, 0.0);
        assert_eq!(state.brightness, 1.0);
        assert!(!state.skipped);
    }

    #[test]
    fn active_output_is_skipped() {
        let mut ctrl = DimController::new(0.8, Some(0.5));
        ctrl.add_output("DP-1".into(), true);

        assert!(ctrl.is_output_skipped("DP-1"));
        assert_eq!(ctrl.active_output(), Some("DP-1"));
    }

    #[test]
    fn focus_change_dims_old_reveals_new() {
        let mut ctrl = DimController::new(0.8, Some(0.5));
        ctrl.add_output("DP-1".into(), true);
        ctrl.add_output("DP-2".into(), false);

        // Snap DP-2 to target (simulating post-fade-in state)
        ctrl.outputs.get_mut("DP-2").unwrap().alpha = 0.8;
        ctrl.outputs.get_mut("DP-2").unwrap().brightness = 0.5;

        let updates = ctrl.focus_changed(Some("DP-2".into()));

        // DP-1 should be immediately dimmed
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].name, "DP-1");
        assert_eq!(updates[0].opacity.as_f64(), 0.8);
        assert_eq!(updates[0].brightness.as_f64(), 0.5);

        // DP-2 should now be skipped, transition started
        assert!(ctrl.is_output_skipped("DP-2"));
        assert!(!ctrl.is_output_skipped("DP-1"));
        assert!(ctrl.is_animating());
    }

    #[test]
    fn tick_animates_revealing_output() {
        let mut ctrl = DimController::new(1.0, Some(0.5));
        ctrl.add_output("DP-1".into(), true);
        ctrl.add_output("DP-2".into(), false);

        // Set DP-2 to target state
        ctrl.outputs.get_mut("DP-2").unwrap().alpha = 1.0;
        ctrl.outputs.get_mut("DP-2").unwrap().brightness = 0.5;

        ctrl.focus_changed(Some("DP-2".into()));

        // Simulate time passing past the delay
        if let Some(ref mut t) = ctrl.transition {
            t.start = Instant::now()
                .checked_sub(Duration::from_millis(400))
                .unwrap();
        }

        let updates = ctrl.tick();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].name, "DP-2");
        // Opacity should be decreasing toward 0
        assert!(updates[0].opacity.as_f64() < 1.0);
        // Brightness should be increasing toward 1
        assert!(updates[0].brightness.as_f64() > 0.5);
    }

    #[test]
    fn fade_in_frame_respects_skipped() {
        let mut ctrl = DimController::new(0.8, Some(0.5));
        ctrl.add_output("DP-1".into(), true); // active, skipped
        ctrl.add_output("DP-2".into(), false); // not skipped

        let updates = ctrl.fade_in_frame(Duration::from_millis(500), Duration::from_secs(1));

        let dp1 = updates.iter().find(|u| u.name == "DP-1").unwrap();
        let dp2 = updates.iter().find(|u| u.name == "DP-2").unwrap();

        // DP-1 is skipped (active): stays transparent
        assert_eq!(dp1.opacity.as_f64(), 0.0);
        assert_eq!(dp1.brightness.as_f64(), 1.0);

        // DP-2 is not skipped: fading toward target
        assert!(dp2.opacity.as_f64() > 0.0);
        assert!(dp2.brightness.as_f64() < 1.0);
    }

    #[test]
    fn remove_output_clears_transition() {
        let mut ctrl = DimController::new(1.0, Some(0.5));
        ctrl.add_output("DP-1".into(), true);
        ctrl.add_output("DP-2".into(), false);

        ctrl.focus_changed(Some("DP-2".into()));
        assert!(ctrl.is_animating());

        ctrl.remove_output("DP-2");
        assert!(!ctrl.is_animating());
    }

    #[test]
    fn ease_out_quad_values() {
        assert!((ease_out_quad(0.0)).abs() < f64::EPSILON);
        assert!((ease_out_quad(1.0) - 1.0).abs() < f64::EPSILON);
        assert!((ease_out_quad(0.5) - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn snap_to_target_sets_final_values() {
        let mut ctrl = DimController::new(0.8, Some(0.4));
        ctrl.add_output("DP-1".into(), true);
        ctrl.add_output("DP-2".into(), false);

        let updates = ctrl.snap_to_target();

        let dp1 = updates.iter().find(|u| u.name == "DP-1").unwrap();
        let dp2 = updates.iter().find(|u| u.name == "DP-2").unwrap();

        assert_eq!(dp1.opacity.as_f64(), 0.0);
        assert_eq!(dp1.brightness.as_f64(), 1.0);
        assert_eq!(dp2.opacity.as_f64(), 0.8);
        assert_eq!(dp2.brightness.as_f64(), 0.4);
    }

    #[test]
    fn reveal_output_starts_transition() {
        let mut ctrl = DimController::new(0.8, Some(0.5));
        ctrl.add_output("DP-1".into(), false);
        ctrl.add_output("DP-2".into(), false);

        // Simulate post-fade-in state: both at target
        ctrl.outputs.get_mut("DP-1").unwrap().alpha = 0.8;
        ctrl.outputs.get_mut("DP-1").unwrap().brightness = 0.5;
        ctrl.outputs.get_mut("DP-2").unwrap().alpha = 0.8;
        ctrl.outputs.get_mut("DP-2").unwrap().brightness = 0.5;

        ctrl.reveal_output("DP-1");

        assert!(ctrl.is_animating());
        assert!(ctrl.is_output_skipped("DP-1"));
        assert_eq!(ctrl.active_output(), Some("DP-1"));
    }
}
