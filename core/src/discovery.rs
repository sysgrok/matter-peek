//! Discovering Matter devices on the local network via mDNS: the commissionable
//! ones, and the ones on our fabric.
//!
//! rs-matter's controller-side discovery is a rendezvous:
//! [`Transport::browse`](rs_matter::transport::Transport::browse) places a browse
//! request and a running **mDNS backend** fires the query and deposits what it
//! finds. So discovery is "run a backend concurrently with a browse".
//!
//! ## Backend selection (cargo features)
//!
//! Every target platform ships a system mDNS daemon; running our own responder
//! on :5353 alongside it is a same-host-test hack, not a deployment strategy.
//! So OS-native backends are the default, `mdns-builtin` is the fallback:
//!
//! | feature        | backend                | platform                     |
//! |----------------|------------------------|------------------------------|
//! | `mdns-avahi`   | `AvahiMdns` (DBus)     | Linux + Avahi (default)      |
//! | `mdns-resolve` | `ResolveMdns` (DBus)\* | Linux + systemd-resolved     |
//! | `mdns-astro`   | `AstroMdns` (Bonjour)  | macOS / iOS                  |
//! | `mdns-builtin` | `BuiltinMdns` (:5353)  | fallback / Windows / tests   |
//!
//! \* the `resolve` path shares rs-matter's `zbus` feature; wiring is a TODO.
//!
//! Exactly one backend feature should be enabled per build; the binary crate
//! (`rpc`/`cli`) picks the platform-appropriate one.

extern crate alloc;
use alloc::vec::Vec;

use core::future::Future;
use core::num::NonZeroU8;

use embassy_futures::select::{select, Either};

use log::{info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::dm::NodeId;
use rs_matter::error::{Error, ErrorCode};
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

/// Discover a commissionable device matching `filter` (use
/// [`CommissionableFilter::default`] — all `None` — to match any), running the
/// compiled-in mDNS backend concurrently with the browse.
///
/// `crypto` is only needed by the builtin backend (randomized send delays); it
/// is accepted uniformly so the signature is backend-independent.
pub async fn discover_commissionable<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    filter: &CommissionableFilter,
    window_ms: Option<u32>,
) -> Result<DiscoveredDevice, Error> {
    with_backend(
        matter,
        crypto,
        discover_all_with_running_backend(matter, filter, 1, window_ms),
    )
    .await?
    .into_iter()
    .next()
    .ok_or_else(|| ErrorCode::NotFound.into())
}

/// Discover *up to* `max` commissionable devices matching `filter`, for a UI
/// pick-list, running the compiled-in mDNS backend concurrently.
pub async fn discover_all<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    filter: &CommissionableFilter,
    max: u8,
    window_ms: Option<u32>,
) -> Vec<DiscoveredDevice> {
    with_backend(
        matter,
        crypto,
        discover_all_with_running_backend(matter, filter, max, window_ms),
    )
    .await
    .unwrap_or_default()
}

/// Like [`discover_all`], but for a host that **already runs the mDNS backend**
/// (via [`run_mdns_backend`]) as a permanent task — this places the browse
/// requests without starting a second backend (which would contend on the shared
/// resolve/browse rendezvous).
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

/// Run `fut` with the mDNS backend alongside.
async fn with_backend<C, F>(matter: &Matter<'_>, crypto: C, fut: F) -> Result<F::Output, Error>
where
    C: Crypto,
    F: Future,
{
    let mut fut = core::pin::pin!(fut);
    let mut backend = core::pin::pin!(run_backend(matter, crypto));

    match select(&mut fut, &mut backend).await {
        Either::First(output) => Ok(output),
        Either::Second(r) => {
            // Backend exited first — always an error (it otherwise runs forever).
            r?;
            Err(ErrorCode::NoNetworkInterface.into())
        }
    }
}

// ---------------------------------------------------------------------------
// Backend wiring (one of these compiles, per feature)
// ---------------------------------------------------------------------------

/// Run the compiled-in mDNS backend **forever** (it only ever returns on error).
///
/// This services *both* browse requests (commissionable discovery) and
/// **operational resolve** requests — the latter is what `Exchange::initiate`
/// needs to re-address a persisted device over CASE (subscribe / peek /
/// decommission on a device whose secure session has lapsed). A long-lived host
/// (the RPC actor) must run this as a permanent concurrent arm; without it, every
/// operational resolve times out with `NotFound`. Short-lived hosts (the CLI's
/// one-shot discover) can instead rely on [`discover_commissionable`], which runs
/// the backend for the duration of the browse.
///
/// Do **not** run two of these against the same `Matter` stack — they'd both poll
/// the single resolve/browse rendezvous. When this is running, use the
/// `*_with_running_backend` functions rather than [`discover_commissionable`].
pub async fn run_mdns_backend<C: Crypto>(matter: &Matter<'_>, crypto: C) -> Result<(), Error> {
    run_backend(matter, crypto).await
}

#[cfg(feature = "mdns-avahi")]
async fn run_backend<C: Crypto>(matter: &Matter<'_>, _crypto: C) -> Result<(), Error> {
    use rs_matter::transport::network::mdns::avahi::AvahiMdns;
    use rs_matter::utils::zbus::Connection;

    let connection = Connection::system()
        .await
        .map_err(|_| Error::from(ErrorCode::NoNetworkInterface))?;
    AvahiMdns::new(connection).run(matter).await
}

#[cfg(feature = "mdns-astro")]
async fn run_backend<C: Crypto>(matter: &Matter<'_>, _crypto: C) -> Result<(), Error> {
    use rs_matter::transport::network::mdns::astro::AstroMdns;
    AstroMdns::new().run(matter).await
}

#[cfg(feature = "mdns-builtin")]
async fn run_backend<C: Crypto>(matter: &Matter<'_>, crypto: C) -> Result<(), Error> {
    builtin::run(matter, crypto).await
}

/// Fallback when no mDNS backend feature is enabled: discovery is unavailable
/// (returns an error at runtime). The crate still *compiles* without a backend —
/// important for `no_std` capability checks and for embedded targets that bring
/// their own backend feature. A real build should enable exactly one of
/// `mdns-avahi` / `mdns-astro` / `mdns-builtin`.
#[cfg(not(any(
    feature = "mdns-avahi",
    feature = "mdns-astro",
    feature = "mdns-builtin"
)))]
async fn run_backend<C: Crypto>(_matter: &Matter<'_>, _crypto: C) -> Result<(), Error> {
    log::error!("discovery: no mDNS backend feature enabled");
    Err(ErrorCode::NoNetworkInterface.into())
}

#[cfg(feature = "mdns-builtin")]
mod builtin {
    //! Same-host / fallback responder. Sets up the multicast socket directly
    //! (SO_REUSEPORT + multicast loopback so it can coexist with a co-located
    //! device on one host — a test convenience, not a deployment mode).
    //! Mirrors rs-matter's `tests/common/mdns.rs`.

    use std::net::UdpSocket;

    use log::{error, info};
    use socket2::{Domain, Protocol, Socket, Type};

    use rs_matter::crypto::Crypto;
    use rs_matter::error::{Error, ErrorCode};
    use rs_matter::transport::network::mdns::builtin::{BuiltinMdns, Host};
    use rs_matter::transport::network::mdns::{
        MDNS_IPV4_BROADCAST_ADDR, MDNS_IPV6_BROADCAST_ADDR, MDNS_SOCKET_DEFAULT_BIND_ADDR,
    };
    use rs_matter::transport::network::{Ipv4Addr, Ipv6Addr};
    use rs_matter::Matter;

    pub async fn run<C: Crypto>(matter: &Matter<'_>, crypto: C) -> Result<(), Error> {
        let (ipv4_addr, ipv6_addr, interface) = initialize_network()?;

        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.set_only_v6(false)?;
        socket.set_multicast_loop_v4(true)?;
        socket.set_multicast_loop_v6(true)?;
        socket.bind(&MDNS_SOCKET_DEFAULT_BIND_ADDR.into())?;
        let socket = async_io::Async::<UdpSocket>::new_nonblocking(socket.into())?;

        socket
            .get_ref()
            .join_multicast_v6(&MDNS_IPV6_BROADCAST_ADDR, interface)?;
        socket
            .get_ref()
            .join_multicast_v4(&MDNS_IPV4_BROADCAST_ADDR, &ipv4_addr)?;

        BuiltinMdns::new()
            .run(
                &socket,
                &socket,
                &Host {
                    hostname: "matter-peek",
                    ip: ipv4_addr,
                    // `Host::ipv6` is a slice of *every* address the host takes
                    // Matter traffic on. `initialize_network` deliberately picks
                    // a single interface, so that is a one-element slice.
                    ipv6: &[ipv6_addr],
                },
                Some(ipv4_addr),
                Some(interface),
                matter,
                crypto,
            )
            .await
    }

    /// Pick the first non-loopback, up, broadcast-capable interface with both an
    /// IPv4 and IPv6 address. Mirrors rs-matter's test helper.
    #[inline(never)]
    fn initialize_network() -> Result<(Ipv4Addr, Ipv6Addr, u32), Error> {
        use nix::{net::if_::InterfaceFlags, sys::socket::SockaddrIn6};

        let interfaces = || {
            nix::ifaddrs::getifaddrs().unwrap().filter(|ia| {
                ia.flags
                    .contains(InterfaceFlags::IFF_UP | InterfaceFlags::IFF_BROADCAST)
                    && !ia
                        .flags
                        .intersects(InterfaceFlags::IFF_LOOPBACK | InterfaceFlags::IFF_POINTOPOINT)
            })
        };

        let (iname, ip, ipv6) = interfaces()
            .filter_map(|ia| {
                ia.address
                    .and_then(|addr| addr.as_sockaddr_in6().map(SockaddrIn6::ip))
                    .map(|ipv6| (ia.interface_name, ipv6))
            })
            .filter_map(|(iname, ipv6)| {
                interfaces()
                    .filter(|ia2| ia2.interface_name == iname)
                    .find_map(|ia2| {
                        ia2.address
                            .and_then(|addr| addr.as_sockaddr_in().map(|addr| addr.ip()))
                            .map(|ip: std::net::Ipv4Addr| (iname.clone(), ip, ipv6))
                    })
            })
            .next()
            .ok_or_else(|| {
                error!("no network interface suitable for mDNS broadcasting");
                Error::from(ErrorCode::NoNetworkInterface)
            })?;

        info!("discovery(builtin): using interface {iname} ({ip}/{ipv6})");
        Ok((ip.octets().into(), ipv6.octets().into(), 0))
    }
}
