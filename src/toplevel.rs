use std::time::Duration;
use std::time::Instant;

use wayland_client::protocol::wl_output;
use wayland_client::Connection;
use wayland_client::Dispatch;
use wayland_client::Proxy as _;
use wayland_client::QueueHandle;
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1;
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::{
    self,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::{
    self,
};

use crate::state::ZenState;
use crate::transition::Transition;

pub(crate) struct TrackedToplevel {
    pub(crate) handle: ZwlrForeignToplevelHandleV1,
    pub(crate) activated: bool,
    pub(crate) output: Option<wl_output::WlOutput>,
}

impl ZenState {
    pub(crate) fn active_output_name(&self) -> Option<String> {
        self.toplevels
            .iter()
            .find(|t| t.activated)
            .and_then(|t| t.output.as_ref())
            .and_then(|output| self.output_state.info(output))
            .and_then(|info| info.name.clone())
    }

    /// Check if the active output changed, and if so, start a cross-fade.
    pub(crate) fn refresh_active_output(&mut self) {
        if !self.skip_active {
            return;
        }

        let new_active = self.active_output_name();
        if new_active == self.active_output {
            return;
        }

        let old_active = self.active_output.take();
        self.active_output = new_active.clone();

        // Immediately dim the old monitor's overlay
        if let Some(ref name) = old_active {
            for idx in 0..self.surfaces.len() {
                if self.surfaces[idx].is_backdrop() {
                    continue;
                }
                if self.surfaces[idx].output_name.as_deref() == Some(name) {
                    let alpha = (self.target_opacity * 255.0) as u8;
                    self.draw_surface_alpha(idx, alpha);
                }
            }
        }

        // Fade out the overlay on the new monitor after the window settles
        self.transition = Some(Transition {
            start: Instant::now(),
            delay: Duration::from_millis(325),
            duration: Duration::from_millis(150),
            revealing: new_active,
        });
    }
}

fn find_toplevel<'a>(
    toplevels: &'a mut Vec<TrackedToplevel>,
    handle: &ZwlrForeignToplevelHandleV1,
) -> Option<&'a mut TrackedToplevel> {
    if !toplevels.iter().any(|t| t.handle.id() == handle.id()) {
        toplevels.push(TrackedToplevel {
            handle: handle.clone(),
            activated: false,
            output: None,
        });
    }
    toplevels.iter_mut().find(|t| t.handle.id() == handle.id())
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for ZenState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Finished = event {
            state.toplevel_manager = None;
        }
    }

    wayland_client::event_created_child!(ZenState, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for ZenState {
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
                // Check if the activated toplevel was on a different output.
                // Gather info before mutating, to avoid borrow conflicts.
                let prev_output_name = state
                    .toplevels
                    .iter()
                    .find(|t| t.handle.id() == proxy.id())
                    .filter(|t| t.activated)
                    .and_then(|t| t.output.as_ref())
                    .and_then(|o| state.output_state.info(o))
                    .and_then(|i| i.name.clone());

                // Dim the old output's overlay immediately — earliest
                // reaction to a window move, before OutputLeave or Done.
                if let Some(ref name) = prev_output_name {
                    let target_alpha = (state.target_opacity * 255.0) as u8;
                    for idx in 0..state.surfaces.len() {
                        if state.surfaces[idx].is_backdrop() {
                            continue;
                        }
                        if state.surfaces[idx].output_name.as_deref() == Some(name.as_str()) {
                            state.draw_surface_alpha(idx, target_alpha);
                        }
                    }
                    if state
                        .transition
                        .as_ref()
                        .and_then(|t| t.revealing.as_deref())
                        == Some(name.as_str())
                    {
                        state.transition = None;
                    }
                }

                if let Some(info) = find_toplevel(&mut state.toplevels, proxy) {
                    info.output = Some(output);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::OutputLeave { output } => {
                let leaving_name = state
                    .output_state
                    .info(&output)
                    .and_then(|i| i.name.clone());

                if let Some(ref name) = leaving_name {
                    let is_active = state.active_output.as_deref() == Some(name.as_str());
                    let is_revealing = state
                        .transition
                        .as_ref()
                        .and_then(|t| t.revealing.as_deref())
                        == Some(name.as_str());

                    // Snap opaque immediately on OutputLeave — earliest
                    // possible reaction, before Done. Covers both:
                    // - settled case (no transition, overlay at alpha=0)
                    // - mid-fade case (transition in progress)
                    if is_active || is_revealing {
                        let target_alpha = (state.target_opacity * 255.0) as u8;
                        for idx in 0..state.surfaces.len() {
                            if state.surfaces[idx].is_backdrop() {
                                continue;
                            }
                            if state.surfaces[idx].output_name.as_deref() == Some(name) {
                                state.draw_surface_alpha(idx, target_alpha);
                            }
                        }
                        if is_revealing {
                            state.transition = None;
                        }
                    }
                }

                if let Some(info) = find_toplevel(&mut state.toplevels, proxy) {
                    info.output = None;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: raw_state } => {
                let activated = raw_state
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .any(|s| s == 2); // 2 = activated

                if let Some(info) = find_toplevel(&mut state.toplevels, proxy) {
                    info.activated = activated;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                // All properties for this toplevel are up to date —
                // immediately update surfaces if active output changed.
                // Main loop flushes after blocking_dispatch returns.
                state.refresh_active_output();
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.retain(|t| t.handle.id() != proxy.id());
            }
            _ => {}
        }
    }
}
