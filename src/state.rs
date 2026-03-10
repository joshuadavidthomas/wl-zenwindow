use std::collections::HashSet;

use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::shell::wlr_layer::LayerShell;
use smithay_client_toolkit::shell::wlr_layer::LayerSurface;
use smithay_client_toolkit::shm::slot::Buffer;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::Shm;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_v1::WpAlphaModifierV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1;
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_v1::ZwlrGammaControlV1;

use crate::toplevel::TrackedToplevel;
use crate::transition::Transition;

/// Convert an opacity value (0.0–1.0) to a premultiplied alpha byte (0–255).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn opacity_to_alpha(opacity: f64) -> u8 {
    // After clamping to [0, 255], the cast is lossless.
    (opacity * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Distinguishes the two kinds of surfaces created per output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurfaceRole {
    /// `Layer::Overlay` — participates in transitions, skip logic, and all
    /// optional protocol features (viewport, alpha surface, gamma control).
    Overlay,
    /// `Layer::Bottom` backdrop — always opaque, never transitions.
    /// Prevents desktop flash when the compositor renders a frame
    /// before we receive foreign-toplevel events.
    Backdrop,
}

/// Whether a surface has been configured by the compositor with its dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurfaceConfig {
    /// Waiting for the compositor's initial configure event.
    Pending,
    /// Configured with the given dimensions.
    Ready { width: u32, height: u32 },
}

/// Dimension accessors.
impl SurfaceConfig {
    /// Returns dimensions if configured with non-zero size, `None` otherwise.
    pub(crate) fn dimensions(&self) -> Option<(u32, u32)> {
        match self {
            SurfaceConfig::Ready { width, height } if *width > 0 && *height > 0 => {
                Some((*width, *height))
            }
            _ => None,
        }
    }
}

/// Per-surface gamma control lifecycle.
#[derive(Debug)]
pub(crate) enum GammaState {
    /// No gamma control for this surface (not requested or protocol unavailable).
    Unavailable,
    /// Control bound, waiting for the compositor's `GammaSize` event.
    Pending(ZwlrGammaControlV1),
    /// Ready to set gamma ramps.
    Ready {
        /// The gamma control protocol handle.
        control: ZwlrGammaControlV1,
        /// Number of entries per channel in the gamma ramp.
        size: u32,
    },
}

/// A layer-shell surface placed on a single output.
///
/// Each output gets up to two of these: an [`SurfaceRole::Overlay`] that
/// participates in transitions and skip logic, and a [`SurfaceRole::Backdrop`]
/// that prevents desktop flash during the gap before focus events arrive.
pub(crate) struct OverlaySurface {
    /// Wayland name of the output this surface is on (e.g. `"DP-1"`).
    pub(crate) output_name: Option<String>,
    /// Whether this is an overlay or a backdrop surface.
    pub(crate) role: SurfaceRole,
    /// The layer-shell surface handle.
    pub(crate) layer: LayerSurface,
    /// Viewporter handle for efficient 1-pixel rendering, if available.
    pub(crate) viewport: Option<WpViewport>,
    /// Alpha modifier handle for hardware-composited alpha, if available.
    pub(crate) alpha_surface: Option<WpAlphaModifierSurfaceV1>,
    /// Gamma control state for brightness dimming on this output.
    pub(crate) gamma: GammaState,
    /// The most recently attached buffer, kept alive to prevent use-after-free.
    pub(crate) buffer: Option<Buffer>,
    /// Whether and how the compositor has configured this surface.
    pub(crate) config: SurfaceConfig,
}

/// Role queries.
impl OverlaySurface {
    /// Returns `true` if this is a backdrop surface (always opaque, never transitions).
    pub(crate) fn is_backdrop(&self) -> bool {
        self.role == SurfaceRole::Backdrop
    }
}

/// Which phase of the event loop lifecycle we're in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopPhase {
    /// Initial fade-in animation is in progress.
    FadingIn,
    /// Steady state — overlays are up, handling focus transitions.
    Running,
    /// Shutting down — exit the event loop.
    ShuttingDown,
}

/// Central runtime state for the Wayland event loop.
///
/// Holds all protocol objects, surface state, configuration, and focus
/// tracking data. Created once in [`run()`](crate::run::run) and passed
/// through the event loop as the `Dispatch` target.
pub(crate) struct ZenState {
    // Wayland protocol state (SCTK)
    /// Registry for global object discovery.
    pub(crate) registry: RegistryState,
    /// Tracks connected outputs and their properties.
    pub(crate) output_state: OutputState,
    /// Creates `wl_surface` objects.
    pub(crate) compositor: CompositorState,
    /// Creates layer-shell surfaces on outputs.
    pub(crate) layer_shell: LayerShell,
    /// Viewporter for efficient 1-pixel rendering. `None` if unsupported.
    pub(crate) viewporter: Option<WpViewporter>,
    /// Alpha modifier for hardware alpha compositing. `None` if unsupported.
    pub(crate) alpha_modifier: Option<WpAlphaModifierV1>,
    /// Gamma control manager for brightness dimming. `None` if unsupported.
    pub(crate) gamma_manager: Option<ZwlrGammaControlManagerV1>,
    /// Shared memory for buffer allocation.
    pub(crate) shm: Shm,
    /// Buffer pool for allocating pixel data.
    pub(crate) pool: SlotPool,

    // Surface state
    /// All overlay and backdrop surfaces, two per output.
    pub(crate) surfaces: Vec<OverlaySurface>,
    /// Current phase of the event loop lifecycle.
    pub(crate) phase: LoopPhase,
    /// Target alpha for dimmed overlays (0.0–1.0).
    pub(crate) target_opacity: f64,
    /// RGB overlay color.
    pub(crate) color: [u8; 3],

    // Skip logic
    /// Output names to always skip (never dim).
    pub(crate) skip_names: HashSet<String>,
    /// Whether to skip the output with the focused window.
    pub(crate) skip_active: bool,
    /// Wayland name of the currently focused output, if known.
    pub(crate) active_output: Option<String>,

    // Transitions and focus tracking
    /// In-progress cross-fade transition, if any.
    pub(crate) transition: Option<Transition>,
    /// Foreign toplevel manager handle. `None` if unsupported or finished.
    pub(crate) toplevel_manager: Option<ZwlrForeignToplevelManagerV1>,
    /// All tracked toplevel windows.
    pub(crate) toplevels: Vec<TrackedToplevel>,
}

/// Whether a surface should be skipped (left transparent).
///
/// Backdrops are never skipped — they're always opaque.
/// An overlay is skipped when its output is in `skip_names`,
/// or when `skip_active` is true and its output is the active one.
pub(crate) fn should_skip(
    role: SurfaceRole,
    output_name: Option<&str>,
    skip_names: &HashSet<String>,
    skip_active: bool,
    active_output: Option<&str>,
) -> bool {
    if role == SurfaceRole::Backdrop {
        return false;
    }
    if let Some(name) = output_name {
        if skip_names.contains(name) {
            return true;
        }
        if skip_active {
            if let Some(active) = active_output {
                return name == active;
            }
        }
    }
    false
}

/// Skip-logic queries.
impl ZenState {
    /// Whether a surface should be skipped (transparent).
    pub(crate) fn is_skipped(&self, idx: usize) -> bool {
        let surface = &self.surfaces[idx];
        should_skip(
            surface.role,
            surface.output_name.as_deref(),
            &self.skip_names,
            self.skip_active,
            self.active_output.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_config_pending_has_no_dimensions() {
        assert_eq!(SurfaceConfig::Pending.dimensions(), None);
    }

    #[test]
    fn surface_config_ready_returns_dimensions() {
        let config = SurfaceConfig::Ready {
            width: 1920,
            height: 1080,
        };
        assert_eq!(config.dimensions(), Some((1920, 1080)));
    }

    #[test]
    fn surface_config_ready_zero_width_returns_none() {
        let config = SurfaceConfig::Ready {
            width: 0,
            height: 1080,
        };
        assert_eq!(config.dimensions(), None);
    }

    #[test]
    fn surface_config_ready_zero_height_returns_none() {
        let config = SurfaceConfig::Ready {
            width: 1920,
            height: 0,
        };
        assert_eq!(config.dimensions(), None);
    }

    #[test]
    fn surface_config_ready_both_zero_returns_none() {
        let config = SurfaceConfig::Ready {
            width: 0,
            height: 0,
        };
        assert_eq!(config.dimensions(), None);
    }

    fn skip_names(names: &[&str]) -> HashSet<String> {
        names.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn backdrop_never_skipped() {
        assert!(!should_skip(
            SurfaceRole::Backdrop,
            Some("DP-1"),
            &skip_names(&["DP-1"]),
            true,
            Some("DP-1"),
        ));
    }

    #[test]
    fn overlay_skipped_by_name() {
        assert!(should_skip(
            SurfaceRole::Overlay,
            Some("DP-1"),
            &skip_names(&["DP-1"]),
            false,
            None,
        ));
    }

    #[test]
    fn overlay_not_skipped_when_name_not_in_set() {
        assert!(!should_skip(
            SurfaceRole::Overlay,
            Some("DP-1"),
            &skip_names(&["HDMI-1"]),
            false,
            None,
        ));
    }

    #[test]
    fn overlay_skipped_when_active() {
        assert!(should_skip(
            SurfaceRole::Overlay,
            Some("DP-1"),
            &HashSet::new(),
            true,
            Some("DP-1"),
        ));
    }

    #[test]
    fn overlay_not_skipped_when_skip_active_false() {
        assert!(!should_skip(
            SurfaceRole::Overlay,
            Some("DP-1"),
            &HashSet::new(),
            false,
            Some("DP-1"),
        ));
    }

    #[test]
    fn overlay_not_skipped_when_different_output_active() {
        assert!(!should_skip(
            SurfaceRole::Overlay,
            Some("DP-1"),
            &HashSet::new(),
            true,
            Some("HDMI-1"),
        ));
    }

    #[test]
    fn overlay_not_skipped_when_no_active_output() {
        assert!(!should_skip(
            SurfaceRole::Overlay,
            Some("DP-1"),
            &HashSet::new(),
            true,
            None,
        ));
    }

    #[test]
    fn overlay_without_name_never_skipped() {
        assert!(!should_skip(
            SurfaceRole::Overlay,
            None,
            &skip_names(&["DP-1"]),
            true,
            Some("DP-1"),
        ));
    }

    #[test]
    fn skip_name_takes_priority_over_active() {
        // Output is in skip_names AND is the active output —
        // should be skipped regardless of skip_active flag.
        assert!(should_skip(
            SurfaceRole::Overlay,
            Some("DP-1"),
            &skip_names(&["DP-1"]),
            false,
            Some("DP-1"),
        ));
    }

    #[test]
    fn multiple_skip_names() {
        let names = skip_names(&["DP-1", "DP-2", "HDMI-1"]);
        assert!(should_skip(
            SurfaceRole::Overlay,
            Some("DP-2"),
            &names,
            false,
            None
        ));
        assert!(!should_skip(
            SurfaceRole::Overlay,
            Some("eDP-1"),
            &names,
            false,
            None
        ));
    }

    #[test]
    fn empty_skip_names_and_no_active() {
        assert!(!should_skip(
            SurfaceRole::Overlay,
            Some("DP-1"),
            &HashSet::new(),
            false,
            None,
        ));
    }
}
