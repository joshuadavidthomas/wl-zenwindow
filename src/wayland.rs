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
//! [`TrackedToplevel`] records per-window state from
//! `zwlr_foreign_toplevel_manager_v1`: which output each toplevel is on and
//! whether it has keyboard focus ([`ToplevelFocus`]). The `Done` event triggers
//! [`App::refresh_active_output`] to start cross-fade transitions when focus
//! moves between monitors. During [`AppPhase::WaitingForReveal`] (after a
//! `spawn_with` callback), `Done` events instead check for new fullscreen
//! toplevels to trigger the reveal.
//!
//! Window movement (focused toplevel changing outputs mid-drag) snaps all
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
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::State as ToplevelState;
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

/// Whether a toplevel has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToplevelFocus {
    Active,
    Inactive,
}

/// Tracked toplevel for focus detection.
pub struct TrackedToplevel {
    handle: ZwlrForeignToplevelHandleV1,
    output: Option<WlOutput>,
    focus: ToplevelFocus,
    states: Vec<ToplevelState>,
}

impl TrackedToplevel {
    pub(crate) fn new(handle: ZwlrForeignToplevelHandleV1) -> Self {
        Self {
            handle,
            output: None,
            focus: ToplevelFocus::Inactive,
            states: Vec::new(),
        }
    }

    pub fn handle_id(&self) -> wayland_client::backend::ObjectId {
        self.handle.id()
    }

    /// The output this toplevel is focused on, if any.
    pub fn active_output(&self) -> Option<&WlOutput> {
        match self.focus {
            ToplevelFocus::Active => self.output.as_ref(),
            ToplevelFocus::Inactive => None,
        }
    }

    /// The output this toplevel is currently on, regardless of focus.
    pub fn output(&self) -> Option<&WlOutput> {
        self.output.as_ref()
    }

    pub fn is_focused(&self) -> bool {
        self.focus == ToplevelFocus::Active
    }

    /// Whether this toplevel has the given state.
    pub fn has_state(&self, state: ToplevelState) -> bool {
        self.states.contains(&state)
    }

    fn enter_output(&mut self, output: WlOutput) {
        self.output = Some(output);
    }

    fn leave_output(&mut self) {
        self.output = None;
    }

    fn update_focus(&mut self, focus: ToplevelFocus) {
        self.focus = focus;
    }

    fn update_states(&mut self, states: Vec<ToplevelState>) {
        self.states = states;
    }

    fn is_moving_to_different_output(&self, new_output: &WlOutput) -> bool {
        self.is_focused()
            && self
                .output
                .as_ref()
                .is_some_and(|o| o.id() != new_output.id())
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
        &mut self.wl_mut().output_state
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
            .surfaces()
            .iter()
            .position(|s| s.layer.wl_surface() == layer.wl_surface());

        if let Some(idx) = idx {
            self.surfaces_mut()[idx].configure = LayerShellHandshake::Ready {
                width: configure.new_size.0,
                height: configure.new_size.1,
            };

            let output_name = self.surfaces()[idx].output_name.clone();

            if self.phase() == AppPhase::FadingIn {
                // During fade-in, both backdrops and overlays start transparent
                // The fade loop will animate them together
            } else {
                // In running state, draw based on dim state
                if let Some(ref name) = output_name {
                    if let Some(update) = self.dim().current_update(name) {
                        self.apply_output_update(name, update.opacity, update.brightness);
                    }
                }
            }
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.wl_mut().shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.wl_mut().registry
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
        let Some(surface) = state.surfaces_mut().get_mut(*surface_idx) else {
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
            state.wl_mut().toplevel_manager = None;
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
                if state
                    .find_toplevel(proxy)
                    .is_some_and(|t| t.is_moving_to_different_output(&output))
                {
                    // Activated window is moving! Snap ALL overlays opaque immediately.
                    state.dim_all_outputs();
                    state.dim_mut().cancel_transition();
                }

                state.find_or_insert_toplevel(proxy).enter_output(output);
            }
            zwlr_foreign_toplevel_handle_v1::Event::OutputLeave { output: _ } => {
                if state
                    .find_toplevel(proxy)
                    .is_some_and(TrackedToplevel::is_focused)
                {
                    // Activated window is leaving an output! Snap ALL overlays opaque.
                    state.dim_all_outputs();
                    state.dim_mut().cancel_transition();
                }

                state.find_or_insert_toplevel(proxy).leave_output();
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: raw_state } => {
                let states: Vec<ToplevelState> = raw_state
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .filter_map(|s| ToplevelState::try_from(s).ok())
                    .collect();

                let toplevel = state.find_or_insert_toplevel(proxy);
                toplevel.update_states(states);

                let focus = if toplevel.has_state(ToplevelState::Activated) {
                    ToplevelFocus::Active
                } else {
                    ToplevelFocus::Inactive
                };
                toplevel.update_focus(focus);
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                if state.phase() == AppPhase::WaitingForReveal {
                    state.check_reveal();
                } else {
                    state.refresh_active_output();
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.remove_toplevel(proxy);
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
