use smithay_client_toolkit::shell::WaylandSurface;
use wayland_client::protocol::wl_shm;

use crate::state::ZenState;

pub(crate) fn premultiply_argb(r: u8, g: u8, b: u8, a: u8) -> u32 {
    let a16 = a as u16;
    let r_pre = ((r as u16 * a16 + 127) / 255) as u8;
    let g_pre = ((g as u16 * a16 + 127) / 255) as u8;
    let b_pre = ((b as u16 * a16 + 127) / 255) as u8;
    (a as u32) << 24 | (r_pre as u32) << 16 | (g_pre as u32) << 8 | b_pre as u32
}

impl ZenState {
    /// Draw a single surface at the given alpha (using viewporter if available).
    pub(crate) fn draw_surface_alpha(&mut self, idx: usize, alpha: u8) {
        let surface = &self.surfaces[idx];
        if !surface.configured || surface.width == 0 || surface.height == 0 {
            return;
        }

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
                viewport.set_destination(surface.width as i32, surface.height as i32);
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

            for surface in self.surfaces.iter_mut() {
                if surface.is_backdrop {
                    continue;
                }
                if self
                    .skip_names
                    .contains(surface.output_name.as_deref().unwrap_or(""))
                    || (self.skip_active
                        && self.active_output.as_deref() == surface.output_name.as_deref())
                {
                    continue;
                }
                if !surface.configured || surface.width == 0 || surface.height == 0 {
                    continue;
                }
                if let Some(ref viewport) = surface.viewport {
                    viewport.set_destination(surface.width as i32, surface.height as i32);
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

    pub(crate) fn draw_fullsize(&mut self, idx: usize, alpha: u8) {
        let surface = &self.surfaces[idx];
        if !surface.configured || surface.width == 0 || surface.height == 0 {
            return;
        }

        let w = surface.width as i32;
        let h = surface.height as i32;
        let stride = w * 4;
        let [r, g, b] = self.color;

        self.surfaces[idx].buffer = None;

        let (buffer, canvas) = self
            .pool
            .create_buffer(w, h, stride, wl_shm::Format::Argb8888)
            .expect("failed to create buffer");

        let pixel = premultiply_argb(r, g, b, alpha);
        let pixels: &mut [u32] = unsafe {
            std::slice::from_raw_parts_mut(canvas.as_mut_ptr() as *mut u32, canvas.len() / 4)
        };
        pixels.fill(pixel);

        let surface = &mut self.surfaces[idx];
        surface
            .layer
            .wl_surface()
            .attach(Some(buffer.wl_buffer()), 0, 0);
        surface.layer.wl_surface().damage_buffer(0, 0, w, h);
        surface.layer.commit();
        surface.buffer = Some(buffer);
    }
}
