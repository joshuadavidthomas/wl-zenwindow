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

/// Distinguishes the two kinds of surfaces created per output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurfaceRole {
    /// Layer::Overlay — participates in transitions, skip logic, and all
    /// optional protocol features (viewport, alpha surface, gamma control).
    Overlay,
    /// Layer::Bottom backdrop — always opaque, never transitions.
    /// Prevents desktop flash when the compositor renders a frame
    /// before we receive foreign-toplevel events.
    Backdrop,
}

pub(crate) struct OverlaySurface {
    pub(crate) output_name: Option<String>,
    pub(crate) role: SurfaceRole,
    pub(crate) layer: LayerSurface,
    pub(crate) viewport: Option<WpViewport>,
    pub(crate) alpha_surface: Option<WpAlphaModifierSurfaceV1>,
    pub(crate) gamma_control: Option<ZwlrGammaControlV1>,
    pub(crate) gamma_size: Option<u32>,
    pub(crate) buffer: Option<Buffer>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) configured: bool,
}

impl OverlaySurface {
    pub(crate) fn is_backdrop(&self) -> bool {
        self.role == SurfaceRole::Backdrop
    }
}

pub(crate) struct ZenState {
    pub(crate) registry: RegistryState,
    pub(crate) output_state: OutputState,
    pub(crate) compositor: CompositorState,
    pub(crate) layer_shell: LayerShell,
    pub(crate) viewporter: Option<WpViewporter>,
    pub(crate) alpha_modifier: Option<WpAlphaModifierV1>,
    pub(crate) gamma_manager: Option<ZwlrGammaControlManagerV1>,
    pub(crate) shm: Shm,
    pub(crate) pool: SlotPool,
    pub(crate) surfaces: Vec<OverlaySurface>,
    pub(crate) fading: bool,
    pub(crate) target_opacity: f64,
    pub(crate) color: [u8; 3],
    pub(crate) skip_names: HashSet<String>,
    pub(crate) skip_active: bool,
    pub(crate) active_output: Option<String>,
    pub(crate) transition: Option<Transition>,
    pub(crate) toplevel_manager: Option<ZwlrForeignToplevelManagerV1>,
    pub(crate) toplevels: Vec<TrackedToplevel>,
    pub(crate) running: bool,
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

    fn skip_names(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
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
