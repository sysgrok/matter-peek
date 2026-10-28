//! Health checks of the listed devices: on demand, and in the background
//! (when the service syncs periodically): soon after a device is seen, every
//! few hours after, and soon after its subscription tells of a restart or of
//! new faults.

use alloc::collections::BTreeMap;
use alloc::format;

use embassy_futures::select::select;
use embassy_time::{Duration, Instant, Timer};

use log::info;

use rs_matter::crypto::Crypto;
use rs_matter::persist::KvBlobStore;

use crate::api::{HealthCheck, PeekError};
use crate::health;
use crate::ota::OtaImageStore;
use crate::time;

use super::locks::Mode;
use super::{failed, Ctx};

/// How often a device's health is checked again.
const CHECK_PERIOD: Duration = Duration::from_secs(6 * 3600);

/// How long after a failed check it is tried again.
const RETRY_PERIOD: Duration = Duration::from_secs(30 * 60);

/// How long after a restart its device is checked: once it has settled.
pub(super) const RESTART_SETTLE: Duration = Duration::from_secs(30);

/// How often the background checks look for devices due, unless woken.
const TICK: Duration = Duration::from_secs(60);

/// The pause between two background checks.
const GAP: Duration = Duration::from_secs(1);

/// The health checks of the devices, and when they are due.
#[derive(Default)]
pub(super) struct HealthChecks {
    /// The last check of each device, by node ID.
    checks: BTreeMap<u64, HealthCheck>,
    /// When each device was last checked or tried, and whether that worked.
    tried: BTreeMap<u64, (Instant, bool)>,
    /// The devices to check again soon, and from when.
    due: BTreeMap<u64, Instant>,
}

impl HealthChecks {
    /// The last check of `node_id`.
    pub(super) fn check(&self, node_id: u64) -> Option<HealthCheck> {
        self.checks.get(&node_id).cloned()
    }

    /// Drop what is known of `node_id`.
    pub(super) fn forget(&mut self, node_id: u64) {
        self.checks.remove(&node_id);
        self.tried.remove(&node_id);
        self.due.remove(&node_id);
    }

    /// Whether `node_id` is due for a check at `now`.
    fn is_due(&self, node_id: u64, now: Instant) -> bool {
        if let Some(from) = self.due.get(&node_id) {
            return *from <= now;
        }

        match self.tried.get(&node_id) {
            None => true,
            Some((at, true)) => now >= *at + CHECK_PERIOD,
            Some((at, false)) => now >= *at + RETRY_PERIOD,
        }
    }
}

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// Check the health of a listed device, and note it.
    pub(super) async fn check_health(&self, node_id: u64) -> Result<HealthCheck, PeekError> {
        let dev = self.lookup_device(node_id)?;

        let read = self.controller.read_health_check(dev.handle).await;

        let mut checks = self.health.borrow_mut();
        checks.due.remove(&node_id);
        let read = match read {
            Ok(read) => read,
            Err(e) => {
                checks.tried.insert(node_id, (Instant::now(), false));
                return Err(failed(
                    &format!("device 0x{node_id:016X} did not answer"),
                    e,
                ));
            }
        };

        let checked_ms = time::host_time().map(|now| time::unix_ms(now.utc_us));
        let check = health::judge(&read, checked_ms);
        checks.tried.insert(node_id, (Instant::now(), true));
        checks.checks.insert(node_id, check.clone());
        drop(checks);

        self.set_online(node_id, true);
        self.tell_devices();

        Ok(check)
    }

    /// Check a device's health again after `delay`.
    pub(super) fn health_due(&self, node_id: u64, delay: Duration) {
        self.health
            .borrow_mut()
            .due
            .insert(node_id, Instant::now() + delay);
        self.health_wake.notify();
    }

    /// Check the health of the listed devices as they fall due, one at a
    /// time, if the service syncs periodically; else never ends.
    pub(super) async fn run_health_checks(&self) -> core::convert::Infallible {
        loop {
            select(Timer::after(TICK), self.health_wake.wait()).await;

            // What answers is known once a sync ran.
            if !self.periodic_sync || !self.synced.get() {
                continue;
            }

            while let Some(node_id) = self.next_health_check() {
                let guard = self.locks.queue(&self.device_keys(node_id, Mode::Read));
                guard.ready().await;

                if let Err(e) = self.check_health(node_id).await {
                    info!("health check of node 0x{node_id:016X}: {e}");
                }
                drop(guard);

                Timer::after(GAP).await;
            }
        }
    }

    /// A listed device due for a check, that can be checked now: it answers,
    /// and is awake.
    fn next_health_check(&self) -> Option<u64> {
        let now = Instant::now();
        let online = self.online.borrow();
        let checks = self.health.borrow();

        self.listed_devices()
            .into_iter()
            .filter(|device| {
                let node_id = device.handle.node_id;
                online.as_ref().is_some_and(|o| o.contains(&node_id))
                    && self.commissioned_node.get() != Some(node_id)
                    && !self.asleep(device.handle)
            })
            .map(|device| device.handle.node_id)
            .find(|node_id| checks.is_due(*node_id, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_a_device_is_due() {
        let mut checks = HealthChecks::default();
        let start = Instant::from_secs(1_000_000);

        // Never tried.
        assert!(checks.is_due(1, start));

        checks.tried.insert(1, (start, true));
        assert!(!checks.is_due(1, start + RETRY_PERIOD));
        assert!(checks.is_due(1, start + CHECK_PERIOD));

        checks.tried.insert(1, (start, false));
        assert!(checks.is_due(1, start + RETRY_PERIOD));

        // A restart makes it due, after a while.
        checks.tried.insert(1, (start, true));
        checks.due.insert(1, start + RESTART_SETTLE);
        assert!(!checks.is_due(1, start));
        assert!(checks.is_due(1, start + RESTART_SETTLE));
    }
}
