//! Discovering commissionable Matter devices on the local network via mDNS.
//!
//! rs-matter's controller-side discovery is a rendezvous:
//! [`Transport::browse_commissionable`](rs_matter::transport::Transport::browse_commissionable)
//! places a browse request and waits for a running **mDNS backend** to fire the
//! query and deposit the first match. So discovery is "run a backend
//! concurrently with a browse".
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

use embassy_futures::select::{select, Either};

use log::info;

use rs_matter::crypto::Crypto;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::Address;
use rs_matter::Matter;

/// How long to wait for a matching commissionable device before giving up.
const DEFAULT_BROWSE_TIMEOUT_MS: u32 = 5_000;

/// A commissionable device found on the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveredDevice {
    /// Operational address — feed straight into commissioning / PASE.
    pub address: Address,
    /// The commissionable instance id from the advertisement (used to step to
    /// the *next* candidate when several share a discriminator).
    pub commissionable_id: u64,
}

/// Discover the first commissionable device matching `filter` (use
/// [`CommissionableFilter::default`] — all `None` — to match any), running the
/// compiled-in mDNS backend concurrently with the browse.
///
/// `crypto` is only needed by the builtin backend (randomized send delays); it
/// is accepted uniformly so the signature is backend-independent.
pub async fn discover_commissionable<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    filter: &CommissionableFilter,
    timeout_ms: Option<u32>,
) -> Result<DiscoveredDevice, Error> {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_BROWSE_TIMEOUT_MS);
    info!("discovery: browsing for a commissionable device (timeout {timeout_ms}ms)");
    browse_once(matter, crypto, filter, &[], timeout_ms).await
}

/// Discover *up to* `max` commissionable devices matching `filter`, for a UI
/// pick-list. Steps to the next candidate via the browse `exclude` set (by
/// commissionable id) until no more match or `max` is reached.
///
/// `crypto` must be `Copy` (each browse round needs its own; the rs-matter
/// backends are `Copy`).
pub async fn discover_all<C: Crypto + Copy>(
    matter: &Matter<'_>,
    crypto: C,
    filter: &CommissionableFilter,
    max: u8,
    timeout_ms: Option<u32>,
) -> Vec<DiscoveredDevice> {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_BROWSE_TIMEOUT_MS);
    let mut found = Vec::new();
    let mut exclude: Vec<u64> = Vec::new();

    while found.len() < max as usize {
        match browse_once(matter, crypto, filter, &exclude, timeout_ms).await {
            Ok(dev) => {
                exclude.push(dev.commissionable_id);
                found.push(dev);
            }
            // `NotFound` = browse exhausted / timed out: stop, return what we have.
            Err(_) => break,
        }
    }

    info!("discovery: found {} device(s)", found.len());
    found
}

/// Run the mDNS backend concurrently with a single `browse_commissionable`
/// round (honoring `exclude`), returning the first non-excluded match.
async fn browse_once<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    filter: &CommissionableFilter,
    exclude: &[u64],
    timeout_ms: u32,
) -> Result<DiscoveredDevice, Error> {
    let browse = matter
        .transport()
        .browse_commissionable(filter, exclude, timeout_ms);

    // The backend only ever ends on error; the browse ends with the result (or
    // NotFound on timeout). rs-matter returns the address in canonical form and
    // prefers IPv6 (link-local → ULA → global → IPv4).
    let backend = run_backend(matter, crypto);

    let mut browse = core::pin::pin!(browse);
    let mut backend = core::pin::pin!(backend);

    match select(&mut browse, &mut backend).await {
        Either::First(found) => {
            let (address, commissionable_id) = found?;
            info!("discovery: found {address} (id={commissionable_id:#x})");
            Ok(DiscoveredDevice {
                address,
                commissionable_id,
            })
        }
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

/// A small reminder if the feature set is misconfigured: with no backend
/// feature, discovery cannot work.
#[cfg(not(any(
    feature = "mdns-avahi",
    feature = "mdns-astro",
    feature = "mdns-builtin"
)))]
async fn run_backend<C: Crypto>(_matter: &Matter<'_>, _crypto: C) -> Result<(), Error> {
    compile_error!(
        "matter-peek-core: enable exactly one mDNS backend feature \
         (mdns-avahi / mdns-astro / mdns-builtin)"
    );
    #[allow(unreachable_code)]
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

        let mut socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
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
                    ipv6: ipv6_addr,
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
