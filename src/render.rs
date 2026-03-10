//! Surface types and rendering operations.
//!
//! Defines per-output [`Surface`] state and the pure functions that draw
//! overlays and update gamma. All Wayland buffer and protocol interactions
//! for rendering live here.
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
//! [`draw_surface`] picks the most efficient available path:
//!
//! 1. Alpha modifier + viewporter — 1×1 opaque buffer, compositor blends
//! 2. Viewporter only — 1×1 premultiplied ARGB, scaled up
//! 3. Neither — full-resolution buffer fill
//!
//! # Types
//!
//! - [`Surface`] — layer, viewport, alpha modifier, gamma, buffer state
//! - [`SurfaceConfig`] — `Pending` until compositor sends dimensions
//! - [`SurfaceRole`] — `Backdrop` vs `Overlay`
//! - [`GammaState`] — `Unavailable` / `Pending` / `Ready`

use std::io::Seek;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::fd::FromRawFd;

use smithay_client_toolkit::shell::wlr_layer::LayerSurface;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::Buffer;
use smithay_client_toolkit::shm::slot::SlotPool;
use wayland_client::protocol::wl_shm;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_v1::ZwlrGammaControlV1;

/// Surface configuration state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceConfig {
    Pending,
    Ready { width: u32, height: u32 },
}

impl SurfaceConfig {
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        match self {
            Self::Ready { width, height } if *width > 0 && *height > 0 => Some((*width, *height)),
            _ => None,
        }
    }
}

/// Per-surface gamma control state.
#[derive(Debug)]
pub enum GammaState {
    Unavailable,
    Pending(ZwlrGammaControlV1),
    Ready {
        control: ZwlrGammaControlV1,
        size: u32,
    },
}

/// Role of a surface in the dual-layer architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceRole {
    /// Backdrop at `Layer::Top` - above waybar/panels, safety net during transitions.
    Backdrop,
    /// Overlay at `Layer::Overlay` - above everything, handles fades.
    Overlay,
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
    pub config: SurfaceConfig,
}

/// Convert alpha from f64 (0.0–1.0) to u8 (0–255).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 0.0–1.0 × 255 fits in u8
pub fn alpha_to_u8(alpha: f64) -> u8 {
    (alpha * 255.0) as u8
}

/// Premultiply RGB values by alpha for ARGB8888 format.
#[allow(clippy::cast_possible_truncation)] // Math guarantees result fits in u8
pub fn premultiply_argb(r: u8, g: u8, b: u8, a: u8) -> u32 {
    let a16 = u16::from(a);
    let r_pre = ((u16::from(r) * a16 + 127) / 255) as u8;
    let g_pre = ((u16::from(g) * a16 + 127) / 255) as u8;
    let b_pre = ((u16::from(b) * a16 + 127) / 255) as u8;
    u32::from(a) << 24 | u32::from(r_pre) << 16 | u32::from(g_pre) << 8 | u32::from(b_pre)
}

/// Draw a surface at the given alpha level.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // alpha multiplier math
pub fn draw_surface(
    surface: &mut Surface,
    pool: &mut SlotPool,
    color: [u8; 3],
    alpha: u8,
    has_viewporter: bool,
) {
    let Some((width, height)) = surface.config.dimensions() else {
        return;
    };

    let width_i32 = width.cast_signed();
    let height_i32 = height.cast_signed();

    // Use alpha modifier if available (more efficient, compositor-side blending)
    if let Some(ref alpha_surf) = surface.alpha_modifier {
        let multiplier = (f64::from(alpha) / 255.0 * f64::from(u32::MAX)) as u32;
        alpha_surf.set_multiplier(multiplier);

        // Draw opaque buffer, let compositor handle alpha
        if has_viewporter {
            draw_1x1_scaled(surface, pool, color, 255, width_i32, height_i32);
        } else {
            draw_fullsize(surface, pool, color, 255);
        }
    } else if has_viewporter {
        // No alpha modifier, bake alpha into buffer
        draw_1x1_scaled(surface, pool, color, alpha, width_i32, height_i32);
    } else {
        draw_fullsize(surface, pool, color, alpha);
    }
}

fn draw_1x1_scaled(
    surface: &mut Surface,
    pool: &mut SlotPool,
    color: [u8; 3],
    alpha: u8,
    width: i32,
    height: i32,
) {
    let [red, green, blue] = color;

    let (buffer, canvas) = pool
        .create_buffer(1, 1, 4, wl_shm::Format::Argb8888)
        .expect("failed to create 1x1 buffer");

    let pixel = premultiply_argb(red, green, blue, alpha);
    canvas[..4].copy_from_slice(&pixel.to_ne_bytes());

    if let Some(ref viewport) = surface.viewport {
        viewport.set_destination(width, height);
    }
    surface
        .layer
        .wl_surface()
        .attach(Some(buffer.wl_buffer()), 0, 0);
    surface.layer.wl_surface().damage_buffer(0, 0, 1, 1);
    surface.layer.commit();
    surface.buffer = Some(buffer);
}

#[allow(clippy::cast_ptr_alignment)] // ARGB8888 buffer is 4-byte aligned
fn draw_fullsize(surface: &mut Surface, pool: &mut SlotPool, color: [u8; 3], alpha: u8) {
    let Some((width, height)) = surface.config.dimensions() else {
        return;
    };

    let width_i32 = width.cast_signed();
    let height_i32 = height.cast_signed();
    let stride = width_i32 * 4;
    let [red, green, blue] = color;

    surface.buffer = None;

    let (buffer, canvas) = pool
        .create_buffer(width_i32, height_i32, stride, wl_shm::Format::Argb8888)
        .expect("failed to create buffer");

    let pixel = premultiply_argb(red, green, blue, alpha);
    let pixels: &mut [u32] = unsafe {
        std::slice::from_raw_parts_mut(canvas.as_mut_ptr().cast::<u32>(), canvas.len() / 4)
    };
    pixels.fill(pixel);

    surface
        .layer
        .wl_surface()
        .attach(Some(buffer.wl_buffer()), 0, 0);
    surface
        .layer
        .wl_surface()
        .damage_buffer(0, 0, width_i32, height_i32);
    surface.layer.commit();
    surface.buffer = Some(buffer);
}

/// Update gamma for a surface.
pub fn update_gamma(surface: &Surface, brightness: f64) {
    if let GammaState::Ready { ref control, size } = surface.gamma {
        if let Ok(ramp) = create_gamma_ramp(size, brightness) {
            control.set_gamma(ramp.as_fd());
        }
    }
}

/// Create a gamma ramp file descriptor for the given size and brightness.
///
/// Returns a seeked-to-start memfd containing R, G, B ramps as u16 arrays.
#[allow(
    clippy::cast_precision_loss,      // usize->f64 acceptable for gamma ramp indices
    clippy::cast_possible_truncation, // Math guarantees result fits in u16
    clippy::cast_sign_loss            // Values are always positive
)]
pub fn create_gamma_ramp(size: u32, brightness: f64) -> std::io::Result<std::fs::File> {
    let name = std::ffi::CString::new("wl-zenwindow-gamma").unwrap();
    let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
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
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn surface_config_pending_has_no_dimensions() {
        assert_eq!(SurfaceConfig::Pending.dimensions(), None);
    }

    #[test]
    fn surface_config_ready_returns_dimensions() {
        let config = SurfaceConfig::Ready {
            width: 1920,
            height: 1080,
        };
        assert_eq!(config.dimensions(), Some((1920, 1080)));
    }

    #[test]
    fn surface_config_ready_zero_width_returns_none() {
        let config = SurfaceConfig::Ready {
            width: 0,
            height: 1080,
        };
        assert_eq!(config.dimensions(), None);
    }

    #[test]
    fn surface_config_ready_zero_height_returns_none() {
        let config = SurfaceConfig::Ready {
            width: 1920,
            height: 0,
        };
        assert_eq!(config.dimensions(), None);
    }

    #[test]
    fn surface_config_ready_both_zero_returns_none() {
        let config = SurfaceConfig::Ready {
            width: 0,
            height: 0,
        };
        assert_eq!(config.dimensions(), None);
    }

    #[test]
    fn premultiply_fully_opaque() {
        assert_eq!(premultiply_argb(255, 128, 0, 255), 0xFF_FF_80_00);
    }

    #[test]
    fn premultiply_fully_transparent() {
        assert_eq!(premultiply_argb(255, 255, 255, 0), 0x00_00_00_00);
    }

    #[test]
    fn premultiply_half_alpha() {
        let result = premultiply_argb(255, 0, 0, 128);
        let r = (result >> 16) & 0xFF;
        let a = (result >> 24) & 0xFF;
        assert_eq!(a, 128);
        assert_eq!(r, 128);
    }

    #[test]
    fn premultiply_channel_order() {
        let result = premultiply_argb(0xAA, 0xBB, 0xCC, 0xFF);
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
