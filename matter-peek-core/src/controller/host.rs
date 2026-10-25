//! Running the controller on this host (`std`): its thread, UDP socket and
//! store directory, which is locked (one controller per store).

use core::net::SocketAddr;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;

use std::fs::{File, TryLockError};
use std::net::UdpSocket;
use std::path::PathBuf;

use async_io::Async;
use directories::ProjectDirs;

use log::{error, info, warn};

use rs_matter::crypto::default_crypto;
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::persist::DirKvBlobStore;
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::Matter;

use crate::api::{MatterPeek, PeekConfig, PeekError, Service};
use crate::mdns::HostMdns;
use crate::persist::kv_buf;
use crate::service::{self, Platform};

use dir_ota::DirOtaStore;

pub(crate) mod clock;
mod dir_ota;

/// The stack size of the controller's thread.
const THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl MatterPeek {
    /// Start a controller on its own thread, keeping its state in the store
    /// directory of `config`. Fails if another controller holds that store.
    /// The controller stops once every handle to it is dropped.
    #[cfg_attr(feature = "uniffi", uniffi::constructor)]
    pub fn start(config: PeekConfig) -> Result<Arc<Self>, PeekError> {
        let store = Store::lock(config.store_dir.as_deref())?;
        let (peek, service) = MatterPeek::new();

        std::thread::Builder::new()
            .name("matter-peek".into())
            .stack_size(THREAD_STACK_SIZE)
            .spawn(move || async_io::block_on(Box::pin(run(service, store, config))))
            .map_err(|e| PeekError::failed(format!("cannot start the controller: {e}")))?;

        Ok(Arc::new(peek))
    }
}

/// The store directory, locked for one controller: two sharing a store would
/// overwrite each other's fabric and device records.
struct Store {
    dir: PathBuf,
    _lock: File,
}

impl Store {
    /// Lock the store in `dir`, by default the platform's data directory.
    fn lock(dir: Option<&str>) -> Result<Self, PeekError> {
        let dir = match dir {
            Some(dir) => PathBuf::from(dir),
            None => data_dir(),
        };

        std::fs::create_dir_all(&dir).map_err(|e| {
            PeekError::failed(format!("cannot create the store at {}: {e}", dir.display()))
        })?;

        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("lock"))
            .map_err(|e| PeekError::failed(format!("cannot lock the store: {e}")))?;

        match lock.try_lock() {
            Ok(()) => Ok(Self { dir, _lock: lock }),
            Err(TryLockError::WouldBlock) => Err(PeekError::failed(format!(
                "the store at {} is in use by another matter-peek process; \
                 if it is a server (matter-peek serve), connect to it with --server",
                dir.display()
            ))),
            Err(TryLockError::Error(e)) => {
                Err(PeekError::failed(format!("cannot lock the store: {e}")))
            }
        }
    }
}

/// Run the service on this host until every handle is dropped.
async fn run(service: Service, store: Store, config: PeekConfig) {
    info!("host: store at {}", store.dir.display());

    let socket = match bind_socket() {
        Ok(socket) => socket,
        Err(e) => {
            error!("host: cannot bind the UDP socket: {e}");
            service::refuse_all(service, PeekError::failed("the controller has no network")).await;
            return;
        }
    };

    // What our operational records advertise: devices reach us on it (a time
    // sync client does).
    let port = socket.get_ref().local_addr().map_or(0, |addr| addr.port());

    // Heap-allocated in place (the stack is big), and left for the process: a
    // `Matter` borrows itself.
    let matter = Box::leak(Box::<Matter>::new_uninit()).init_with(Matter::init(
        &TEST_DEV_DET,
        TEST_DEV_COMM,
        &TEST_DEV_ATT,
        port,
    ));
    let kv_buf = Box::leak(Box::new(kv_buf()));

    let platform = Platform {
        matter,
        crypto: default_crypto(rand::rng(), DAC_PRIVKEY),
        kv: DirKvBlobStore::new(store.dir.clone()),
        kv_buf,
        ota: DirOtaStore::new(store.dir.join("ota")),
        network: &socket,
        mdns: HostMdns::default(),
    };

    service.run(platform, &config).await;
}

/// An ephemeral dual-stack IPv6 UDP socket: rs-matter canonicalizes
/// IPv4-mapped peer addresses, so it serves IPv4 peers too.
fn bind_socket() -> std::io::Result<Async<UdpSocket>> {
    let socket = Async::<UdpSocket>::bind(SocketAddr::from(([0u16; 8], 0)))?;
    info!(
        "host: controller bound on {}",
        socket.get_ref().local_addr()?
    );

    Ok(socket)
}

/// The platform's per-user data directory (`~/.local/share/matter-peek` on
/// Linux, `~/Library/Application Support/com.sysgrok.matter-peek` on macOS), or
/// else one under the temp directory, which may not survive a reboot: where the
/// store goes by default.
pub fn data_dir() -> PathBuf {
    match ProjectDirs::from("com", "sysgrok", "matter-peek") {
        Some(dirs) => dirs.data_dir().to_path_buf(),
        None => {
            warn!("host: no platform data directory; using the temp directory");
            std::env::temp_dir().join("matter-peek")
        }
    }
}

/// Read the setup code out of a picture of a QR code (PNG or JPEG).
pub(crate) fn read_qr_image(image: &[u8]) -> Result<String, PeekError> {
    use rs_matter::pairing::qr::{scan, QrPayload};

    let luma = image::load_from_memory(image)
        .map_err(|e| PeekError::failed(format!("cannot read the image: {e}")))?
        .to_luma8();

    let text = scan::scan_luma(luma.width() as _, luma.height() as _, luma.as_raw())
        .map_err(|_| PeekError::failed("no Matter QR code found in the image"))?;

    // Only a Matter onboarding payload is a setup code.
    let mut buf = [0; 128];
    QrPayload::parse(&text, &mut buf)
        .map_err(|_| PeekError::failed(format!("not a Matter QR code: {text}")))?;

    Ok(text.to_string())
}

/// Whether this host has a route to `prefix` (e.g. `fdac:bcdf:a3c9:1::/64`)
/// other than its default route; `None` where it cannot tell.
pub(crate) fn routes_to(prefix: &str) -> Option<bool> {
    let (address, len) = prefix.split_once('/')?;
    let address: std::net::Ipv6Addr = address.parse().ok()?;
    let len: u32 = len.parse().ok()?;

    #[cfg(target_os = "linux")]
    {
        let routes = std::fs::read_to_string("/proc/net/ipv6_route").ok()?;
        Some(
            routes
                .lines()
                .filter_map(ipv6_route)
                .any(|(dest, dest_len)| covers(dest, dest_len, address, len)),
        )
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (address, len);
        None
    }
}

/// A route of `/proc/net/ipv6_route` that leads somewhere: its destination
/// and prefix length.
#[cfg(target_os = "linux")]
fn ipv6_route(line: &str) -> Option<(std::net::Ipv6Addr, u32)> {
    /// A route that rejects what it matches.
    const RTF_REJECT: u32 = 0x0200;

    let fields: alloc::vec::Vec<&str> = line.split_whitespace().collect();
    let dest = u128::from_str_radix(fields.first()?, 16).ok()?;
    let dest_len = u32::from_str_radix(fields.get(1)?, 16).ok()?;
    let flags = u32::from_str_radix(fields.get(8)?, 16).ok()?;

    (flags & RTF_REJECT == 0).then_some((std::net::Ipv6Addr::from(dest), dest_len))
}

/// Whether the route to `dest/dest_len` covers `address/len`; a default
/// route (length 0) leads to the internet, not to a Thread network.
#[cfg(any(target_os = "linux", test))]
fn covers(dest: std::net::Ipv6Addr, dest_len: u32, address: std::net::Ipv6Addr, len: u32) -> bool {
    if dest_len == 0 || dest_len > len || dest_len > 128 {
        return false;
    }

    let mask = u128::MAX << (128 - dest_len);
    u128::from(dest) & mask == u128::from(address) & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_cover_prefixes() {
        let thread: std::net::Ipv6Addr = "fdac:bcdf:a3c9:1::".parse().unwrap();

        assert!(covers(thread, 64, thread, 64));
        assert!(covers("fdac::".parse().unwrap(), 16, thread, 64));
        // The default route does not count, nor a route narrower than the prefix.
        assert!(!covers(std::net::Ipv6Addr::UNSPECIFIED, 0, thread, 64));
        assert!(!covers(thread, 80, thread, 64));
        assert!(!covers(
            "fdac:bcdf:a3c9:2::".parse().unwrap(),
            64,
            thread,
            64
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_routes_parse() {
        let line = "fdacbcdfa3c900010000000000000000 40 00000000000000000000000000000000 00 \
                    fe80000000000000b6feb90a7b4e64a8 00000258 0000000d 00000000 00000003  wlp44s0";
        assert_eq!(
            ipv6_route(line),
            Some(("fdac:bcdf:a3c9:1::".parse().unwrap(), 64))
        );

        let reject = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 \
                      00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo";
        assert_eq!(ipv6_route(reject), None);
    }
}
