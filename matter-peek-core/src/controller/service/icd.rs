//! Our registrations for the Check-Ins of intermittently connected devices.

use alloc::vec::Vec;

use embassy_futures::select::{select, Either};

use log::{info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::error::ErrorCode;
use rs_matter::persist::KvBlobStore;

use crate::api::PeekError;
use crate::icd;
use crate::model::{DeviceHandle, IcdClient};
use crate::ota::OtaImageStore;

use super::{failed, Ctx};

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
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
            watch.sleepy.set(register);
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
