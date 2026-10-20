//! The controller's own node: what devices reach on it.
//!
//! One responder serves all of it: the OTA Provider cluster on endpoint 0, OTA
//! images over BDX, and the reports of our health subscriptions, which
//! [`Reports`] routes to them. A single responder matters: several would race for
//! the same inbound exchanges.
//!
//! No Secure Channel responder: devices reach us over the CASE sessions we opened
//! to them, e.g. to announce ourselves as their OTA Provider.

use core::num::NonZeroU8;

use rs_matter::acl::{AclEntry, AuthMode, Target};
use rs_matter::bdx::{Bdx, PROTO_ID_BDX};
use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _, DescHandler};
use rs_matter::dm::clusters::net_comm::{DummyNetworks, NetworkType};
use rs_matter::dm::clusters::ota_prov::{
    self, OtaBdxHandler, OtaImages, OtaImagesRegistry, OtaProviderHandler,
};
use rs_matter::dm::clusters::ota_req;
use rs_matter::dm::devices::DEV_TYPE_OTA_PROVIDER;
use rs_matter::dm::endpoints::ROOT_ENDPOINT_ID;
use rs_matter::dm::networks::wireless::NoopWirelessNetCtl;
use rs_matter::dm::{Async, Cluster, Dataver, EmptyHandler, Endpoint, Node, Privilege};
use rs_matter::error::Error;
use rs_matter::im::{InteractionModel, InteractionModelState};
use rs_matter::persist::{DummyKvBlobStore, KvBlobStore};
use rs_matter::respond::{ChainedExchangeHandler, Responder};
use rs_matter::transport::exchange::MatterBuffers;
use rs_matter::{clusters, devices, with, Matter};

use crate::persist::ControllerStore;
use crate::subscribe::Reports;

/// The OTA Provider cluster, with its mandatory attributes.
const OTA_PROVIDER_CLUSTER: Cluster<'static> = ota_prov::FULL_CLUSTER.with_attrs(with!(required));

/// The node: endpoint 0, an OTA Provider that announces itself to requestors.
const NODE: Node<'static> = Node {
    endpoints: &[Endpoint {
        id: ROOT_ENDPOINT_ID,
        device_types: devices!(DEV_TYPE_OTA_PROVIDER),
        clusters: clusters!(DescHandler::CLUSTER, OTA_PROVIDER_CLUSTER),
        client_clusters: &[ota_req::FULL_CLUSTER.id],
        unique_id: None,
        semantic_tags: &[],
    }],
};

/// Serve the controller's node until an error: the OTA Provider of `ota` and the
/// subscription `reports`.
pub async fn run<C, P>(
    matter: &Matter<'_>,
    crypto: C,
    reports: &Reports,
    ota: &P,
) -> Result<(), Error>
where
    C: Crypto + Copy,
    P: OtaImagesRegistry + OtaImages,
{
    let mut rand = crypto.weak_rand()?;

    let handler = EmptyHandler
        .chain(
            |e, c| e == ROOT_ENDPOINT_ID && c == DescHandler::CLUSTER.id,
            Async(desc::HandlerAdaptor(DescHandler::new(Dataver::new_rand(
                &mut rand,
            )))),
        )
        .chain(
            |e, c| e == ROOT_ENDPOINT_ID && c == OTA_PROVIDER_CLUSTER.id,
            OtaProviderHandler::new(Dataver::new_rand(&mut rand), ota).adapt(),
        );

    let buffers: MatterBuffers = MatterBuffers::new();
    let state: InteractionModelState<DummyNetworks, 3, 1024> =
        InteractionModelState::new(DummyNetworks);
    let kv = matter.kv(DummyKvBlobStore);

    let im = InteractionModel::new_with_reports(
        matter,
        crypto,
        &buffers,
        (NODE, handler),
        &kv,
        NoopWirelessNetCtl::new(NetworkType::Ethernet),
        reports,
        &state,
    );

    let bdx_buffers: MatterBuffers<2> = MatterBuffers::new();

    let handler = ChainedExchangeHandler::new(
        PROTO_ID_BDX,
        Bdx::new(OtaBdxHandler::new(&bdx_buffers, ota)),
        &im,
    );

    Responder::new("matter-peek", handler, matter, 0)
        .run::<4>()
        .await
}

/// Let the devices on fabric `fab_idx` use our OTA Provider cluster: an ACL
/// entry granting any CASE peer Operate on it, added (and the fabric persisted)
/// unless there already is one.
pub fn allow_ota_requestors<S: KvBlobStore>(
    matter: &Matter<'_>,
    store: &ControllerStore<'_, S>,
    fab_idx: NonZeroU8,
) -> Result<(), Error> {
    let target = Target::new(Some(ROOT_ENDPOINT_ID), Some(OTA_PROVIDER_CLUSTER.id), None);

    matter.with_state(|state| {
        let fabric = state.fabrics.fabric_mut(fab_idx)?;

        let present = fabric.acl_iter().any(|entry| {
            entry
                .targets()
                .into_option()
                .is_some_and(|targets| targets.contains(&target))
        });

        if present {
            return Ok(());
        }

        let mut entry = AclEntry::new(None, Privilege::OPERATE, AuthMode::Case);
        entry.add_target(target.clone())?;
        fabric.acl_add(entry)?;

        store.save_fabric(fabric)
    })
}
