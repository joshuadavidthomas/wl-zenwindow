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

impl ZenState {
    /// Whether a surface should be skipped (transparent).
    /// Backdrops are never skipped — they're always opaque.
    pub(crate) fn is_skipped(&self, idx: usize) -> bool {
        if self.surfaces[idx].is_backdrop() {
            return false;
        }
        let name = self.surfaces[idx].output_name.as_deref();
        if let Some(name) = name {
            if self.skip_names.contains(name) {
                return true;
            }
            if self.skip_active {
                if let Some(ref active) = self.active_output {
                    return name == active;
                }
            }
        }
        false
    }
}
