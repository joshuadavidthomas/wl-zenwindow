//! Dim Wayland monitors using wlr-layer-shell overlay surfaces.
//!
//! Creates overlay surfaces on selected Wayland outputs with configurable color,
//! opacity, and optional brightness dimming. Follows the focused window across
//! monitors automatically. Works with any compositor that supports the
//! wlr-layer-shell protocol (sway, niri, hyprland, river, etc.).
//!
//! # Quick start
//!
//! ```no_run
//! use wl_zenwindow::ZenWindow;
//! use std::time::Duration;
//!
//! // Dim all monitors except the one with the focused window
//! let zen = ZenWindow::builder()
//!     .skip_active()
//!     .fade_in(Duration::from_millis(500))
//!     .spawn()
//!     .expect("failed to start zen overlays");
//!
//! // Monitors stay dimmed until dropped
//! drop(zen);
//! ```
//!
//! # Non-blocking spawn
//!
//! For UI applications that can't afford to block the main thread:
//!
//! ```no_run
//! # use wl_zenwindow::ZenWindow;
//! # use std::time::Duration;
//! let zen = ZenWindow::builder()
//!     .skip_active()
//!     .settle_delay(Duration::from_millis(100))
//!     .fade_in(Duration::from_millis(500))
//!     .spawn_nonblocking();
//! ```

mod error;
mod gamma;
mod handlers;
mod run;
mod state;
mod surface;
mod toplevel;
mod transition;
mod window;

pub use error::SpawnError;
pub use window::ZenWindow;
pub use window::ZenWindowBuilder;
