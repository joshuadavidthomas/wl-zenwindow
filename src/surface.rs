use smithay_client_toolkit::shell::WaylandSurface;
use wayland_client::protocol::wl_shm;

use crate::state::should_skip;
use crate::state::ZenState;

/// Convert a `u32` surface dimension to `i32` for Wayland APIs.
///
/// Wayland protocol methods expect `i32` for coordinates and sizes.
/// Surface dimensions from configure events are always reasonable
/// monitor sizes, so this conversion is safe in practice.
fn i32_dim(value: u32) -> i32 {
    i32::try_from(value).expect("surface dimension exceeds i32::MAX")
}

/// Convert RGBA components into a single premultiplied ARGB `u32` pixel.
///
/// Wayland's `ARGB8888` format expects premultiplied alpha, meaning each
/// color channel is scaled by the alpha value before packing.
/// Scale a single channel by alpha using integer rounding: `(c * a + 127) / 255`.
///
/// Result is always ≤ 255 because both inputs are ≤ 255.
#[allow(clippy::cast_possible_truncation)]
fn premultiply_channel(channel: u8, alpha: u16) -> u8 {
    // Max: (255 * 255 + 127) / 255 = 255. Truncation cannot happen.
    ((u16::from(channel) * alpha + 127) / 255) as u8
}

pub(crate) fn premultiply_argb(r: u8, g: u8, b: u8, a: u8) -> u32 {
    let a16 = u16::from(a);
    let r_pre = premultiply_channel(r, a16);
    let g_pre = premultiply_channel(g, a16);
    let b_pre = premultiply_channel(b, a16);
    u32::from(a) << 24 | u32::from(r_pre) << 16 | u32::from(g_pre) << 8 | u32::from(b_pre)
}

/// Surface drawing methods.
impl ZenState {
    /// Draw a single surface at the given alpha (using viewporter if available).
    pub(crate) fn draw_surface_alpha(&mut self, idx: usize, alpha: u8) {
        let Some((width, height)) = self.surfaces[idx].config.dimensions() else {
            return;
        };

        if self.viewporter.is_some() {
            let [r, g, b] = self.color;
            let (buffer, canvas) = self
                .pool
                .create_buffer(1, 1, 4, wl_shm::Format::Argb8888)
                .expect("failed to create 1x1 buffer");

            let pixel = premultiply_argb(r, g, b, alpha);
            canvas[..4].copy_from_slice(&pixel.to_ne_bytes());

            let surface = &mut self.surfaces[idx];
            if let Some(ref viewport) = surface.viewport {
                viewport.set_destination(i32_dim(width), i32_dim(height));
            }
            surface
                .layer
                .wl_surface()
                .attach(Some(buffer.wl_buffer()), 0, 0);
            surface.layer.wl_surface().damage_buffer(0, 0, 1, 1);
            surface.layer.commit();
            surface.buffer = Some(buffer);
        } else {
            self.draw_fullsize(idx, alpha);
        }
    }

    /// Batch-draw all non-skipped surfaces at the given alpha.
    /// Uses a single shared 1x1 buffer via viewporter when available.
    pub(crate) fn draw_dimmed(&mut self, alpha: u8) {
        if self.viewporter.is_some() {
            let [r, g, b] = self.color;
            let (buffer, canvas) = self
                .pool
                .create_buffer(1, 1, 4, wl_shm::Format::Argb8888)
                .expect("failed to create 1x1 buffer");

            let pixel = premultiply_argb(r, g, b, alpha);
            canvas[..4].copy_from_slice(&pixel.to_ne_bytes());

            for surface in &mut self.surfaces {
                if surface.is_backdrop()
                    || should_skip(
                        surface.role,
                        surface.output_name.as_deref(),
                        &self.skip_names,
                        self.skip_active,
                        self.active_output.as_deref(),
                    )
                {
                    continue;
                }
                let Some((width, height)) = surface.config.dimensions() else {
                    continue;
                };
                if let Some(ref viewport) = surface.viewport {
                    viewport.set_destination(i32_dim(width), i32_dim(height));
                }
                surface
                    .layer
                    .wl_surface()
                    .attach(Some(buffer.wl_buffer()), 0, 0);
                surface.layer.wl_surface().damage_buffer(0, 0, 1, 1);
                surface.layer.commit();
            }

            // Keep the shared buffer alive
            if let Some(first) = self.surfaces.first_mut() {
                first.buffer = Some(buffer);
            }
        } else {
            for idx in 0..self.surfaces.len() {
                if !self.is_skipped(idx) {
                    self.draw_fullsize(idx, alpha);
                }
            }
        }
    }

    /// Draw a surface by filling a full-resolution buffer with the overlay color.
    ///
    /// This is the fallback path used when `wp_viewporter` is unavailable.
    /// Allocates a buffer matching the surface dimensions and fills every
    /// pixel with the premultiplied overlay color at the given alpha.
    pub(crate) fn draw_fullsize(&mut self, idx: usize, alpha: u8) {
        let Some((width, height)) = self.surfaces[idx].config.dimensions() else {
            return;
        };

        let signed_w = i32_dim(width);
        let signed_h = i32_dim(height);
        let stride = signed_w * 4;
        let color = self.color;

        self.surfaces[idx].buffer = None;

        let (buffer, canvas) = self
            .pool
            .create_buffer(signed_w, signed_h, stride, wl_shm::Format::Argb8888)
            .expect("failed to create buffer");

        let pixel = premultiply_argb(color[0], color[1], color[2], alpha);
        // SAFETY: `SlotPool` buffers are 4-byte aligned and sized as
        // `width * height * 4`, so reinterpreting as `&mut [u32]` is valid.
        #[allow(clippy::cast_ptr_alignment)]
        let pixels: &mut [u32] = unsafe {
            std::slice::from_raw_parts_mut(canvas.as_mut_ptr().cast::<u32>(), canvas.len() / 4)
        };
        pixels.fill(pixel);

        let surface = &mut self.surfaces[idx];
        surface
            .layer
            .wl_surface()
            .attach(Some(buffer.wl_buffer()), 0, 0);
        surface
            .layer
            .wl_surface()
            .damage_buffer(0, 0, signed_w, signed_h);
        surface.layer.commit();
        surface.buffer = Some(buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // 255 * 128 / 255 ≈ 128, with rounding: (255*128+127)/255 = 128
        assert_eq!(r, 128);
    }

    #[test]
    fn premultiply_all_max() {
        assert_eq!(premultiply_argb(255, 255, 255, 255), 0xFF_FF_FF_FF);
    }

    #[test]
    fn premultiply_all_zero() {
        assert_eq!(premultiply_argb(0, 0, 0, 0), 0x00_00_00_00);
    }

    #[test]
    fn premultiply_channel_order() {
        // With full alpha, channels pass through unchanged
        let result = premultiply_argb(0xAA, 0xBB, 0xCC, 0xFF);
        assert_eq!(result, 0xFF_AA_BB_CC);
    }
}
