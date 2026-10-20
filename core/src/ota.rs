//! The OTA Provider: firmware images the user uploads, offered to the devices
//! the user picks, and served to them over BDX.
//!
//! [`OtaProvider`] is the registry rs-matter's OTA Provider cluster and BDX
//! handler consult. It offers an image only to a device the user allowed to
//! update to it ([`OtaProvider::allow`]), the way Home Assistant does. The image
//! files themselves come from an [`OtaStore`], which the platform supplies.

use core::cell::RefCell;
use core::num::NonZeroU8;

use rs_matter::crypto::{Crypto, CryptoSensitive, Digest, HASH_LEN};
use rs_matter::dm::clusters::ota_prov::{
    OtaApplyOutcome, OtaImageMeta, OtaImages, OtaImagesRegistry, OtaQueryOutcome, OtaRequestorId,
};
use rs_matter::dm::clusters::ota_req::{
    self, AnnouncementReasonEnum, OtaSoftwareUpdateRequestorAttrResponses as _,
    OtaSoftwareUpdateRequestorClient as _,
};
use rs_matter::dm::endpoints::ROOT_ENDPOINT_ID;
use rs_matter::dm::EndptId;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::im::client::ImClient as _;
use rs_matter::im::{AttrPath, GenericPath};
use rs_matter::ota::{OtaImageHeader, OTA_IMAGE_DIGEST_SHA256};
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

extern crate alloc;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The most of a file's start [`OtaVerifier`] holds to find its header: the
/// prefix plus the largest header CHIP accepts.
const MAX_HEAD_LEN: usize = 16 + 1024;

/// The image files of the provider.
pub trait OtaStore {
    /// The stored images.
    fn images(&self) -> Vec<OtaImage>;

    /// The size of image file `name`, or `None` if there is no such file.
    fn size(&self, name: &str) -> Option<u64>;

    /// Read up to `buf.len()` bytes of image file `name` at `offset`.
    fn read(&self, name: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Error>;
}

impl<T> OtaStore for &T
where
    T: OtaStore,
{
    fn images(&self) -> Vec<OtaImage> {
        T::images(self)
    }

    fn size(&self, name: &str) -> Option<u64> {
        T::size(self, name)
    }

    fn read(&self, name: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        T::read(self, name, offset, buf)
    }
}

impl OtaStore for () {
    fn images(&self) -> Vec<OtaImage> {
        Vec::new()
    }

    fn size(&self, _name: &str) -> Option<u64> {
        None
    }

    fn read(&self, _name: &str, _offset: u64, _buf: &mut [u8]) -> Result<usize, Error> {
        Err(ErrorCode::NotFound.into())
    }
}

/// A stored OTA image, as its header describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtaImage {
    /// The file name: also the BDX file designator and the update token.
    pub name: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub version: u32,
    pub version_str: String,
    pub min_applicable_version: Option<u32>,
    pub max_applicable_version: Option<u32>,
    pub release_notes_url: Option<String>,
    /// The size of the whole file.
    pub size: u64,
}

impl OtaImage {
    /// The image whose file of `size` bytes starts with `header`, stored under
    /// [`Self::file_name`].
    pub fn new(header: &OtaImageHeader, size: u64) -> Self {
        Self {
            name: Self::file_name(header),
            vendor_id: header.vendor_id,
            product_id: header.product_id,
            version: header.software_version,
            version_str: header.software_version_str.to_string(),
            min_applicable_version: header.min_applicable_software_version,
            max_applicable_version: header.max_applicable_software_version,
            release_notes_url: header.release_notes_url.map(ToString::to_string),
            size,
        }
    }

    /// The file name of an image: one per vendor, product and version, e.g.
    /// `fff1-8001-2.ota`. It is 8 to 32 characters, as an update token must be.
    pub fn file_name(header: &OtaImageHeader) -> String {
        format!(
            "{:04x}-{:04x}-{}.ota",
            header.vendor_id, header.product_id, header.software_version
        )
    }

    /// Whether the image is for nodes of `vendor_id` and `product_id`; a zero
    /// ID in the image matches any.
    pub fn is_for_product(&self, vendor_id: u16, product_id: u16) -> bool {
        (self.vendor_id == 0 || self.vendor_id == vendor_id)
            && (self.product_id == 0 || self.product_id == product_id)
    }

    /// Whether a node running `software_version` can update to the image.
    pub fn is_update_for(&self, software_version: u32) -> bool {
        self.version > software_version
            && self
                .min_applicable_version
                .is_none_or(|min| software_version >= min)
            && self
                .max_applicable_version
                .is_none_or(|max| software_version <= max)
    }
}

/// What happens to an update, as the provider sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtaEvent {
    /// `node` asked for an update and was offered `image`.
    Offered { node: OtaRequestorId, image: String },
    /// `node` asked for an update and was told there is none for it.
    NotOffered { node: OtaRequestorId },
    /// `image` has been downloaded up to `offset` of `size` bytes.
    Downloaded {
        image: String,
        offset: u64,
        size: u64,
    },
    /// `node` downloaded its update and is told to apply it now.
    Applying { node: OtaRequestorId, version: u32 },
    /// `node` reports that it runs `version` now.
    Applied { node: OtaRequestorId, version: u32 },
}

/// The OTA Provider registry: offers the images of an [`OtaStore`] to the nodes
/// allowed to update, and serves them.
pub struct OtaProvider<S, F> {
    store: S,
    on_event: F,
    /// The nodes allowed to update, and to which image.
    allowed: RefCell<Vec<(OtaRequestorId, String)>>,
    /// The image being downloaded, and its last reported percentage.
    progress: RefCell<Option<(String, u8)>>,
}

impl<S, F> OtaProvider<S, F>
where
    S: OtaStore,
    F: Fn(OtaEvent),
{
    /// Create a provider over `store`, reporting what happens to `on_event`.
    pub const fn new(store: S, on_event: F) -> Self {
        Self {
            store,
            on_event,
            allowed: RefCell::new(Vec::new()),
            progress: RefCell::new(None),
        }
    }

    /// The image files.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Allow `node` to update to image `name`, the next time it asks.
    pub fn allow(&self, node: OtaRequestorId, name: &str) {
        let mut allowed = self.allowed.borrow_mut();

        allowed.retain(|(allowed, _)| *allowed != node);
        allowed.push((node, name.to_string()));
    }

    /// The image `node` is allowed to update to, if any.
    pub fn allowed(&self, node: OtaRequestorId) -> Option<String> {
        self.allowed
            .borrow()
            .iter()
            .find(|(allowed, _)| *allowed == node)
            .map(|(_, name)| name.clone())
    }

    /// The image `node` may be offered now.
    fn offer(
        &self,
        node: OtaRequestorId,
        vendor_id: u16,
        product_id: u16,
        current_version: u32,
    ) -> Option<OtaImage> {
        let name = self.allowed(node)?;

        self.store.images().into_iter().find(|image| {
            image.name == name
                && image.is_for_product(vendor_id, product_id)
                && image.is_update_for(current_version)
        })
    }
}

impl<S, F> OtaImagesRegistry for OtaProvider<S, F>
where
    S: OtaStore,
    F: Fn(OtaEvent),
{
    async fn query<'b>(
        &self,
        requestor: OtaRequestorId,
        vendor_id: u16,
        product_id: u16,
        current_version: u32,
        _requestor_can_consent: bool,
        buf: &'b mut [u8],
    ) -> OtaQueryOutcome<'b> {
        let offer = self
            .offer(requestor, vendor_id, product_id, current_version)
            .and_then(|image| {
                let (version_str, rest) = copy_str(buf, &image.version_str)?;
                let (name, _) = copy_str(rest, &image.name)?;

                Some((image.version, version_str, name))
            });

        let Some((version, version_str, name)) = offer else {
            (self.on_event)(OtaEvent::NotOffered { node: requestor });
            return OtaQueryOutcome::NotAvailable;
        };

        (self.on_event)(OtaEvent::Offered {
            node: requestor,
            image: name.to_string(),
        });

        OtaQueryOutcome::Available(OtaImageMeta {
            version,
            version_str,
            file_designator: name,
            update_token: name.as_bytes(),
            user_consent_needed: false,
        })
    }

    async fn apply(
        &self,
        requestor: OtaRequestorId,
        _update_token: &[u8],
        new_version: u32,
    ) -> OtaApplyOutcome {
        (self.on_event)(OtaEvent::Applying {
            node: requestor,
            version: new_version,
        });

        OtaApplyOutcome::Proceed { delay_secs: 0 }
    }

    async fn applied(
        &self,
        requestor: OtaRequestorId,
        _update_token: &[u8],
        software_version: u32,
    ) {
        self.allowed
            .borrow_mut()
            .retain(|(allowed, _)| *allowed != requestor);

        (self.on_event)(OtaEvent::Applied {
            node: requestor,
            version: software_version,
        });
    }
}

impl<S, F> OtaImages for OtaProvider<S, F>
where
    S: OtaStore,
    F: Fn(OtaEvent),
{
    async fn size(&self, file_designator: &[u8]) -> Option<u64> {
        self.store.size(core::str::from_utf8(file_designator).ok()?)
    }

    async fn read(
        &self,
        file_designator: &[u8],
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        let name = core::str::from_utf8(file_designator).map_err(|_| ErrorCode::NotFound)?;
        let size = self.store.size(name).ok_or(ErrorCode::NotFound)?;

        let read = self.store.read(name, offset, buf)?;

        // Report each new percent, not every block.
        let downloaded = offset + read as u64;
        let percent = (downloaded as u128 * 100 / size.max(1) as u128) as u8;

        let report = {
            let mut progress = self.progress.borrow_mut();
            let report = progress
                .as_ref()
                .is_none_or(|(image, last)| image != name || *last != percent);

            *progress = Some((name.to_string(), percent));
            report
        };

        if report {
            (self.on_event)(OtaEvent::Downloaded {
                image: name.to_string(),
                offset: downloaded,
                size,
            });
        }

        Ok(read)
    }
}

/// Checks an OTA image file as it arrives in pieces, of any size: its header,
/// then its payload against the header's size and SHA-256 digest. Holds only the
/// file's start, until the header is in.
pub struct OtaVerifier<H> {
    hasher: H,
    /// The start of the file, until it holds the whole header.
    head: Vec<u8>,
    /// What the header says, once it is in.
    expected: Option<Expected>,
    /// The payload bytes taken so far.
    payload_len: u64,
}

/// What an image's header says its payload is.
struct Expected {
    image: OtaImage,
    payload_size: u64,
    digest: [u8; HASH_LEN],
}

impl<H> OtaVerifier<H>
where
    H: Digest<HASH_LEN>,
{
    /// Check a file with `hasher`, a SHA-256 one.
    pub fn new(hasher: H) -> Self {
        Self {
            hasher,
            head: Vec::new(),
            expected: None,
            payload_len: 0,
        }
    }

    /// The image, once its header is in.
    pub fn image(&self) -> Option<&OtaImage> {
        self.expected.as_ref().map(|expected| &expected.image)
    }

    /// Take the next bytes of the file.
    pub fn update(&mut self, mut data: &[u8]) -> Result<(), Error> {
        if self.expected.is_none() {
            let len = data.len().min(MAX_HEAD_LEN - self.head.len());
            self.head.extend_from_slice(&data[..len]);
            data = &data[len..];

            let Some(payload_offset) = self.take_header()? else {
                return Ok(());
            };

            let payload = self.head.split_off(payload_offset);
            self.head = Vec::new();
            self.hash(&payload)?;
        }

        self.hash(data)
    }

    /// Check that the whole file is in, and right.
    pub fn finish(self) -> Result<OtaImage, Error> {
        let expected = self.expected.ok_or(ErrorCode::InvalidData)?;

        if self.payload_len != expected.payload_size {
            return Err(ErrorCode::InvalidData.into());
        }

        let mut digest = CryptoSensitive::<HASH_LEN>::new();
        self.hasher.finish(&mut digest)?;

        if *digest.access() != expected.digest {
            return Err(ErrorCode::InvalidData.into());
        }

        Ok(expected.image)
    }

    /// Decode the header once the file's start holds it, returning the payload
    /// offset; `None` while more is needed.
    fn take_header(&mut self) -> Result<Option<usize>, Error> {
        let (header, payload_offset) = match OtaImageHeader::decode(&self.head) {
            Ok(decoded) => decoded,
            // Any valid header fits in `MAX_HEAD_LEN`.
            Err(e) if self.head.len() >= MAX_HEAD_LEN => return Err(e),
            Err(_) => return Ok(None),
        };

        if header.image_digest_type != OTA_IMAGE_DIGEST_SHA256 {
            return Err(ErrorCode::InvalidData.into());
        }

        let digest = header
            .image_digest
            .0
            .try_into()
            .map_err(|_| ErrorCode::InvalidData)?;

        self.expected = Some(Expected {
            image: OtaImage::new(&header, payload_offset as u64 + header.payload_size),
            payload_size: header.payload_size,
            digest,
        });

        Ok(Some(payload_offset))
    }

    /// Hash payload bytes, refusing more than the header announced.
    fn hash(&mut self, data: &[u8]) -> Result<(), Error> {
        let expected = self.expected.as_ref().ok_or(ErrorCode::InvalidData)?;

        self.payload_len += data.len() as u64;
        if self.payload_len > expected.payload_size {
            return Err(ErrorCode::InvalidData.into());
        }

        self.hasher.update(data)
    }
}

/// Check a whole OTA image file: a valid header, and a payload matching its size
/// and SHA-256 digest.
pub fn verify(crypto: impl Crypto, file: &[u8]) -> Result<OtaImage, Error> {
    let mut verifier = OtaVerifier::new(crypto.hash()?);
    verifier.update(file)?;
    verifier.finish()
}

/// Announce us to node `node_id` on `fab_idx` as an OTA Provider with an update
/// for it, so that it asks us now. Sent to the endpoint of its OTA Requestor.
pub async fn announce<C>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> Result<(), Error>
where
    C: Crypto + Copy,
{
    let endpoint = requestor_endpoint(matter, crypto, fab_idx, node_id)
        .await?
        .ok_or(ErrorCode::NotFound)?;

    let (provider_node_id, vendor_id) = matter.with_state(|state| {
        let fabric = state.fabrics.fabric(fab_idx)?;
        Ok::<_, Error>((fabric.node_id(), fabric.vendor_id()))
    })?;

    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;

    exchange
        .ota_software_update_requestor()
        .announce_ota_provider(endpoint, |b| {
            b.provider_node_id(provider_node_id)?
                .vendor_id(vendor_id)?
                .announcement_reason(AnnouncementReasonEnum::UpdateAvailable)?
                .metadata_for_node(None)?
                .endpoint(ROOT_ENDPOINT_ID)?
                .end()
        })
        .await
}

/// The endpoint of the node's OTA Requestor cluster, if it has one.
async fn requestor_endpoint<C>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> Result<Option<EndptId>, Error>
where
    C: Crypto,
{
    let path = AttrPath::from_gp(&GenericPath::new(
        None,
        Some(ota_req::FULL_CLUSTER.id),
        Some(ota_req::AttributeId::UpdatePossible as _),
    ));

    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;
    let mut chunk = exchange
        .read_with(|msg| {
            msg.attr_requests_from(core::slice::from_ref(&path))?
                .fabric_filtered(true)?
                .end()
        })
        .await?;

    let mut endpoint = None;

    loop {
        if endpoint.is_none() {
            let resp = chunk.response()?;
            endpoint = resp
                .ota_software_update_requestor_read_resp()
                .update_possible()
                .find_map(|(endpoint, value)| value.is_ok().then_some(endpoint));
        }

        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => break,
        }
    }

    Ok(endpoint)
}

/// Copy `s` to the start of `buf`, returning the copy and the rest of `buf`.
fn copy_str<'b>(buf: &'b mut [u8], s: &str) -> Option<(&'b str, &'b mut [u8])> {
    let (head, rest) = buf.split_at_mut_checked(s.len())?;
    head.copy_from_slice(s.as_bytes());

    Some((core::str::from_utf8(head).ok()?, rest))
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use embassy_futures::block_on;

    use rs_matter::crypto::test_only_crypto;
    use rs_matter::tlv::Octets;

    use super::*;

    const NODE: OtaRequestorId = OtaRequestorId {
        fab_idx: NonZeroU8::MIN,
        node_id: 0x10,
    };

    const OTHER: OtaRequestorId = OtaRequestorId {
        fab_idx: NonZeroU8::MIN,
        node_id: 0x11,
    };

    const PAYLOAD: &[u8] = b"climanode firmware";

    /// Image files in memory.
    struct MemStore(Vec<Vec<u8>>);

    impl MemStore {
        fn file(&self, name: &str) -> Option<&[u8]> {
            self.0
                .iter()
                .find(|file| verify(test_only_crypto(), file).is_ok_and(|image| image.name == name))
                .map(Vec::as_slice)
        }
    }

    impl OtaStore for MemStore {
        fn images(&self) -> Vec<OtaImage> {
            self.0
                .iter()
                .filter_map(|file| verify(test_only_crypto(), file).ok())
                .collect()
        }

        fn size(&self, name: &str) -> Option<u64> {
            self.file(name).map(|file| file.len() as u64)
        }

        fn read(&self, name: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
            let file = self.file(name).ok_or(ErrorCode::NotFound)?;
            let rest = file.get(offset as usize..).unwrap_or(&[]);
            let len = rest.len().min(buf.len());
            buf[..len].copy_from_slice(&rest[..len]);

            Ok(len)
        }
    }

    /// A valid image file of `version` for VID 0xFFF1 / PID 0x8001.
    fn image_file(version: u32, version_str: &str) -> Vec<u8> {
        let crypto = test_only_crypto();
        let mut hasher = crypto.hash().unwrap();
        hasher.update(PAYLOAD).unwrap();
        let mut digest = CryptoSensitive::<HASH_LEN>::new();
        hasher.finish(&mut digest).unwrap();

        let header = OtaImageHeader {
            vendor_id: 0xFFF1,
            product_id: 0x8001,
            software_version: version,
            software_version_str: version_str,
            payload_size: PAYLOAD.len() as u64,
            min_applicable_software_version: None,
            max_applicable_software_version: None,
            release_notes_url: None,
            image_digest_type: OTA_IMAGE_DIGEST_SHA256,
            image_digest: Octets(digest.access()),
        };

        let mut file = vec![0; 512];
        let len = header.encode(&mut file).unwrap();
        file.truncate(len);
        file.extend_from_slice(PAYLOAD);

        file
    }

    fn query(
        provider: &OtaProvider<MemStore, impl Fn(OtaEvent)>,
        node: OtaRequestorId,
        product_id: u16,
        current_version: u32,
    ) -> Option<(u32, String, String)> {
        let mut buf = [0; 192];

        match block_on(provider.query(node, 0xFFF1, product_id, current_version, false, &mut buf)) {
            OtaQueryOutcome::Available(meta) => {
                assert_eq!(meta.update_token, meta.file_designator.as_bytes());
                Some((
                    meta.version,
                    meta.version_str.to_string(),
                    meta.file_designator.to_string(),
                ))
            }
            _ => None,
        }
    }

    #[test]
    fn verify_checks_header_and_payload() {
        let file = image_file(2, "climanode-espidf-c6-8-0.0.2");
        let image = verify(test_only_crypto(), &file).unwrap();

        assert_eq!(image.name, "fff1-8001-2.ota");
        assert_eq!(image.version_str, "climanode-espidf-c6-8-0.0.2");
        assert_eq!(image.size, file.len() as u64);

        let mut corrupt = file.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert!(verify(test_only_crypto(), &corrupt).is_err());

        assert!(verify(test_only_crypto(), &file[..file.len() - 1]).is_err());
        assert!(verify(test_only_crypto(), PAYLOAD).is_err());
    }

    #[test]
    fn verifies_in_chunks_of_any_size() {
        let file = image_file(2, "2.0");
        let crypto = test_only_crypto();

        for chunk in [1, 7, 100, file.len()] {
            let mut verifier = OtaVerifier::new(crypto.hash().unwrap());
            for piece in file.chunks(chunk) {
                verifier.update(piece).unwrap();
            }
            assert_eq!(verifier.finish().unwrap().name, "fff1-8001-2.ota");
        }

        // Too much data is refused as it arrives.
        let mut verifier = OtaVerifier::new(crypto.hash().unwrap());
        verifier.update(&file).unwrap();
        assert!(verifier.update(b"x").is_err());

        // Too little, at the end.
        let mut verifier = OtaVerifier::new(crypto.hash().unwrap());
        verifier.update(&file[..file.len() - 1]).unwrap();
        assert!(verifier.finish().is_err());

        // Not an image: no header in the first `MAX_HEAD_LEN` bytes.
        let mut verifier = OtaVerifier::new(crypto.hash().unwrap());
        assert!(verifier.update(&[0x55; MAX_HEAD_LEN]).is_err());
    }

    #[test]
    fn offers_only_the_allowed_image_to_the_allowed_node() {
        let events = RefCell::new(Vec::new());
        let provider = OtaProvider::new(MemStore(vec![image_file(2, "2.0")]), |event| {
            events.borrow_mut().push(event)
        });

        // Not before the user allows it.
        assert_eq!(query(&provider, NODE, 0x8001, 1), None);

        provider.allow(NODE, "fff1-8001-2.ota");

        assert_eq!(
            query(&provider, NODE, 0x8001, 1),
            Some((2, "2.0".into(), "fff1-8001-2.ota".into()))
        );

        // Not to another node, another product, or a node already running it.
        assert_eq!(query(&provider, OTHER, 0x8001, 1), None);
        assert_eq!(query(&provider, NODE, 0x8002, 1), None);
        assert_eq!(query(&provider, NODE, 0x8001, 2), None);

        assert!(events.borrow().contains(&OtaEvent::Offered {
            node: NODE,
            image: "fff1-8001-2.ota".into(),
        }));
        assert!(events
            .borrow()
            .contains(&OtaEvent::NotOffered { node: OTHER }));
    }

    #[test]
    fn applied_ends_the_permission() {
        let events = RefCell::new(Vec::new());
        let provider = OtaProvider::new(MemStore(vec![image_file(2, "2.0")]), |event| {
            events.borrow_mut().push(event)
        });

        provider.allow(NODE, "fff1-8001-2.ota");
        block_on(provider.applied(NODE, b"fff1-8001-2.ota", 2));

        assert_eq!(provider.allowed(NODE), None);
        assert_eq!(
            events.borrow().last(),
            Some(&OtaEvent::Applied {
                node: NODE,
                version: 2,
            })
        );
    }

    #[test]
    fn serves_the_image_and_reports_progress() {
        let file = image_file(2, "2.0");
        let events = RefCell::new(Vec::new());
        let provider = OtaProvider::new(MemStore(vec![file.clone()]), |event| {
            events.borrow_mut().push(event)
        });

        let name = b"fff1-8001-2.ota";
        assert_eq!(block_on(provider.size(name)), Some(file.len() as u64));
        assert_eq!(block_on(provider.size(b"none.ota")), None);

        let mut served = Vec::new();
        let mut buf = [0; 7];
        loop {
            let n = block_on(provider.read(name, served.len() as u64, &mut buf)).unwrap();
            if n == 0 {
                break;
            }
            served.extend_from_slice(&buf[..n]);
        }

        assert_eq!(served, file);

        let events = events.borrow();
        assert_eq!(
            events.last(),
            Some(&OtaEvent::Downloaded {
                image: "fff1-8001-2.ota".into(),
                offset: file.len() as u64,
                size: file.len() as u64,
            })
        );
        // One event per percent at most.
        assert!(events.len() <= 101);
    }
}
