//! Commissioning windows on our devices, for other controllers.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embassy_time::{Duration, Instant};

use log::info;

use rs_matter::crypto::Crypto;
use rs_matter::persist::KvBlobStore;

use crate::api::{CommissioningWindow, PeekError, WindowStatus};
use crate::ota::OtaImageStore;
use crate::window::{self, WindowState};

use super::{failed, Ctx};

/// How long a commissioning window may stay open, as the spec allows.
const MIN_TIMEOUT_SECS: u16 = 3 * 60;
const MAX_TIMEOUT_SECS: u16 = 15 * 60;

/// A commissioning window we opened: its codes, and when it closes.
pub(super) struct OpenedWindow {
    manual_code: String,
    qr_code: String,
    qr_rows: Vec<String>,
    discriminator: u16,
    until: Instant,
}

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// Open a commissioning window on the device, unless one is open: ours is
    /// answered again, another controller's refused, as is a device with no
    /// free fabric.
    pub(super) async fn open_window(
        &self,
        node_id: u64,
        timeout_secs: Option<u16>,
    ) -> Result<CommissioningWindow, PeekError> {
        let timeout = timeout_secs.unwrap_or(MAX_TIMEOUT_SECS);
        if !(MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&timeout) {
            return Err(PeekError::failed(format!(
                "a commissioning window stays open 3 to 15 minutes, not {timeout} s"
            )));
        }

        let dev = self.lookup_device(node_id)?;
        let state = self.read_window(node_id).await?;

        match state.open {
            Some(true) if state.ours && self.windows.borrow().contains_key(&node_id) => {
                return Ok(self.window_info(node_id, &state));
            }
            Some(_) => {
                return Err(PeekError::failed(
                    "a commissioning window is open on it already: close it first",
                ))
            }
            None => {}
        }
        if let (Some(fabrics), Some(supported)) = (state.fabrics, state.supported_fabrics) {
            if fabrics >= supported {
                return Err(PeekError::failed(format!(
                    "it is on {fabrics} fabrics, as many as it supports: remove one first"
                )));
            }
        }

        let code = self
            .controller
            .open_window(dev.handle, timeout)
            .await
            .map_err(|e| failed("cannot open a commissioning window", e))?;

        let fp = &dev.fingerprint;
        let (manual_code, qr_code) = window::setup_codes(
            &code,
            fp.vendor_id.unwrap_or_default(),
            fp.product_id.unwrap_or_default(),
        )
        .map_err(|e| failed("cannot make its setup code", e))?;
        let qr_rows = window::qr_rows(&qr_code).unwrap_or_default();

        self.windows.borrow_mut().insert(
            node_id,
            OpenedWindow {
                manual_code,
                qr_code,
                qr_rows,
                discriminator: code.discriminator,
                until: Instant::now() + Duration::from_secs(timeout.into()),
            },
        );
        info!("service: opened a commissioning window on 0x{node_id:016X} for {timeout} s");

        let state = WindowState {
            open: Some(true),
            ours: true,
            ..state
        };

        Ok(self.window_info(node_id, &state))
    }

    /// Close the device's commissioning window, if open.
    pub(super) async fn close_window(
        &self,
        node_id: u64,
    ) -> Result<CommissioningWindow, PeekError> {
        let dev = self.lookup_device(node_id)?;
        let state = self.read_window(node_id).await?;

        if state.open.is_some() {
            self.controller
                .revoke_window(dev.handle)
                .await
                .map_err(|e| failed("cannot close its commissioning window", e))?;
            info!("service: closed the commissioning window on 0x{node_id:016X}");
        }
        self.windows.borrow_mut().remove(&node_id);

        let state = WindowState {
            open: None,
            ours: false,
            admin_vendor_id: None,
            ..state
        };

        Ok(self.window_info(node_id, &state))
    }

    /// The device's commissioning window.
    pub(super) async fn window(&self, node_id: u64) -> Result<CommissioningWindow, PeekError> {
        let state = self.read_window(node_id).await?;

        Ok(self.window_info(node_id, &state))
    }

    async fn read_window(&self, node_id: u64) -> Result<WindowState, PeekError> {
        let dev = self.lookup_device(node_id)?;

        self.controller
            .read_window(dev.handle)
            .await
            .map_err(|e| failed("cannot read its commissioning window", e))
    }

    /// The window as the device reports it, with our codes while it is ours;
    /// forgets the windows of ours that closed.
    fn window_info(&self, node_id: u64, state: &WindowState) -> CommissioningWindow {
        let now = Instant::now();
        let mut windows = self.windows.borrow_mut();

        windows.retain(|_, window| window.until > now);
        if !(state.open == Some(true) && state.ours) {
            windows.remove(&node_id);
        }
        let ours = windows.get(&node_id);

        CommissioningWindow {
            node_id,
            status: match state.open {
                None => WindowStatus::Closed,
                Some(true) => WindowStatus::Enhanced,
                Some(false) => WindowStatus::Basic,
            },
            manual_code: ours.map(|window| window.manual_code.clone()),
            qr_code: ours.map(|window| window.qr_code.clone()),
            qr_rows: ours
                .map(|window| window.qr_rows.clone())
                .unwrap_or_default(),
            discriminator: ours.map(|window| window.discriminator),
            remaining_secs: ours.map(|window| (window.until - now).as_secs()),
            opened_by_vendor: state.admin_vendor_id,
            fabrics: state.fabrics,
            supported_fabrics: state.supported_fabrics,
        }
    }
}
