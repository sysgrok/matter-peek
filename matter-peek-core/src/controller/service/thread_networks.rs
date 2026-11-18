//! Thread networks: getting them from border routers, from Matter devices or
//! as a dataset, and giving them to Matter border routers.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use log::info;
use qrcodegen_no_heap::{QrCode, QrCodeEcc, Version};

use rs_matter::crypto::Crypto;
use rs_matter::dm::networks::wireless::ThreadTLV;
use rs_matter::persist::KvBlobStore;
use rs_matter::utils::sync::MaybeSend;
use rs_matter::utils::timers::{with_timeout, Duration};

use crate::api::{
    NetworkCredentials, PeekError, SavedNetwork, SharingBorderRouter, ThreadDataset, ThreadSource,
    ThreadTarget,
};
use crate::netconfig::Target;
use crate::network::{decode_hex, hex};
use crate::ota::OtaImageStore;
use crate::tbr;

use super::commission::saved_network;
use super::{failed, Ctx};

/// The fail-safe of a border router taking a network: time to form or join
/// it, and to answer.
const JOIN_FAIL_SAFE_SECS: u16 = 120;

/// The dataset TLVs with the network name and key.
const THREAD_NETWORK_NAME: u8 = 3;
const THREAD_NETWORK_KEY: u8 = 5;

impl<'a, C: Crypto, S: KvBlobStore + MaybeSend, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// The border routers that share their network with a one-time code now.
    pub(super) async fn sharing_border_routers(
        &self,
    ) -> Result<Vec<SharingBorderRouter>, PeekError> {
        #[cfg(feature = "meshcop")]
        {
            Ok(crate::mdns::sharing_border_routers().await)
        }

        #[cfg(not(feature = "meshcop"))]
        {
            Err(no_sharing_codes())
        }
    }

    /// Get a Thread network from `source`, and save it as the newest.
    pub(super) async fn get_thread_network(
        &self,
        source: ThreadSource,
    ) -> Result<SavedNetwork, PeekError> {
        let dataset = match source {
            ThreadSource::SharingCode { code, addr } => {
                self.dataset_from_code(&code, addr.as_deref()).await?
            }
            ThreadSource::Device {
                node_id,
                extended_pan_id,
            } => {
                self.dataset_from_device(node_id, extended_pan_id.as_deref())
                    .await?
            }
            ThreadSource::Dataset { dataset } => decode_hex(dataset.trim())
                .map_err(|_| PeekError::failed("the Thread dataset is not hex"))?,
        };

        check_dataset(&dataset)?;

        let network = NetworkCredentials::Thread {
            dataset: hex(&dataset),
        };
        self.save_network(&network);

        let saved = saved_network(&network);
        info!("thread: got the network {} ({})", saved.name, saved.id);

        Ok(saved)
    }

    /// The dataset of the saved Thread network `id`, with its QR code.
    pub(super) fn show_thread_dataset(&self, id: &str) -> Result<ThreadDataset, PeekError> {
        let dataset = self.thread_dataset(id)?.to_uppercase();
        let qr_rows = dataset_qr_rows(&dataset);

        Ok(ThreadDataset {
            id: id.into(),
            dataset,
            qr_rows,
        })
    }

    /// The dataset of the saved Thread network `id`, in hex.
    fn thread_dataset(&self, id: &str) -> Result<String, PeekError> {
        let networks = self
            .store
            .load_networks()
            .map_err(|e| failed("cannot read the saved networks", e))?;

        networks
            .iter()
            .find_map(|network| match network {
                NetworkCredentials::Thread { dataset } if saved_network(network).id == id => {
                    Some(dataset.clone())
                }
                _ => None,
            })
            .ok_or_else(|| PeekError::failed(format!("no saved Thread network {id}")))
    }

    /// Give the saved Thread network `id` to the listed device `node_id`.
    pub(super) async fn give_thread_network(
        &self,
        node_id: u64,
        id: &str,
        to: ThreadTarget,
    ) -> Result<(), PeekError> {
        let dataset = decode_hex(&self.thread_dataset(id)?)
            .map_err(|_| PeekError::failed("the saved dataset is not hex"))?;
        let dev = self.lookup_device(node_id)?;
        let target = Target {
            matter: self.matter,
            crypto: self.crypto,
            fab_idx: dev.handle.fabric_index,
            node_id,
        };

        let clusters = tbr::read(&target)
            .await
            .map_err(|e| failed("cannot read the device's Thread clusters", e))?;

        match to {
            ThreadTarget::BorderRouter => {
                let Some((endpoint, has_network)) = clusters.border_router else {
                    return Err(PeekError::failed(
                        "the device is not a Matter Thread border router",
                    ));
                };

                if has_network {
                    let current = tbr::active_dataset(&target, endpoint).await?;
                    if extended_pan_id(&current) == extended_pan_id(&dataset) {
                        info!("thread: device 0x{node_id:016X} is on that network already");
                        return Ok(());
                    }

                    return Err(PeekError::failed(format!(
                        "the border router is on another Thread network already ({}): it takes \
                         a new one only once it has left that one",
                        network_name(&current)
                    )));
                }

                target.arm_fail_safe(JOIN_FAIL_SAFE_SECS).await?;

                let set = with_timeout(
                    Duration::from_secs(JOIN_FAIL_SAFE_SECS as u64 - 10),
                    tbr::set_active_dataset(&target, endpoint, &dataset),
                )
                .await
                .unwrap_or_else(|_| {
                    Err(PeekError::failed(
                        "the border router did not get onto the network in time",
                    ))
                });
                if let Err(e) = set {
                    // Revert what it took, if anything.
                    let _ = target.arm_fail_safe(0).await;
                    return Err(e);
                }

                target.commit().await?;
                info!("thread: device 0x{node_id:016X} took the network {id}");
            }
            ThreadTarget::Directory { preferred } => {
                let Some((endpoint, _)) = clusters.directory else {
                    return Err(PeekError::failed(
                        "the device has no Thread Network Directory",
                    ));
                };

                tbr::add_to_directory(&target, endpoint, &dataset).await?;
                if preferred {
                    let xpan = extended_pan_id(&dataset)
                        .ok_or_else(|| PeekError::failed("the dataset has no extended PAN ID"))?;
                    tbr::set_preferred(&target, endpoint, &xpan).await?;
                }
                info!("thread: device 0x{node_id:016X} has the network {id} in its directory");
            }
        }

        Ok(())
    }

    /// The dataset a border router shares with the one-time `code`: the one
    /// at `addr`, else the only one sharing now.
    #[cfg(feature = "meshcop")]
    async fn dataset_from_code(
        &self,
        code: &str,
        addr: Option<&str>,
    ) -> Result<Vec<u8>, PeekError> {
        let code = crate::meshcop::sharing_code(code)?;

        let addr: String = match addr {
            Some(addr) => addr.trim().into(),
            None => {
                let routers = crate::mdns::sharing_border_routers().await;
                match routers.as_slice() {
                    [] => {
                        return Err(PeekError::failed(
                            "no border router shares its network now: have it show a sharing \
                             code first",
                        ))
                    }
                    [router] => router.addr.clone(),
                    routers => {
                        let names: Vec<_> = routers.iter().map(|r| r.name.as_str()).collect();
                        return Err(PeekError::failed(format!(
                            "{} border routers share their networks now; say which: {}",
                            routers.len(),
                            names.join(", ")
                        )));
                    }
                }
            }
        };
        let addr = addr
            .parse()
            .map_err(|_| PeekError::failed(format!("not an address and port: {addr}")))?;

        crate::meshcop::fetch_dataset(addr, &code).await
    }

    #[cfg(not(feature = "meshcop"))]
    async fn dataset_from_code(
        &self,
        _code: &str,
        _addr: Option<&str>,
    ) -> Result<Vec<u8>, PeekError> {
        Err(no_sharing_codes())
    }

    /// The dataset of a listed Matter device: of its own network, or of
    /// network `extended_pan_id` (hex) of its Thread Network Directory.
    async fn dataset_from_device(
        &self,
        node_id: u64,
        extended_pan_id: Option<&str>,
    ) -> Result<Vec<u8>, PeekError> {
        let dev = self.lookup_device(node_id)?;
        let target = Target {
            matter: self.matter,
            crypto: self.crypto,
            fab_idx: dev.handle.fabric_index,
            node_id,
        };

        let clusters = tbr::read(&target)
            .await
            .map_err(|e| failed("cannot read the device's Thread clusters", e))?;

        if let Some(xpan) = extended_pan_id {
            let xpan = decode_hex(xpan.trim())
                .map_err(|_| PeekError::failed("the extended PAN ID is not hex"))?;
            let Some((endpoint, _)) = clusters.directory else {
                return Err(PeekError::failed(
                    "the device has no Thread Network Directory",
                ));
            };
            return tbr::directory_dataset(&target, endpoint, &xpan).await;
        }

        match (clusters.border_router, clusters.directory) {
            (Some((endpoint, true)), _) => tbr::active_dataset(&target, endpoint).await,
            (_, Some((endpoint, Some(preferred)))) => {
                tbr::directory_dataset(&target, endpoint, &preferred).await
            }
            (Some((_, false)), _) => Err(PeekError::failed(
                "the border router has no Thread network yet",
            )),
            (None, Some((_, None))) => Err(PeekError::failed(
                "the device's Thread Network Directory has no preferred network: say which",
            )),
            (None, None) => Err(PeekError::failed(
                "the device is not a Matter Thread border router and has no Thread Network \
                 Directory",
            )),
        }
    }
}

/// A dataset we keep has a network key and an extended PAN ID.
fn check_dataset(dataset: &[u8]) -> Result<(), PeekError> {
    if dataset.is_empty() {
        return Err(PeekError::failed("the dataset is empty: no Thread network"));
    }
    if extended_pan_id(dataset).is_none() {
        return Err(PeekError::failed("the dataset has no extended PAN ID"));
    }
    if !ThreadTLV::new(dataset).any(|(tlv_type, _)| tlv_type == THREAD_NETWORK_KEY) {
        return Err(PeekError::failed("the dataset has no network key"));
    }

    Ok(())
}

/// What the QR code of a dataset starts with, before the dataset in uppercase
/// hex. As in Google's Home sample app for Matter.
const DATASET_QR_PREFIX: &str = "TD:";

/// The modules of the QR code of `dataset`, in uppercase hex.
fn dataset_qr_rows(dataset: &str) -> Vec<String> {
    qr_rows(&format!("{DATASET_QR_PREFIX}{dataset}"))
}

/// The modules of the QR code of `text`, row by row, `1` dark; none if it does
/// not fit. `TD:` and uppercase hex are alphanumeric to QR codes: a smaller code.
fn qr_rows(text: &str) -> Vec<String> {
    let len = Version::MAX.buffer_len();
    let (mut tmp, mut out) = (vec![0; len], vec![0; len]);
    let Ok(qr) = QrCode::encode_text(
        text,
        &mut tmp,
        &mut out,
        QrCodeEcc::Low,
        Version::MIN,
        Version::MAX,
        None,
        true,
    ) else {
        return Vec::new();
    };

    (0..qr.size())
        .map(|y| {
            (0..qr.size())
                .map(|x| if qr.get_module(x, y) { '1' } else { '0' })
                .collect()
        })
        .collect()
}

fn extended_pan_id(dataset: &[u8]) -> Option<Vec<u8>> {
    ThreadTLV::new(dataset)
        .ext_pan_id()
        .ok()
        .map(<[u8]>::to_vec)
}

fn network_name(dataset: &[u8]) -> String {
    ThreadTLV::new(dataset)
        .find_map(|(tlv_type, value)| {
            (tlv_type == THREAD_NETWORK_NAME).then(|| String::from_utf8_lossy(value).into_owned())
        })
        .unwrap_or_else(|| "unnamed".into())
}

#[cfg(not(feature = "meshcop"))]
fn no_sharing_codes() -> PeekError {
    PeekError::failed("this build of matter-peek cannot take sharing codes")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dataset as `ot-ctl dataset active -x` prints it, uppercase.
    const DATASET: &str = "0E080000000000010000000300001935060004001FFFE00208DEAD00BEEF00CAFE\
                           0708FD00DB8A0000AA00051000112233445566778899AABBCCDDEEFF03106D6174\
                           7465722D7065656B2D746573740102FACE0410445F2B5CA6F2A93A55CE570A70EF\
                           EECB0C0402A0F7F8";

    #[test]
    fn dataset_qr_code_reads_back() {
        const QUIET: usize = 4;
        const SCALE: usize = 4;

        let rows = dataset_qr_rows(DATASET);
        assert!(!rows.is_empty());

        let size = (rows.len() + 2 * QUIET) * SCALE;
        let mut image = rqrr::PreparedImage::prepare_from_greyscale(size, size, |x, y| {
            let (x, y) = (
                (x / SCALE).wrapping_sub(QUIET),
                (y / SCALE).wrapping_sub(QUIET),
            );
            let dark = rows
                .get(y)
                .and_then(|row| row.as_bytes().get(x))
                .is_some_and(|module| *module == b'1');
            if dark {
                0
            } else {
                255
            }
        });

        let grids = image.detect_grids();
        assert_eq!(grids.len(), 1);
        let (_, text) = grids[0].decode().unwrap();
        assert_eq!(text, format!("TD:{DATASET}"));
    }
}
