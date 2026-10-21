//! Discovering Matter devices on the local network via mDNS: the commissionable
//! ones, and the ones on our fabric.
//!
//! rs-matter's controller-side discovery is a rendezvous:
//! [`Transport::browse`](rs_matter::transport::Transport::browse) places a browse
//! request and a running **mDNS backend** fires the query and deposits what it
//! finds. So discovery is "run a backend concurrently with a browse".
//!
//! The backend is the host's: see [`crate::mdns`].

extern crate alloc;
use alloc::vec::Vec;

use core::num::NonZeroU8;

use log::{info, warn};

use rs_matter::dm::NodeId;
use rs_matter::error::Error;
use rs_matter::transport::network::mdns::{CommissionableFilter, MdnsBrowseQuery};
use rs_matter::transport::network::Address;
use rs_matter::Matter;

/// How long each mDNS browse window lasts (see `Transport::browse`).
const DEFAULT_BROWSE_WINDOW_MS: u32 = 2_000;

/// A commissionable device found on the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveredDevice {
    /// Its address — feed straight into commissioning / PASE.
    pub address: Address,
    /// The commissionable instance id from the advertisement.
    pub commissionable_id: u64,
}

/// Up to `max` commissionable devices matching `filter`. Assumes the mDNS
/// backend runs (see [`crate::mdns`]): this only places the browse
/// requests.
pub async fn discover_all_with_running_backend(
    matter: &Matter<'_>,
    filter: &CommissionableFilter,
    max: u8,
    window_ms: Option<u32>,
) -> Vec<DiscoveredDevice> {
    let query = MdnsBrowseQuery::Commissionable(filter.clone());
    let window_ms = window_ms.unwrap_or(DEFAULT_BROWSE_WINDOW_MS);

    let mut ids = Vec::new();
    if let Err(e) = matter
        .transport()
        .browse(&query, window_ms, |id| ids.push(id))
        .await
    {
        warn!("discovery: browse failed: {e:?}");
    }

    let mut found = Vec::new();

    for commissionable_id in ids {
        if found.len() >= max as usize {
            break;
        }

        match matter
            .transport()
            .resolve_commissionable_addrs(commissionable_id)
            .await
        {
            Ok(addrs) => {
                // rs-matter ranks the addresses, preferring IPv6 (link-local →
                // ULA → global → IPv4).
                if let Some(address) = addrs.first().copied() {
                    info!("discovery: found {address} (id={commissionable_id:#x})");
                    found.push(DiscoveredDevice {
                        address,
                        commissionable_id,
                    });
                }
            }
            Err(e) => info!("discovery: cannot resolve {commissionable_id:#x}: {e:?}"),
        }
    }

    info!("discovery: found {} device(s)", found.len());
    found
}

/// The nodes advertising on our fabric `fab_idx` - the devices holding it that
/// are reachable - other than ourselves, for a host already running the mDNS
/// backend.
pub async fn browse_fabric_with_running_backend(
    matter: &Matter<'_>,
    fab_idx: NonZeroU8,
    window_ms: Option<u32>,
) -> Result<Vec<NodeId>, Error> {
    let (compressed_fabric_id, own_node_id) = matter.with_state(|state| {
        let fabric = state.fabrics.fabric(fab_idx)?;

        Ok::<_, Error>((fabric.compressed_fabric_id(), fabric.node_id()))
    })?;

    let query = MdnsBrowseQuery::Operational {
        compressed_fabric_id,
    };

    let mut nodes = Vec::new();
    matter
        .transport()
        .browse(
            &query,
            window_ms.unwrap_or(DEFAULT_BROWSE_WINDOW_MS),
            |node_id| {
                if node_id != own_node_id {
                    nodes.push(node_id);
                }
            },
        )
        .await?;

    Ok(nodes)
}
