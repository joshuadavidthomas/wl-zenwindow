//! Event loop for zen overlays.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::shell::wlr_layer::LayerShell;
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

use crate::app::App;
use crate::app::AppPhase;
use crate::dim::DimController;
use crate::error::SpawnError;
use crate::wayland::Wayland;
use crate::window::Config;

/// Attempt to bind an optional Wayland global, returning `None` if absent.
fn try_bind<P: wayland_client::Proxy + 'static>(
    globals: &GlobalList,
    qh: &QueueHandle<App>,
) -> Option<P>
where
    App: Dispatch<P, ()>,
{
    globals.bind::<P, _, _>(qh, 1..=1, ()).ok()
}

/// Entry point for the background thread that manages overlay surfaces.
///
/// Connects to Wayland, binds required and optional protocols, creates
/// overlay and backdrop surfaces on each output, runs the initial fade-in
/// (if configured), then enters the steady-state event loop. Exits when
/// `shutdown` is set to `true` or the Wayland connection is lost.
///
/// If `ready_tx` is `Some`, sends `()` once all surfaces are configured
/// and ready (used by [`ZenWindowBuilder::spawn`] to unblock the caller).
pub(crate) fn run(
    config: &Config,
    ready_tx: Option<mpsc::Sender<()>>,
    shutdown: &Arc<AtomicBool>,
) -> Result<(), SpawnError> {
    if let Some(delay) = config.settle_delay {
        std::thread::sleep(delay);
    }

    let conn = Connection::connect_to_env().map_err(|e| SpawnError::WaylandConnection(e.into()))?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).map_err(|e| SpawnError::Setup(e.into()))?;
    let qh = event_queue.handle();

    // Bind required protocols
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
    let shm = Shm::bind(&globals, &qh).map_err(|e| SpawnError::MissingProtocol {
        protocol: "wl_shm",
        source: e.into(),
    })?;
    let pool = SlotPool::new(256, &shm).map_err(|e| SpawnError::Setup(e.into()))?;

    // Bind optional protocols
    let viewporter: Option<WpViewporter> = try_bind(&globals, &qh);
    let alpha_modifier: Option<WpAlphaModifierV1> = try_bind(&globals, &qh);
    let gamma_manager: Option<ZwlrGammaControlManagerV1> = try_bind(&globals, &qh);
    let toplevel_manager: Option<ZwlrForeignToplevelManagerV1> = if config.skip_active {
        try_bind(&globals, &qh)
    } else {
        None
    };

    // Create DimController
    let dim = DimController::new(
        config.target_opacity.as_f64(),
        config
            .target_brightness
            .map(super::render::Brightness::as_f64),
        config.skip_active,
    );

    let phase = if config.fade_duration.is_some() {
        AppPhase::FadingIn
    } else {
        AppPhase::Running
    };

    let wl = Wayland {
        registry,
        output_state,
        compositor,
        layer_shell,
        shm,
        pool,
        viewporter,
        alpha_modifier,
        gamma_manager,
        toplevel_manager,
    };

    let mut app = App::new(wl, config.clone(), phase, dim);

    // Discover outputs and toplevels
    event_queue
        .roundtrip(&mut app)
        .map_err(|e| SpawnError::Setup(e.into()))?;
    event_queue
        .roundtrip(&mut app)
        .map_err(|e| SpawnError::Setup(e.into()))?;

    // Create surfaces on all outputs
    app.create_surfaces(&qh);

    // Wait for all surfaces to be configured
    event_queue
        .roundtrip(&mut app)
        .map_err(|e| SpawnError::Setup(e.into()))?;
    while !app.all_surfaces_configured() {
        event_queue
            .blocking_dispatch(&mut app)
            .map_err(|e| SpawnError::Setup(e.into()))?;
    }

    // Signal ready
    if let Some(tx) = ready_tx {
        tx.send(()).ok();
    }

    // Run fade-in if configured, otherwise snap to target.
    // Fade-in uses the event queue directly (flush + dispatch_pending)
    // to avoid reading new events that could disturb DimController state
    // while the animation is running.
    if let Some(duration) = app.config().fade_duration {
        run_fade_in(&mut app, &mut event_queue, duration)?;
    } else {
        let updates = app.dim_mut().snap_to_target();
        app.apply_updates(&updates);
    }

    // Hand the event queue to calloop for steady-state dispatch
    app.set_phase(AppPhase::Running);
    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().map_err(|e| SpawnError::Setup(e.into()))?;
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| SpawnError::Setup(e.error.into()))?;

    run_steady_state(&mut app, &mut event_loop, shutdown)?;

    Ok(())
}

/// Run the initial fade-in animation.
///
/// Uses the event queue directly rather than calloop — `dispatch_pending`
/// processes only already-buffered events without reading new ones from
/// the compositor, which prevents transient focus events (from the window
/// settling) from disturbing the animation.
fn run_fade_in(
    app: &mut App,
    event_queue: &mut wayland_client::EventQueue<App>,
    duration: Duration,
) -> Result<(), SpawnError> {
    let start = Instant::now();
    let tick = Duration::from_millis(8);

    loop {
        let elapsed = start.elapsed();
        let updates = app.dim_mut().fade_in_frame(elapsed, duration);
        // During fade-in, animate BOTH backdrop and overlay together
        app.apply_updates_all_layers(&updates);

        event_queue
            .flush()
            .map_err(|e| SpawnError::Setup(e.into()))?;
        event_queue
            .dispatch_pending(app)
            .map_err(|e| SpawnError::Setup(e.into()))?;

        if elapsed >= duration {
            break;
        }

        std::thread::sleep(tick);
    }

    // Fade-in complete: snap backdrops to target_opacity (permanent safety net)
    // Overlays are already at correct state from fade_in_frame
    app.snap_backdrops_to_target();

    Ok(())
}

fn run_steady_state(
    app: &mut App,
    event_loop: &mut EventLoop<App>,
    shutdown: &Arc<AtomicBool>,
) -> Result<(), SpawnError> {
    let animation_tick = Duration::from_millis(8);
    let idle_timeout = Duration::from_millis(100);

    while app.phase() == AppPhase::Running {
        if shutdown.load(Ordering::Acquire) {
            app.set_phase(AppPhase::ShuttingDown);
            break;
        }

        let timeout = if app.is_animating() {
            app.tick_transition();
            animation_tick
        } else {
            idle_timeout
        };

        event_loop
            .dispatch(timeout, app)
            .map_err(|e| SpawnError::Setup(e.into()))?;
    }

    Ok(())
}
