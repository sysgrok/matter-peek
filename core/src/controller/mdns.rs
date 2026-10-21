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
//! | `mdns-builtin` | rs-matter's own, on :5353     | fallback / Windows / tests |

use core::future::Future;

use rs_matter::crypto::Crypto;
use rs_matter::error::Error;
use rs_matter::transport::network::mdns::builtin::{BuiltinMdns, Host};
use rs_matter::transport::network::{Ipv4Addr, NetworkReceive, NetworkSend};
use rs_matter::Matter;

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

/// [`Builtin`] on its own socket, on the first suitable interface. Shares the
/// port with a co-located device (`SO_REUSEPORT`, multicast loopback): a test
/// convenience, not a deployment mode.
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

    log::info!("mdns: using interface {iname} ({ip}/{ipv6})");
    Ok((ip.octets().into(), ipv6.octets().into(), 0))
}
