use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::run::run;

/// Builder for configuring which outputs to dim.
pub struct ZenWindowBuilder {
    skip_names: HashSet<String>,
    skip_active: bool,
    namespace: String,
    settle_delay: Option<Duration>,
    fade_duration: Option<Duration>,
    opacity: f64,
    color: [u8; 3],
    brightness: Option<f64>,
}

impl ZenWindowBuilder {
    fn new() -> Self {
        Self {
            skip_names: HashSet::new(),
            skip_active: false,
            namespace: "wl-zenwindow".into(),
            settle_delay: None,
            fade_duration: None,
            opacity: 1.0,
            color: [0, 0, 0],
            brightness: None,
        }
    }

    /// Skip an output by its Wayland name (e.g., "DP-6", "eDP-1").
    pub fn skip_output(mut self, name: impl Into<String>) -> Self {
        self.skip_names.insert(name.into());
        self
    }

    /// Automatically skip the output that has the focused window.
    ///
    /// Uses `zwlr_foreign_toplevel_manager_v1` to detect which output
    /// has the currently activated toplevel. Falls back to dimming all
    /// outputs if the protocol is unavailable or no toplevel is focused.
    pub fn skip_active(mut self) -> Self {
        self.skip_active = true;
        self
    }

    /// Set the layer-shell namespace (default: "wl-zenwindow").
    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = ns.into();
        self
    }

    /// Wait before creating surfaces. Useful when launching alongside
    /// a UI window — gives the window time to render and gain focus
    /// before detecting the active output.
    pub fn settle_delay(mut self, delay: Duration) -> Self {
        self.settle_delay = Some(delay);
        self
    }

    /// Fade in the overlays over the given duration.
    pub fn fade_in(mut self, duration: Duration) -> Self {
        self.fade_duration = Some(duration);
        self
    }

    /// Set the overlay color as RGB (default: black `(0, 0, 0)`).
    pub fn color(mut self, r: u8, g: u8, b: u8) -> Self {
        self.color = [r, g, b];
        self
    }

    /// Set the final overlay opacity (0.0 = transparent, 1.0 = fully opaque).
    /// Default: 1.0.
    pub fn opacity(mut self, opacity: f64) -> Self {
        self.opacity = opacity.clamp(0.0, 1.0);
        self
    }

    /// Dim monitor brightness via gamma control.
    /// 0.0 = completely dark, 1.0 = normal brightness.
    /// Default: unset (gamma untouched).
    ///
    /// Uses the `zwlr_gamma_control_v1` protocol. Falls back gracefully
    /// if another client (e.g., `wlsunset`) already controls gamma.
    pub fn brightness(mut self, brightness: f64) -> Self {
        self.brightness = Some(brightness.clamp(0.0, 1.0));
        self
    }

    /// Spawn on a background thread.
    ///
    /// Blocks briefly until Wayland setup completes (typically a few
    /// milliseconds). Returns an error if the Wayland connection fails.
    ///
    /// Returns a [`ZenWindow`] handle. Dropping it removes overlays
    /// and restores gamma.
    pub fn spawn(self) -> Result<ZenWindow, Box<dyn std::error::Error + Send + Sync>> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = std::thread::Builder::new()
            .name("wl-zenwindow".into())
            .spawn({
                let config = ZenConfig::from_builder(&self);
                let shutdown = Arc::clone(&shutdown);
                move || run(config, Some(ready_tx), shutdown)
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {}
        }

        Ok(ZenWindow {
            _handle: Some(handle),
            shutdown,
        })
    }

    /// Spawn without blocking the calling thread.
    ///
    /// Returns immediately. Wayland setup and fade happen entirely
    /// in the background. If setup fails, it fails silently.
    ///
    /// Returns a [`ZenWindow`] handle. Dropping it removes overlays.
    pub fn spawn_nonblocking(self) -> ZenWindow {
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = std::thread::Builder::new()
            .name("wl-zenwindow".into())
            .spawn({
                let config = ZenConfig::from_builder(&self);
                let shutdown = Arc::clone(&shutdown);
                move || run(config, None, shutdown)
            })
            .ok();

        ZenWindow {
            _handle: handle,
            shutdown,
        }
    }
}

/// Handle to running zen overlays.
///
/// Overlay surfaces remain visible as long as this handle exists.
/// Dropping it disconnects from Wayland, removes overlays, and restores gamma.
pub struct ZenWindow {
    _handle: Option<JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>>,
    shutdown: Arc<AtomicBool>,
}

impl ZenWindow {
    /// Create a new builder to configure which outputs to dim.
    pub fn builder() -> ZenWindowBuilder {
        ZenWindowBuilder::new()
    }
}

impl Drop for ZenWindow {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self._handle.take() {
            // Give the event loop time to notice the shutdown signal and clean up.
            // The poll timeout in the event loop is 100ms, so 1 second is generous.
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while !handle.is_finished() {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

pub(crate) struct ZenConfig {
    pub(crate) skip_names: HashSet<String>,
    pub(crate) skip_active: bool,
    pub(crate) namespace: String,
    pub(crate) settle_delay: Option<Duration>,
    pub(crate) fade_duration: Option<Duration>,
    pub(crate) target_opacity: f64,
    pub(crate) color: [u8; 3],
    pub(crate) brightness: Option<f64>,
}

impl ZenConfig {
    fn from_builder(b: &ZenWindowBuilder) -> Self {
        Self {
            skip_names: b.skip_names.clone(),
            skip_active: b.skip_active,
            namespace: b.namespace.clone(),
            settle_delay: b.settle_delay,
            fade_duration: b.fade_duration,
            target_opacity: b.opacity,
            color: b.color,
            brightness: b.brightness,
        }
    }
}
