//! The mDNS backend: it serves the stack's browse requests (discovery) and
//! operational resolves (re-addressing a listed device over CASE, which needs
//! its current address), so the service runs one for its whole lifetime.
//!
//! An embedded host passes [`Builtin`] over its own multicast socket, or its own
//! [`Mdns`]. This host's backend is chosen by cargo feature, the OS daemons
//! first:
//!
//! | feature        | backend                       | platform                   |
//! |----------------|-------------------------------|----------------------------|
//! | `mdns-avahi`   | Avahi (D-Bus)                 | Linux + Avahi (default)    |
//! | `mdns-resolve` | systemd-resolved (D-Bus)      | Linux + systemd-resolved   |
//! | `mdns-astro`   | Bonjour                       | macOS / iOS                |
//! | `mdns-builtin` | rs-matter's own, on :5353     | Android / Windows / tests  |

use core::future::Future;

use alloc::string::String;
#[cfg(any(feature = "mdns-avahi", test))]
use alloc::string::ToString;
use alloc::vec::Vec;

use rs_matter::crypto::Crypto;
use rs_matter::error::Error;
use rs_matter::transport::network::mdns::builtin::{BuiltinMdns, Host};
use rs_matter::transport::network::{Ipv4Addr, NetworkReceive, NetworkSend};
use rs_matter::Matter;

use crate::api::BorderRouter;

/// An mDNS backend for the controller's stack. It carries what it needs (a
/// D-Bus connection, sockets, …). Only one may run against a stack: two would
/// compete for its requests.
pub trait Mdns {
    /// Serve the browse and resolve requests of `matter`, until an error.
    fn run<C: Crypto>(
        &mut self,
        matter: &Matter<'_>,
        crypto: C,
    ) -> impl Future<Output = Result<(), Error>>;
}

impl<T: Mdns> Mdns for &mut T {
    fn run<C: Crypto>(
        &mut self,
        matter: &Matter<'_>,
        crypto: C,
    ) -> impl Future<Output = Result<(), Error>> {
        (**self).run(matter, crypto)
    }
}

/// rs-matter's own mDNS implementation, over the host's UDP socket bound to the
/// mDNS port and joined to its multicast groups.
pub struct Builtin<'a, S, R> {
    pub send: S,
    pub recv: R,
    /// The host's name and addresses.
    pub host: Host<'a>,
    /// The IPv4 address of the interface to multicast on.
    pub ipv4_interface: Option<Ipv4Addr>,
    /// The index of the interface to multicast on, for IPv6.
    pub ipv6_interface: Option<u32>,
}

impl<S: NetworkSend, R: NetworkReceive> Mdns for Builtin<'_, S, R> {
    async fn run<C: Crypto>(&mut self, matter: &Matter<'_>, crypto: C) -> Result<(), Error> {
        BuiltinMdns::new()
            .run(
                &mut self.send,
                &mut self.recv,
                &self.host,
                self.ipv4_interface,
                self.ipv6_interface,
                matter,
                crypto,
            )
            .await
    }
}

/// This host's backend, as the build's features choose it.
#[cfg(feature = "mdns-avahi")]
pub(crate) type HostMdns = Avahi;
#[cfg(all(feature = "mdns-resolve", not(feature = "mdns-avahi")))]
pub(crate) type HostMdns = Resolve;
#[cfg(all(
    feature = "mdns-astro",
    not(any(feature = "mdns-avahi", feature = "mdns-resolve"))
))]
pub(crate) type HostMdns = Astro;
#[cfg(all(
    feature = "mdns-builtin",
    not(any(
        feature = "mdns-avahi",
        feature = "mdns-resolve",
        feature = "mdns-astro"
    ))
))]
pub(crate) type HostMdns = HostBuiltin;
#[cfg(all(
    feature = "std",
    not(any(
        feature = "mdns-avahi",
        feature = "mdns-resolve",
        feature = "mdns-astro",
        feature = "mdns-builtin"
    ))
))]
pub(crate) type HostMdns = NoMdns;

/// Avahi, over the system D-Bus.
#[cfg(feature = "mdns-avahi")]
#[derive(Default)]
pub(crate) struct Avahi;

#[cfg(feature = "mdns-avahi")]
impl Mdns for Avahi {
    async fn run<C: Crypto>(&mut self, matter: &Matter<'_>, _crypto: C) -> Result<(), Error> {
        use rs_matter::transport::network::mdns::avahi::AvahiMdns;

        AvahiMdns::new(system_bus().await?).run(matter).await
    }
}

/// systemd-resolved, over the system D-Bus.
#[cfg(feature = "mdns-resolve")]
#[derive(Default)]
pub(crate) struct Resolve;

#[cfg(feature = "mdns-resolve")]
impl Mdns for Resolve {
    async fn run<C: Crypto>(&mut self, matter: &Matter<'_>, _crypto: C) -> Result<(), Error> {
        use rs_matter::transport::network::mdns::resolve::ResolveMdns;

        ResolveMdns::new(system_bus().await?).run(matter).await
    }
}

/// Bonjour.
#[cfg(feature = "mdns-astro")]
#[derive(Default)]
pub(crate) struct Astro;

#[cfg(feature = "mdns-astro")]
impl Mdns for Astro {
    async fn run<C: Crypto>(&mut self, matter: &Matter<'_>, _crypto: C) -> Result<(), Error> {
        use rs_matter::transport::network::mdns::astro::AstroMdns;

        AstroMdns::new().run(matter).await
    }
}

/// [`Builtin`] on its own socket, on the first suitable interface: Android's
/// backend, and Windows'. It shares the port with the system's responder, or
/// a device on the same host (`SO_REUSEADDR` / `SO_REUSEPORT`, multicast
/// loopback); on Android, the app lets multicast through Wi-Fi's power save.
#[cfg(feature = "mdns-builtin")]
#[derive(Default)]
pub(crate) struct HostBuiltin;

#[cfg(feature = "mdns-builtin")]
impl Mdns for HostBuiltin {
    async fn run<C: Crypto>(&mut self, matter: &Matter<'_>, crypto: C) -> Result<(), Error> {
        use std::net::UdpSocket;

        use socket2::{Domain, Protocol, Socket, Type};

        use rs_matter::transport::network::mdns::{
            MDNS_IPV4_BROADCAST_ADDR, MDNS_IPV6_BROADCAST_ADDR, MDNS_SOCKET_DEFAULT_BIND_ADDR,
        };

        let (ipv4_addr, ipv6_addr, interface) = interface()?;

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

        let ipv6 = [ipv6_addr];

        Builtin {
            send: &socket,
            recv: &socket,
            host: Host {
                hostname: "matter-peek",
                ip: ipv4_addr,
                ipv6: &ipv6,
            },
            ipv4_interface: Some(ipv4_addr),
            ipv6_interface: Some(interface),
        }
        .run(matter, crypto)
        .await
    }
}

/// No mDNS: discovery and operational resolves fail.
#[cfg(all(
    feature = "std",
    not(any(
        feature = "mdns-avahi",
        feature = "mdns-resolve",
        feature = "mdns-astro",
        feature = "mdns-builtin"
    ))
))]
#[derive(Default)]
pub(crate) struct NoMdns;

#[cfg(all(
    feature = "std",
    not(any(
        feature = "mdns-avahi",
        feature = "mdns-resolve",
        feature = "mdns-astro",
        feature = "mdns-builtin"
    ))
))]
impl Mdns for NoMdns {
    async fn run<C: Crypto>(&mut self, _matter: &Matter<'_>, _crypto: C) -> Result<(), Error> {
        log::error!("mdns: this build has no mDNS backend");
        Err(rs_matter::error::ErrorCode::NoNetworkInterface.into())
    }
}

/// The Thread border routers on the local network (MeshCoP, `_meshcop._udp`).
/// Looks until every extended PAN ID of `wanted` (if any) is found, the daemon
/// has nothing more, or [`BROWSE_WINDOW`] has passed. Avahi only; none with
/// the other backends.
pub(crate) async fn border_routers(wanted: &[u64]) -> Vec<BorderRouter> {
    #[cfg(feature = "mdns-avahi")]
    {
        let done = |found: &[Resolved]| {
            !wanted.is_empty()
                && wanted.iter().all(|xpan| {
                    found
                        .iter()
                        .any(|r| border_router(r).extended_pan_id == Some(*xpan))
                })
        };

        match avahi_browse("_meshcop._udp", done).await {
            Ok(found) => {
                let mut routers: Vec<BorderRouter> = Vec::new();
                for router in found.iter().map(border_router) {
                    if !routers.iter().any(|known| known.name == router.name) {
                        routers.push(router);
                    }
                }
                routers
            }
            Err(e) => {
                log::debug!("mdns: no border routers from Avahi: {e}");
                Vec::new()
            }
        }
    }

    #[cfg(not(feature = "mdns-avahi"))]
    {
        let _ = wanted;
        Vec::new()
    }
}

/// The Thread networks of [`border_routers`]: their extended PAN IDs and
/// names.
pub(crate) async fn thread_networks(wanted: &[u64]) -> Vec<(u64, String)> {
    border_routers(wanted)
        .await
        .into_iter()
        .filter_map(|router| Some((router.extended_pan_id?, router.network_name?)))
        .collect()
}

/// A Matter node the local network advertises (`_matter._tcp`), of any
/// fabric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MatterHost {
    pub compressed_fabric_id: u64,
    pub node_id: u64,
    /// Its host name: not its Thread extended address.
    pub host: String,
    pub address: String,
    /// How often it polls while idle and while active, and how long it stays
    /// active, ms (`SII`, `SAI`, `SAT`).
    pub idle_interval_ms: Option<u32>,
    pub active_interval_ms: Option<u32>,
    pub active_threshold_ms: Option<u32>,
    /// Whether it is an ICD of long idle time (`ICD=1`) or short (`ICD=0`).
    pub long_idle: Option<bool>,
}

/// The Matter nodes the local network advertises (`_matter._tcp`), looking
/// for [`BROWSE_WINDOW`] at most. Avahi only.
pub(crate) async fn matter_hosts() -> Vec<MatterHost> {
    #[cfg(feature = "mdns-avahi")]
    match avahi_browse("_matter._tcp", |_| false).await {
        Ok(found) => found.iter().filter_map(matter_host).collect(),
        Err(e) => {
            log::debug!("mdns: no Matter hosts from Avahi: {e}");
            Vec::new()
        }
    }

    #[cfg(not(feature = "mdns-avahi"))]
    Vec::new()
}

/// How long a browse looks at most.
#[cfg(feature = "mdns-avahi")]
const BROWSE_WINDOW: embassy_time::Duration = embassy_time::Duration::from_secs(2);

/// How long a browse looks at least, though the daemon has nothing more: on a
/// cold cache, it says so before slower responders have answered.
#[cfg(feature = "mdns-avahi")]
const BROWSE_MIN: embassy_time::Duration = embassy_time::Duration::from_millis(1500);

/// A service instance, resolved.
#[cfg(any(feature = "mdns-avahi", test))]
struct Resolved {
    name: String,
    host: String,
    address: String,
    txt: Vec<Vec<u8>>,
}

/// The instances of `service_type`, resolved, until `done` says so with
/// those found, the daemon has nothing more, or [`BROWSE_WINDOW`] has passed.
#[cfg(feature = "mdns-avahi")]
async fn avahi_browse(
    service_type: &str,
    done: impl Fn(&[Resolved]) -> bool,
) -> rs_matter::utils::zbus::Result<Vec<Resolved>> {
    use embassy_futures::select::{select, select3, Either, Either3};
    use embassy_time::{Instant, Timer};
    use futures_util::StreamExt;
    use rs_matter::utils::zbus_proxies::avahi::server2::Server2Proxy;
    use rs_matter::utils::zbus_proxies::avahi::service_browser::ServiceBrowserProxy;

    const IF_UNSPEC: i32 = -1;
    const PROTO_UNSPEC: i32 = -1;

    let connection = rs_matter::utils::zbus::Connection::system().await?;
    let avahi = Server2Proxy::new(&connection).await?;
    let path = avahi
        .service_browser_prepare(IF_UNSPEC, PROTO_UNSPEC, service_type, "", 0)
        .await?;
    let browser = ServiceBrowserProxy::builder(&connection)
        .path(path)?
        .build()
        .await?;
    let mut items = browser.receive_item_new().await?;
    let mut all_for_now = browser.receive_all_for_now().await?;
    browser.start().await?;

    let started = Instant::now();
    let deadline = started + BROWSE_WINDOW;
    let mut found = Vec::new();
    let mut nothing_more = false;

    while !done(&found) {
        let item = if nothing_more {
            match select(items.next(), Timer::at(started + BROWSE_MIN)).await {
                Either::First(Some(item)) => item,
                _ => break,
            }
        } else {
            match select3(items.next(), all_for_now.next(), Timer::at(deadline)).await {
                Either3::First(Some(item)) => item,
                Either3::Second(Some(_)) if Instant::now() < started + BROWSE_MIN => {
                    nothing_more = true;
                    continue;
                }
                _ => break,
            }
        };
        let Ok(args) = item.args() else { continue };

        let resolve = avahi.resolve_service(
            args.interface,
            args.protocol,
            args.name,
            args.type_,
            args.domain,
            PROTO_UNSPEC,
            0,
        );
        match select(resolve, Timer::at(deadline)).await {
            Either::First(Ok(resolved)) => found.push(Resolved {
                name: resolved.2,
                host: resolved.5,
                address: resolved.7,
                txt: resolved.9,
            }),
            Either::First(Err(_)) => continue,
            Either::Second(()) => break,
        }
    }

    let _ = browser.free().await;

    Ok(found)
}

/// A border router, as its MeshCoP TXT record describes it.
#[cfg(any(feature = "mdns-avahi", test))]
fn border_router(resolved: &Resolved) -> BorderRouter {
    let value = |key: &[u8]| {
        resolved.txt.iter().find_map(|entry| {
            entry
                .strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(b"="))
        })
    };
    let text = |key: &[u8]| {
        value(key)
            .and_then(|v| core::str::from_utf8(v).ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let u64_of = |key: &[u8]| Some(u64::from_be_bytes(value(key)?.try_into().ok()?));

    BorderRouter {
        name: resolved.name.clone(),
        vendor: text(b"vn"),
        model: text(b"mn"),
        network_name: text(b"nn"),
        extended_pan_id: u64_of(b"xp"),
        ext_address: u64_of(b"xa"),
        partition_id: value(b"pt")
            .and_then(|v| v.try_into().ok())
            .map(u32::from_be_bytes),
        thread_version: text(b"tv"),
        omr_prefix: value(b"omr").and_then(omr_prefix),
        address: Some(resolved.address.clone()).filter(|a| !a.is_empty()),
    }
}

/// An OMR prefix as MeshCoP gives it (its length in bits, then its bytes) in
/// the usual notation.
#[cfg(any(feature = "mdns-avahi", test))]
fn omr_prefix(value: &[u8]) -> Option<String> {
    let (&len, bytes) = value.split_first()?;
    if len > 128 || bytes.len() < (len as usize).div_ceil(8) {
        return None;
    }

    let mut octets = [0u8; 16];
    octets[..bytes.len().min(16)].copy_from_slice(&bytes[..bytes.len().min(16)]);

    Some(alloc::format!(
        "{}/{len}",
        core::net::Ipv6Addr::from(octets)
    ))
}

/// A Matter node, as `_matter._tcp` names it: instance
/// `<compressed fabric ID>-<node ID>`.
#[cfg(any(feature = "mdns-avahi", test))]
fn matter_host(resolved: &Resolved) -> Option<MatterHost> {
    let hex = |text: &str| (text.len() == 16).then(|| u64::from_str_radix(text, 16).ok())?;

    let number = |key: &[u8]| {
        resolved.txt.iter().find_map(|entry| {
            let value = entry.strip_prefix(key)?.strip_prefix(b"=")?;
            core::str::from_utf8(value).ok()?.trim().parse::<u32>().ok()
        })
    };

    let (fabric, node) = resolved.name.split_once('-')?;

    Some(MatterHost {
        compressed_fabric_id: hex(fabric)?,
        node_id: hex(node)?,
        host: resolved.host.clone(),
        address: resolved.address.clone(),
        idle_interval_ms: number(b"SII"),
        active_interval_ms: number(b"SAI"),
        active_threshold_ms: number(b"SAT"),
        long_idle: number(b"ICD").map(|icd| icd == 1),
    })
}

/// A connection to the system D-Bus.
#[cfg(any(feature = "mdns-avahi", feature = "mdns-resolve"))]
async fn system_bus() -> Result<rs_matter::utils::zbus::Connection, Error> {
    rs_matter::utils::zbus::Connection::system()
        .await
        .map_err(|_| rs_matter::error::ErrorCode::NoNetworkInterface.into())
}

/// The first non-loopback, up, broadcast-capable interface with both an IPv4 and
/// an IPv6 address.
#[cfg(feature = "mdns-builtin")]
#[inline(never)]
fn interface() -> Result<(Ipv4Addr, rs_matter::transport::network::Ipv6Addr, u32), Error> {
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
            log::error!("mdns: no network interface suitable for mDNS broadcasting");
            Error::from(rs_matter::error::ErrorCode::NoNetworkInterface)
        })?;

    // IPv6 multicast joins on this interface, not the default route's (on a
    // phone, maybe mobile data's or a VPN's).
    let index = nix::net::if_::if_nametoindex(iname.as_str()).map_err(|e| {
        log::error!("mdns: no index for interface {iname}: {e}");
        Error::from(rs_matter::error::ErrorCode::NoNetworkInterface)
    })?;

    log::info!("mdns: using interface {iname} (#{index}, {ip}/{ipv6})");
    Ok((ip.octets().into(), ipv6.octets().into(), index))
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn meshcop_txt_describes_a_border_router() {
        // As a Google Nest Hub advertises it.
        let resolved = Resolved {
            name: "Google Inc. Google Nest Hub #9206".into(),
            host: "fuchsia-1c53-f9d6-9dfe.local".into(),
            address: "192.168.10.170".into(),
            txt: vec![
                b"mn=Google Nest Hub".to_vec(),
                b"vn=Google Inc.".to_vec(),
                b"nn=NEST-PAN-0FD8".to_vec(),
                [
                    b"xp=".as_slice(),
                    &[0xB5, 0x66, 0x14, 0x7D, 0x38, 0xE3, 0x84, 0x20],
                ]
                .concat(),
                [
                    b"xa=".as_slice(),
                    &[0x6A, 0x94, 0x70, 0x54, 0x00, 0x26, 0x92, 0x06],
                ]
                .concat(),
                [b"pt=".as_slice(), &[0x24, 0x98, 0xFA, 0x62]].concat(),
                [
                    b"omr=".as_slice(),
                    &[0x40, 0xFD, 0xAC, 0xBC, 0xDF, 0xA3, 0xC9, 0x00, 0x01],
                ]
                .concat(),
                b"tv=1.4.0".to_vec(),
            ],
        };

        let router = border_router(&resolved);
        assert_eq!(router.vendor.as_deref(), Some("Google Inc."));
        assert_eq!(router.model.as_deref(), Some("Google Nest Hub"));
        assert_eq!(router.network_name.as_deref(), Some("NEST-PAN-0FD8"));
        assert_eq!(router.extended_pan_id, Some(0xB566_147D_38E3_8420));
        assert_eq!(router.ext_address, Some(0x6A94_7054_0026_9206));
        assert_eq!(router.partition_id, Some(0x2498_FA62));
        assert_eq!(router.omr_prefix.as_deref(), Some("fdac:bcdf:a3c9:1::/64"));
        assert_eq!(router.thread_version.as_deref(), Some("1.4.0"));

        // A short `xp`: no extended PAN ID.
        let short = Resolved {
            txt: vec![b"xp=12".to_vec(), b"nn=X".to_vec()],
            ..resolved
        };
        assert_eq!(border_router(&short).extended_pan_id, None);
    }

    #[test]
    fn matter_hosts_by_their_instance_names() {
        let resolved = |name: &str, txt: Vec<Vec<u8>>| Resolved {
            name: name.into(),
            host: "430500CDAE446BE3.local".into(),
            address: "fdac:bcdf:a3c9:1:62cb:eb4c:4e4a:8c73".into(),
            txt,
        };

        let host = matter_host(&resolved("EADDBAEB88C41C26-000000000001B686", Vec::new())).unwrap();
        assert_eq!(
            (host.compressed_fabric_id, host.node_id, host.host.as_str()),
            (0xEADD_BAEB_88C4_1C26, 0x1B686, "430500CDAE446BE3.local")
        );

        // A sleepy one says how it polls.
        let sleepy = resolved(
            "EADDBAEB88C41C26-0000000000000042",
            vec![b"SII=5000".to_vec(), b"SAI=300".to_vec(), b"ICD=1".to_vec()],
        );
        let host = matter_host(&sleepy).unwrap();
        assert_eq!(
            (
                host.idle_interval_ms,
                host.active_interval_ms,
                host.long_idle
            ),
            (Some(5000), Some(300), Some(true))
        );

        // Not an operational instance.
        assert_eq!(matter_host(&resolved("ivan-laptop", Vec::new())), None);
    }
}
