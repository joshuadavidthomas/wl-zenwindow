use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::error::SpawnError;
use crate::render::Brightness;
use crate::render::Color;
use crate::render::Opacity;
use crate::run::run;

/// How the background thread was spawned — determines lifecycle phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnMode {
    /// Standard spawn. Focus tracking is always on.
    Standard,
    /// Choreographed launch: fade in all outputs, run callback, reveal.
    SpawnWith,
}

/// Resolved configuration passed to the background thread.
///
/// Created from [`ZenWindowBuilder`] at spawn time. This is a plain
/// data struct with no builder methods — all validation and defaults are
/// handled by the builder.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    /// Output names to never create surfaces on.
    pub(crate) skip_names: HashSet<String>,
    /// How this instance was spawned.
    pub(crate) spawn_mode: SpawnMode,
    /// Layer-shell namespace for overlay surfaces.
    pub(crate) namespace: String,
    /// Delay before connecting to Wayland and creating surfaces.
    pub(crate) settle_delay: Option<Duration>,
    /// Duration of the initial fade-in animation.
    pub(crate) fade_duration: Option<Duration>,
    /// Target opacity for dimmed overlays.
    pub(crate) target_opacity: Opacity,
    /// Overlay color.
    pub(crate) color: Color,
    /// Target brightness for gamma dimming. `None` means gamma is untouched.
    pub(crate) target_brightness: Option<Brightness>,
}

/// Builder for configuring which outputs to dim.
pub struct ZenWindowBuilder {
    skip_names: HashSet<String>,
    namespace: String,
    settle_delay: Option<Duration>,
    fade_duration: Option<Duration>,
    opacity: Opacity,
    color: Color,
    brightness: Option<Brightness>,
}

/// Builder configuration and spawn methods.
impl ZenWindowBuilder {
    fn new() -> Self {
        Self {
            skip_names: HashSet::new(),
            namespace: "wl-zenwindow".into(),
            settle_delay: None,
            fade_duration: None,
            opacity: Opacity::OPAQUE,
            color: Color::BLACK,
            brightness: None,
        }
    }

    /// Skip an output by its Wayland name (e.g., "DP-6", "eDP-1").
    #[must_use]
    pub fn skip_output(mut self, name: impl Into<String>) -> Self {
        self.skip_names.insert(name.into());
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

    /// Set the overlay color (default: black).
    ///
    /// # Examples
    ///
    /// ```
    /// use wl_zenwindow::{ZenWindow, Color};
    ///
    /// let _ = ZenWindow::builder().color(Color::new(255, 0, 0));
    /// let _ = ZenWindow::builder().color([0x1a, 0x1a, 0x1a]);
    /// let _ = ZenWindow::builder().color(Color::BLACK);
    /// ```
    #[must_use]
    pub fn color(mut self, color: impl Into<Color>) -> Self {
        self.color = color.into();
        self
    }

    /// Set the final overlay opacity (default: fully opaque).
    ///
    /// Accepts [`Opacity`] or a raw `f64` (clamped to 0.0–1.0).
    /// `0.0` = transparent, `1.0` = fully opaque.
    ///
    /// # Examples
    ///
    /// ```
    /// use wl_zenwindow::{ZenWindow, Opacity};
    ///
    /// // Using a raw f64
    /// let _ = ZenWindow::builder().opacity(0.85);
    ///
    /// // Using Opacity directly
    /// let _ = ZenWindow::builder().opacity(Opacity::new(0.85));
    /// ```
    #[must_use]
    pub fn opacity(mut self, opacity: impl Into<Opacity>) -> Self {
        self.opacity = opacity.into();
        self
    }

    /// Dim monitor brightness via gamma control.
    ///
    /// `0.0` = completely dark, `1.0` = normal brightness.
    /// Default: unset (gamma untouched).
    ///
    /// Uses the `zwlr_gamma_control_v1` protocol. Falls back gracefully
    /// if another client (e.g., `wlsunset`) already controls gamma.
    ///
    /// # Examples
    ///
    /// ```
    /// use wl_zenwindow::{ZenWindow, Brightness};
    ///
    /// let _ = ZenWindow::builder().brightness(0.7);
    /// let _ = ZenWindow::builder().brightness(Brightness::new(0.7));
    /// ```
    #[must_use]
    pub fn brightness(mut self, brightness: impl Into<Brightness>) -> Self {
        self.brightness = Some(brightness.into());
        self
    }

    /// Spawn on a background thread.
    ///
    /// Blocks briefly until Wayland setup completes (typically a few
    /// milliseconds). Returns a [`ZenWindow`] handle. Dropping it removes
    /// overlays and restores gamma.
    ///
    /// Focus tracking is automatic: the focused output stays undimmed
    /// while all other outputs are dimmed. If the compositor doesn't
    /// support `zwlr_foreign_toplevel_manager_v1`, all outputs are dimmed.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError`] if the Wayland connection fails, a required
    /// protocol is missing, setup fails after connecting, or the background
    /// thread cannot be created.
    pub fn spawn(self) -> Result<ZenWindow, SpawnError> {
        self.spawn_inner(SpawnMode::Standard, None)
    }

    /// Spawn with a choreographed launch sequence.
    ///
    /// 1. Fades in ALL outputs to target opacity (everything goes dark)
    /// 2. Calls the callback (your app launches behind the opaque overlay)
    /// 3. Watches for the new window via toplevel protocol
    /// 4. Once the window settles (fullscreen detected, or timeout), reveals
    ///    the active output's overlay
    ///
    /// After the reveal, focus tracking is active — the focused output stays
    /// undimmed while others remain dimmed.
    ///
    /// The callback runs on the background thread after the fade-in completes.
    /// `FnOnce` since it's called exactly once. `Send + 'static` because it
    /// crosses into the background thread.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError`] if the Wayland connection fails, a required
    /// protocol is missing, setup fails after connecting, or the background
    /// thread cannot be created.
    pub fn spawn_with<F>(self, f: F) -> Result<ZenWindow, SpawnError>
    where
        F: FnOnce() + Send + 'static,
    {
        self.spawn_inner(SpawnMode::SpawnWith, Some(Box::new(f)))
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
                let config = self.to_config(SpawnMode::Standard);
                let shutdown = Arc::clone(&shutdown);
                move || run(&config, None, &shutdown, None)
            })
            .ok();

        ZenWindow { handle, shutdown }
    }

    fn spawn_inner(
        self,
        spawn_mode: SpawnMode,
        callback: Option<Box<dyn FnOnce() + Send + 'static>>,
    ) -> Result<ZenWindow, SpawnError> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = std::thread::Builder::new()
            .name("wl-zenwindow".into())
            .spawn({
                let config = self.to_config(spawn_mode);
                let shutdown = Arc::clone(&shutdown);
                move || run(&config, Some(ready_tx), &shutdown, callback)
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
            // The calloop dispatch timeout in the event loop is 100ms, so 1 second
            // is generous.
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

impl ZenWindowBuilder {
    fn to_config(&self, spawn_mode: SpawnMode) -> Config {
        Config {
            skip_names: self.skip_names.clone(),
            spawn_mode,
            namespace: self.namespace.clone(),
            settle_delay: self.settle_delay,
            fade_duration: self.fade_duration,
            target_opacity: self.opacity,
            color: self.color,
            target_brightness: self.brightness,
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
        assert_eq!(b.namespace, "wl-zenwindow");
        assert!(b.settle_delay.is_none());
        assert!(b.fade_duration.is_none());
        assert_eq!(b.opacity, Opacity::OPAQUE);
        assert_eq!(b.color, Color::BLACK);
        assert!(b.brightness.is_none());
    }

    #[test]
    fn opacity_clamped_above() {
        let b = ZenWindow::builder().opacity(1.5);
        assert_eq!(b.opacity.as_f64(), 1.0);
    }

    #[test]
    fn opacity_clamped_below() {
        let b = ZenWindow::builder().opacity(-0.5);
        assert_eq!(b.opacity.as_f64(), 0.0);
    }

    #[test]
    fn opacity_within_range() {
        let b = ZenWindow::builder().opacity(0.7);
        assert!((b.opacity.as_f64() - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn brightness_clamped_above() {
        let b = ZenWindow::builder().brightness(2.0);
        assert_eq!(
            b.brightness.map(super::super::render::Brightness::as_f64),
            Some(1.0)
        );
    }

    #[test]
    fn brightness_clamped_below() {
        let b = ZenWindow::builder().brightness(-1.0);
        assert_eq!(
            b.brightness.map(super::super::render::Brightness::as_f64),
            Some(0.0)
        );
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
            .namespace("custom")
            .color([255, 0, 128])
            .opacity(0.5)
            .brightness(0.3)
            .settle_delay(Duration::from_millis(200))
            .fade_in(Duration::from_millis(500));

        assert_eq!(b.namespace, "custom");
        assert_eq!(b.color, Color::new(255, 0, 128));
        assert!((b.opacity.as_f64() - 0.5).abs() < f64::EPSILON);
        assert_eq!(
            b.brightness.map(super::super::render::Brightness::as_f64),
            Some(0.3)
        );
        assert_eq!(b.settle_delay, Some(Duration::from_millis(200)));
        assert_eq!(b.fade_duration, Some(Duration::from_millis(500)));
    }

    #[test]
    fn config_transfers_all_fields() {
        let b = ZenWindow::builder()
            .skip_output("HDMI-1")
            .namespace("test-ns")
            .settle_delay(Duration::from_millis(100))
            .fade_in(Duration::from_secs(1))
            .opacity(0.8)
            .color([10, 20, 30])
            .brightness(0.6);

        let config = b.to_config(SpawnMode::Standard);

        assert!(config.skip_names.contains("HDMI-1"));
        assert_eq!(config.spawn_mode, SpawnMode::Standard);
        assert_eq!(config.namespace, "test-ns");
        assert_eq!(config.settle_delay, Some(Duration::from_millis(100)));
        assert_eq!(config.fade_duration, Some(Duration::from_secs(1)));
        assert!((config.target_opacity.as_f64() - 0.8).abs() < f64::EPSILON);
        assert_eq!(config.color, Color::new(10, 20, 30));
        assert_eq!(
            config
                .target_brightness
                .map(super::super::render::Brightness::as_f64),
            Some(0.6)
        );
    }

    #[test]
    fn config_spawn_with_mode() {
        let b = ZenWindowBuilder::new();
        let config = b.to_config(SpawnMode::SpawnWith);
        assert_eq!(config.spawn_mode, SpawnMode::SpawnWith);
    }

    #[test]
    fn config_defaults() {
        let b = ZenWindowBuilder::new();
        let config = b.to_config(SpawnMode::Standard);

        assert!(config.skip_names.is_empty());
        assert_eq!(config.spawn_mode, SpawnMode::Standard);
        assert_eq!(config.namespace, "wl-zenwindow");
        assert!(config.settle_delay.is_none());
        assert!(config.fade_duration.is_none());
        assert_eq!(config.target_opacity.as_f64(), 1.0);
        assert_eq!(config.color, Color::BLACK);
        assert!(config.target_brightness.is_none());
    }
}
