//! What a host provides to run the controller itself, e.g. without `std`:
//!
//! ```ignore
//! let (peek, service) = MatterPeek::new();
//!
//! // On the host's executor, until every handle to `peek` is dropped:
//! service
//!     .run(
//!         Platform { matter, crypto, kv, kv_buf: &KV_BUF, ota, network, mdns },
//!         &PeekConfig::default(),
//!     )
//!     .await;
//! ```
//!
//! - `matter`: the stack (`rs_matter::Matter`), with the controller's device
//!   details, not started yet;
//! - `crypto`: an `rs_matter::crypto::Crypto`;
//! - `kv`: an `rs_matter::persist::KvBlobStore` for the controller's fabric and
//!   device records, with its scratch buffer `kv_buf` ([`kv_buf()`]);
//! - `ota`: an [`OtaImageStore`] for the OTA images (`()`-like stores may keep
//!   none);
//! - `network`: the UDP network, as `rs_matter`'s `NetworkSend +
//!   NetworkReceive`;
//! - `mdns`: an [`Mdns`] backend, e.g. [`Builtin`] over the host's multicast
//!   socket.
//!
//! With `std`, [`MatterPeek::start`](super::MatterPeek::start) does all this on
//! its own thread.

#[cfg(feature = "std")]
pub use crate::host::data_dir;
pub use crate::mdns::{Builtin, Mdns};
pub use crate::ota::{OtaImage, OtaImageStore, OtaStore};
pub use crate::persist::{kv_buf, KvBuf};
pub use crate::service::Platform;

pub use super::peek::Service;
