use std::io::Seek;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::fd::FromRawFd;

use wayland_client::Connection;
use wayland_client::Dispatch;
use wayland_client::QueueHandle;
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1;
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_v1::ZwlrGammaControlV1;
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_v1::{
    self,
};

use crate::state::GammaState;
use crate::state::ZenState;

/// Clamp an `f64` to the `u16` range and convert.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn clamp_u16(value: f64) -> u16 {
    // After clamping to [0, 65535], the cast is lossless.
    value.round().clamp(0.0, f64::from(u16::MAX)) as u16
}

/// Build a linear gamma ramp scaled by `brightness` and return it as a memfd.
///
/// Creates an in-memory file containing three identical channels (R, G, B),
/// each with `size` entries of `u16` values forming a linear ramp from 0 to
/// `65535 * brightness`. The file is seeked back to the start so it can be
/// passed directly to `zwlr_gamma_control_v1::set_gamma`.
pub(crate) fn create_gamma_ramp(size: u32, brightness: f64) -> std::io::Result<std::fs::File> {
    let name = std::ffi::CString::new("wl-zenwindow-gamma").unwrap();
    let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
    let n = usize::try_from(size).expect("u32 always fits in usize");
    let entries = size.saturating_sub(1).max(1);

    let mut ramp = Vec::with_capacity(n * 3 * 2);
    for _ in 0..3u8 {
        for i in 0..size {
            // i / entries is in [0, 1], so scaled value is in [0, 65535].
            let normalized = f64::from(i) / f64::from(entries);
            let value = clamp_u16(normalized * 65_535.0 * brightness);
            ramp.extend_from_slice(&value.to_ne_bytes());
        }
    }

    file.write_all(&ramp)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    Ok(file)
}

/// Gamma control methods.
impl ZenState {
    /// Apply a dimmed gamma ramp to all non-skipped surfaces that have gamma control.
    ///
    /// Iterates over surfaces and sends a scaled gamma ramp to each output's
    /// `zwlr_gamma_control_v1` instance. Surfaces without gamma control
    /// (because the protocol is unavailable or another client holds it) are
    /// silently skipped.
    pub(crate) fn set_gamma_dimmed(&self, brightness: f64) {
        for (idx, surface) in self.surfaces.iter().enumerate() {
            if self.is_skipped(idx) {
                continue;
            }
            if let GammaState::Ready { ref control, size } = surface.gamma {
                match create_gamma_ramp(size, brightness) {
                    Ok(file) => {
                        control.set_gamma(file.as_fd());
                    }
                    Err(e) => {
                        eprintln!("wl-zenwindow: gamma ramp error: {e}");
                    }
                }
            }
        }
    }
}

/// No-op dispatch — the gamma control manager has no client-side events.
impl Dispatch<ZwlrGammaControlManagerV1, ()> for ZenState {
    fn event(
        _: &mut Self,
        _: &ZwlrGammaControlManagerV1,
        _: <ZwlrGammaControlManagerV1 as wayland_client::Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Handles per-output gamma control events.
///
/// - `GammaSize` — transitions the surface's gamma state from `Pending` to
///   `Ready` with the ramp size reported by the compositor.
/// - `Failed` — another client already holds gamma control on this output;
///   resets the surface's gamma state to `Unavailable`.
impl Dispatch<ZwlrGammaControlV1, usize> for ZenState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrGammaControlV1,
        event: zwlr_gamma_control_v1::Event,
        surface_idx: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => {
                if let Some(surface) = state.surfaces.get_mut(*surface_idx) {
                    if let GammaState::Pending(control) =
                        std::mem::replace(&mut surface.gamma, GammaState::Unavailable)
                    {
                        surface.gamma = GammaState::Ready { control, size };
                    }
                }
            }
            zwlr_gamma_control_v1::Event::Failed => {
                if let Some(surface) = state.surfaces.get_mut(*surface_idx) {
                    surface.gamma = GammaState::Unavailable;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn read_ramp(size: u32, brightness: f64) -> Vec<u16> {
        let mut file = create_gamma_ramp(size, brightness).unwrap();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();
        buf.chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
            .collect()
    }

    #[test]
    fn gamma_ramp_size_one_full_brightness() {
        let values = read_ramp(1, 1.0);
        // 3 channels × 1 entry each
        assert_eq!(values.len(), 3);
        // i=0, divisor=max(0,1)=1 → 0/1 * 65535 = 0
        assert!(values.iter().all(|&v| v == 0));
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

        // Each channel should be a linear ramp 0..65535
        for channel in 0..3 {
            let start = channel * 256;
            assert_eq!(values[start], 0);
            assert_eq!(values[start + 255], 65535);
            // Monotonically increasing
            for i in 1..256 {
                assert!(values[start + i] >= values[start + i - 1]);
            }
        }
    }

    #[test]
    fn gamma_ramp_half_brightness() {
        let values = read_ramp(256, 0.5);
        // Last entry of first channel: round(65535 * 0.5) = round(32767.5) = 32768
        assert_eq!(values[255], 32768);
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

    #[test]
    fn gamma_ramp_size_zero() {
        let values = read_ramp(0, 1.0);
        assert!(values.is_empty());
    }
}
