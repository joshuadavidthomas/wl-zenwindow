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
