//! Our registrations for the Check-Ins of intermittently connected devices.

use alloc::vec::Vec;

use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};

use log::{info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::error::ErrorCode;
use rs_matter::persist::KvBlobStore;

use crate::api::PeekError;
use crate::icd;
use crate::model::{DeviceHandle, IcdClient};
use crate::ota::OtaImageStore;

use super::{failed, Ctx};

/// How long a wake-up asks a device to stay awake: the most it guarantees.
const STAY_AWAKE_MS: u32 = 30_000;
/// How long past its idle time a wake-up waits for a device to wake up.
const WAKE_MARGIN: Duration = Duration::from_secs(30);
/// How often a wake-up looks whether the device woke up meanwhile.
const WAKE_POLL: Duration = Duration::from_millis(200);
/// The idle time of a device that does not say: the most CHIP LIT devices use.
const DEFAULT_IDLE_SECS: u32 = 3600;
/// How long a device stays awake after a message of its, if it does not say:
/// the least a LIT may.
const DEFAULT_ACTIVE_THRESHOLD_MS: u32 = 5000;
/// How long a device is kept awake at most, unless asked again: a forgotten
/// one would drain its battery.
const STAY_AWAKE_FOR: Duration = Duration::from_secs(10 * 60);
/// How long before a device kept awake would fall asleep it is asked again:
/// the ask takes a round trip or two.
const RENEW_BEFORE_MS: u32 = 10_000;
/// How often the devices kept awake are looked at.
const STAY_AWAKE_TICK: Duration = Duration::from_secs(1);

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// Keep a listed device awake for a while (see `MatterPeek::wake_device`):
    /// one that sleeps long and sleeps now is waited for first.
    pub(super) async fn wake(&self, node_id: u64) -> Result<u32, PeekError> {
        let dev = self.lookup_device(node_id)?;

        self.until_awake(node_id).await?;

        let promised = self
            .controller
            .stay_active(dev.handle, STAY_AWAKE_MS)
            .await
            .map_err(|e| failed("it did not stay awake", e))?;
        info!("icd: node 0x{node_id:016X} stays awake for {promised} ms");

        self.note_awake(node_id, promised);

        Ok(promised)
    }

    /// Wait until a listed device can be reached: at once unless it sleeps long
    /// and sleeps now, else until it wakes up, at most its idle time.
    async fn until_awake(&self, node_id: u64) -> Result<(), PeekError> {
        let dev = self.lookup_device(node_id)?;
        if !self.has_icd_client(dev.handle) || self.awake_ms(node_id).is_some() {
            return Ok(());
        }

        let idle_secs = dev
            .details
            .icd
            .as_ref()
            .and_then(|icd| icd.idle_mode_secs)
            .unwrap_or(DEFAULT_IDLE_SECS);
        let deadline = Instant::now() + Duration::from_secs(idle_secs.into()) + WAKE_MARGIN;

        info!("icd: waiting for node 0x{node_id:016X} to wake up");
        while self.awake_ms(node_id).is_none() {
            if Instant::now() >= deadline {
                return Err(PeekError::failed("it did not wake up"));
            }

            Timer::after(WAKE_POLL).await;
        }

        Ok(())
    }

    /// Keep a listed device awake (see `MatterPeek::stay_awake`); answers the
    /// holder's ID.
    pub(super) fn stay_awake(&self, node_id: u64) -> Result<u64, PeekError> {
        self.lookup_device(node_id)?;

        let holder = self.next_watcher.get();
        self.next_watcher.set(holder + 1);

        self.stay_awake
            .borrow_mut()
            .entry(node_id)
            .or_default()
            .insert(holder, Instant::now() + STAY_AWAKE_FOR);
        info!("icd: keeping node 0x{node_id:016X} awake");

        self.tell_devices();

        Ok(holder)
    }

    /// Stop keeping a device awake, as far as `holder` goes.
    pub(super) fn end_stay_awake(&self, node_id: u64, holder: u64) {
        let ended = {
            let mut stay_awake = self.stay_awake.borrow_mut();
            let Some(holders) = stay_awake.get_mut(&node_id) else {
                return;
            };
            holders.remove(&holder);
            let ended = holders.is_empty();
            if ended {
                stay_awake.remove(&node_id);
            }
            ended
        };

        if ended {
            info!("icd: letting node 0x{node_id:016X} sleep");
            self.tell_devices();
        }
    }

    /// Let a device sleep again, whoever kept it awake.
    pub(super) fn let_sleep(&self, node_id: u64) {
        if self.stay_awake.borrow_mut().remove(&node_id).is_some() {
            info!("icd: letting node 0x{node_id:016X} sleep");
            self.tell_devices();
        }
    }

    /// Whether a device is kept awake.
    pub(super) fn stays_awake(&self, node_id: u64) -> bool {
        self.stay_awake.borrow().contains_key(&node_id)
    }

    /// Keep the devices asked to stay awake so: ask each again before it would
    /// fall asleep, once it is awake. Never returns.
    pub(super) async fn run_stay_awake(&self) -> core::convert::Infallible {
        loop {
            Timer::after(STAY_AWAKE_TICK).await;

            let now = Instant::now();
            let mut expired = Vec::new();
            self.stay_awake.borrow_mut().retain(|node_id, holders| {
                holders.retain(|_, until| *until > now);
                if holders.is_empty() {
                    expired.push(*node_id);
                }
                !holders.is_empty()
            });
            for node_id in &expired {
                info!("icd: kept node 0x{node_id:016X} awake long enough; letting it sleep");
            }
            if !expired.is_empty() {
                self.tell_devices();
            }

            let kept: Vec<u64> = self.stay_awake.borrow().keys().copied().collect();
            for node_id in kept {
                // Asleep, it is asked once it wakes up.
                let due = self
                    .awake_ms(node_id)
                    .is_some_and(|ms| ms < RENEW_BEFORE_MS);
                let Some(dev) = due.then(|| self.lookup_device(node_id).ok()).flatten() else {
                    continue;
                };

                match self.controller.stay_active(dev.handle, STAY_AWAKE_MS).await {
                    Ok(promised) => self.note_awake(node_id, promised),
                    Err(e) => warn!("icd: node 0x{node_id:016X} did not stay awake: {e:?}"),
                }
            }
        }
    }

    /// Whether a device sleeps long and sleeps now, as far as we know: it does
    /// not answer.
    pub(super) fn asleep(&self, handle: DeviceHandle) -> bool {
        self.has_icd_client(handle) && self.awake_ms(handle.node_id).is_none()
    }

    /// How much longer a device that sleeps long is awake, as far as we know.
    pub(super) fn awake_ms(&self, node_id: u64) -> Option<u32> {
        let until = *self.awake.borrow().get(&node_id)?;
        let left = until.checked_duration_since(Instant::now())?.as_millis();

        (left > 0).then(|| left.min(u32::MAX.into()) as u32)
    }

    /// Note that a device that sleeps long is awake, for `ms` at least.
    pub(super) fn note_awake(&self, node_id: u64, ms: u32) {
        let until = Instant::now() + Duration::from_millis(ms.into());

        let longer = {
            let mut awake = self.awake.borrow_mut();
            let longer = awake.get(&node_id).is_none_or(|known| *known < until);
            if longer {
                awake.insert(node_id, until);
            }
            longer
        };

        if longer {
            // Its subscription, if waiting for it, can be made now.
            if let Some(watch) = self.subs.borrow().get(&node_id) {
                watch.sleepy.awake_until(until);
            }
            self.tell_devices();
        }
    }

    /// Note that a device that sleeps long is awake, for as long as it stays
    /// so after one of its messages: its active threshold.
    pub(super) fn note_message_of(&self, node_id: u64) {
        let threshold = self
            .lookup_device(node_id)
            .ok()
            .and_then(|dev| dev.details.icd)
            .and_then(|icd| icd.active_threshold_ms)
            .map_or(DEFAULT_ACTIVE_THRESHOLD_MS, u32::from);

        self.note_awake(node_id, threshold);
    }

    /// Take the Check-Ins from `payloads`, until the channel closes, and the
    /// reports for subscriptions we lost: either says that a device is awake,
    /// and without our subscription.
    pub(super) async fn run_check_ins(&self, payloads: async_channel::Receiver<Vec<u8>>) {
        loop {
            match select(payloads.recv(), self.reports.wait_stray()).await {
                Either::First(Ok(payload)) => self.take_check_in(&payload).await,
                Either::First(Err(_)) => break,
                Either::Second(node_id) => {
                    info!(
                        "icd: node 0x{node_id:016X} reported for a subscription we lost; \
                         subscribing again"
                    );
                    self.note_message_of(node_id);
                    self.resubscribe(node_id);
                }
            }
        }
    }

    /// Make our subscription of a device again now, if we have one: it is
    /// awake, and lost it.
    fn resubscribe(&self, node_id: u64) {
        if let Some(watch) = self.subs.borrow().get(&node_id) {
            watch.retry.notify();
        }
    }

    /// Take a Check-In: whose it is, whether it is a fresh one; then make our
    /// subscription of the device again, which it lost, while it is awake.
    async fn take_check_in(&self, payload: &[u8]) {
        let mut clients = match self.store.load_icd_clients() {
            Ok(clients) => clients,
            Err(e) => {
                warn!("icd: a Check-In, but no registrations: {e:?}");
                return;
            }
        };

        let Some((index, counter)) = icd::whose(self.crypto, &clients, payload) else {
            info!("icd: a Check-In from a device we are not registered with; dropped");
            return;
        };

        let client = &mut clients[index];
        let node_id = client.handle.node_id;
        if !icd::take(client, counter) {
            info!("icd: node 0x{node_id:016X} sent an old Check-In (counter {counter}); dropped");
            return;
        }
        let refresh = icd::needs_refresh(client);

        if let Err(e) = self.store.save_icd_clients(&clients) {
            warn!("icd: cannot note the Check-In of node 0x{node_id:016X}: {e:?}");
        }
        info!("icd: node 0x{node_id:016X} checked in (counter {counter})");

        self.set_online(node_id, true);
        self.note_message_of(node_id);
        self.resubscribe(node_id);

        // Half its counter values are used: a fresh key, while it is awake.
        if refresh {
            if let Err(e) = self.check_ins(node_id, true).await {
                warn!("icd: the key of node 0x{node_id:016X} is due for a refresh, but {e}");
            }
        }
    }

    /// Register for the Check-Ins of a listed device, or with `!register` drop
    /// our registration. Either changes how it idles, so its record is read
    /// again.
    pub(super) async fn check_ins(&self, node_id: u64, register: bool) -> Result<(), PeekError> {
        let dev = self.lookup_device(node_id)?;
        let our_node_id = self.our_node_id(dev.handle)?;

        // It hears us only while awake.
        self.until_awake(node_id).await?;

        if register {
            let (key, start_counter) = self
                .controller
                .register_check_ins(dev.handle, our_node_id)
                .await
                .map_err(|e| failed("registering for its Check-Ins failed", e))?;

            self.keep_icd_client(IcdClient {
                handle: dev.handle,
                key,
                start_counter,
                offset: 0,
            })?;
        } else {
            match self
                .controller
                .unregister_check_ins(dev.handle, our_node_id)
                .await
            {
                // Not registered there (any more): nothing to drop.
                Err(e) if e.code() == ErrorCode::NotFound => {}
                result => {
                    result.map_err(|e| failed("unregistering from its Check-Ins failed", e))?
                }
            }

            self.forget_icd_client(dev.handle)?;
        }

        // Its subscription waits for it to wake up from now on, or no longer.
        if let Some(watch) = self.subs.borrow().get(&node_id) {
            watch.sleepy.set_long(register);
        }

        let ident = self.controller.read_basic_info(dev.handle).await;
        self.refresh_listed(node_id, ident);

        Ok(())
    }

    /// Register for the Check-Ins of a device just commissioned, if it can
    /// idle long and check in.
    pub(super) async fn register_if_long_idle(&self, node_id: u64) {
        let long_idle = self
            .lookup_device(node_id)
            .ok()
            .and_then(|dev| dev.details.icd)
            .is_some_and(|icd| icd.long_idle && icd.check_in);
        if !long_idle {
            return;
        }

        match self.check_ins(node_id, true).await {
            Ok(()) => info!("service: registered for the Check-Ins of node 0x{node_id:016X}"),
            Err(e) => warn!("service: node 0x{node_id:016X} idles long, but {e}"),
        }
    }

    /// Whether we are registered for the Check-Ins of `handle`: then it sleeps
    /// long.
    pub(super) fn has_icd_client(&self, handle: DeviceHandle) -> bool {
        self.store
            .load_icd_clients()
            .is_ok_and(|clients| clients.iter().any(|c| c.handle == handle))
    }

    /// Drop our registration for the Check-Ins of `handle`, if any.
    pub(super) fn forget_icd_client(&self, handle: DeviceHandle) -> Result<(), PeekError> {
        let mut clients = self.load_icd_clients()?;
        let before = clients.len();
        clients.retain(|c| c.handle != handle);

        if clients.len() != before {
            self.store
                .save_icd_clients(&clients)
                .map_err(|e| failed("cannot save the Check-In registrations", e))?;
        }

        Ok(())
    }

    /// Keep `client`, in place of an earlier one for the same device.
    fn keep_icd_client(&self, client: IcdClient) -> Result<(), PeekError> {
        let mut clients = self.load_icd_clients()?;
        clients.retain(|c| c.handle != client.handle);
        clients.push(client);

        self.store
            .save_icd_clients(&clients)
            .map_err(|e| failed("cannot save the Check-In registrations", e))
    }

    fn load_icd_clients(&self) -> Result<alloc::vec::Vec<IcdClient>, PeekError> {
        self.store
            .load_icd_clients()
            .map_err(|e| failed("cannot load the Check-In registrations", e))
    }
}
