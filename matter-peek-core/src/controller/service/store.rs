//! The controller's store as a whole: exporting it, and whether one may be
//! imported in its place.

use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use rs_matter::crypto::Crypto;
use rs_matter::persist::KvBlobStore;

use crate::api::{PeekError, StoreArchive, StoreEntry};
use crate::ota::OtaImageStore;

use super::{failed, Ctx};

impl<C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'_, C, S, O> {
    /// The store's key-value entries, as an archive (with no other files: the
    /// host adds them).
    pub(super) fn export_store(&self) -> Result<StoreArchive, PeekError> {
        let entries = self
            .store
            .entries()
            .map_err(|e| failed("cannot read the store", e))?
            .into_iter()
            .map(|(key, data)| StoreEntry {
                key,
                data: BASE64.encode(data),
            })
            .collect();

        Ok(StoreArchive {
            format: StoreArchive::FORMAT.to_string(),
            version: StoreArchive::VERSION,
            entries,
            files: Vec::new(),
        })
    }

    /// Whether another store may replace this one: only without devices on
    /// our fabrics, which would keep a fabric nobody holds then.
    pub(super) fn import_check(&self) -> Result<(), PeekError> {
        let devices = self.list_devices(None)?.devices.len();
        if devices > 0 {
            return Err(PeekError::failed(format!(
                "this controller has {devices} device(s) on its fabrics: a store is imported only \
                 into one without devices (decommission them, or import into a new controller)"
            )));
        }

        Ok(())
    }
}
