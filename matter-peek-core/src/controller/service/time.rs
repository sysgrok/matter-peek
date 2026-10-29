//! The clocks of our devices: reading them, and setting them from ours.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use log::{info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::error::Error;
use rs_matter::persist::KvBlobStore;
use rs_matter::utils::sync::MaybeSend;

use crate::api::display::{hex_u64, utc_label, utc_offset_label};
use crate::api::{DeviceTime, PeekError, TimeSetup, TimeSetupStep};
use crate::model::DeviceHandle;
use crate::ota::OtaImageStore;
use crate::time::{self, DstPeriod, HostZone};

use super::{failed, Ctx};

impl<'a, C: Crypto, S: KvBlobStore + MaybeSend, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// The device's clock and time setup.
    pub(super) async fn device_time(&self, node_id: u64) -> Result<DeviceTime, PeekError> {
        let dev = self.lookup_device(node_id)?;

        self.read_time(dev.handle).await
    }

    /// Set the device's clock from ours, making us its time source even if it
    /// has another.
    pub(super) async fn set_device_time(&self, node_id: u64) -> Result<TimeSetup, PeekError> {
        let dev = self.lookup_device(node_id)?;

        self.setup_time(dev.handle, true).await
    }

    /// Set the device's clock from ours: its UTC time; its time zone and
    /// daylight saving time, if it keeps them; us as its trusted time source,
    /// if it takes its time from one and has none (or, with `replace_source`,
    /// another).
    pub(super) async fn setup_time(
        &self,
        handle: DeviceHandle,
        replace_source: bool,
    ) -> Result<TimeSetup, PeekError> {
        let before = self.read_time(handle).await?;
        if !before.supported {
            return Err(PeekError::failed(
                "it keeps no clock we can set (it has no Time Synchronization cluster)",
            ));
        }

        let mut steps = Vec::new();
        let now = time::host_time();

        match &now {
            Some(now) => steps.push(step(
                format!("UTC time {}", utc_label(time::unix_ms(now.utc_us))),
                self.controller.set_utc_time(handle, now).await,
            )),
            None => steps.push(not_set("UTC time", "this host has no clock")),
        }

        if before.features.time_zone {
            match now.and_then(|now| time::host_zone(now.utc_us)) {
                Some(zone) => {
                    let set = self.controller.set_time_zone(handle, &zone).await;
                    let dst_required = *set.as_ref().unwrap_or(&false);
                    steps.push(step(zone_label(&zone), set.map(|_| ())));

                    if dst_required {
                        let max = before.max_dst_offsets.unwrap_or(1).max(1) as usize;
                        let dst = &zone.dst[..zone.dst.len().min(max)];
                        steps.push(step(
                            dst_label(dst),
                            self.controller.set_dst_offsets(handle, dst, max).await,
                        ));
                    }
                }
                None => steps.push(not_set("time zone", "this host's time zone is unknown")),
            }
        }

        if before.features.time_sync_client {
            let ours = before.trusted_source.as_ref().is_some_and(|s| s.ours);

            if !ours && (replace_source || before.trusted_source.is_none()) {
                let our_node_id = self.our_node_id(handle)?;
                steps.push(step(
                    format!("trusted time source: us, node {}", hex_u64(our_node_id)),
                    self.controller
                        .set_trusted_time_source(handle, our_node_id)
                        .await,
                ));
            }
        }

        for s in &steps {
            match &s.error {
                None => info!("time: 0x{:016X}: set {}", handle.node_id, s.what),
                Some(e) => warn!("time: 0x{:016X}: not set {}: {e}", handle.node_id, s.what),
            }
        }

        let time = match self.read_time(handle).await {
            Ok(time) => time,
            Err(_) => before,
        };

        Ok(TimeSetup { steps, time })
    }

    async fn read_time(&self, handle: DeviceHandle) -> Result<DeviceTime, PeekError> {
        let our_node_id = self.our_node_id(handle)?;

        self.controller
            .read_time(handle, our_node_id)
            .await
            .map_err(|e| failed("cannot read its clock", e))
    }

    /// Our node ID on the device's fabric.
    pub(super) fn our_node_id(&self, handle: DeviceHandle) -> Result<u64, PeekError> {
        self.fabrics
            .borrow()
            .iter()
            .find(|fabric| fabric.fab_idx == handle.fabric_index)
            .map(|fabric| fabric.node_id)
            .ok_or_else(|| PeekError::failed("its fabric is not ours any more"))
    }
}

fn step(what: String, result: Result<(), Error>) -> TimeSetupStep {
    TimeSetupStep {
        what,
        error: result
            .err()
            .map(|e| crate::explain::describe(e.code()).to_string()),
    }
}

fn not_set(what: &str, why: &str) -> TimeSetupStep {
    TimeSetupStep {
        what: what.to_string(),
        error: Some(why.to_string()),
    }
}

/// `time zone Europe/Sofia, +02:00`.
fn zone_label(zone: &HostZone) -> String {
    match &zone.name {
        Some(name) => format!("time zone {name}, {}", utc_offset_label(zone.offset)),
        None => format!("time zone {}", utc_offset_label(zone.offset)),
    }
}

/// `daylight saving time +01:00 from ... until ...`, or `no daylight saving
/// time`.
fn dst_label(dst: &[DstPeriod]) -> String {
    if dst.is_empty() {
        return "no daylight saving time".to_string();
    }

    let periods: Vec<String> = dst
        .iter()
        .map(|period| {
            let from = utc_label(time::unix_ms(period.start_us));
            match period.until_us {
                Some(until) => format!(
                    "{} from {from} until {}",
                    utc_offset_label(period.offset),
                    utc_label(time::unix_ms(until))
                ),
                None => format!("{} from {from}", utc_offset_label(period.offset)),
            }
        })
        .collect();

    format!("daylight saving time {}", periods.join("; "))
}
