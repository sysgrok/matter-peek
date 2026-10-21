//! OTA images and updates.

use core::cell::RefCell;

use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use log::{info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::ota_prov::OtaRequestorId;
use rs_matter::persist::KvBlobStore;

use crate::api::{OtaImageInfo, OtaListener, OtaProgress, PeekError};
use crate::ota::{OtaEvent, OtaImageStore, OtaVerifier};

use super::{convert, failed, Ctx};

/// The largest upload chunk taken.
const MAX_UPLOAD_CHUNK_LEN: usize = 4 * 1024 * 1024;

/// An OTA image file being uploaded in chunks: written as it arrives, and
/// checked as it is.
pub(super) struct Upload<'a, C: Crypto + 'a, O: OtaImageStore> {
    verifier: OtaVerifier<C::Hash<'a>>,
    partial: O::Partial,
    size: u64,
    received: u64,
}

/// Follows the OTA update of a device.
pub(super) struct OtaFollower {
    node_id: u64,
    name: String,
    listener: Arc<dyn OtaListener>,
}

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
    pub(super) fn ota_images(&self) -> Vec<OtaImageInfo> {
        self.ota
            .store()
            .images()
            .into_iter()
            .map(convert::ota_image)
            .collect()
    }

    pub(super) fn ota_upload_begin(&self, size: u64) -> Result<u32, PeekError> {
        let crypto: &'a C = self.crypto;
        let id = self.next_upload_id.get();
        self.next_upload_id.set(id.wrapping_add(1));

        let upload = crypto
            .hash()
            .and_then(|hasher| {
                Ok(Upload {
                    verifier: OtaVerifier::new(hasher),
                    partial: self.ota.store().begin(id)?,
                    size,
                    received: 0,
                })
            })
            .map_err(|e| failed("cannot start the upload", e))?;

        self.uploads.borrow_mut().insert(id, upload);

        Ok(id)
    }

    pub(super) fn ota_upload_chunk(
        &self,
        id: u32,
        offset: u64,
        chunk: &[u8],
    ) -> Result<(), PeekError> {
        if chunk.len() > MAX_UPLOAD_CHUNK_LEN {
            return self.ota_upload_fail(id, "a chunk is too big".into());
        }

        let mut uploads = self.uploads.borrow_mut();
        let Some(upload) = uploads.get_mut(&id) else {
            return Err(no_upload(id));
        };

        let taken = if offset != upload.received {
            Err(format!(
                "expected the chunk at {}, got {offset}",
                upload.received
            ))
        } else if upload.received + chunk.len() as u64 > upload.size {
            Err("more data than announced".to_string())
        } else if let Err(e) = upload.verifier.update(chunk) {
            Err(format!("not a valid OTA image ({:?})", e.code()))
        } else if let Err(e) = self.ota.store().write(&mut upload.partial, chunk) {
            Err(format!("cannot write the image ({:?})", e.code()))
        } else {
            upload.received += chunk.len() as u64;
            Ok(())
        };

        drop(uploads);

        match taken {
            Ok(()) => Ok(()),
            Err(message) => self.ota_upload_fail(id, message),
        }
    }

    pub(super) fn ota_upload_end(&self, id: u32) -> Result<Vec<OtaImageInfo>, PeekError> {
        let Some(upload) = self.uploads.borrow_mut().remove(&id) else {
            return Err(no_upload(id));
        };

        let store = self.ota.store();

        if upload.received != upload.size {
            store.discard(upload.partial);
            return Err(PeekError::failed(format!(
                "the upload ended at {} of {} bytes",
                upload.received, upload.size
            )));
        }

        let image = match upload.verifier.finish() {
            Ok(image) => image,
            Err(e) => {
                store.discard(upload.partial);
                return Err(failed("not a valid OTA image", e));
            }
        };

        store
            .finish(upload.partial, &image)
            .map_err(|e| failed("cannot store the OTA image", e))?;
        info!(
            "service: stored OTA image {} ({})",
            image.name, image.version_str
        );

        Ok(self.ota_images())
    }

    /// Drop upload `id` and its partial file.
    pub(super) fn ota_upload_discard(&self, id: u32) {
        if let Some(upload) = self.uploads.borrow_mut().remove(&id) {
            warn!("service: dropping OTA upload {id}");
            self.ota.store().discard(upload.partial);
        }
    }

    /// Drop upload `id`, failing with `message`.
    fn ota_upload_fail(&self, id: u32, message: String) -> Result<(), PeekError> {
        self.ota_upload_discard(id);
        Err(PeekError::failed(message))
    }

    pub(super) fn ota_delete(&self, name: &str) -> Result<Vec<OtaImageInfo>, PeekError> {
        self.ota
            .store()
            .remove(name)
            .map_err(|e| failed(&format!("cannot delete OTA image {name}"), e))?;

        Ok(self.ota_images())
    }

    /// Allow the device image `name`, have `progress` follow its update, and
    /// announce us to it as its OTA Provider.
    pub(super) async fn ota_update(
        &self,
        node_id: u64,
        name: &str,
        progress: Option<Arc<dyn OtaListener>>,
    ) -> Result<(), PeekError> {
        let dev = self.lookup_device(node_id)?;

        if !self
            .ota
            .store()
            .images()
            .iter()
            .any(|image| image.name == name)
        {
            return Err(PeekError::failed(format!("no OTA image {name}")));
        }

        // Before the announcement: the device asks for the update right away.
        self.ota.allow(
            OtaRequestorId {
                fab_idx: dev.handle.fabric_index,
                node_id,
            },
            name,
        );

        {
            let mut followers = self.ota_followers.borrow_mut();
            followers.retain(|follower| follower.node_id != node_id);
            if let Some(listener) = progress {
                followers.push(OtaFollower {
                    node_id,
                    name: name.to_string(),
                    listener,
                });
            }
        }

        self.controller
            .announce_ota_provider(dev.handle)
            .await
            .map_err(|e| failed("cannot announce us to the device", e))
    }
}

/// Hand each OTA event to the followers of that update; an update that ended
/// loses its followers.
pub(super) fn events(followers: Rc<RefCell<Vec<OtaFollower>>>) -> Box<dyn Fn(OtaEvent)> {
    Box::new(move |event| {
        let mut followers = followers.borrow_mut();

        let (node_id, image, progress) = match event {
            OtaEvent::Offered { node, image } => (
                Some(node.node_id),
                None,
                OtaProgress::Offered { name: image },
            ),
            OtaEvent::NotOffered { node } => (Some(node.node_id), None, OtaProgress::NotOffered),
            OtaEvent::Downloaded {
                image,
                offset,
                size,
            } => (
                None,
                Some(image),
                OtaProgress::Downloading {
                    percent: (offset as u128 * 100 / size.max(1) as u128) as u8,
                },
            ),
            OtaEvent::Applying { node, version } => {
                (Some(node.node_id), None, OtaProgress::Applying { version })
            }
            OtaEvent::Applied { node, version } => {
                (Some(node.node_id), None, OtaProgress::Applied { version })
            }
        };

        for follower in followers.iter() {
            let follows = match (&node_id, &image) {
                (Some(node_id), _) => follower.node_id == *node_id,
                (None, Some(image)) => follower.name == *image,
                (None, None) => false,
            };
            if follows {
                follower
                    .listener
                    .on_progress(follower.node_id, progress.clone());
            }
        }

        if let (Some(node_id), OtaProgress::Applied { .. } | OtaProgress::NotOffered) =
            (node_id, &progress)
        {
            followers.retain(|follower| follower.node_id != node_id);
        }
    })
}

fn no_upload(id: u32) -> PeekError {
    PeekError::failed(format!("no upload {id}"))
}
