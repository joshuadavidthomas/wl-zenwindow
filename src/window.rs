use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::error::SpawnError;
use crate::run::run;

/// Resolved configuration passed to the background thread.
///
/// Created from [`ZenWindowBuilder`] via the [`From`] impl. This is a plain
/// data struct with no builder methods — all validation and defaults are
/// handled by the builder.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    /// Output names to never create surfaces on.
    pub(crate) skip_names: HashSet<String>,
    /// Whether to leave the focused output undimmed.
    pub(crate) skip_active: bool,
    /// Layer-shell namespace for overlay surfaces.
    pub(crate) namespace: String,
    /// Delay before connecting to Wayland and creating surfaces.
    pub(crate) settle_delay: Option<Duration>,
    /// Duration of the initial fade-in animation.
    pub(crate) fade_duration: Option<Duration>,
    /// Target alpha for dimmed overlays (0.0–1.0, already clamped).
    pub(crate) target_opacity: f64,
    /// RGB overlay color.
    pub(crate) color: [u8; 3],
    /// Target brightness for gamma dimming. `None` means gamma is untouched.
    pub(crate) target_brightness: Option<f64>,
}

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

/// Builder configuration and spawn methods.
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
    #[must_use]
    pub fn skip_output(mut self, name: impl Into<String>) -> Self {
        self.skip_names.insert(name.into());
        self
    }

    /// Automatically skip the output that has the focused window.
    ///
    /// Uses `zwlr_foreign_toplevel_manager_v1` to detect which output
    /// has the currently activated toplevel. Falls back to dimming all
    /// outputs if the protocol is unavailable or no toplevel is focused.
    #[must_use]
    pub fn skip_active(mut self) -> Self {
        self.skip_active = true;
        self
    }

    /// Set the layer-shell namespace (default: "wl-zenwindow").
    #[must_use]
    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = ns.into();
        self
    }

    /// Wait before creating surfaces. Useful when launching alongside
    /// a UI window — gives the window time to render and gain focus
    /// before detecting the active output.
    #[must_use]
    pub fn settle_delay(mut self, delay: Duration) -> Self {
        self.settle_delay = Some(delay);
        self
    }

    /// Fade in the overlays over the given duration.
    #[must_use]
    pub fn fade_in(mut self, duration: Duration) -> Self {
        self.fade_duration = Some(duration);
        self
    }

    /// Set the overlay color as RGB (default: black `(0, 0, 0)`).
    #[must_use]
    pub fn color(mut self, r: u8, g: u8, b: u8) -> Self {
        self.color = [r, g, b];
        self
    }

    /// Set the final overlay opacity (0.0 = transparent, 1.0 = fully opaque).
    /// Default: 1.0.
    #[must_use]
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
    #[must_use]
    pub fn brightness(mut self, brightness: f64) -> Self {
        self.brightness = Some(brightness.clamp(0.0, 1.0));
        self
    }

    /// Spawn on a background thread.
    ///
    /// Blocks briefly until Wayland setup completes (typically a few
    /// milliseconds). Returns a [`ZenWindow`] handle. Dropping it removes
    /// overlays and restores gamma.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError`] if the Wayland connection fails, a required
    /// protocol is missing, setup fails after connecting, or the background
    /// thread cannot be created.
    pub fn spawn(self) -> Result<ZenWindow, SpawnError> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = std::thread::Builder::new()
            .name("wl-zenwindow".into())
            .spawn({
                let config = Config::from(&self);
                let shutdown = Arc::clone(&shutdown);
                move || run(&config, Some(ready_tx), &shutdown)
            })
            .map_err(SpawnError::ThreadSpawn)?;

        match ready_rx.recv() {
            Ok(()) => Ok(ZenWindow {
                handle: Some(handle),
                shutdown,
            }),
            Err(_) => {
                // Channel closed without a ready signal — the thread
                // returned an error or panicked during setup.
                match handle.join() {
                    Ok(Err(e)) => Err(e),
                    Err(payload) => std::panic::resume_unwind(payload),
                    Ok(Ok(())) => Ok(ZenWindow {
                        handle: None,
                        shutdown,
                    }),
                }
            }
        }
    }

    /// Spawn without blocking the calling thread.
    ///
    /// Returns immediately. Wayland setup and fade happen entirely
    /// in the background. If setup fails, it fails silently.
    ///
    /// Returns a [`ZenWindow`] handle. Dropping it removes overlays.
    #[must_use]
    pub fn spawn_nonblocking(self) -> ZenWindow {
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = std::thread::Builder::new()
            .name("wl-zenwindow".into())
            .spawn({
                let config = Config::from(&self);
                let shutdown = Arc::clone(&shutdown);
                move || run(&config, None, &shutdown)
            })
            .ok();

        ZenWindow { handle, shutdown }
    }
}

/// Handle to running zen overlays.
///
/// Overlay surfaces remain visible as long as this handle exists.
/// Dropping it disconnects from Wayland, removes overlays, and restores gamma.
pub struct ZenWindow {
    handle: Option<JoinHandle<Result<(), SpawnError>>>,
    shutdown: Arc<AtomicBool>,
}

/// Public API.
impl ZenWindow {
    /// Create a new builder to configure which outputs to dim.
    #[must_use]
    pub fn builder() -> ZenWindowBuilder {
        ZenWindowBuilder::new()
    }
}

/// Signals shutdown, waits for the background thread to clean up, and joins it.
impl Drop for ZenWindow {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
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

/// Converts builder settings into internal config.
impl From<&ZenWindowBuilder> for Config {
    fn from(b: &ZenWindowBuilder) -> Self {
        Self {
            skip_names: b.skip_names.clone(),
            skip_active: b.skip_active,
            namespace: b.namespace.clone(),
            settle_delay: b.settle_delay,
            fade_duration: b.fade_duration,
            target_opacity: b.opacity,
            color: b.color,
            target_brightness: b.brightness,
        }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // Exact equality is intentional in these tests
mod tests {
    use super::*;

    #[test]
    fn builder_defaults() {
        let b = ZenWindowBuilder::new();
        assert!(b.skip_names.is_empty());
        assert!(!b.skip_active);
        assert_eq!(b.namespace, "wl-zenwindow");
        assert!(b.settle_delay.is_none());
        assert!(b.fade_duration.is_none());
        assert_eq!(b.opacity, 1.0);
        assert_eq!(b.color, [0, 0, 0]);
        assert!(b.brightness.is_none());
    }

    #[test]
    fn opacity_clamped_above() {
        let b = ZenWindow::builder().opacity(1.5);
        assert_eq!(b.opacity, 1.0);
    }

    #[test]
    fn opacity_clamped_below() {
        let b = ZenWindow::builder().opacity(-0.5);
        assert_eq!(b.opacity, 0.0);
    }

    #[test]
    fn opacity_within_range() {
        let b = ZenWindow::builder().opacity(0.7);
        assert!((b.opacity - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn brightness_clamped_above() {
        let b = ZenWindow::builder().brightness(2.0);
        assert_eq!(b.brightness, Some(1.0));
    }

    #[test]
    fn brightness_clamped_below() {
        let b = ZenWindow::builder().brightness(-1.0);
        assert_eq!(b.brightness, Some(0.0));
    }

    #[test]
    fn skip_output_accumulates() {
        let b = ZenWindow::builder()
            .skip_output("DP-1")
            .skip_output("eDP-1");
        assert!(b.skip_names.contains("DP-1"));
        assert!(b.skip_names.contains("eDP-1"));
        assert_eq!(b.skip_names.len(), 2);
    }

    #[test]
    fn builder_chaining() {
        let b = ZenWindow::builder()
            .skip_active()
            .namespace("custom")
            .color(255, 0, 128)
            .opacity(0.5)
            .brightness(0.3)
            .settle_delay(Duration::from_millis(200))
            .fade_in(Duration::from_millis(500));

        assert!(b.skip_active);
        assert_eq!(b.namespace, "custom");
        assert_eq!(b.color, [255, 0, 128]);
        assert!((b.opacity - 0.5).abs() < f64::EPSILON);
        assert_eq!(b.brightness, Some(0.3));
        assert_eq!(b.settle_delay, Some(Duration::from_millis(200)));
        assert_eq!(b.fade_duration, Some(Duration::from_millis(500)));
    }

    #[test]
    fn config_from_builder_transfers_all_fields() {
        let b = ZenWindow::builder()
            .skip_output("HDMI-1")
            .skip_active()
            .namespace("test-ns")
            .settle_delay(Duration::from_millis(100))
            .fade_in(Duration::from_secs(1))
            .opacity(0.8)
            .color(10, 20, 30)
            .brightness(0.6);

        let config = Config::from(&b);

        assert!(config.skip_names.contains("HDMI-1"));
        assert!(config.skip_active);
        assert_eq!(config.namespace, "test-ns");
        assert_eq!(config.settle_delay, Some(Duration::from_millis(100)));
        assert_eq!(config.fade_duration, Some(Duration::from_secs(1)));
        assert!((config.target_opacity - 0.8).abs() < f64::EPSILON);
        assert_eq!(config.color, [10, 20, 30]);
        assert_eq!(config.target_brightness, Some(0.6));
    }

    #[test]
    fn config_from_builder_defaults() {
        let b = ZenWindowBuilder::new();
        let config = Config::from(&b);

        assert!(config.skip_names.is_empty());
        assert!(!config.skip_active);
        assert_eq!(config.namespace, "wl-zenwindow");
        assert!(config.settle_delay.is_none());
        assert!(config.fade_duration.is_none());
        assert_eq!(config.target_opacity, 1.0);
        assert_eq!(config.color, [0, 0, 0]);
        assert!(config.target_brightness.is_none());
    }
}
