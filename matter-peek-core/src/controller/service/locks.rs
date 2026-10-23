//! Which operations may run side by side.

use core::cell::{Cell, RefCell};

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::vec::Vec;

use rs_matter::utils::sync::Notification;

/// What an operation works on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Key {
    /// A listed device, by node ID.
    Node(u64),
    /// One of our fabrics, by index.
    Fabric(u8),
    /// The sync of the device list.
    Sync,
}

/// How an operation uses a [`Key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Side by side with other reads.
    Read,
    /// Alone.
    Write,
}

/// Read/write locks by [`Key`], granted in arrival order: an operation queues
/// on all its keys at once, and runs once no earlier operation still queued on
/// one of them conflicts with it.
pub(super) struct Locks {
    queues: RefCell<BTreeMap<Key, Vec<Entry>>>,
    next_ticket: Cell<u64>,
}

impl Locks {
    pub(super) const fn new() -> Self {
        Self {
            queues: RefCell::new(BTreeMap::new()),
            next_ticket: Cell::new(0),
        }
    }

    /// Queue on `keys`, behind every operation queued before.
    pub(super) fn queue(&self, keys: &[(Key, Mode)]) -> Guard<'_> {
        let ticket = self.next_ticket.get();
        self.next_ticket.set(ticket + 1);

        // A key named twice is written if either use writes.
        let mut merged = BTreeMap::new();
        for &(key, mode) in keys {
            let mode = match merged.get(&key) {
                Some(Mode::Write) => Mode::Write,
                _ => mode,
            };
            merged.insert(key, mode);
        }

        let notification = Rc::new(Notification::new());

        let mut queues = self.queues.borrow_mut();
        for (&key, &mode) in &merged {
            queues.entry(key).or_default().push(Entry {
                ticket,
                mode,
                notification: notification.clone(),
            });
        }

        Guard {
            locks: self,
            ticket,
            keys: merged.into_keys().collect(),
            notification,
        }
    }

    /// Whether any operation is queued on `key`.
    pub(super) fn busy(&self, key: Key) -> bool {
        self.queues.borrow().contains_key(&key)
    }

    fn may_run(&self, ticket: u64, keys: &[Key]) -> bool {
        let queues = self.queues.borrow();

        keys.iter().all(|key| {
            let Some(queue) = queues.get(key) else {
                return true;
            };
            let Some(mine) = queue.iter().position(|e| e.ticket == ticket) else {
                return true;
            };

            let reads = queue[mine].mode == Mode::Read;
            queue[..mine].iter().all(|e| reads && e.mode == Mode::Read)
        })
    }

    fn release(&self, ticket: u64, keys: &[Key]) {
        let mut queues = self.queues.borrow_mut();

        for key in keys {
            let Some(queue) = queues.get_mut(key) else {
                continue;
            };

            queue.retain(|e| e.ticket != ticket);
            if queue.is_empty() {
                queues.remove(key);
            } else {
                for entry in queue.iter() {
                    entry.notification.notify();
                }
            }
        }
    }
}

/// An operation's place in the [`Locks`]; dropping it frees the place.
pub(super) struct Guard<'l> {
    locks: &'l Locks,
    ticket: u64,
    keys: Vec<Key>,
    notification: Rc<Notification>,
}

impl Guard<'_> {
    /// Wait until the operation may run.
    pub(super) async fn ready(&self) {
        while !self.locks.may_run(self.ticket, &self.keys) {
            self.notification.wait().await;
        }
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.locks.release(self.ticket, &self.keys);
    }
}

struct Entry {
    ticket: u64,
    mode: Mode,
    /// Wakes the operation to check again.
    notification: Rc<Notification>,
}

#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Waker};

    use super::*;

    const A: Key = Key::Node(1);
    const B: Key = Key::Node(2);
    const F: Key = Key::Fabric(1);

    /// Whether `guard` may run now.
    fn ready(guard: &Guard<'_>) -> bool {
        let mut ready = pin!(guard.ready());
        ready
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    }

    #[test]
    fn reads_share() {
        let locks = Locks::new();
        let first = locks.queue(&[(A, Mode::Read)]);
        let second = locks.queue(&[(A, Mode::Read)]);

        assert!(ready(&first));
        assert!(ready(&second));
    }

    #[test]
    fn a_write_waits_for_earlier_reads_and_holds_back_later_ones() {
        let locks = Locks::new();
        let read = locks.queue(&[(A, Mode::Read)]);
        let write = locks.queue(&[(A, Mode::Write)]);
        let late = locks.queue(&[(A, Mode::Read)]);

        assert!(ready(&read));
        assert!(!ready(&write));
        assert!(!ready(&late));

        drop(read);
        assert!(ready(&write));
        assert!(!ready(&late));

        drop(write);
        assert!(ready(&late));
    }

    #[test]
    fn keys_are_independent() {
        let locks = Locks::new();
        let a = locks.queue(&[(A, Mode::Write)]);
        let b = locks.queue(&[(B, Mode::Write)]);

        assert!(ready(&a));
        assert!(ready(&b));
    }

    #[test]
    fn every_key_must_be_free() {
        let locks = Locks::new();
        let device = locks.queue(&[(A, Mode::Write), (F, Mode::Read)]);
        let fabric = locks.queue(&[(F, Mode::Write)]);
        let other = locks.queue(&[(B, Mode::Read), (F, Mode::Read)]);

        assert!(ready(&device));
        assert!(!ready(&fabric));
        // Behind the fabric's write, although its device is free.
        assert!(!ready(&other));

        drop(device);
        assert!(ready(&fabric));

        drop(fabric);
        assert!(ready(&other));
    }

    #[test]
    fn a_key_named_twice_is_written() {
        let locks = Locks::new();
        let both = locks.queue(&[(A, Mode::Read), (A, Mode::Write)]);
        let read = locks.queue(&[(A, Mode::Read)]);

        assert!(ready(&both));
        assert!(!ready(&read));
    }

    #[test]
    fn no_keys_run_at_once() {
        let locks = Locks::new();
        let _write = locks.queue(&[(A, Mode::Write)]);

        assert!(ready(&locks.queue(&[])));
    }

    #[test]
    fn busy_until_released() {
        let locks = Locks::new();
        assert!(!locks.busy(Key::Sync));

        let sync = locks.queue(&[(Key::Sync, Mode::Write)]);
        assert!(locks.busy(Key::Sync));

        drop(sync);
        assert!(!locks.busy(Key::Sync));
    }
}
