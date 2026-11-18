//! A one-shot DNS-SD browse, for the services rs-matter does not browse, on
//! hosts without an mDNS daemon to ask.
//!
//! It sends legacy unicast mDNS queries, from an ephemeral port, which
//! responders answer directly to that port (RFC 6762, section 6.7), so it
//! does not compete with an mDNS responder for port 5353.

use alloc::string::String;
use alloc::vec::Vec;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};

use async_io::Async;
use log::debug;

use rs_matter::utils::futures::{select3, Either3};
use rs_matter::utils::timers::{Duration, Instant, Timer};

const MDNS_PORT: u16 = 5353;
const MDNS_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_AAAA: u16 = 28;
const TYPE_SRV: u16 = 33;
const CLASS_IN: u16 = 1;

/// A service instance found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// Its instance name, the first label of its full name.
    pub name: String,
    /// Where it listens. A link-local IPv6 address carries the interface it
    /// was heard on.
    pub addrs: Vec<SocketAddr>,
}

/// What the answers say so far.
#[derive(Default)]
struct Records {
    /// Full instance names.
    instances: Vec<String>,
    /// Instance name → (host, port).
    srv: Vec<(String, String, u16)>,
    /// Host name → address.
    addrs: Vec<(String, IpAddr)>,
    /// Instance name → the address of whoever answered for it.
    from: Vec<(String, SocketAddr)>,
}

impl Records {
    /// The instances, resolved as far as the answers go.
    fn instances(&self, service: &str) -> Vec<Instance> {
        self.instances
            .iter()
            .filter_map(|full| {
                let name = full
                    .strip_suffix(service)?
                    .strip_suffix('.')
                    .unwrap_or(full)
                    .into();
                let (_, host, port) = self
                    .srv
                    .iter()
                    .find(|(i, ..)| i.eq_ignore_ascii_case(full))?;

                let mut addrs: Vec<SocketAddr> = self
                    .addrs
                    .iter()
                    .filter(|(h, _)| h.eq_ignore_ascii_case(host))
                    .map(|(_, ip)| SocketAddr::new(*ip, *port))
                    .collect();
                // A responder answers for its own services, mostly.
                if addrs.is_empty() {
                    addrs.extend(
                        self.from
                            .iter()
                            .filter(|(i, _)| i.eq_ignore_ascii_case(full))
                            .map(|(_, from)| SocketAddr::new(from.ip(), *port)),
                    );
                }
                // A link-local address is only good on the interface it came from.
                for addr in &mut addrs {
                    if let SocketAddr::V6(v6) = addr {
                        if v6.ip().is_unicast_link_local() && v6.scope_id() == 0 {
                            let scope = self
                                .from
                                .iter()
                                .find_map(|(_, from)| match from {
                                    SocketAddr::V6(from) => Some(from.scope_id()),
                                    _ => None,
                                })
                                .unwrap_or(0);
                            v6.set_scope_id(scope);
                        }
                    }
                }

                Some(Instance { name, addrs })
            })
            .collect()
    }

    /// Take the records of the DNS message `msg`, which came from `from`.
    fn take(&mut self, msg: &[u8], from: SocketAddr) -> Option<()> {
        if msg.len() < 12 || msg[2] & 0x80 == 0 {
            // Not a response.
            return None;
        }
        let count = |at: usize| u16::from_be_bytes([msg[at], msg[at + 1]]) as usize;
        let (questions, records) = (count(4), count(6) + count(8) + count(10));

        let mut at = 12;
        for _ in 0..questions {
            at = read_name(msg, at)?.1 + 4;
        }

        for _ in 0..records {
            let (owner, next) = read_name(msg, at)?;
            let header = msg.get(next..next + 10)?;
            let rtype = u16::from_be_bytes([header[0], header[1]]);
            let len = u16::from_be_bytes([header[8], header[9]]) as usize;
            let data_at = next + 10;
            let data = msg.get(data_at..data_at + len)?;

            match rtype {
                TYPE_PTR => {
                    let (instance, _) = read_name(msg, data_at)?;
                    if !self
                        .instances
                        .iter()
                        .any(|i| i.eq_ignore_ascii_case(&instance))
                    {
                        self.from.push((instance.clone(), from));
                        self.instances.push(instance);
                    }
                }
                TYPE_SRV if len >= 6 => {
                    let port = u16::from_be_bytes([data[4], data[5]]);
                    let (host, _) = read_name(msg, data_at + 6)?;
                    self.srv.retain(|(i, ..)| !i.eq_ignore_ascii_case(&owner));
                    self.from.push((owner.clone(), from));
                    self.srv.push((owner, host, port));
                }
                TYPE_A if len == 4 => {
                    let ip = IpAddr::from(<[u8; 4]>::try_from(data).ok()?);
                    self.add_addr(owner, ip);
                }
                TYPE_AAAA if len == 16 => {
                    let ip = IpAddr::from(<[u8; 16]>::try_from(data).ok()?);
                    self.add_addr(owner, ip);
                }
                _ => (),
            }

            at = data_at + len;
        }

        Some(())
    }

    fn add_addr(&mut self, host: String, ip: IpAddr) {
        if !self
            .addrs
            .iter()
            .any(|(h, a)| h.eq_ignore_ascii_case(&host) && *a == ip)
        {
            self.addrs.push((host, ip));
        }
    }
}

/// The instances of `service` (e.g. `_meshcop-e._udp.local`) that answer
/// within `window`.
pub async fn browse(service: &str, window: Duration) -> Vec<Instance> {
    let v4 = Async::<UdpSocket>::bind((Ipv4Addr::UNSPECIFIED, 0)).ok();
    let v6 = Async::<UdpSocket>::bind((Ipv6Addr::UNSPECIFIED, 0)).ok();

    let query = query(rand::random(), service, TYPE_PTR);
    if let Some(v4) = &v4 {
        if let Err(e) = v4.send_to(&query, (MDNS_V4, MDNS_PORT)).await {
            debug!("dnssd: no IPv4 query: {e}");
        }
    }
    if let Some(v6) = &v6 {
        let to = SocketAddrV6::new(MDNS_V6, MDNS_PORT, 0, 0);
        if let Err(e) = v6.send_to(&query, to).await {
            debug!("dnssd: no IPv6 query: {e}");
        }
    }

    let mut records = Records::default();
    let deadline = Instant::now() + window;
    let (mut buf4, mut buf6) = ([0; 1500], [0; 1500]);
    let (mut ok4, mut ok6) = (true, true);

    loop {
        let (socket4, socket6) = (v4.as_ref().filter(|_| ok4), v6.as_ref().filter(|_| ok6));
        let (into4, into6) = (&mut buf4, &mut buf6);
        let recv4 = async move {
            match socket4 {
                Some(socket) => socket.recv_from(into4).await,
                None => core::future::pending().await,
            }
        };
        let recv6 = async move {
            match socket6 {
                Some(socket) => socket.recv_from(into6).await,
                None => core::future::pending().await,
            }
        };

        let received = select3(recv4, recv6, Timer::at(deadline)).await;
        match received {
            Either3::First(Ok((len, from))) => {
                records.take(&buf4[..len], from);
            }
            Either3::Second(Ok((len, from))) => {
                records.take(&buf6[..len], from);
            }
            Either3::First(Err(e)) => {
                debug!("dnssd: IPv4 receive failed: {e}");
                ok4 = false;
            }
            Either3::Second(Err(e)) => {
                debug!("dnssd: IPv6 receive failed: {e}");
                ok6 = false;
            }
            Either3::Third(()) => break,
        }
    }

    records.instances(service)
}

/// A DNS query for `name`, of `rtype`, as a legacy unicast mDNS query.
fn query(id: u16, name: &str, rtype: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(18 + name.len());
    msg.extend_from_slice(&id.to_be_bytes());
    // Flags 0, one question, no records.
    msg.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.split('.').filter(|label| !label.is_empty()) {
        msg.push(label.len() as u8);
        msg.extend_from_slice(label.as_bytes());
    }
    msg.push(0);
    msg.extend_from_slice(&rtype.to_be_bytes());
    msg.extend_from_slice(&CLASS_IN.to_be_bytes());
    msg
}

/// The name at `at` in `msg`, dotted, compression pointers followed; and
/// where it ends in place.
fn read_name(msg: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    let mut end = None;
    let mut jumps = 0;

    loop {
        let len = *msg.get(at)? as usize;
        match len {
            0 => break,
            len if len & 0xC0 == 0xC0 => {
                let target = ((len & 0x3F) << 8) | *msg.get(at + 1)? as usize;
                end.get_or_insert(at + 2);
                jumps += 1;
                if jumps > 32 {
                    return None;
                }
                at = target;
                continue;
            }
            len => {
                let label = msg.get(at + 1..at + 1 + len)?;
                if !name.is_empty() {
                    name.push('.');
                }
                name.push_str(core::str::from_utf8(label).ok()?);
                at += 1 + len;
            }
        }
    }

    Some((name, end.unwrap_or(at + 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response with a PTR, an SRV and an AAAA record, compressed.
    fn response() -> Vec<u8> {
        let mut msg = alloc::vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 2];
        // PTR: _meshcop-e._udp.local -> TbrNode._meshcop-e._udp.local
        let service_at = msg.len();
        for label in ["_meshcop-e", "_udp", "local"] {
            msg.push(label.len() as u8);
            msg.extend_from_slice(label.as_bytes());
        }
        msg.push(0);
        msg.extend_from_slice(&[0, 12, 0, 1, 0, 0, 0, 120, 0, 10]);
        let instance_at = msg.len();
        msg.push(7);
        msg.extend_from_slice(b"TbrNode");
        msg.extend_from_slice(&[0xC0, service_at as u8]);
        // SRV: port 49191 (0xC027), host tbr.local
        msg.extend_from_slice(&[0xC0, instance_at as u8]);
        msg.extend_from_slice(&[0, 33, 0x80, 1, 0, 0, 0, 120, 0, 12, 0, 0, 0, 0, 0xC0, 0x27]);
        let host_at = msg.len();
        msg.push(3);
        msg.extend_from_slice(b"tbr");
        msg.extend_from_slice(&[0xC0, (service_at + 16) as u8]); // "local"
                                                                 // AAAA: tbr.local -> fd00::1
        msg.extend_from_slice(&[0xC0, host_at as u8]);
        msg.extend_from_slice(&[0, 28, 0x80, 1, 0, 0, 0, 120, 0, 16]);
        msg.extend_from_slice(&[0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        msg
    }

    #[test]
    fn reads_a_compressed_response() {
        let msg = response();
        let mut records = Records::default();
        let from: SocketAddr = "[fd00::1]:5353".parse().unwrap();
        records.take(&msg, from).unwrap();

        let found = records.instances("_meshcop-e._udp.local");
        assert_eq!(
            found,
            [Instance {
                name: "TbrNode".into(),
                addrs: alloc::vec!["[fd00::1]:49191".parse().unwrap()],
            }]
        );
    }

    #[test]
    fn names_query() {
        let msg = query(7, "_meshcop-e._udp.local", TYPE_PTR);
        assert_eq!(read_name(&msg, 12).unwrap().0, "_meshcop-e._udp.local");
        assert_eq!(&msg[msg.len() - 4..], &[0, 12, 0, 1]);
    }
}
