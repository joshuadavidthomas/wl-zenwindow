//! Application coordinator.
//!
//! [`App`] is the Wayland dispatch target and the bridge between protocol
//! events and domain logic. Owns:
//!
//! - [`Wayland`] protocol bindings (`wl`)
//! - [`DimController`] for dimming state (`dim`)
//! - [`Surface`] list for all outputs (`surfaces`)
//! - [`TrackedToplevel`] list for focus tracking (`toplevels`)
//!
//! # Lifecycle
//!
//! [`AppPhase`] tracks event loop state:
//!
//! - **`FadingIn`** — Initial animation. Both backdrops and overlays animate
//!   from transparent to target together.
//! - **`Running`** — Steady state. Only overlays participate in focus
//!   transitions; backdrops stay at target opacity as a safety net.
//! - **`ShuttingDown`** — Cleanup.
//!
//! # Update flow
//!
//! State changes go through [`DimController`], which returns [`DimUpdates`].
//! App then calls [`apply_updates`] (overlays only) or
//! [`apply_updates_all_layers`] (both layers, for fade-in) to render.
//!
//! This separation keeps dimming logic testable without Wayland.

use smithay_client_toolkit::shell::wlr_layer::Anchor;
use smithay_client_toolkit::shell::wlr_layer::KeyboardInteractivity;
use smithay_client_toolkit::shell::wlr_layer::LayerSurface;
use smithay_client_toolkit::shell::WaylandSurface;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::QueueHandle;

use crate::dim::DimController;
use crate::dim::DimUpdates;
use crate::render::Brightness;
use crate::render::GammaState;
use crate::render::LayerShellHandshake;
use crate::render::Opacity;
use crate::render::Surface;
use crate::render::SurfaceRole;
use crate::wayland::TrackedToplevel;
use crate::wayland::Wayland;
use crate::window::Config;

/// Which phase of the event loop we're in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPhase {
    /// Initial fade-in animation.
    FadingIn,
    /// Steady state — overlays are up, handling focus transitions.
    Running,
    /// Shutting down.
    ShuttingDown,
}

/// The main application state — Wayland dispatch target.
pub struct App {
    /// Wayland protocol bindings.
    pub wl: Wayland,
    /// Configuration.
    pub config: Config,
    /// Managed surfaces.
    pub surfaces: Vec<Surface>,
    /// Tracked toplevels for focus detection.
    pub toplevels: Vec<TrackedToplevel>,
    /// Current loop phase.
    pub phase: AppPhase,
    /// Dimming state machine.
    pub dim: DimController,
}

impl App {
    /// Create surfaces for all outputs.
    pub fn create_surfaces(&mut self, qh: &QueueHandle<Self>) {
        let outputs: Vec<_> = self.wl.output_state.outputs().collect();

        for output in outputs {
            let info = self.wl.output_state.info(&output);
            let output_name = info.as_ref().and_then(|i| i.name.clone());

            // Skip explicitly named outputs
            if let Some(ref name) = output_name {
                if self.config.skip_names.contains(name) {
                    continue;
                }
            }

            // Create backdrop (safety net) and overlay (fade surface)
            self.create_surface(&output, output_name.clone(), SurfaceRole::Backdrop, qh);
            self.create_surface(&output, output_name.clone(), SurfaceRole::Overlay, qh);

            // Register with DimController
            if let Some(ref name) = output_name {
                let is_active = self.dim.active_output() == Some(name.as_str());
                self.dim.add_output(name.clone(), is_active);
            }
        }
    }

    fn create_surface(
        &mut self,
        output: &WlOutput,
        output_name: Option<String>,
        role: SurfaceRole,
        qh: &QueueHandle<Self>,
    ) {
        let wl_surface = self.wl.compositor.create_surface(qh);
        let viewport = self
            .wl
            .viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(&wl_surface, qh, ()));

        let layer_surface = self.wl.layer_shell.create_layer_surface(
            qh,
            wl_surface,
            role.into(),
            Some(self.config.namespace.clone()),
            Some(output),
        );

        layer_surface.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);

        // Only overlays need alpha modifiers
        let alpha_modifier = if role == SurfaceRole::Overlay {
            self.wl.alpha_modifier.as_ref().map(|am| {
                let alpha_surf = am.get_surface(layer_surface.wl_surface(), qh, ());
                if self.config.fade_duration.is_some() {
                    alpha_surf.set_multiplier(0);
                }
                alpha_surf
            })
        } else {
            None
        };

        // Only overlays need gamma control
        let surface_idx = self.surfaces.len();
        let gamma = if role == SurfaceRole::Overlay && self.config.target_brightness.is_some() {
            self.wl
                .gamma_manager
                .as_ref()
                .map_or(GammaState::Unavailable, |gm| {
                    GammaState::Pending(gm.get_gamma_control(output, qh, surface_idx))
                })
        } else {
            GammaState::Unavailable
        };

        layer_surface.commit();

        self.surfaces.push(Surface {
            output_name,
            role,
            layer: layer_surface,
            viewport,
            alpha_modifier,
            gamma,
            buffer: None,
            configure: LayerShellHandshake::Pending,
        });
    }

    /// Check if all surfaces are configured.
    pub fn all_surfaces_configured(&self) -> bool {
        self.surfaces
            .iter()
            .all(|s| !matches!(s.configure, LayerShellHandshake::Pending))
    }

    /// Get the currently active output name from toplevel tracking.
    pub fn active_output_name(&self) -> Option<String> {
        self.toplevels
            .iter()
            .find(|t| t.activated)
            .and_then(|t| t.output.as_ref())
            .and_then(|output| self.wl.output_state.info(output))
            .and_then(|info| info.name.clone())
    }

    /// Apply dimming updates to overlay surfaces only.
    pub fn apply_updates(&mut self, updates: &DimUpdates) {
        for update in updates.iter() {
            self.apply_output_update(&update.name, update.opacity, update.brightness);
        }
    }

    /// Apply a single output update to its overlay surface.
    pub fn apply_output_update(&mut self, name: &str, opacity: Opacity, brightness: Brightness) {
        let has_viewporter = self.wl.has_viewporter();
        let color = self.config.color;

        if let Some(surface) = self
            .surfaces
            .iter_mut()
            .find(|s| s.output_name.as_deref() == Some(name) && s.role == SurfaceRole::Overlay)
        {
            surface.draw(&mut self.wl.pool, color, opacity, has_viewporter);
            surface.update_gamma(brightness);
        }
    }

    /// Apply dimming updates to BOTH backdrop and overlay (for fade-in).
    pub fn apply_updates_all_layers(&mut self, updates: &DimUpdates) {
        let has_viewporter = self.wl.has_viewporter();
        let color = self.config.color;

        for update in updates.iter() {
            for surface in &mut self.surfaces {
                if surface.output_name.as_deref() != Some(&update.name) {
                    continue;
                }
                surface.draw(&mut self.wl.pool, color, update.opacity, has_viewporter);
                surface.update_gamma(update.brightness);
            }
        }
    }

    /// Snap all backdrops to `target_opacity` (called after fade-in).
    pub fn snap_backdrops_to_target(&mut self) {
        let has_viewporter = self.wl.has_viewporter();
        let color = self.config.color;
        let opacity = self.config.target_opacity;

        for surface in &mut self.surfaces {
            if surface.role == SurfaceRole::Backdrop {
                surface.draw(&mut self.wl.pool, color, opacity, has_viewporter);
            }
        }
    }

    /// Handle layer surface closed by compositor.
    pub fn on_surface_closed(&mut self, layer: &LayerSurface) {
        let output_name = self
            .surfaces
            .iter()
            .find(|s| s.layer.wl_surface() == layer.wl_surface())
            .and_then(|s| s.output_name.clone());

        self.surfaces
            .retain(|s| s.layer.wl_surface() != layer.wl_surface());

        if let Some(name) = output_name {
            self.dim.remove_output(&name);
        }
    }

    /// Handle output destroyed.
    pub fn on_output_destroyed(&mut self, output: &WlOutput) {
        let output_name = self
            .wl
            .output_state
            .info(output)
            .and_then(|i| i.name.clone());

        if let Some(ref name) = output_name {
            self.surfaces
                .retain(|s| s.output_name.as_deref() != Some(name));
            self.dim.remove_output(name);
        }
    }

    /// Refresh active output and start transition if needed.
    pub fn refresh_active_output(&mut self) {
        let new_active = self.active_output_name();
        let updates = self.dim.focus_changed(new_active);
        self.apply_updates(&updates);
    }

    /// Check if currently animating.
    pub fn is_animating(&self) -> bool {
        self.dim.is_animating()
    }

    /// Tick the transition animation.
    pub fn tick_transition(&mut self) {
        let updates = self.dim.tick();
        self.apply_updates(&updates);
    }

    /// Immediately dim ALL outputs (snap all overlays to opaque).
    pub fn dim_all_outputs(&mut self) {
        let updates = self.dim.snap_all_to_dimmed();
        self.apply_updates(&updates);
    }
}
