//! Dim Wayland monitors using wlr-layer-shell overlay surfaces.
//!
//! Creates translucent overlay surfaces on Wayland outputs with configurable
//! color, opacity, and optional brightness dimming. Follows the focused window
//! across monitors automatically. Works with any compositor that supports the
//! wlr-layer-shell protocol (Sway, Hyprland, Niri, River, Labwc, etc.).
//!
//! # Quick start
//!
//! ```no_run
//! use wl_zenwindow::ZenWindow;
//! use std::time::Duration;
//!
//! // Dim all monitors except the one with the focused window
//! let zen = ZenWindow::builder()
//!     .opacity(0.85)
//!     .fade_in(Duration::from_millis(500))
//!     .spawn()
//!     .expect("failed to start zen overlays");
//!
//! // Overlays stay up as long as the handle is alive
//! // Dropping it removes overlays and restores gamma
//! drop(zen);
//! ```
//!
//! # Choreographed launch
//!
//! Use [`spawn_with()`](ZenWindowBuilder::spawn_with) to own the sequencing
//! when launching an application. The library fades all outputs to target
//! opacity, calls your callback, then reveals the output where the new
//! window appears.
//!
//! ```no_run
//! # use wl_zenwindow::ZenWindow;
//! # use std::time::Duration;
//! # use std::process::Command;
//! let zen = ZenWindow::builder()
//!     .opacity(0.85)
//!     .fade_in(Duration::from_millis(300))
//!     .spawn_with(|| {
//!         Command::new("my-fullscreen-app").spawn().unwrap();
//!     })
//!     .expect("failed to start zen overlays");
//! ```
//!
//! # Non-blocking spawn
//!
//! For UI applications that can't block the main thread, use
//! [`spawn_nonblocking()`](ZenWindowBuilder::spawn_nonblocking). Setup and
//! fade happen entirely in the background. If setup fails, it fails silently.
//!
//! ```no_run
//! # use wl_zenwindow::ZenWindow;
//! # use std::time::Duration;
//! let _zen = ZenWindow::builder()
//!     .settle_delay(Duration::from_millis(100))
//!     .fade_in(Duration::from_millis(500))
//!     .spawn_nonblocking();
//! ```
//!
//! # Handling errors
//!
//! [`spawn()`](ZenWindowBuilder::spawn) returns a [`Result`] with
//! [`SpawnError`] variants you can match on to fall back gracefully:
//!
//! ```no_run
//! use wl_zenwindow::{ZenWindow, SpawnError};
//!
//! let zen = match ZenWindow::builder().spawn() {
//!     Ok(handle) => Some(handle),
//!     Err(SpawnError::WaylandConnection(_)) => {
//!         eprintln!("not running on Wayland, skipping overlays");
//!         None
//!     }
//!     Err(SpawnError::MissingProtocol { protocol, .. }) => {
//!         eprintln!("compositor missing {protocol}, skipping overlays");
//!         None
//!     }
//!     Err(e) => {
//!         eprintln!("overlay setup failed: {e}");
//!         None
//!     }
//! };
//! ```
//!
//! # Configuration
//!
//! All configuration is done through [`ZenWindowBuilder`]. Every setting has a
//! sensible default — the only required call is one of
//! [`spawn()`](ZenWindowBuilder::spawn),
//! [`spawn_with()`](ZenWindowBuilder::spawn_with), or
//! [`spawn_nonblocking()`](ZenWindowBuilder::spawn_nonblocking).
//!
//! | Method | Default | Range | Description |
//! |--------|---------|-------|-------------|
//! | [`opacity()`](ZenWindowBuilder::opacity) | `1.0` | `0.0` – `1.0` (clamped) | Final overlay opacity. `0.0` = transparent, `1.0` = fully opaque. |
//! | [`brightness()`](ZenWindowBuilder::brightness) | unset | `0.0` – `1.0` (clamped) | Monitor brightness via gamma. Requires `zwlr_gamma_control_v1`. |
//! | [`color()`](ZenWindowBuilder::color) | `(0, 0, 0)` | RGB `u8` triplet | Overlay color. |
//! | [`fade_in()`](ZenWindowBuilder::fade_in) | `None` | `Duration` | Fade-in duration (ease-out curve). `None` = instant. |
//! | [`settle_delay()`](ZenWindowBuilder::settle_delay) | `None` | `Duration` | Delay before creating surfaces. Runs on the background thread. |
//! | [`skip_output()`](ZenWindowBuilder::skip_output) | empty | — | Skip specific outputs by Wayland name (e.g. `"DP-1"`). |
//! | [`namespace()`](ZenWindowBuilder::namespace) | `"wl-zenwindow"` | string | Layer-shell namespace for the overlay surfaces. |
//!
//! # Wayland protocols
//!
//! The library requires a small set of core protocols and optionally uses
//! several others to improve rendering or enable features. Missing optional
//! protocols degrade gracefully — they never prevent the library from working.
//!
//! | Protocol | Required | Enables | Fallback without it |
//! |----------|----------|---------|---------------------|
//! | `wlr-layer-shell-v1` | **yes** | Overlay surfaces | — |
//! | `wl_compositor` | **yes** | Surface creation | — |
//! | `wl_shm` | **yes** | Shared-memory buffers | — |
//! | `zwlr_foreign_toplevel_manager_v1` | no | Focus tracking across outputs | All outputs are dimmed (no active-output skip) |
//! | `zwlr_gamma_control_v1` | no | Brightness dimming | `brightness()` setting is ignored |
//! | `wp_viewporter` | no | Efficient 1-pixel overlay scaled to output size | Full-resolution buffer fill (higher memory) |
//! | `wp_alpha_modifier_v1` | no | Hardware-composited alpha | Software alpha via premultiplied ARGB buffers |
//!
//! # How it works
//!
//! Understanding the internals isn't necessary to use the library, but it
//! helps explain some of the configuration options and edge cases.
//!
//! ## Two surfaces per output
//!
//! Each output gets two layer-shell surfaces: an **overlay** on `Layer::Overlay`
//! and a **backdrop** on `Layer::Top`. The overlay is the visible dim surface
//! that participates in transitions and skip logic. The backdrop is always
//! at the target opacity and sits above panels/waybar but below the overlay.
//!
//! The backdrop exists to prevent a flash of the desktop wallpaper. There's a
//! brief window between when surfaces are created and when the
//! foreign-toplevel protocol reports which window is focused. Without the
//! backdrop, the compositor would render a frame showing the un-dimmed desktop
//! through the gap.
//!
//! ## Focus tracking
//!
//! The library automatically binds `zwlr_foreign_toplevel_manager_v1` and
//! watches for activated toplevel events. Each toplevel reports which output
//! it's on. When the activated toplevel changes outputs, the library
//! cross-fades: the overlay on the newly active output fades out while the
//! previously active output returns to its dimmed state.
//!
//! If the protocol isn't available (e.g., on compositors that don't implement
//! it), all non-skipped outputs are dimmed uniformly.
//!
//! ## Gamma control contention
//!
//! The `zwlr_gamma_control_v1` protocol only allows one client per output.
//! If another client already holds gamma control (like `wlsunset` or
//! `gammastep`), the compositor rejects the request and the library silently
//! skips brightness dimming for that output. Your overlay still works — you
//! just don't get the gamma adjustment.
//!
//! When the [`ZenWindow`] handle is dropped, gamma ramps are automatically
//! restored to their previous values.

mod app;
mod dim;
mod error;
mod render;
mod run;
mod wayland;
mod window;

pub use error::SpawnError;
pub use render::Brightness;
pub use render::Color;
pub use render::Opacity;
pub use window::ZenWindow;
pub use window::ZenWindowBuilder;
