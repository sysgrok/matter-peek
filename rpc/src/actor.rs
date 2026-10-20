//! The Matter actor.
//!
//! One **persistent** task owns a single Matter stack for the whole process
//! lifetime: on startup it loads any persisted controller fabric
//! (`Matter::startup`) and ensures a CA ([`ControllerCa::ensure`]), then
//! runs the transport pump concurrently with a command loop. Each command is
//! serviced against the *shared* stack / CA / fabric, so persistently-
//! commissioned devices can be re-addressed over CASE without re-commissioning.
//!
//! The stack is `&'static` (via a `StaticCell`, fed `'static` device details) —
//! this sidesteps the self-referential `Matter<'a>` lifetime entirely: a
//! `&'static Matter` is freely shared between the pump and the command loop.

use core::cell::{Cell, RefCell};
use core::net::SocketAddr;
use core::num::NonZeroU8;
use core::pin::pin;

use std::collections::HashMap;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::rc::Rc;

use directories::ProjectDirs;
use embassy_futures::select::{select, select4, Either, Either4};
use embassy_time::{Duration, Timer};
use futures_util::stream::{FuturesUnordered, StreamExt};

use log::{error, info, warn};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use rs_matter::crypto::{default_crypto, Crypto};
use rs_matter::dm::clusters::ota_prov::OtaRequestorId;
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::persist::DirKvBlobStore;
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::NoNetwork;
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::Matter;

use static_cell::StaticCell;

use matter_peek_core::ca::{CaTier, ControllerCa};
use matter_peek_core::controller::FabricIdentity;
use matter_peek_core::node;
use matter_peek_core::ota::{OtaEvent, OtaProvider, OtaStore as _, OtaVerifier};
use matter_peek_core::persist::{kv_buf, ControllerStore};
use matter_peek_core::setup_code::SetupCode;
use matter_peek_core::subscribe::Reports;
use matter_peek_core::{Controller, DeviceHandle, DiagSet, LogIntent};

use matter_peek_proto as proto;

use crate::convert;
use crate::ota::{DirOtaStore, PartialImage};

/// The device NodeID the controller assigns to the **next** newly-commissioned
/// device. Persistent mode allocates a fresh id per device (Phase 1: a simple
/// incrementing counter seeded past the store's max).
const FIRST_DEVICE_NODE_ID: u64 = 112234;

/// The persistent controller stack, fed `'static` device details so `Matter`'s
/// lifetime is `'static` and it can be shared freely across tasks.
static PERSIST_MATTER: StaticCell<Matter> = StaticCell::new();

/// A cooperative cancel flag for the in-flight vertical, shared (single-threaded,
/// so `Rc<Cell>` — no atomics) between the server's dispatch (which sets it when
/// a [`proto::Request::Cancel`] arrives, bypassing the serial command queue) and
/// the actor (whose vertical polls it at phase boundaries). See [`vertical_flow`].
pub type CancelFlag = Rc<Cell<bool>>;

/// A unit of work handed to the actor, with the channels to report back on.
pub struct Command {
    pub req_id: proto::ReqId,
    pub request: proto::Request,
    /// Progress events for this command (per-command; the relay closes it when
    /// the command's reply lands — used by the vertical's `Step` events).
    pub events: async_channel::Sender<proto::Event>,
    /// The single final reply.
    pub reply: async_channel::Sender<proto::Response>,
    /// The **connection's** outbound sink — lives for the whole connection, not
    /// just this command. A detached health subscription streams its
    /// `HealthReport` / `SubscriptionState` frames here so they keep flowing long
    /// after the `Subscribe` command has replied. `None` for callers (e.g. the
    /// `ws_client` example paths) that don't host long-lived subscriptions.
    pub out: Option<async_channel::Sender<proto::ServerMsg>>,
}

/// Run the persistent actor: stand up one Matter stack for the process, restore
/// persisted state, ensure the controller CA + fabric, then service commands
/// against that shared stack until the channel closes.
pub async fn run(rx: async_channel::Receiver<Command>, cancel: CancelFlag) {
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);

    // One `&'static` stack for the whole process (see module docs). Kept `&mut`
    // just long enough to `startup`, then reborrowed immutably for sharing.
    let matter: &'static mut Matter<'static> = PERSIST_MATTER.uninit().init_with(Matter::init(
        &TEST_DEV_DET,
        TEST_DEV_COMM,
        &TEST_DEV_ATT,
        0,
    ));

    // Directory-backed KV (desktop), under the platform-standard data dir so the
    // controller fabric + device records survive a reboot (unlike `/tmp`).
    // rs-matter's `SharedKvBlobStore` now borrows its scratch buffer, so the
    // actor owns one (`kv_buf`) for the whole run and lends it to the
    // `ControllerStore`. `startup` takes that store's KV access (which carries
    // the buffer) — a single argument now.
    let dir = store_dir();
    info!("actor: persistence store at {}", dir.display());
    let kv_buf = kv_buf();
    let store = ControllerStore::new(DirKvBlobStore::new(dir.clone()), &kv_buf);
    if let Err(e) = matter.startup(store.kv()) {
        error!("actor: startup failed: {e:?} (continuing with fresh state)");
    }
    let matter: &'static Matter<'static> = matter;
    let (ca, fab_idx) = match ControllerCa::ensure(
        &crypto,
        matter,
        &store,
        FabricIdentity::default(),
        CaTier::RcacDirect,
    ) {
        Ok(v) => v,
        Err(e) => {
            error!("actor: CA ensure failed: {e:?}");
            // Drain commands with an error so clients don't hang.
            while let Ok(cmd) = rx.recv().await {
                let _ = cmd
                    .reply
                    .send(proto::Response::Error {
                        message: "controller CA init failed".into(),
                    })
                    .await;
            }
            return;
        }
    };

    let socket = match bind_controller_socket() {
        Ok(s) => s,
        Err(e) => {
            error!("actor: socket bind failed: {e:?}");
            return;
        }
    };

    let controller = Controller::new(matter, &crypto);

    // Our OTA Provider: the devices on our fabric may ask it for updates.
    if let Err(e) = node::allow_ota_requestors(matter, &store, fab_idx) {
        warn!("actor: cannot open our OTA Provider to the devices: {e:?}");
    }

    let ota_listeners = Rc::new(RefCell::new(Vec::new()));
    let ota = OtaProvider::new(
        DirOtaStore::new(dir.join("ota")),
        ota_events(ota_listeners.clone()),
    );

    // Channel over which the command handler hands new subscription *jobs* to the
    // concurrent driver. Jobs carry only owned data, so this is `'static` and can
    // be declared before `ctx`. Unbounded so `Subscribe` never blocks the loop.
    let (sub_submit, sub_recv) = async_channel::unbounded::<SubJob>();

    let ctx = ActorCtx {
        matter,
        crypto: &crypto,
        controller,
        ca,
        fab_idx,
        store,
        cancel,
        subs: RefCell::new(HashMap::new()),
        reports: Reports::new(),
        ota,
        ota_listeners,
        uploads: RefCell::new(HashMap::new()),
        next_upload_id: Cell::new(1),
    };

    // Four concurrent arms, for the actor's lifetime:
    //   1. the transport pump (`matter.run`),
    //   2. the command loop (services requests one at a time),
    //   3. the subscription driver (polls all live subscriptions concurrently), and
    //   4. the mDNS backend, which services **operational resolves** — CASE
    //      re-addressing of a persisted device (subscribe / peek / decommission
    //      after its secure session lapses) needs a running resolver; without it
    //      every such resolve times out with `NotFound`. (It also services
    //      discovery browses, so those must NOT start a second backend — the
    //      `discover` handler uses the `_bg` variant.)
    // Because subscriptions run in their own arm — not inside a command — the
    // command loop stays free while any number of subscriptions stream, so a
    // `Subscribe` returns immediately and multiple devices can be watched at once.
    let mut pump = pin!(matter.run(&crypto, &socket, &socket, NoNetwork));
    let mut loop_ = pin!(command_loop(rx, &ctx, &sub_submit));
    let mut subs = pin!(run_subscriptions(sub_recv, &ctx));
    let mut mdns = pin!(ctx.controller.run_mdns_backend());
    // What devices reach on us: our OTA Provider and the subscription reports.
    let mut node = pin!(ctx.controller.run_node(&ctx.reports, &ctx.ota));

    match select4(
        &mut pump,
        &mut loop_,
        &mut subs,
        select(&mut mdns, &mut node),
    )
    .await
    {
        Either4::First(r) => error!("actor: transport pump exited: {r:?}"),
        Either4::Second(()) => info!("actor: command channel closed, exiting"),
        Either4::Third(()) => info!("actor: subscription driver exited"),
        Either4::Fourth(Either::First(r)) => error!("actor: mDNS backend exited: {r:?}"),
        Either4::Fourth(Either::Second(r)) => error!("actor: controller node exited: {r:?}"),
    }
}

/// Drive every live subscription concurrently. New subscription *jobs* arrive on
/// `recv` (submitted by the [`subscribe`] handler); the driver builds each one's
/// future against the shared stack (`ctx`) and adds it to the running set. A job's
/// future ends when its subscription is stopped (its `stop_rx` fires or its `out`
/// sink closes), then it's dropped from the set. Runs for the actor's lifetime.
async fn run_subscriptions<C: Crypto>(
    recv: async_channel::Receiver<SubJob>,
    ctx: &ActorCtx<'_, C>,
) {
    let mut running = FuturesUnordered::new();

    loop {
        if running.is_empty() {
            // Nothing running — just wait for the next job (polling an empty
            // `FuturesUnordered` returns `None` immediately, which would busy-loop).
            match recv.recv().await {
                Ok(job) => running.push(run_subscription(job, ctx)),
                Err(_) => return, // actor shutting down
            }
        } else {
            match select(recv.recv(), running.next()).await {
                Either::First(Ok(job)) => running.push(run_subscription(job, ctx)),
                Either::First(Err(_)) => return,
                // A subscription finished (stopped); already removed by `next()`.
                Either::Second(_) => {}
            }
        }
    }
}

/// The future for a single live subscription: stream `subscribe_health` output to
/// the connection sink until stopped, then emit a terminal `Stopped` and drop the
/// registry entry (if it's still ours). Built here (not in [`subscribe`]) so it
/// can borrow the shared stack without entangling the submission channel's type.
async fn run_subscription<C: Crypto>(job: SubJob, ctx: &ActorCtx<'_, C>) {
    let SubJob {
        node_id,
        handle,
        out,
        stop_rx,
        my_stop,
    } = job;

    let seq = Cell::new(0u64);
    let out_reports = out.clone();
    let out_state = out.clone();

    let subscription = ctx.controller.subscribe_health(
        &ctx.reports,
        handle,
        DiagSet::ALL,
        move |health| {
            seq.set(seq.get() + 1);
            let _ = out_reports.try_send(proto::ServerMsg::Event {
                event: proto::Event::HealthReport {
                    node_id,
                    seq: seq.get(),
                    health: convert::health(health),
                },
            });
        },
        move |state| {
            let _ = out_state.try_send(proto::ServerMsg::Event {
                event: proto::Event::SubscriptionState {
                    node_id,
                    state: convert::sub_state(state),
                },
            });
        },
    );

    // Stop when EITHER the explicit stop signal fires OR the connection's sink
    // closes (client disconnected — don't leak the device-side subscription).
    let stop = async {
        let _ = select(stop_rx.recv(), out.closed()).await;
    };
    let _ = select(subscription, stop).await;

    // Drop our registry entry, but only if it's still *ours* (a quick
    // unsubscribe+resubscribe of the same device could have replaced it).
    {
        let mut map = ctx.subs.borrow_mut();
        if map.get(&node_id).is_some_and(|s| s.same_channel(&my_stop)) {
            map.remove(&node_id);
        }
    }

    // Terminal state (delivered only if the connection is still up).
    let _ = out.try_send(proto::ServerMsg::Event {
        event: proto::Event::SubscriptionState {
            node_id,
            state: proto::SubscriptionState::Stopped,
        },
    });
}

/// Shared state every command handler operates on.
struct ActorCtx<'a, C: Crypto> {
    matter: &'a Matter<'a>,
    crypto: &'a C,
    controller: Controller<'a, C>,
    ca: ControllerCa,
    fab_idx: NonZeroU8,
    store: ControllerStore<'a, DirKvBlobStore>,
    /// Cooperative cancel flag for the in-flight vertical (set by the server's
    /// dispatch, polled by [`vertical_flow`] at phase boundaries).
    cancel: CancelFlag,
    /// Live health subscriptions, keyed by device `node_id`. The value is the
    /// **stop** sender: send `()` (or drop it) to make the subscription task's
    /// `select` complete, dropping the `subscribe_health` future and tearing the
    /// subscription down. Present ⇒ that device is being watched.
    subs: RefCell<HashMap<u64, async_channel::Sender<()>>>,
    /// Routes the reports of the live subscriptions to them.
    reports: Reports,
    /// Our OTA Provider, over the images in the data directory.
    ota: OtaProvider<DirOtaStore, Box<dyn Fn(OtaEvent)>>,
    /// The connections following OTA updates.
    ota_listeners: Rc<RefCell<Vec<OtaListener>>>,
    /// The OTA image uploads in progress, by ID.
    uploads: RefCell<HashMap<u32, Upload<'a, C>>>,
    next_upload_id: Cell<u32>,
}

/// An OTA image file being uploaded in chunks: written as it arrives, and
/// checked as it is.
struct Upload<'a, C: Crypto + 'a> {
    verifier: OtaVerifier<C::Hash<'a>>,
    partial: PartialImage,
    size: u64,
    received: u64,
    /// The connection uploading it; the upload is dropped once it closes.
    out: async_channel::Sender<proto::ServerMsg>,
}

/// A connection following the OTA update of a device.
struct OtaListener {
    node_id: u64,
    name: String,
    out: async_channel::Sender<proto::ServerMsg>,
}

/// A subscription request submitted to the concurrent driver. Carries only owned
/// data (no borrow of the shared stack), so the submission channel is `'static`;
/// the driver itself borrows the stack (`&ActorCtx`) to build+run the future.
struct SubJob {
    node_id: u64,
    handle: DeviceHandle,
    /// The connection's outbound sink — where this subscription streams frames.
    out: async_channel::Sender<proto::ServerMsg>,
    /// Fires (or its sender drops) to stop the subscription.
    stop_rx: async_channel::Receiver<()>,
    /// A clone of the registry stop-sender, so the driver can verify at cleanup
    /// that the registry entry is still this subscription before removing it.
    my_stop: async_channel::Sender<()>,
}

/// Pull commands and service them one at a time against the shared stack.
///
/// `ctx` is borrowed for the actor's whole lifetime (`'a`) because a `Subscribe`
/// submits a subscription future that borrows `ctx.controller` for that long.
async fn command_loop<C: Crypto>(
    rx: async_channel::Receiver<Command>,
    ctx: &ActorCtx<'_, C>,
    sub_submit: &async_channel::Sender<SubJob>,
) {
    while let Ok(cmd) = rx.recv().await {
        let response = service(&cmd, ctx, sub_submit).await;
        // Best-effort: if the client vanished, dropping the reply is fine.
        let _ = cmd.reply.send(response).await;
    }
}

/// Service one command against the shared stack, emitting step events as it goes.
async fn service<C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'_, C>,
    sub_submit: &async_channel::Sender<SubJob>,
) -> proto::Response {
    match &cmd.request {
        proto::Request::RunVertical {
            peer_addr,
            setup_code,
            policy,
        } => run_vertical(cmd, ctx, peer_addr, setup_code, *policy).await,
        proto::Request::Discover {
            discriminator,
            setup_code,
            max,
        } => run_discover(ctx, *discriminator, setup_code.as_deref(), *max).await,
        proto::Request::ListDevices => list_devices(ctx),
        proto::Request::PeekDevice { node_id, with_logs } => {
            peek_device(ctx, *node_id, *with_logs).await
        }
        proto::Request::Identify { node_id, secs } => identify(ctx, *node_id, *secs).await,
        proto::Request::Subscribe { node_id } => subscribe(cmd, ctx, sub_submit, *node_id).await,
        proto::Request::Unsubscribe { node_id } => unsubscribe(ctx, *node_id),
        proto::Request::Decommission { node_id, force } => {
            decommission(ctx, *node_id, *force).await
        }
        proto::Request::SetTags { node_id, tags } => set_tags(ctx, *node_id, tags),
        // `Cancel` is normally intercepted by the server (it sets the shared flag
        // out-of-band rather than enqueuing). If one reaches here, no vertical is
        // running — clear any stale flag and ack.
        proto::Request::Cancel => {
            ctx.cancel.set(false);
            proto::Response::Ok
        }
        proto::Request::OtaImages => ota_images(ctx),
        proto::Request::OtaUploadBegin { size } => ota_upload_begin(cmd, ctx, *size),
        proto::Request::OtaUploadChunk {
            upload_id,
            offset,
            data,
        } => ota_upload_chunk(ctx, *upload_id, *offset, data),
        proto::Request::OtaUploadEnd { upload_id } => ota_upload_end(ctx, *upload_id),
        proto::Request::OtaDelete { name } => ota_delete(ctx, name),
        proto::Request::OtaUpdate { node_id, name } => ota_update(cmd, ctx, *node_id, name).await,
    }
}

fn ota_images<C: Crypto>(ctx: &ActorCtx<'_, C>) -> proto::Response {
    proto::Response::OtaImages {
        images: ctx
            .ota
            .store()
            .images()
            .into_iter()
            .map(convert::ota_image)
            .collect(),
    }
}

/// The largest upload chunk taken, decoded.
const MAX_UPLOAD_CHUNK_LEN: usize = 4 * 1024 * 1024;

fn ota_upload_begin<'a, C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'a, C>,
    size: u64,
) -> proto::Response {
    let Some(out) = cmd.out.clone() else {
        return proto::Response::Error {
            message: "uploads require a streaming connection".into(),
        };
    };

    drop_closed_uploads(ctx);

    let crypto: &'a C = ctx.crypto;
    let upload_id = ctx.next_upload_id.get();
    ctx.next_upload_id.set(upload_id.wrapping_add(1));

    let upload = crypto.hash().and_then(|hasher| {
        Ok(Upload {
            verifier: OtaVerifier::new(hasher),
            partial: ctx.ota.store().begin(upload_id)?,
            size,
            received: 0,
            out,
        })
    });

    match upload {
        Ok(upload) => {
            ctx.uploads.borrow_mut().insert(upload_id, upload);
            proto::Response::OtaUploadStarted { upload_id }
        }
        Err(e) => proto::Response::Error {
            message: format!("cannot start the upload: {:?}", e.code()),
        },
    }
}

fn ota_upload_chunk<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    upload_id: u32,
    offset: u64,
    data: &str,
) -> proto::Response {
    let chunk = match BASE64.decode(data) {
        Ok(chunk) if chunk.len() <= MAX_UPLOAD_CHUNK_LEN => chunk,
        Ok(_) => return ota_upload_fail(ctx, upload_id, "a chunk is too big".into()),
        Err(e) => return ota_upload_fail(ctx, upload_id, format!("a chunk is not base64: {e}")),
    };

    let mut uploads = ctx.uploads.borrow_mut();
    let Some(upload) = uploads.get_mut(&upload_id) else {
        return proto::Response::Error {
            message: format!("no upload {upload_id}"),
        };
    };

    if offset != upload.received {
        let message = format!("expected the chunk at {}, got {offset}", upload.received);
        drop(uploads);
        return ota_upload_fail(ctx, upload_id, message);
    }

    let taken = if upload.received + chunk.len() as u64 > upload.size {
        Err("more data than announced".to_string())
    } else if let Err(e) = upload.verifier.update(&chunk) {
        Err(format!("not a valid OTA image: {:?}", e.code()))
    } else if let Err(e) = upload.partial.write(&chunk) {
        Err(format!("cannot write the image: {:?}", e.code()))
    } else {
        upload.received += chunk.len() as u64;
        Ok(())
    };

    drop(uploads);

    match taken {
        Ok(()) => proto::Response::Ok,
        Err(message) => ota_upload_fail(ctx, upload_id, message),
    }
}

fn ota_upload_end<C: Crypto>(ctx: &ActorCtx<'_, C>, upload_id: u32) -> proto::Response {
    let Some(upload) = ctx.uploads.borrow_mut().remove(&upload_id) else {
        return proto::Response::Error {
            message: format!("no upload {upload_id}"),
        };
    };

    let store = ctx.ota.store();

    if upload.received != upload.size {
        store.discard(upload.partial);
        return proto::Response::Error {
            message: format!(
                "the upload ended at {} of {} bytes",
                upload.received, upload.size
            ),
        };
    }

    let image = match upload.verifier.finish() {
        Ok(image) => image,
        Err(e) => {
            store.discard(upload.partial);
            return proto::Response::Error {
                message: format!("not a valid OTA image: {:?}", e.code()),
            };
        }
    };

    match store.finish(upload.partial, &image).map(|()| image) {
        Ok(image) => {
            info!(
                "actor: stored OTA image {} ({})",
                image.name, image.version_str
            );
            ota_images(ctx)
        }
        Err(e) => proto::Response::Error {
            message: format!("cannot store the OTA image: {:?}", e.code()),
        },
    }
}

/// Drop upload `upload_id` and its partial file, answering with `message`.
fn ota_upload_fail<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    upload_id: u32,
    message: String,
) -> proto::Response {
    if let Some(upload) = ctx.uploads.borrow_mut().remove(&upload_id) {
        ctx.ota.store().discard(upload.partial);
    }

    proto::Response::Error { message }
}

/// Drop the uploads of the connections that closed.
fn drop_closed_uploads<C: Crypto>(ctx: &ActorCtx<'_, C>) {
    let mut uploads = ctx.uploads.borrow_mut();

    let closed: Vec<u32> = uploads
        .iter()
        .filter(|(_, upload)| upload.out.is_closed())
        .map(|(id, _)| *id)
        .collect();

    for id in closed {
        if let Some(upload) = uploads.remove(&id) {
            warn!("actor: dropping OTA upload {id}: its connection closed");
            ctx.ota.store().discard(upload.partial);
        }
    }
}

fn ota_delete<C: Crypto>(ctx: &ActorCtx<'_, C>, name: &str) -> proto::Response {
    match ctx.ota.store().remove(name) {
        Ok(()) => ota_images(ctx),
        Err(e) => proto::Response::Error {
            message: format!("cannot delete OTA image {name}: {:?}", e.code()),
        },
    }
}

/// Allow the device image `name`, follow its update on the connection, and
/// announce us to it as its OTA Provider.
async fn ota_update<C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'_, C>,
    node_id: u64,
    name: &str,
) -> proto::Response {
    let Some(dev) = lookup_stored(ctx, node_id) else {
        return proto::Response::Error {
            message: format!("no stored device with node_id 0x{node_id:016x}"),
        };
    };

    if !ctx
        .ota
        .store()
        .images()
        .iter()
        .any(|image| image.name == name)
    {
        return proto::Response::Error {
            message: format!("no OTA image {name}"),
        };
    }

    // Before the announcement: the device asks for the update right away.
    ctx.ota.allow(
        OtaRequestorId {
            fab_idx: dev.handle.fabric_index,
            node_id,
        },
        name,
    );

    if let Some(out) = cmd.out.clone() {
        let mut listeners = ctx.ota_listeners.borrow_mut();
        listeners.retain(|listener| listener.node_id != node_id);
        listeners.push(OtaListener {
            node_id,
            name: name.to_string(),
            out,
        });
    }

    match ctx.controller.announce_ota_provider(dev.handle).await {
        Ok(()) => proto::Response::Ok,
        Err(e) => proto::Response::Error {
            message: format!("cannot announce us to the device: {:?}", e.code()),
        },
    }
}

/// Send each OTA event to the connections following that update.
fn ota_events(listeners: Rc<RefCell<Vec<OtaListener>>>) -> Box<dyn Fn(OtaEvent)> {
    Box::new(move |event| {
        let mut listeners = listeners.borrow_mut();
        listeners.retain(|listener| !listener.out.is_closed());

        let (followers, progress): (Vec<&OtaListener>, _) = match event {
            OtaEvent::Offered { node, image } => (
                listeners
                    .iter()
                    .filter(|l| l.node_id == node.node_id)
                    .collect(),
                proto::OtaProgress::Offered { name: image },
            ),
            OtaEvent::NotOffered { node } => (
                listeners
                    .iter()
                    .filter(|l| l.node_id == node.node_id)
                    .collect(),
                proto::OtaProgress::NotOffered,
            ),
            OtaEvent::Downloaded {
                image,
                offset,
                size,
            } => (
                listeners.iter().filter(|l| l.name == image).collect(),
                proto::OtaProgress::Downloading {
                    percent: (offset as u128 * 100 / size.max(1) as u128) as u8,
                },
            ),
            OtaEvent::Applying { node, version } => (
                listeners
                    .iter()
                    .filter(|l| l.node_id == node.node_id)
                    .collect(),
                proto::OtaProgress::Applying { version },
            ),
            OtaEvent::Applied { node, version } => (
                listeners
                    .iter()
                    .filter(|l| l.node_id == node.node_id)
                    .collect(),
                proto::OtaProgress::Applied { version },
            ),
        };

        for listener in followers {
            let _ = listener.out.try_send(proto::ServerMsg::Event {
                event: proto::Event::OtaProgress {
                    node_id: listener.node_id,
                    progress: progress.clone(),
                },
            });
        }
    })
}

/// Browse for commissionable devices and return a pick-list. Uses the shared
/// stack's transport/mDNS rendezvous (the actor's pump is already running).
async fn run_discover<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    discriminator: Option<u16>,
    setup_code: Option<&str>,
    max: u8,
) -> proto::Response {
    let filter = match (discriminator, setup_code) {
        (Some(discriminator), _) => CommissionableFilter {
            discriminator: Some(discriminator),
            ..Default::default()
        },
        (None, Some(code)) => match SetupCode::parse(code) {
            Ok(code) => code.filter,
            Err(e) => return invalid_setup_code(e),
        },
        (None, None) => CommissionableFilter::default(),
    };

    // The actor already runs the mDNS backend (a permanent arm); use the `_bg`
    // variant so discovery doesn't start a competing second backend.
    let devices = ctx
        .controller
        .discover_all_bg(&filter, max.max(1), None)
        .await
        .into_iter()
        .map(|d| proto::DiscoveredDevice {
            addr: match d.address {
                rs_matter::transport::network::Address::Udp(a) => a.to_string(),
                other => format!("{other:?}"),
            },
            commissionable_id: d.commissionable_id,
        })
        .collect();

    proto::Response::Discovered { devices }
}

/// List the persistently-commissioned devices from the store.
fn list_devices<C: Crypto>(ctx: &ActorCtx<'_, C>) -> proto::Response {
    list_devices_with(ctx, None)
}

/// As [`list_devices`], but attaches a non-fatal `notice` about the operation
/// that produced the list (e.g. a decommission that could not reach the device).
fn list_devices_with<C: Crypto>(ctx: &ActorCtx<'_, C>, notice: Option<String>) -> proto::Response {
    match ctx.store.load_devices() {
        Ok(devices) => proto::Response::Devices {
            devices: devices.into_iter().map(convert::stored_device).collect(),
            notice,
        },
        Err(e) => proto::Response::Error {
            message: format!("load devices failed: {e:?}"),
        },
    }
}

/// Re-address a persisted device by NodeID over CASE (no re-commission) and read
/// its health (+ optional logs).
async fn peek_device<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    node_id: u64,
    with_logs: bool,
) -> proto::Response {
    let Some(dev) = lookup_stored(ctx, node_id) else {
        return proto::Response::Error {
            message: format!("no stored device with node_id 0x{node_id:016x}"),
        };
    };

    let health = ctx.controller.read_health(dev.handle, DiagSet::ALL).await;
    let crash_log_bytes = if with_logs {
        ctx.controller
            .retrieve_logs(dev.handle, LogIntent::CrashLogs)
            .await
            .ok()
            .map(|b| b.len())
    } else {
        None
    };

    proto::Response::Peek {
        handle: convert::handle(dev.handle),
        health: convert::health(health),
        crash_log_bytes,
    }
}

/// Blink a persisted device (Identify cluster) on its application endpoint.
async fn identify<C: Crypto>(ctx: &ActorCtx<'_, C>, node_id: u64, secs: u16) -> proto::Response {
    let Some(dev) = lookup_stored(ctx, node_id) else {
        return proto::Response::Error {
            message: format!("no stored device with node_id 0x{node_id:016x}"),
        };
    };
    match ctx.controller.identify(dev.handle, 1, secs).await {
        Ok(()) => proto::Response::Ok,
        Err(e) => proto::Response::Error {
            message: format!("identify failed: {e:?}"),
        },
    }
}

/// Start a live health subscription to a persisted device and reply
/// **immediately** — the subscription then runs as its own arm of the actor (see
/// [`run_subscriptions`]), streaming `HealthReport` / `SubscriptionState` frames
/// to the connection sink until [`unsubscribe`]d or the connection drops. Because
/// it does not run *inside* this command, the command loop stays free and many
/// devices can be watched at once.
async fn subscribe<C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'_, C>,
    sub_submit: &async_channel::Sender<SubJob>,
    node_id: u64,
) -> proto::Response {
    let Some(dev) = lookup_stored(ctx, node_id) else {
        return proto::Response::Error {
            message: format!("no stored device with node_id 0x{node_id:016x}"),
        };
    };

    // Already watching this device? No-op success (the existing subscription
    // keeps streaming).
    if ctx.subs.borrow().contains_key(&node_id) {
        return proto::Response::Subscribed {
            node_id,
            already: true,
        };
    }

    // Events stream to the connection's outbound sink, which outlives this
    // command. Without one (non-WS caller) there's nowhere to stream, so decline.
    let Some(out) = cmd.out.clone() else {
        return proto::Response::Error {
            message: "subscriptions require a streaming connection".into(),
        };
    };

    // Stop signal: `Unsubscribe` (or connection teardown) fires it. Register the
    // sender under `node_id`; `my_stop` (a clone) lets the driver verify ownership
    // at cleanup.
    let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
    let my_stop = stop_tx.clone();
    ctx.subs.borrow_mut().insert(node_id, stop_tx);

    // Hand a job (owned data only) to the driver, which builds+runs the actual
    // subscription against the shared stack. Reply immediately.
    let job = SubJob {
        node_id,
        handle: dev.handle,
        out,
        stop_rx,
        my_stop,
    };
    if sub_submit.send(job).await.is_err() {
        ctx.subs.borrow_mut().remove(&node_id);
        return proto::Response::Error {
            message: "subscription driver unavailable".into(),
        };
    }

    proto::Response::Subscribed {
        node_id,
        already: false,
    }
}

/// Stop a live subscription: fire its stop signal (which ends the subscription
/// task) and forget it. Idempotent — unsubscribing an unknown device is `Ok`.
fn unsubscribe<C: Crypto>(ctx: &ActorCtx<'_, C>, node_id: u64) -> proto::Response {
    if let Some(stop) = ctx.subs.borrow_mut().remove(&node_id) {
        // Best-effort: send the stop; if the receiver is already gone the task is
        // ending anyway. Dropping `stop` here also closes the channel, which the
        // task's `recv()` sees as `Err` — either way it wakes and completes.
        let _ = stop.try_send(());
        info!("unsubscribed node 0x{node_id:016x}");
    }
    proto::Response::Ok
}

/// How many times to try reaching the device for `RemoveFabric` before giving
/// up. A persisted device is addressed over CASE, which first needs an
/// *operational mDNS resolve* — a device that is asleep, slow to answer, or
/// re-announcing itself can miss one attempt and answer the next, so a single
/// failure is not proof that it is gone.
const DECOMMISSION_ATTEMPTS: u32 = 3;

/// Pause between those attempts, to give a slow resolve room to land.
const DECOMMISSION_RETRY_DELAY_SECS: u64 = 2;

/// `RemoveFabric` on the device, retried up to [`DECOMMISSION_ATTEMPTS`] times.
/// `Err` carries the last failure, formatted, once every attempt is spent.
async fn try_remove_fabric<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    handle: DeviceHandle,
    node_id: u64,
) -> Result<(), String> {
    let mut last = String::new();

    for attempt in 1..=DECOMMISSION_ATTEMPTS {
        match ctx.controller.decommission(handle).await {
            Ok(()) => {
                info!("decommissioned node 0x{node_id:016x} (RemoveFabric ok, attempt {attempt})");
                return Ok(());
            }
            Err(e) => {
                warn!(
                    "decommission node 0x{node_id:016x}: attempt {attempt}/{DECOMMISSION_ATTEMPTS} failed: {e:?}"
                );
                last = format!("{e:?}");
                if attempt < DECOMMISSION_ATTEMPTS {
                    Timer::after(Duration::from_secs(DECOMMISSION_RETRY_DELAY_SECS)).await;
                }
            }
        }
    }

    Err(last)
}

/// Decommission a persisted device: stop any live subscription, `RemoveFabric`
/// our fabric on the device (freeing its fabric slot), then drop the local
/// record. Returns the refreshed device list.
///
/// **The local record goes either way.** Removing it is the part we can always
/// do, and a device that has been unplugged, factory-reset or binned must not be
/// able to wedge itself permanently in the list. So the device is tried
/// [`DECOMMISSION_ATTEMPTS`] times and, if it stays unreachable, the record is
/// dropped anyway and the caller is told the device-side fabric slot leaked (it
/// is reclaimed by factory-resetting the device, or by our orphan sweep if the
/// unit ever comes back for re-commissioning).
///
/// `force` skips contacting the device at all — for a unit already known to be
/// gone, where waiting out the retries is pointless.
async fn decommission<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    node_id: u64,
    force: bool,
) -> proto::Response {
    let Some(dev) = lookup_stored(ctx, node_id) else {
        return proto::Response::Error {
            message: format!("no stored device with node_id 0x{node_id:016x}"),
        };
    };

    // Stop any live subscription first (its CASE session is about to be torn down).
    if let Some(stop) = ctx.subs.borrow_mut().remove(&node_id) {
        let _ = stop.try_send(());
    }

    let device_side = if force {
        info!("decommission node 0x{node_id:016x}: forced — not contacting the device");
        Err("not attempted (forced removal)".to_string())
    } else {
        try_remove_fabric(ctx, dev.handle, node_id).await
    };

    // Drop any secure session we still hold - but only when the device was *not*
    // torn down successfully.
    //
    // On the success path `RemoveFabric` has already invalidated both halves, and
    // the exchange that carried it queues a trailing close on that session for the
    // transport pump; evicting first only makes the pump log a TX failure for a
    // session we removed out from under it. Nothing addresses that node again
    // either - its record is about to go, and a re-commission assigns a fresh
    // NodeID - so the dead session simply ages out.
    //
    // On the give-up path there is no such exchange, and nothing else would ever
    // clear the session: that is the one that lingers pointing at a device we no
    // longer track.
    if device_side.is_err() {
        ctx.controller.forget_sessions(dev.handle);
    }

    // Drop the local record (load-all / filter / save-all).
    match ctx.store.load_devices() {
        Ok(mut devices) => {
            devices.retain(|d| d.handle.node_id != node_id);
            if let Err(e) = ctx.store.save_devices(&devices) {
                return proto::Response::Error {
                    message: format!("failed to update store: {e:?}"),
                };
            }
        }
        Err(e) => {
            return proto::Response::Error {
                message: format!("failed to load store: {e:?}"),
            };
        }
    }

    let notice = match device_side {
        Ok(()) => None,
        Err(why) => {
            warn!(
                "decommission node 0x{node_id:016x}: dropped the local record without removing the \
                 device-side fabric ({why}) — that fabric slot is leaked on the device"
            );
            Some(format!(
                "Device 0x{node_id:016x} could not be reached — it has been removed from this \
                 controller, but our fabric slot is still allocated on the device. Factory-reset \
                 it to reclaim the slot; re-commissioning it here will also clean it up."
            ))
        }
    };

    list_devices_with(ctx, notice)
}

/// Replace a stored device's local tags, then return the refreshed device list
/// (so the UI updates in one round-trip).
fn set_tags<C: Crypto>(ctx: &ActorCtx<'_, C>, node_id: u64, tags: &[String]) -> proto::Response {
    match ctx.store.set_tags(node_id, tags) {
        Ok(true) => list_devices(ctx),
        Ok(false) => proto::Response::Error {
            message: format!("no stored device with node_id 0x{node_id:016x}"),
        },
        Err(e) => proto::Response::Error {
            message: format!("set tags failed: {e:?}"),
        },
    }
}

/// Find a stored device by NodeID.
fn lookup_stored<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    node_id: u64,
) -> Option<matter_peek_core::StoredDevice> {
    ctx.store
        .load_devices()
        .ok()?
        .into_iter()
        .find(|d| d.handle.node_id == node_id)
}

/// Run the full vertical against a freshly-addressed peer, using the shared
/// persistent stack + CA. `Persistent` policy keeps the fabric and records the
/// device in the store; `Ephemeral` removes it afterward.
async fn run_vertical<C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'_, C>,
    peer_addr: &str,
    setup_code: &str,
    policy: proto::FabricPolicy,
) -> proto::Response {
    let peer_addr: SocketAddr = match peer_addr.parse() {
        Ok(a) => a,
        Err(e) => {
            return proto::Response::Error {
                message: format!("invalid peer addr: {e}"),
            }
        }
    };

    let passcode = match SetupCode::parse(setup_code) {
        Ok(code) => code.passcode,
        Err(e) => return invalid_setup_code(e),
    };

    vertical_flow(cmd, ctx, peer_addr, passcode, policy).await
}

fn invalid_setup_code(e: Error) -> proto::Response {
    proto::Response::Error {
        message: format!(
            "invalid setup code ({:?}): expected a passcode, a manual pairing code or an MT: QR code payload",
            e.code()
        ),
    }
}

/// The vertical proper: commission → health → logs → (persist | decommission),
/// reporting each step as a [`proto::Event`].
async fn vertical_flow<C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'_, C>,
    peer_addr: SocketAddr,
    passcode: u32,
    policy: proto::FabricPolicy,
) -> proto::Response {
    use proto::VerticalStep as Step;

    // Fresh cancel state for this run (any stale request from a prior vertical is
    // cleared). The flag is set out-of-band by the server's dispatch and polled
    // at the phase boundaries below.
    ctx.cancel.set(false);

    // Allocate the device's NodeID: one past the current store max (persistent),
    // so re-runs don't collide; ephemeral reuses the base id (removed anyway).
    let device_node_id = next_device_node_id(ctx);

    // SAFEPOINT 1 — before commission: the device has no fabric of ours yet, so
    // cancelling here is side-effect-free.
    if ctx.cancel.get() {
        return cancelled(false);
    }

    // --- Commission ---
    // Cancellable up to AddNOC: the closure lets `commission_cancellable` poll
    // the flag and abandon the (often-hung-on-a-closed-window) orphan sweep / PASE
    // — all pre-AddNOC, so abandoning is safe and needs no cleanup. A `Cancelled`
    // here means no device state was created.
    emit(cmd, Step::Commission, started()).await;
    let cancel = ctx.cancel.clone();
    // Fabrics we still have a record for are live commissionings, not leaks —
    // the sweep must not reclaim them (that would decommission the device we are
    // about to re-commission). Everything else of ours on the device is fair game.
    let live_node_ids = live_node_ids(ctx);
    let outcome = match ctx
        .controller
        .commission_cancellable(
            &ctx.ca,
            ctx.fab_idx,
            peer_addr,
            passcode,
            device_node_id,
            convert::policy(policy),
            &live_node_ids,
            move || cancel.get(),
        )
        .await
    {
        Ok(matter_peek_core::controller::CommissionFlow::Done(o)) => {
            emit(cmd, Step::Commission, done()).await;
            o
        }
        Ok(matter_peek_core::controller::CommissionFlow::Cancelled) => {
            emit(cmd, Step::Commission, failed_msg("cancelled")).await;
            return cancelled(false);
        }
        Err(e) => {
            emit(cmd, Step::Commission, failed(&e)).await;
            return proto::Response::Error {
                message: format!("commission failed: {e:?}"),
            };
        }
    };

    // SAFEPOINT 2 — after commission: the device now holds our fabric. We don't
    // interrupt `commission()` itself mid-protocol (it's atomic), but a cancel
    // request that arrived during it takes effect here. Leaving the fabric would
    // orphan it, so clean up by decommissioning (RemoveFabric) before returning.
    if ctx.cancel.get() {
        let removed = ctx.controller.decommission(outcome.handle).await.is_ok();
        if !removed {
            error!("actor: cancel cleanup decommission failed for the just-commissioned device");
        }
        return cancelled(removed);
    }

    // --- Read health ---
    emit(cmd, Step::ReadHealth, started()).await;
    let health = ctx
        .controller
        .read_health(outcome.handle, DiagSet::ALL)
        .await;
    emit(cmd, Step::ReadHealth, done()).await;

    // --- Retrieve CrashLogs (soft-fail: a device may simply have none) ---
    emit(cmd, Step::RetrieveLogs, started()).await;
    let crash_log_bytes = match ctx
        .controller
        .retrieve_logs(outcome.handle, LogIntent::CrashLogs)
        .await
    {
        Ok(bytes) => {
            emit(cmd, Step::RetrieveLogs, done()).await;
            Some(bytes.len())
        }
        Err(e) => {
            emit(cmd, Step::RetrieveLogs, failed(&e)).await;
            None
        }
    };

    // SAFEPOINT 3 — after the (read-only) health/logs phases, before we decide
    // the device's fate. A cancel that arrived during those reads is honored
    // here: the device is decommissioned regardless of policy (don't persist a
    // device the user asked to cancel), leaving nothing behind.
    if ctx.cancel.get() {
        let removed = ctx.controller.decommission(outcome.handle).await.is_ok();
        if !removed {
            error!("actor: cancel cleanup decommission failed");
        }
        return cancelled(removed);
    }

    // --- Persist (Persistent) | Decommission (Ephemeral) ---
    let mut rematch_candidate = None;
    let decommissioned = match policy {
        proto::FabricPolicy::Persistent => {
            // Keep the device: persist our fabric (extended by AddNOC) + a store
            // record (with BasicInformation identity) so it can be re-addressed
            // over CASE later without re-commissioning. Re-matching reuses the
            // record of a re-commissioned unit; a fingerprint-only hit comes back
            // as a candidate to surface for linking.
            match persist_commissioned(ctx, outcome.handle).await {
                Ok(candidate) => rematch_candidate = candidate,
                Err(e) => error!("actor: persisting commissioned device failed: {e:?}"),
            }
            false
        }
        proto::FabricPolicy::Ephemeral => {
            emit(cmd, Step::Decommission, started()).await;
            match ctx.controller.decommission(outcome.handle).await {
                Ok(()) => {
                    emit(cmd, Step::Decommission, done()).await;
                    true
                }
                Err(e) => {
                    emit(cmd, Step::Decommission, failed(&e)).await;
                    false
                }
            }
        }
    };

    proto::Response::Vertical(proto::VerticalResult {
        handle: convert::handle(outcome.handle),
        health: convert::health(health),
        crash_log_bytes,
        decommissioned,
        rematch_candidate,
    })
}

/// Persist a freshly-commissioned device, re-matching it against the store so a
/// re-commissioned unit reuses its existing record (and the user's tags / label)
/// rather than spawning a duplicate.
///
/// Returns `Some(existing_node_id)` when only the per-model fingerprint matched
/// (a [`MatchResult::Candidate`]) — surfaced so the UI can offer to *link* the
/// two; a strong-id ([`MatchResult::Exact`]) merge and a brand-new device both
/// return `None`.
async fn persist_commissioned<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    handle: DeviceHandle,
) -> Result<Option<u64>, Error> {
    use matter_peek_core::{match_device, MatchResult, StoredDevice};

    // Save the updated controller fabric (it now lists the new device's NOC).
    ctx.matter
        .with_state(|state| ctx.store.save_fabric(state.fabrics.fabric(ctx.fab_idx)?))?;

    // Read the device's stable identity (best-effort) and classify it.
    let ident = ctx.controller.read_basic_info(handle).await;
    let mut devices = ctx.store.load_devices().unwrap_or_default();
    let outcome = match_device(&ident, &devices);

    // Drop any record carrying this exact node_id (a clean re-run of the same id).
    devices.retain(|d| d.handle.node_id != handle.node_id);

    let mut candidate = None;
    match outcome {
        // Strong-id match: this is the same physical unit, re-addressed. Reuse
        // its record — carry over the user's tags + node_label — and just update
        // the handle + refresh the read identity.
        MatchResult::Exact { node_id } => {
            let prior = devices.iter().position(|d| d.handle.node_id == node_id);
            let (tags, label) = prior
                .map(|i| {
                    let d = devices.remove(i);
                    (d.tags, d.node_label)
                })
                .unwrap_or_default();
            // Keep an existing user label if BasicInformation didn't serve one.
            let node_label = if ident.node_label.is_empty() {
                label
            } else {
                ident.node_label.clone()
            };
            devices.push(StoredDevice {
                handle,
                unique_id: ident.unique_id,
                serial_number: ident.serial_number,
                node_label,
                vendor_name: ident.vendor_name,
                product_name: ident.product_name,
                fingerprint: ident.fingerprint,
                tags,
            });
        }
        // Fingerprint-only: possibly the same unit, possibly a sibling of the
        // same model. Add it as a NEW record (don't silently merge) but flag the
        // candidate so the UI can offer to link them.
        MatchResult::Candidate { node_id } => {
            candidate = Some(node_id);
            devices.push(new_record(handle, &ident));
        }
        MatchResult::New => {
            devices.push(new_record(handle, &ident));
        }
    }

    ctx.store.save_devices(&devices)?;
    Ok(candidate)
}

extern crate alloc;

/// A brand-new store record (no tags yet) from a fresh identity read.
fn new_record(
    handle: DeviceHandle,
    ident: &matter_peek_core::DeviceIdentity,
) -> matter_peek_core::StoredDevice {
    matter_peek_core::StoredDevice {
        handle,
        unique_id: ident.unique_id.clone(),
        serial_number: ident.serial_number.clone(),
        node_label: ident.node_label.clone(),
        vendor_name: ident.vendor_name.clone(),
        product_name: ident.product_name.clone(),
        fingerprint: ident.fingerprint,
        tags: alloc::vec::Vec::new(),
    }
}

/// The NodeIDs the store currently vouches for — the devices we believe we are
/// legitimately commissioned into. Handed to the orphan sweep so it can tell a
/// leaked fabric from a live one (see `sweep_orphaned_fabrics`).
fn live_node_ids<C: Crypto>(ctx: &ActorCtx<'_, C>) -> alloc::vec::Vec<u64> {
    ctx.store
        .load_devices()
        .unwrap_or_default()
        .iter()
        .map(|d| d.handle.node_id)
        .collect()
}

/// Next device NodeID: one past the current store max, so persistent re-runs
/// don't collide. Ephemeral runs are fine reusing the base (fabric is removed).
fn next_device_node_id<C: Crypto>(ctx: &ActorCtx<'_, C>) -> u64 {
    ctx.store
        .load_devices()
        .ok()
        .and_then(|d| d.iter().map(|x| x.handle.node_id).max())
        .map(|m| m + 1)
        .unwrap_or(FIRST_DEVICE_NODE_ID)
}

async fn emit(cmd: &Command, step: proto::VerticalStep, status: proto::StepStatus) {
    let _ = cmd
        .events
        .send(proto::Event::Step {
            req_id: cmd.req_id,
            step,
            status,
        })
        .await;
}

fn started() -> proto::StepStatus {
    proto::StepStatus::Started
}
fn done() -> proto::StepStatus {
    proto::StepStatus::Done
}
fn failed(e: &rs_matter::error::Error) -> proto::StepStatus {
    proto::StepStatus::Failed {
        message: format!("{e:?}"),
    }
}

fn failed_msg(msg: &str) -> proto::StepStatus {
    proto::StepStatus::Failed {
        message: msg.into(),
    }
}

/// The reply for a vertical cancelled at a safepoint. `decommissioned` says
/// whether a just-installed device fabric had to be cleaned up on the way out.
fn cancelled(decommissioned: bool) -> proto::Response {
    proto::Response::Cancelled { decommissioned }
}

/// Bind an ephemeral dual-stack IPv6 UDP socket for the controller transport.
///
/// Socket creation is the binary's job (so `matter-peek-core` stays `no_std`).
/// A single `[::]:0` socket serves both IPv4 and IPv6 peers — rs-matter
/// canonicalizes IPv4-mapped-IPv6 peer addresses so session lookup matches
/// regardless of the socket's family.
fn bind_controller_socket() -> Result<async_io::Async<UdpSocket>, Error> {
    let bind_addr: SocketAddr = "[::]:0".parse().unwrap();
    let socket =
        async_io::Async::<UdpSocket>::bind(bind_addr).map_err(|_| ErrorCode::NoNetworkInterface)?;
    info!(
        "controller bound on {}",
        socket.get_ref().local_addr().unwrap()
    );
    Ok(socket)
}

/// The directory the persistence store lives in: the platform-standard
/// per-user data dir (`~/.local/share/matter-peek` on Linux,
/// `~/Library/Application Support/com.sysgrok.matter-peek` on macOS), created if
/// missing. Falls back to a `matter-peek` dir under the temp dir if no data dir
/// can be determined (e.g. `HOME` is unset) or it can't be created — the store
/// still works, it just won't survive a reboot.
fn store_dir() -> PathBuf {
    let preferred = ProjectDirs::from("com", "sysgrok", "matter-peek")
        .map(|dirs| dirs.data_dir().to_path_buf());

    if let Some(dir) = preferred {
        match std::fs::create_dir_all(&dir) {
            Ok(()) => return dir,
            Err(e) => warn!(
                "actor: could not create data dir {} ({e}); falling back to temp",
                dir.display()
            ),
        }
    } else {
        warn!("actor: no platform data dir available; falling back to temp");
    }

    std::env::temp_dir().join("matter-peek")
}
