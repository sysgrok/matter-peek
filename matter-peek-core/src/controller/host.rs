//! Running the controller on this host (`std`): its thread, UDP socket and
//! store directory, which is locked (one controller per store).

use core::future::Future;
use core::net::SocketAddr;
use core::pin::Pin;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use std::fs::{File, TryLockError};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};

use async_io::Async;
use directories::ProjectDirs;
use embassy_futures::select::{select, select_slice};
use rand::rngs::StdRng;

use log::{error, info, warn};

use rs_matter::crypto::default_crypto;
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::persist::DirKvBlobStore;
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::Matter;

use crate::api::{MatterPeek, PeekConfig, PeekError, Service, StoreArchive};
use crate::mdns::HostMdns;
use crate::persist::kv_buf;
use crate::service::{self, Platform, Runner, Task};

use dir_ota::DirOtaStore;

pub(crate) mod clock;
mod dir_ota;
mod import;

/// The stack size of the controller's thread.
const THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;

/// The file a controller locks in its store's directory while it runs.
const LOCK_FILE: &str = "lock";

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl MatterPeek {
    /// Start a controller on its own thread, keeping its state in the store
    /// directory of `config`; all of it runs on that thread (see
    /// [`MatterPeek::start_with`] for more). Fails if another controller holds
    /// that store. The controller stops once every handle to it is dropped.
    #[cfg_attr(feature = "uniffi", uniffi::constructor)]
    pub fn start(config: PeekConfig) -> Result<Arc<Self>, PeekError> {
        Self::start_with(config, Joined)
    }

    /// Replace this controller's store with `archive` (see
    /// [`MatterPeek::export_store`]) from its next start, which is for the
    /// host to do. Only while it has no devices on its fabrics: they would
    /// keep a fabric nobody holds then.
    pub async fn import_store(&self, archive: StoreArchive) -> Result<(), PeekError> {
        import::check(&archive)?;
        let Some(dir) = &self.store_dir else {
            return Err(PeekError::failed(
                "this controller keeps no store to replace",
            ));
        };
        self.import_check().await?;

        import::stage(dir, &archive)
    }
}

impl MatterPeek {
    /// [`MatterPeek::start`], with `runner` running the controller's futures
    /// from its thread: e.g. on an executor of the host's, on more threads.
    pub fn start_with<R>(config: PeekConfig, runner: R) -> Result<Arc<Self>, PeekError>
    where
        R: Runner + Send + 'static,
    {
        let store = Store::lock(config.store_dir.as_deref())?;
        import::apply(&store.dir);

        let (mut peek, service) = MatterPeek::new();
        peek.store_dir = Some(store.dir.clone());

        std::thread::Builder::new()
            .name("matter-peek".into())
            .stack_size(THREAD_STACK_SIZE)
            .spawn(move || run(service, store, config, runner))
            .map_err(|e| PeekError::failed(format!("cannot start the controller: {e}")))?;

        Ok(Arc::new(peek))
    }
}

/// Runs all of the controller in one future, on the thread it is called on.
pub struct Joined;

impl Runner for Joined {
    fn run<'a>(self, mut tasks: Vec<Task<'a>>, main: impl Future<Output = ()> + 'a) {
        let tasks = async {
            if tasks.is_empty() {
                core::future::pending::<()>().await;
            }
            select_slice(Pin::new(&mut tasks[..])).await;
        };

        async_io::block_on(select(Box::pin(main), tasks));
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
            .open(dir.join(LOCK_FILE))
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

/// Run the service on this host with `runner`, until every handle is dropped.
fn run<R: Runner>(service: Service, store: Store, config: PeekConfig, runner: R) {
    info!("host: store at {}", store.dir.display());

    let socket = match bind_socket() {
        Ok(socket) => socket,
        Err(e) => {
            error!("host: cannot bind the UDP socket: {e}");
            runner.run(
                Vec::new(),
                service::refuse_all(service, PeekError::failed("the controller has no network")),
            );
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
        crypto: default_crypto(rand::make_rng::<StdRng>(), DAC_PRIVKEY),
        kv: DirKvBlobStore::new(store.dir.clone()),
        kv_buf,
        ota: DirOtaStore::new(store.dir.join("ota")),
        network: &socket,
        mdns: HostMdns,
    };

    service.run(platform, &config, runner);
}

/// An ephemeral dual-stack IPv6 UDP socket: rs-matter canonicalizes
/// IPv4-mapped peer addresses, so it serves IPv4 peers too. Dual-stack set
/// explicitly: Windows makes IPv6 sockets IPv6-only by default.
fn bind_socket() -> std::io::Result<Async<UdpSocket>> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(false)?;
    socket.bind(&SocketAddr::from(([0u16; 8], 0)).into())?;
    let socket = Async::new(UdpSocket::from(socket))?;
    info!(
        "host: controller bound on {}",
        socket.get_ref().local_addr()?
    );

    Ok(socket)
}

/// Whether a controller runs on the store in `dir`.
pub fn store_in_use(dir: &Path) -> bool {
    File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))
        .is_ok_and(|lock| matches!(lock.try_lock(), Err(TryLockError::WouldBlock)))
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
    use rs_matter::pairing::qr::{scan::QrLuma, QrPayload};

    let luma = image::load_from_memory(image)
        .map_err(|e| PeekError::failed(format!("cannot read the image: {e}")))?
        .to_luma8();

    let text = QrLuma::new(luma.as_raw(), luma.width() as _)
        .scan()
        .map_err(|_| PeekError::failed("no Matter QR code found in the image"))?;

    // Only a Matter onboarding payload is a setup code.
    let mut buf = [0; 128];
    QrPayload::parse(&text, &mut buf)
        .map_err(|_| PeekError::failed(format!("not a Matter QR code: {text}")))?;

    Ok(text)
}

/// Whether this host has a route to `prefix` (e.g. `fd12:3456:789a:1::/64`)
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
        let thread: std::net::Ipv6Addr = "fd12:3456:789a:1::".parse().unwrap();

        assert!(covers(thread, 64, thread, 64));
        assert!(covers("fd12::".parse().unwrap(), 16, thread, 64));
        // The default route does not count, nor a route narrower than the prefix.
        assert!(!covers(std::net::Ipv6Addr::UNSPECIFIED, 0, thread, 64));
        assert!(!covers(thread, 80, thread, 64));
        assert!(!covers(
            "fd12:3456:789a:2::".parse().unwrap(),
            64,
            thread,
            64
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_routes_parse() {
        let line = "fd123456789a00010000000000000000 40 00000000000000000000000000000000 00 \
                    fe80000000000000b6feb90a7b4e64a8 00000258 0000000d 00000000 00000003  wlp44s0";
        assert_eq!(
            ipv6_route(line),
            Some(("fd12:3456:789a:1::".parse().unwrap(), 64))
        );

        let reject = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 \
                      00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo";
        assert_eq!(ipv6_route(reject), None);
    }
}
