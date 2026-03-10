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

use crate::state::ZenState;

pub(crate) fn create_gamma_ramp(size: u32, brightness: f64) -> std::io::Result<std::fs::File> {
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

impl ZenState {
    pub(crate) fn set_gamma_dimmed(&self, brightness: f64) {
        for (idx, surface) in self.surfaces.iter().enumerate() {
            if self.is_skipped(idx) {
                continue;
            }
            if let (Some(ref ctrl), Some(size)) = (&surface.gamma_control, surface.gamma_size) {
                match create_gamma_ramp(size, brightness) {
                    Ok(file) => {
                        ctrl.set_gamma(file.as_fd());
                    }
                    Err(e) => {
                        eprintln!("wl-zenwindow: gamma ramp error: {e}");
                    }
                }
            }
        }
    }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for ZenState {
    fn event(
        _: &mut Self,
        _: &ZwlrGammaControlManagerV1,
        _: <ZwlrGammaControlManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

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
                    surface.gamma_size = Some(size);
                }
            }
            zwlr_gamma_control_v1::Event::Failed => {
                if let Some(surface) = state.surfaces.get_mut(*surface_idx) {
                    surface.gamma_control = None;
                    surface.gamma_size = None;
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
        // Last entry of first channel: 65535 * 0.5 = 32767
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

    #[test]
    fn gamma_ramp_size_zero() {
        let values = read_ramp(0, 1.0);
        assert!(values.is_empty());
    }
}
