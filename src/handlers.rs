use smithay_client_toolkit::compositor::CompositorHandler;
use smithay_client_toolkit::delegate_compositor;
use smithay_client_toolkit::delegate_layer;
use smithay_client_toolkit::delegate_output;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::delegate_shm;
use smithay_client_toolkit::output::OutputHandler;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::ProvidesRegistryState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shell::wlr_layer::LayerShellHandler;
use smithay_client_toolkit::shell::wlr_layer::LayerSurface;
use smithay_client_toolkit::shell::wlr_layer::LayerSurfaceConfigure;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::ShmHandler;
use wayland_client::protocol::wl_output;
use wayland_client::protocol::wl_surface;
use wayland_client::Connection;
use wayland_client::Dispatch;
use wayland_client::QueueHandle;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_v1::WpAlphaModifierV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use crate::state::opacity_to_alpha;
use crate::state::LoopPhase;
use crate::state::SurfaceConfig;
use crate::state::ZenState;

/// No-op compositor handler — we don't react to scale, transform, or frame events.
///
/// Overlay surfaces are simple solid-color fills, so scale factor changes
/// and output transforms don't require re-rendering.
impl CompositorHandler for ZenState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

/// No-op output handler — outputs are enumerated at startup only.
///
/// Hot-plugged monitors after startup won't get overlay surfaces.
/// This is a known limitation; the event loop would need to create
/// new surfaces on `new_output` to support it.
impl OutputHandler for ZenState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

/// Handles layer-shell surface lifecycle events.
///
/// `configure` records the surface dimensions and draws the initial frame.
/// Backdrops are always drawn at full target opacity. Overlays start
/// transparent if a fade-in is configured or the surface is skipped,
/// otherwise they start at target opacity.
impl LayerShellHandler for ZenState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {}

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let idx = self
            .surfaces
            .iter()
            .position(|s| s.layer.wl_surface() == layer.wl_surface());

        if let Some(idx) = idx {
            self.surfaces[idx].config = SurfaceConfig::Ready {
                width: configure.new_size.0,
                height: configure.new_size.1,
            };

            let alpha = if self.surfaces[idx].is_backdrop() {
                // Backdrops are always fully opaque
                opacity_to_alpha(self.target_opacity)
            } else if self.phase == LoopPhase::FadingIn || self.is_skipped(idx) {
                0
            } else {
                opacity_to_alpha(self.target_opacity)
            };
            self.draw_fullsize(idx, alpha);
        }
    }
}

/// Provides access to the shared memory state for SCTK's buffer allocation.
impl ShmHandler for ZenState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

/// Provides access to the registry state for SCTK's global object discovery.
impl ProvidesRegistryState for ZenState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }

    registry_handlers!(OutputState);
}

/// No-op dispatch — the viewporter global has no client-side events.
impl Dispatch<WpViewporter, ()> for ZenState {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as wayland_client::Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// No-op dispatch — per-surface viewports have no client-side events.
impl Dispatch<WpViewport, ()> for ZenState {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: <WpViewport as wayland_client::Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// No-op dispatch — the alpha modifier global has no client-side events.
impl Dispatch<WpAlphaModifierV1, ()> for ZenState {
    fn event(
        _: &mut Self,
        _: &WpAlphaModifierV1,
        _: <WpAlphaModifierV1 as wayland_client::Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// No-op dispatch — per-surface alpha modifiers have no client-side events.
impl Dispatch<WpAlphaModifierSurfaceV1, ()> for ZenState {
    fn event(
        _: &mut Self,
        _: &WpAlphaModifierSurfaceV1,
        _: <WpAlphaModifierSurfaceV1 as wayland_client::Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_compositor!(ZenState);
delegate_output!(ZenState);
delegate_layer!(ZenState);
delegate_shm!(ZenState);
delegate_registry!(ZenState);
