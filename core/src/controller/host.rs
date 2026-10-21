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
            None => default_store_dir(),
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
                 if it is a matter-peek-server, connect to it instead",
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

    // Heap-allocated in place (the stack is big), and left for the process: a
    // `Matter` borrows itself.
    let matter = Box::leak(Box::<Matter>::new_uninit()).init_with(Matter::init(
        &TEST_DEV_DET,
        TEST_DEV_COMM,
        &TEST_DEV_ATT,
        0,
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
/// else one under the temp directory, which may not survive a reboot.
fn default_store_dir() -> PathBuf {
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
