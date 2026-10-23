//! Live health subscriptions: one per device, shared by all its listeners.

use alloc::rc::Rc;
use alloc::sync::Arc;
use alloc::vec::Vec;

use embassy_futures::select::{select, Either};
use futures_util::stream::{FuturesUnordered, StreamExt};

use log::info;

use rs_matter::crypto::Crypto;
use rs_matter::persist::KvBlobStore;
use rs_matter::utils::sync::Notification;

use crate::api::{DeviceFabric, DeviceHealth, HealthListener, PeekError, SubscriptionState};
use crate::model::{DeviceHandle, DiagSet};
use crate::ota::OtaImageStore;

use super::{convert, Ctx};

/// A device's live health subscription, and its listeners.
pub(super) struct Watch {
    /// Fires (or drops) to stop the device subscription.
    stop: async_channel::Sender<()>,
    /// Cuts the wait before the next attempt to re-establish it short.
    pub(super) retry: Rc<Notification>,
    /// The listeners, by ID.
    listeners: Vec<(u64, Arc<dyn HealthListener>)>,
    /// The state, and the health merged from the reports, so far: what a
    /// listener joining now gets first.
    pub(super) state: Option<SubscriptionState>,
    health: DeviceHealth,
    reports: u64,
}

impl Watch {
    /// The fabrics the device reported being on, so far.
    pub(super) fn fabrics(&self) -> &[DeviceFabric] {
        self.health.fabrics.as_deref().unwrap_or_default()
    }

    fn report(&mut self, node_id: u64, health: DeviceHealth) {
        self.reports += 1;
        self.health.merge(health.clone());

        for (_, listener) in &self.listeners {
            listener.on_report(node_id, self.reports, health.clone());
        }
    }

    fn set_state(&mut self, node_id: u64, state: SubscriptionState) {
        // Failed retries repeat `Offline`; tell the listeners once.
        if self.state != Some(state) {
            self.state = Some(state);

            for (_, listener) in &self.listeners {
                listener.on_state(node_id, state);
            }
        }
    }

    /// Add `listener`, telling it the state and health so far.
    fn join(&mut self, node_id: u64, id: u64, listener: Arc<dyn HealthListener>) {
        if self.reports > 0 {
            listener.on_report(node_id, self.reports, self.health.clone());
        }
        if let Some(state) = self.state {
            listener.on_state(node_id, state);
        }

        self.listeners.push((id, listener));
    }
}

/// A subscription for the driver to run. Owned data only (no borrow of the
/// stack), so its channel can be created before the state it runs against.
pub(super) struct SubJob {
    node_id: u64,
    handle: DeviceHandle,
    /// Fires (or its sender drops) to stop the subscription.
    stop_rx: async_channel::Receiver<()>,
    /// A clone of the [`Watch::stop`] sender, telling whether the device's
    /// watch is still this subscription's.
    my_stop: async_channel::Sender<()>,
    retry: Rc<Notification>,
}

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// Add `listener` to the subscription of a listed device, starting the
    /// subscription if it is the first; answers with the listener's ID.
    pub(super) async fn watch(
        &self,
        node_id: u64,
        listener: Arc<dyn HealthListener>,
    ) -> Result<u64, PeekError> {
        let dev = self.lookup_device(node_id)?;

        let id = self.next_watcher.get();
        self.next_watcher.set(id + 1);

        if let Some(watch) = self.subs.borrow_mut().get_mut(&node_id) {
            watch.join(node_id, id, listener);
            return Ok(id);
        }

        let (stop, stop_rx) = async_channel::bounded(1);
        let retry = Rc::new(Notification::new());

        self.subs.borrow_mut().insert(
            node_id,
            Watch {
                stop: stop.clone(),
                retry: retry.clone(),
                listeners: alloc::vec![(id, listener)],
                state: None,
                health: DeviceHealth::default(),
                reports: 0,
            },
        );

        let job = SubJob {
            node_id,
            handle: dev.handle,
            stop_rx,
            my_stop: stop,
            retry,
        };
        if self.sub_jobs.send(job).await.is_err() {
            self.subs.borrow_mut().remove(&node_id);
            return Err(PeekError::failed("the subscriptions are not running"));
        }

        Ok(id)
    }

    /// Remove listener `id` of a device; its subscription stops with the last.
    pub(super) fn unwatch(&self, node_id: u64, id: u64) {
        let mut subs = self.subs.borrow_mut();

        let Some(watch) = subs.get_mut(&node_id) else {
            return;
        };

        watch.listeners.retain(|(listener, _)| *listener != id);

        if watch.listeners.is_empty() {
            if let Some(watch) = subs.remove(&node_id) {
                let _ = watch.stop.try_send(());
            }
            info!("unsubscribed node 0x{node_id:016X}");
        }
    }

    /// Stop the subscription of a device, telling its listeners.
    pub(super) fn stop_watch(&self, node_id: u64) {
        let watch = self.subs.borrow_mut().remove(&node_id);

        if let Some(mut watch) = watch {
            let _ = watch.stop.try_send(());
            watch.set_state(node_id, SubscriptionState::Stopped);
        }
    }

    /// Run every live subscription, taking new ones from `jobs`, until the
    /// service ends.
    pub(super) async fn run_subscriptions(&self, jobs: async_channel::Receiver<SubJob>) {
        let mut running = FuturesUnordered::new();

        loop {
            if running.is_empty() {
                // An empty `FuturesUnordered` is ready at once (with `None`).
                match jobs.recv().await {
                    Ok(job) => running.push(self.run_subscription(job)),
                    Err(_) => return,
                }
            } else {
                match select(jobs.recv(), running.next()).await {
                    Either::First(Ok(job)) => running.push(self.run_subscription(job)),
                    Either::First(Err(_)) => return,
                    // A subscription ended; `next()` dropped it.
                    Either::Second(_) => {}
                }
            }
        }
    }

    /// Run one subscription until stopped, handing its reports and states to
    /// the listeners of its device.
    async fn run_subscription(&self, job: SubJob) {
        let SubJob {
            node_id,
            handle,
            stop_rx,
            my_stop,
            retry,
        } = job;

        // The device's watch, unless it was replaced or removed.
        let with_watch = |f: &mut dyn FnMut(&mut Watch)| {
            if let Some(watch) = self
                .subs
                .borrow_mut()
                .get_mut(&node_id)
                .filter(|watch| watch.stop.same_channel(&my_stop))
            {
                f(watch);
            }
        };

        let subscription = self.controller.subscribe_health(
            &self.reports,
            handle,
            DiagSet::LIVE,
            &retry,
            |mut health| {
                self.mark_our_fabric(node_id, &mut health);
                with_watch(&mut |watch| watch.report(node_id, health.clone()))
            },
            |state| {
                let state = convert::sub_state(state);
                match state {
                    SubscriptionState::Live { .. } => self.set_online(node_id, true),
                    SubscriptionState::Offline => self.set_online(node_id, false),
                    SubscriptionState::Stopped => {}
                }
                with_watch(&mut |watch| watch.set_state(node_id, state));
            },
        );

        if let Either::First(_) = select(subscription, stop_rx.recv()).await {
            // It ended on its own: the watch, if still its own, goes with it.
            let ours = self
                .subs
                .borrow()
                .get(&node_id)
                .is_some_and(|watch| watch.stop.same_channel(&my_stop));
            if ours {
                self.stop_watch(node_id);
            }
        }
    }
}
