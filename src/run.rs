use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::shell::wlr_layer::Anchor;
use smithay_client_toolkit::shell::wlr_layer::KeyboardInteractivity;
use smithay_client_toolkit::shell::wlr_layer::Layer;
use smithay_client_toolkit::shell::wlr_layer::LayerShell;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::Shm;
use wayland_client::globals::registry_queue_init;
use wayland_client::globals::GlobalList;
use wayland_client::Connection;
use wayland_client::Dispatch;
use wayland_client::QueueHandle;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_v1::WpAlphaModifierV1;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1;

use crate::error::SpawnError;
use crate::state::OverlaySurface;
use crate::state::SurfaceConfig;
use crate::state::SurfaceRole;
use crate::state::ZenState;
use crate::transition::FadeIn;
use crate::window::ZenConfig;

fn try_bind<P: wayland_client::Proxy + 'static>(
    globals: &GlobalList,
    qh: &QueueHandle<ZenState>,
) -> Option<P>
where
    ZenState: Dispatch<P, ()>,
{
    globals.bind::<P, _, _>(qh, 1..=1, ()).ok()
}

/// Poll the Wayland connection fd with a timeout.
///
/// Returns `true` if the fd is readable, `false` on timeout or error.
fn poll_wayland_fd(fd: std::os::unix::io::BorrowedFd<'_>, timeout_ms: i32) -> bool {
    use std::os::unix::io::AsRawFd;
    let mut pollfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ret = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
    ret > 0
}

pub(crate) fn run(
    config: ZenConfig,
    ready_tx: Option<mpsc::Sender<()>>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), SpawnError> {
    if let Some(delay) = config.settle_delay {
        std::thread::sleep(delay);
    }

    let conn = Connection::connect_to_env().map_err(|e| SpawnError::WaylandConnection(e.into()))?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).map_err(|e| SpawnError::Setup(e.into()))?;
    let qh = event_queue.handle();

    let registry = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);
    let compositor =
        CompositorState::bind(&globals, &qh).map_err(|e| SpawnError::MissingProtocol {
            protocol: "wl_compositor",
            source: e.into(),
        })?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| SpawnError::MissingProtocol {
        protocol: "zwlr_layer_shell_v1",
        source: e.into(),
    })?;
    let viewporter: Option<WpViewporter> = try_bind(&globals, &qh);
    let alpha_modifier: Option<WpAlphaModifierV1> = try_bind(&globals, &qh);
    let gamma_manager: Option<ZwlrGammaControlManagerV1> = try_bind(&globals, &qh);
    let shm = Shm::bind(&globals, &qh).map_err(|e| SpawnError::MissingProtocol {
        protocol: "wl_shm",
        source: e.into(),
    })?;
    let pool = SlotPool::new(256, &shm).map_err(|e| SpawnError::Setup(e.into()))?;

    let toplevel_manager: Option<ZwlrForeignToplevelManagerV1> = if config.skip_active {
        try_bind(&globals, &qh)
    } else {
        None
    };

    let has_alpha_mod = alpha_modifier.is_some();
    let target_opacity = config.target_opacity;
    let brightness = config.brightness;

    let mut state = ZenState {
        registry,
        output_state,
        compositor,
        layer_shell,
        viewporter,
        alpha_modifier,
        gamma_manager,
        shm,
        pool,
        surfaces: Vec::new(),
        fading: config.fade_duration.is_some(),
        target_opacity,
        color: config.color,
        skip_names: config.skip_names.clone(),
        skip_active: config.skip_active,
        active_output: None,
        transition: None,
        toplevel_manager,
        toplevels: Vec::new(),
        running: true,
    };

    // Discover outputs and toplevels
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| SpawnError::Setup(e.into()))?;
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| SpawnError::Setup(e.into()))?;

    // Snapshot the active output
    state.active_output = state.active_output_name();

    // Create surfaces on ALL outputs — active ones start transparent
    let outputs: Vec<_> = state.output_state.outputs().collect();

    for output in outputs {
        let info = state.output_state.info(&output);
        let output_name = info.as_ref().and_then(|i| i.name.clone());

        // Skip explicitly named outputs entirely (they never get surfaces)
        if let Some(ref name) = output_name {
            if config.skip_names.contains(name) {
                continue;
            }
        }

        let wl_surface = state.compositor.create_surface(&qh);

        let viewport = state
            .viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(&wl_surface, &qh, ()));

        let layer_surface = state.layer_shell.create_layer_surface(
            &qh,
            wl_surface,
            Layer::Overlay,
            Some(config.namespace.clone()),
            Some(&output),
        );

        layer_surface.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);

        let alpha_surface = state.alpha_modifier.as_ref().map(|am| {
            let alpha_surf = am.get_surface(layer_surface.wl_surface(), &qh, ());
            if config.fade_duration.is_some() {
                alpha_surf.set_multiplier(0);
            }
            alpha_surf
        });

        let surface_idx = state.surfaces.len();
        let gamma_control = if brightness.is_some() {
            state
                .gamma_manager
                .as_ref()
                .map(|gm| gm.get_gamma_control(&output, &qh, surface_idx))
        } else {
            None
        };

        layer_surface.commit();

        state.surfaces.push(OverlaySurface {
            output_name: output_name.clone(),
            role: SurfaceRole::Overlay,
            layer: layer_surface,
            viewport,
            alpha_surface,
            gamma_control,
            gamma_size: None,
            buffer: None,
            config: SurfaceConfig::Pending,
        });

        // Layer::Bottom backdrop — always opaque, prevents desktop flash
        // when the compositor renders before sending us events.
        let backdrop_surface = state.compositor.create_surface(&qh);
        let backdrop_layer = state.layer_shell.create_layer_surface(
            &qh,
            backdrop_surface,
            Layer::Bottom,
            Some(format!("{}-backdrop", config.namespace)),
            Some(&output),
        );
        backdrop_layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        backdrop_layer.set_exclusive_zone(-1);
        backdrop_layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        backdrop_layer.commit();

        state.surfaces.push(OverlaySurface {
            output_name,
            role: SurfaceRole::Backdrop,
            layer: backdrop_layer,
            viewport: None,
            alpha_surface: None,
            gamma_control: None,
            gamma_size: None,
            buffer: None,
            config: SurfaceConfig::Pending,
        });
    }

    event_queue
        .roundtrip(&mut state)
        .map_err(|e| SpawnError::Setup(e.into()))?;
    while state
        .surfaces
        .iter()
        .any(|s| matches!(s.config, SurfaceConfig::Pending))
    {
        event_queue
            .blocking_dispatch(&mut state)
            .map_err(|e| SpawnError::Setup(e.into()))?;
    }

    if let Some(tx) = ready_tx {
        tx.send(()).ok();
    }

    if let Some(duration) = config.fade_duration {
        let start = Instant::now();
        let tick = Duration::from_millis(8);
        let fade_in = FadeIn {
            duration,
            target_opacity,
            target_brightness: brightness.unwrap_or(1.0),
        };

        loop {
            let frame = fade_in.frame_at(start.elapsed());

            if has_alpha_mod {
                for (idx, surface) in state.surfaces.iter().enumerate() {
                    if let Some(ref alpha_surf) = surface.alpha_surface {
                        let m = if state.is_skipped(idx) {
                            0
                        } else {
                            frame.multiplier
                        };
                        alpha_surf.set_multiplier(m);
                    }
                    surface.layer.commit();
                }
            } else {
                state.draw_dimmed(frame.alpha);
            }

            if brightness.is_some() {
                state.set_gamma_dimmed(frame.brightness);
            }

            event_queue
                .flush()
                .map_err(|e| SpawnError::Setup(e.into()))?;
            event_queue
                .dispatch_pending(&mut state)
                .map_err(|e| SpawnError::Setup(e.into()))?;

            if frame.done {
                break;
            }

            std::thread::sleep(tick);
        }
    } else if let Some(target_brightness) = brightness {
        state.set_gamma_dimmed(target_brightness);
    }

    // Steady state — toplevel Done handler starts cross-fade transitions
    let transition_tick = Duration::from_millis(8);
    while state.running {
        if shutdown.load(Ordering::Acquire) {
            state.running = false;
            break;
        }

        if state.is_transitioning() {
            state.tick_transition();
            event_queue
                .flush()
                .map_err(|e| SpawnError::Setup(e.into()))?;
            event_queue
                .dispatch_pending(&mut state)
                .map_err(|e| SpawnError::Setup(e.into()))?;
            std::thread::sleep(transition_tick);
        } else {
            // Poll-based dispatch: wait up to 100ms for events so we
            // can periodically check the shutdown signal.
            event_queue
                .flush()
                .map_err(|e| SpawnError::Setup(e.into()))?;
            if let Some(guard) = event_queue.prepare_read() {
                let fd = guard.connection_fd();
                if poll_wayland_fd(fd, 100) {
                    if let Err(e) = guard.read() {
                        state.running = false;
                        return Err(SpawnError::Setup(e.into()));
                    }
                }
                // If poll timed out, guard is dropped which cancels the read.
            }
            event_queue
                .dispatch_pending(&mut state)
                .map_err(|e| SpawnError::Setup(e.into()))?;
            event_queue
                .flush()
                .map_err(|e| SpawnError::Setup(e.into()))?;
        }
    }

    // Dropping state/surfaces/connection here cleans up Wayland
    // resources: surfaces are destroyed and gamma ramps are restored.
    Ok(())
}
