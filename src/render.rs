//! Surface types and rendering operations.
//!
//! Defines per-output [`Surface`] state and the methods that draw overlays
//! and update gamma. All Wayland buffer and protocol interactions for
//! rendering live here.
//!
//! # Dual-layer architecture
//!
//! Each output gets two surfaces:
//!
//! - **Backdrop** (`Layer::Top`) — Always at target opacity
//! - **Overlay** (`Layer::Overlay`) — Fades during focus transitions
//!
//! The second layer exists to prevent flash. Focus detection via
//! `zwlr_foreign_toplevel_manager_v1` arrives *after* the compositor has
//! already rendered a frame with the new surfaces. Without the backdrop,
//! that frame shows the un-dimmed desktop underneath.
//!
//! The backdrop catches those early frames. It sits above panels but below
//! the overlay, always opaque. Once focus info arrives, the overlay handles
//! all transitions while the backdrop remains static.
//!
//! # Rendering paths
//!
//! [`Surface::draw`] picks the most efficient available path:
//!
//! 1. Alpha modifier + viewporter — 1×1 opaque buffer, compositor blends
//! 2. Viewporter only — 1×1 premultiplied ARGB, scaled up
//! 3. Neither — full-resolution buffer fill
//!
//! # Types
//!
//! - [`Surface`] — layer, viewport, alpha modifier, gamma, buffer state
//! - [`LayerShellHandshake`] — `Pending` until compositor sends dimensions
//! - [`SurfaceRole`] — `Backdrop` vs `Overlay`
//! - [`GammaState`] — `Unavailable` / `Pending` / `Ready`
//! - [`Color`] — overlay color
//! - [`Opacity`] — normalized opacity value (0.0–1.0)
//! - [`Brightness`] — monitor brightness level (0.0–1.0)

use std::io::Seek;
use std::io::Write;
use std::os::fd::AsFd;

use smithay_client_toolkit::shell::wlr_layer::Layer;
use smithay_client_toolkit::shell::wlr_layer::LayerSurface;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::Buffer;
use smithay_client_toolkit::shm::slot::SlotPool;
use wayland_client::protocol::wl_shm;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_v1::ZwlrGammaControlV1;

/// RGB color for overlay surfaces.
///
/// A simple color type that can be constructed from individual components
/// or converted from a `[u8; 3]` array.
///
/// # Examples
///
/// ```
/// use wl_zenwindow::Color;
///
/// let black = Color::BLACK;
/// let red = Color::new(255, 0, 0);
/// let from_array: Color = [0x1a, 0x1a, 0x1a].into();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Color {
    /// Red component (0–255).
    pub r: u8,
    /// Green component (0–255).
    pub g: u8,
    /// Blue component (0–255).
    pub b: u8,
}

impl Color {
    /// Black (`#000000`).
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };

    /// White (`#FFFFFF`).
    pub const WHITE: Self = Self {
        r: 255,
        g: 255,
        b: 255,
    };

    /// Create a new RGB color.
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

impl From<[u8; 3]> for Color {
    fn from([r, g, b]: [u8; 3]) -> Self {
        Self { r, g, b }
    }
}

impl From<Color> for [u8; 3] {
    fn from(rgb: Color) -> Self {
        [rgb.r, rgb.g, rgb.b]
    }
}

/// Opacity value, clamped to 0.0–1.0.
///
/// Represents how opaque a surface should be. `0.0` is fully transparent,
/// `1.0` is fully opaque. Values outside this range are clamped on construction.
///
/// Internally stores the normalized `f64` value, but provides conversion to
/// `u8` (0–255) for buffer operations.
///
/// # Examples
///
/// ```
/// use wl_zenwindow::Opacity;
///
/// let half = Opacity::new(0.5);
/// assert_eq!(half.as_u8(), 127);
///
/// let clamped = Opacity::new(1.5); // clamped to 1.0
/// assert_eq!(clamped.as_f64(), 1.0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Opacity(f64);

impl Opacity {
    /// Fully transparent.
    pub const TRANSPARENT: Self = Self(0.0);

    /// Fully opaque.
    pub const OPAQUE: Self = Self(1.0);

    /// Create a new opacity value, clamping to 0.0–1.0.
    #[must_use]
    pub fn new(value: f64) -> Self {
        Self(value.clamp(0.0, 1.0))
    }

    /// Get the opacity as a normalized f64 (0.0–1.0).
    #[must_use]
    pub fn as_f64(self) -> f64 {
        self.0
    }

    /// Get the opacity as a u8 (0–255) for buffer operations.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn as_u8(self) -> u8 {
        (self.0 * 255.0) as u8
    }
}

impl From<f64> for Opacity {
    fn from(value: f64) -> Self {
        Self::new(value)
    }
}

/// Monitor brightness level, clamped to 0.0–1.0.
///
/// Controls monitor brightness via the `zwlr_gamma_control_v1` protocol.
/// `0.0` is completely dark, `1.0` is normal brightness.
///
/// # Examples
///
/// ```
/// use wl_zenwindow::Brightness;
///
/// let dim = Brightness::new(0.7);
/// assert_eq!(dim.as_f64(), 0.7);
///
/// let clamped = Brightness::new(1.5); // clamped to 1.0
/// assert_eq!(clamped.as_f64(), 1.0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Brightness(f64);

impl Brightness {
    /// Completely dark.
    pub const DARK: Self = Self(0.0);

    /// Normal brightness.
    pub const NORMAL: Self = Self(1.0);

    /// Create a new brightness value, clamping to 0.0–1.0.
    #[must_use]
    pub fn new(value: f64) -> Self {
        Self(value.clamp(0.0, 1.0))
    }

    /// Get the brightness as a normalized f64 (0.0–1.0).
    #[must_use]
    pub fn as_f64(self) -> f64 {
        self.0
    }
}

impl From<f64> for Brightness {
    fn from(value: f64) -> Self {
        Self::new(value)
    }
}

/// State of the layer-shell configure handshake.
///
/// In Wayland's layer-shell protocol, clients can't just create a surface and
/// start drawing — they must wait for the compositor to tell them their
/// dimensions first. This happens through a "configure" event that the
/// compositor sends after the client commits an initial (empty) surface.
///
/// The handshake flow:
/// 1. Client creates a layer surface and commits it (no buffer attached)
/// 2. Compositor sends a `configure` event with the surface dimensions
/// 3. Client can now create appropriately-sized buffers and draw
///
/// Surfaces start [`Pending`](Self::Pending) and transition to
/// [`Ready`](Self::Ready) once the compositor sends dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerShellHandshake {
    /// Waiting for the compositor to send dimensions.
    ///
    /// The surface has been created and committed, but we haven't received
    /// the configure event yet. Drawing is not possible in this state.
    Pending,
    /// Compositor has configured the surface with these dimensions.
    ///
    /// The surface is ready to draw. Width and height may be zero if the
    /// compositor hasn't assigned real dimensions yet (e.g., the output
    /// is disabled).
    Ready { width: u32, height: u32 },
}

impl LayerShellHandshake {
    /// Returns dimensions if configured and non-zero.
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        match self {
            Self::Ready { width, height } if *width > 0 && *height > 0 => Some((*width, *height)),
            _ => None,
        }
    }
}

/// Per-surface gamma control state.
///
/// Tracks the `zwlr_gamma_control_v1` protocol handshake. Like layer-shell,
/// gamma control requires a back-and-forth with the compositor:
///
/// 1. Client requests gamma control for an output
/// 2. Compositor sends `gamma_size` event with the ramp size (or `failed`)
/// 3. Client can now set gamma ramps
///
/// If another client already owns gamma control (e.g., `wlsunset`), the
/// compositor sends `failed` and we fall back to [`Unavailable`](Self::Unavailable).
#[derive(Debug)]
pub enum GammaState {
    /// Gamma control not available (protocol missing or another client owns it).
    Unavailable,
    /// Waiting for the compositor to report gamma ramp size.
    Pending(ZwlrGammaControlV1),
    /// Ready to set gamma ramps.
    Ready {
        control: ZwlrGammaControlV1,
        size: u32,
    },
}

impl GammaState {
    /// Handle the `gamma_size` event — transition from `Pending` to `Ready`.
    ///
    /// If not currently `Pending`, this is a no-op.
    pub fn receive_size(&mut self, size: u32) {
        if let Self::Pending(control) = std::mem::replace(self, Self::Unavailable) {
            *self = Self::Ready { control, size };
        }
    }

    /// Handle the `failed` event — transition to `Unavailable`.
    ///
    /// Called when the compositor rejects our gamma control request
    /// (typically because another client like `wlsunset` already owns it).
    pub fn fail(&mut self) {
        *self = Self::Unavailable;
    }
}

/// Role of a surface in the dual-layer architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceRole {
    /// Backdrop at `Layer::Top` — above waybar/panels, safety net during transitions.
    Backdrop,
    /// Overlay at `Layer::Overlay` — above everything, handles fades.
    Overlay,
}

impl From<SurfaceRole> for Layer {
    fn from(role: SurfaceRole) -> Self {
        match role {
            SurfaceRole::Backdrop => Layer::Top,
            SurfaceRole::Overlay => Layer::Overlay,
        }
    }
}

/// A managed overlay surface.
pub struct Surface {
    pub output_name: Option<String>,
    pub role: SurfaceRole,
    pub layer: LayerSurface,
    pub viewport: Option<WpViewport>,
    pub alpha_modifier: Option<WpAlphaModifierSurfaceV1>,
    pub gamma: GammaState,
    pub buffer: Option<Buffer>,
    pub configure: LayerShellHandshake,
}

impl Surface {
    /// Draw the surface at the given opacity level.
    ///
    /// Picks the most efficient rendering path based on available protocols:
    /// 1. Alpha modifier + viewporter — 1×1 opaque buffer, compositor blends
    /// 2. Viewporter only — 1×1 premultiplied ARGB, scaled up
    /// 3. Neither — full-resolution buffer fill
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn draw(
        &mut self,
        pool: &mut SlotPool,
        color: Color,
        opacity: Opacity,
        has_viewporter: bool,
    ) {
        let Some((width, height)) = self.configure.dimensions() else {
            return;
        };

        // If compositor handles alpha, draw opaque and let it blend.
        // Otherwise, bake alpha into the buffer.
        let buffer_alpha = if let Some(ref alpha_surf) = self.alpha_modifier {
            let multiplier = (opacity.as_f64() * f64::from(u32::MAX)) as u32;
            alpha_surf.set_multiplier(multiplier);
            255
        } else {
            opacity.as_u8()
        };

        if has_viewporter {
            self.draw_1x1_scaled(
                pool,
                color,
                buffer_alpha,
                width.cast_signed(),
                height.cast_signed(),
            );
        } else {
            self.draw_fullsize(pool, color, buffer_alpha);
        }
    }

    /// Update gamma brightness for this surface.
    pub fn update_gamma(&self, brightness: Brightness) {
        if let GammaState::Ready { ref control, size } = self.gamma {
            if let Ok(ramp) = create_gamma_ramp(size, brightness.as_f64()) {
                control.set_gamma(ramp.as_fd());
            }
        }
    }

    fn draw_1x1_scaled(
        &mut self,
        pool: &mut SlotPool,
        color: Color,
        alpha: u8,
        width: i32,
        height: i32,
    ) {
        let (buffer, canvas) = pool
            .create_buffer(1, 1, 4, wl_shm::Format::Argb8888)
            .expect("failed to create 1x1 buffer");

        let pixel = premultiply_argb(color, alpha);
        canvas[..4].copy_from_slice(&pixel.to_ne_bytes());

        if let Some(ref viewport) = self.viewport {
            viewport.set_destination(width, height);
        }
        self.layer
            .wl_surface()
            .attach(Some(buffer.wl_buffer()), 0, 0);
        self.layer.wl_surface().damage_buffer(0, 0, 1, 1);
        self.layer.commit();
        self.buffer = Some(buffer);
    }

    #[allow(clippy::cast_ptr_alignment)] // ARGB8888 buffer is 4-byte aligned
    fn draw_fullsize(&mut self, pool: &mut SlotPool, color: Color, alpha: u8) {
        let Some((width, height)) = self.configure.dimensions() else {
            return;
        };

        let width_i32 = width.cast_signed();
        let height_i32 = height.cast_signed();
        let stride = width_i32 * 4;

        self.buffer = None;

        let (buffer, canvas) = pool
            .create_buffer(width_i32, height_i32, stride, wl_shm::Format::Argb8888)
            .expect("failed to create buffer");

        let pixel = premultiply_argb(color, alpha);
        let pixels: &mut [u32] = bytemuck::cast_slice_mut(canvas);
        pixels.fill(pixel);

        self.layer
            .wl_surface()
            .attach(Some(buffer.wl_buffer()), 0, 0);
        self.layer
            .wl_surface()
            .damage_buffer(0, 0, width_i32, height_i32);
        self.layer.commit();
        self.buffer = Some(buffer);
    }
}

/// Premultiply RGB color by alpha for ARGB8888 format.
#[allow(clippy::cast_possible_truncation)]
fn premultiply_argb(color: Color, alpha: u8) -> u32 {
    let a16 = u16::from(alpha);
    let r_pre = ((u16::from(color.r) * a16 + 127) / 255) as u8;
    let g_pre = ((u16::from(color.g) * a16 + 127) / 255) as u8;
    let b_pre = ((u16::from(color.b) * a16 + 127) / 255) as u8;
    u32::from(alpha) << 24 | u32::from(r_pre) << 16 | u32::from(g_pre) << 8 | u32::from(b_pre)
}

/// Create a gamma ramp file descriptor for the given size and brightness.
///
/// Returns a seeked-to-start memfd containing R, G, B ramps as u16 arrays.
#[allow(
    clippy::cast_precision_loss,      // usize->f64 acceptable for gamma ramp indices
    clippy::cast_possible_truncation, // Math guarantees result fits in u16
    clippy::cast_sign_loss            // Values are always positive
)]
fn create_gamma_ramp(size: u32, brightness: f64) -> std::io::Result<std::fs::File> {
    let fd = rustix::fs::memfd_create("wl-zenwindow-gamma", rustix::fs::MemfdFlags::CLOEXEC)?;
    let mut file = std::fs::File::from(fd);
    let n = size as usize;
    let divisor = n.saturating_sub(1).max(1) as f64;

    let mut ramp = Vec::with_capacity(n * 3 * 2);
    for _ in 0..3 {
        for i in 0..n {
            let value = (i as f64 / divisor * 65535.0 * brightness) as u16;
            ramp.extend_from_slice(&value.to_ne_bytes());
        }
    }

    file.write_all(&ramp)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    Ok(file)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn layer_shell_handshake_pending_has_no_dimensions() {
        assert_eq!(LayerShellHandshake::Pending.dimensions(), None);
    }

    #[test]
    fn layer_shell_handshake_ready_returns_dimensions() {
        let state = LayerShellHandshake::Ready {
            width: 1920,
            height: 1080,
        };
        assert_eq!(state.dimensions(), Some((1920, 1080)));
    }

    #[test]
    fn layer_shell_handshake_ready_zero_width_returns_none() {
        let state = LayerShellHandshake::Ready {
            width: 0,
            height: 1080,
        };
        assert_eq!(state.dimensions(), None);
    }

    #[test]
    fn layer_shell_handshake_ready_zero_height_returns_none() {
        let state = LayerShellHandshake::Ready {
            width: 1920,
            height: 0,
        };
        assert_eq!(state.dimensions(), None);
    }

    #[test]
    fn layer_shell_handshake_ready_both_zero_returns_none() {
        let state = LayerShellHandshake::Ready {
            width: 0,
            height: 0,
        };
        assert_eq!(state.dimensions(), None);
    }

    #[test]
    fn surface_role_to_layer_backdrop() {
        assert_eq!(Layer::from(SurfaceRole::Backdrop), Layer::Top);
    }

    #[test]
    fn surface_role_to_layer_overlay() {
        assert_eq!(Layer::from(SurfaceRole::Overlay), Layer::Overlay);
    }

    #[test]
    fn rgb_from_array() {
        let rgb: Color = [255, 128, 0].into();
        assert_eq!(rgb.r, 255);
        assert_eq!(rgb.g, 128);
        assert_eq!(rgb.b, 0);
    }

    #[test]
    fn rgb_to_array() {
        let arr: [u8; 3] = Color::new(10, 20, 30).into();
        assert_eq!(arr, [10, 20, 30]);
    }

    #[test]
    fn rgb_constants() {
        assert_eq!(Color::BLACK, Color::new(0, 0, 0));
        assert_eq!(Color::WHITE, Color::new(255, 255, 255));
    }

    #[test]
    fn opacity_clamps_above() {
        assert_eq!(Opacity::new(1.5).as_f64(), 1.0);
    }

    #[test]
    fn opacity_clamps_below() {
        assert_eq!(Opacity::new(-0.5).as_f64(), 0.0);
    }

    #[test]
    fn opacity_as_u8_zero() {
        assert_eq!(Opacity::new(0.0).as_u8(), 0);
    }

    #[test]
    fn opacity_as_u8_one() {
        assert_eq!(Opacity::new(1.0).as_u8(), 255);
    }

    #[test]
    fn opacity_as_u8_half() {
        assert_eq!(Opacity::new(0.5).as_u8(), 127);
    }

    #[test]
    fn opacity_constants() {
        assert_eq!(Opacity::TRANSPARENT.as_f64(), 0.0);
        assert_eq!(Opacity::OPAQUE.as_f64(), 1.0);
    }

    #[test]
    fn premultiply_fully_opaque() {
        assert_eq!(
            premultiply_argb(Color::new(255, 128, 0), 255),
            0xFF_FF_80_00
        );
    }

    #[test]
    fn premultiply_fully_transparent() {
        assert_eq!(premultiply_argb(Color::WHITE, 0), 0x00_00_00_00);
    }

    #[test]
    fn premultiply_half_alpha() {
        let result = premultiply_argb(Color::new(255, 0, 0), 128);
        let r = (result >> 16) & 0xFF;
        let a = (result >> 24) & 0xFF;
        assert_eq!(a, 128);
        assert_eq!(r, 128);
    }

    #[test]
    fn premultiply_channel_order() {
        let result = premultiply_argb(Color::new(0xAA, 0xBB, 0xCC), 0xFF);
        assert_eq!(result, 0xFF_AA_BB_CC);
    }

    fn read_ramp(size: u32, brightness: f64) -> Vec<u16> {
        let mut file = create_gamma_ramp(size, brightness).unwrap();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();
        buf.chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
            .collect()
    }

    #[test]
    fn gamma_ramp_zero_brightness() {
        let values = read_ramp(256, 0.0);
        assert_eq!(values.len(), 256 * 3);
        assert!(values.iter().all(|&v| v == 0));
    }

    #[test]
    fn gamma_ramp_full_brightness_linear() {
        let values = read_ramp(256, 1.0);
        assert_eq!(values.len(), 256 * 3);
        assert_eq!(values[0], 0);
        assert_eq!(values[255], 65535);
    }

    #[test]
    fn gamma_ramp_half_brightness() {
        let values = read_ramp(256, 0.5);
        assert_eq!(values[255], 32767);
    }

    #[test]
    fn gamma_ramp_three_identical_channels() {
        let values = read_ramp(256, 0.7);
        let r = &values[0..256];
        let g = &values[256..512];
        let b = &values[512..768];
        assert_eq!(r, g);
        assert_eq!(g, b);
    }
}
