//! Wayland protocol bindings and dispatch handlers.
//!
//! All compositor bindings live in [`Wayland`]. The `smithay-client-toolkit`
//! handler traits are implemented here for [`App`], translating protocol
//! events into domain logic calls.
//!
//! # Protocol state
//!
//! [`Wayland`] holds all globals. Required protocols (registry, compositor,
//! layer-shell, shm, output state) must be present or setup fails. Optional
//! protocols (viewporter, alpha modifier, gamma manager, toplevel manager)
//! degrade gracefully when absent.
//!
//! # Focus tracking
//!
//! [`TrackedToplevel`] records state from `zwlr_foreign_toplevel_manager_v1`:
//! which output each toplevel is on, and whether it's activated. The `Done`
//! event triggers [`App::refresh_active_output`] to start cross-fade
//! transitions when focus moves between monitors.
//!
//! Window movement (activated toplevel changing outputs mid-drag) snaps all
//! overlays opaque immediately to prevent flash, then resumes normal
//! transitions once the window settles.
//!
//! # Handler notes
//!
//! - `LayerShellHandler::configure` — stores dimensions, triggers initial draw
//! - `OutputHandler::output_destroyed` — cleans up surfaces for unplugged monitors
//! - Toplevel handlers — update [`TrackedToplevel`] state, detect cross-output movement

use smithay_client_toolkit::compositor::CompositorHandler;
use smithay_client_toolkit::compositor::CompositorState;
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
use smithay_client_toolkit::shell::wlr_layer::LayerShell;
use smithay_client_toolkit::shell::wlr_layer::LayerShellHandler;
use smithay_client_toolkit::shell::wlr_layer::LayerSurface;
use smithay_client_toolkit::shell::wlr_layer::LayerSurfaceConfigure;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::ShmHandler;
use wayland_client::protocol::wl_output;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_surface;
use wayland_client::Connection;
use wayland_client::Dispatch;
use wayland_client::Proxy as _;
use wayland_client::QueueHandle;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_v1::WpAlphaModifierV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1;
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::{
    self,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::{
    self,
};
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1;
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_v1::ZwlrGammaControlV1;
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_v1::{
    self,
};

use crate::app::App;
use crate::app::AppPhase;
use crate::render::LayerShellHandshake;

/// Wayland protocol state — all the compositor bindings.
pub struct Wayland {
    pub registry: RegistryState,
    pub output_state: OutputState,
    pub compositor: CompositorState,
    pub layer_shell: LayerShell,
    pub shm: Shm,
    pub pool: SlotPool,
    pub viewporter: Option<WpViewporter>,
    pub alpha_modifier: Option<WpAlphaModifierV1>,
    pub gamma_manager: Option<ZwlrGammaControlManagerV1>,
    pub toplevel_manager: Option<ZwlrForeignToplevelManagerV1>,
}

impl Wayland {
    pub fn has_viewporter(&self) -> bool {
        self.viewporter.is_some()
    }
}

/// Tracked toplevel for focus detection.
pub struct TrackedToplevel {
    pub handle: ZwlrForeignToplevelHandleV1,
    pub activated: bool,
    pub output: Option<WlOutput>,
}

/// Find or create a tracked toplevel entry for the given handle.
fn find_or_insert_toplevel<'a>(
    toplevels: &'a mut Vec<TrackedToplevel>,
    handle: &ZwlrForeignToplevelHandleV1,
) -> &'a mut TrackedToplevel {
    let idx = toplevels.iter().position(|t| t.handle.id() == handle.id());

    if let Some(i) = idx {
        &mut toplevels[i]
    } else {
        toplevels.push(TrackedToplevel {
            handle: handle.clone(),
            activated: false,
            output: None,
        });
        toplevels.last_mut().expect("just pushed")
    }
}

impl CompositorHandler for App {
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

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.wl.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.on_output_destroyed(&output);
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.on_surface_closed(layer);
    }

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
            self.surfaces[idx].configure = LayerShellHandshake::Ready {
                width: configure.new_size.0,
                height: configure.new_size.1,
            };

            let output_name = self.surfaces[idx].output_name.clone();

            if self.phase == AppPhase::FadingIn {
                // During fade-in, both backdrops and overlays start transparent
                // The fade loop will animate them together
            } else {
                // In running state, draw based on dim state
                if let Some(ref name) = output_name {
                    if let Some(update) = self.dim.current_update(name) {
                        self.apply_output_update(name, update.opacity, update.brightness);
                    }
                }
            }
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.wl.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.wl.registry
    }

    registry_handlers!(OutputState);
}

// Gamma control handlers

impl Dispatch<ZwlrGammaControlManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwlrGammaControlManagerV1,
        _: <ZwlrGammaControlManagerV1 as wayland_client::Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrGammaControlV1, usize> for App {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrGammaControlV1,
        event: zwlr_gamma_control_v1::Event,
        surface_idx: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(surface) = state.surfaces.get_mut(*surface_idx) else {
            return;
        };

        match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => {
                surface.gamma.receive_size(size);
            }
            zwlr_gamma_control_v1::Event::Failed => {
                surface.gamma.fail();
            }
            _ => {}
        }
    }
}

// Toplevel tracking handlers

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Finished = event {
            state.wl.toplevel_manager = None;
        }
    }

    wayland_client::event_created_child!(App, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::OutputEnter { output } => {
                // Check if this is an activated toplevel moving between outputs
                let toplevel = state.toplevels.iter().find(|t| t.handle.id() == proxy.id());
                let is_activated = toplevel.is_some_and(|t| t.activated);
                let had_different_output = toplevel
                    .and_then(|t| t.output.as_ref())
                    .is_some_and(|o| o.id() != output.id());

                if is_activated && had_different_output {
                    // Activated window is moving! Snap ALL overlays opaque immediately.
                    state.dim_all_outputs();
                    state.dim.cancel_transition();
                }

                find_or_insert_toplevel(&mut state.toplevels, proxy).output = Some(output);
            }
            zwlr_foreign_toplevel_handle_v1::Event::OutputLeave { output: _ } => {
                // Check if this is the activated toplevel leaving
                let toplevel = state.toplevels.iter().find(|t| t.handle.id() == proxy.id());
                let is_activated = toplevel.is_some_and(|t| t.activated);

                if is_activated {
                    // Activated window is leaving an output! Snap ALL overlays opaque.
                    state.dim_all_outputs();
                    state.dim.cancel_transition();
                }

                find_or_insert_toplevel(&mut state.toplevels, proxy).output = None;
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: raw_state } => {
                let activated = raw_state
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .any(|s| s == 2); // 2 = activated

                find_or_insert_toplevel(&mut state.toplevels, proxy).activated = activated;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                state.refresh_active_output();
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.retain(|t| t.handle.id() != proxy.id());
            }
            _ => {}
        }
    }
}

// No-op dispatch for protocols with no client-side events

impl Dispatch<WpViewporter, ()> for App {
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

impl Dispatch<WpViewport, ()> for App {
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

impl Dispatch<WpAlphaModifierV1, ()> for App {
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

impl Dispatch<WpAlphaModifierSurfaceV1, ()> for App {
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

delegate_compositor!(App);
delegate_output!(App);
delegate_layer!(App);
delegate_shm!(App);
delegate_registry!(App);

#[cfg(test)]
mod tests {
    // Helper for testing detect_output_change logic
    fn detect_output_change(
        old_active: Option<&str>,
        new_active: Option<&str>,
    ) -> Option<(Option<String>, Option<String>)> {
        if old_active == new_active {
            return None;
        }
        Some((old_active.map(str::to_owned), new_active.map(str::to_owned)))
    }

    #[test]
    fn no_change_when_both_none() {
        assert_eq!(detect_output_change(None, None), None);
    }

    #[test]
    fn no_change_when_same_output() {
        assert_eq!(detect_output_change(Some("DP-1"), Some("DP-1")), None);
    }

    #[test]
    fn change_from_none_to_some() {
        let change = detect_output_change(None, Some("DP-1")).unwrap();
        assert_eq!(change.0, None);
        assert_eq!(change.1, Some("DP-1".to_string()));
    }

    #[test]
    fn change_from_some_to_none() {
        let change = detect_output_change(Some("DP-1"), None).unwrap();
        assert_eq!(change.0, Some("DP-1".to_string()));
        assert_eq!(change.1, None);
    }

    #[test]
    fn change_between_two_outputs() {
        let change = detect_output_change(Some("DP-1"), Some("HDMI-1")).unwrap();
        assert_eq!(change.0, Some("DP-1".to_string()));
        assert_eq!(change.1, Some("HDMI-1".to_string()));
    }
}
