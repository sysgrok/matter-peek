//! Changing the networks a device knows, over CASE.
//!
//! Every change needs an armed fail-safe, and stays tentative until
//! `CommissioningComplete` commits it: when the fail-safe expires first, the
//! device reverts to the networks it had before it was armed.

use core::num::NonZeroU8;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::general_commissioning::{
    CommissioningErrorEnum, GeneralCommissioningClient as _,
};
use rs_matter::dm::clusters::decl::network_commissioning::{
    NetworkCommissioningClient as _, NetworkCommissioningStatusEnum,
};
use rs_matter::dm::clusters::net_comm::WirelessCreds;
use rs_matter::dm::endpoints::ROOT_ENDPOINT_ID;
use rs_matter::dm::EndptId;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::tlv::{Nullable, OctetStr};
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

use crate::api::PeekError;
use crate::network::hardware_address;

/// The device a change is made to.
#[derive(Clone, Copy)]
pub struct Target<'a, 'm, C> {
    pub matter: &'a Matter<'m>,
    pub crypto: &'a C,
    pub fab_idx: NonZeroU8,
    pub node_id: u64,
}

impl<C: Crypto> Target<'_, '_, C> {
    async fn exchange(&self) -> Result<Exchange<'_>, Error> {
        Exchange::initiate(self.matter, self.crypto, self.fab_idx, self.node_id).await
    }

    /// Arm the fail-safe for `secs` seconds; `0` disarms it, reverting what was
    /// not committed.
    pub async fn arm_fail_safe(&self, secs: u16) -> Result<(), PeekError> {
        let step = "ArmFailSafe";

        let handle = self
            .exchange()
            .await
            .map_err(|e| failed(step, e))?
            .general_commissioning()
            .arm_fail_safe(ROOT_ENDPOINT_ID, |req| {
                req.expiry_length_seconds(secs)?.breadcrumb(0)?.end()
            })
            .await
            .map_err(|e| failed(step, e))?;

        let code = handle.response().and_then(|resp| resp.error_code());
        handle.complete().await.map_err(|e| failed(step, e))?;

        commissioning_status(step, code.map_err(|e| failed(step, e))?)
    }

    /// Commit the changes made since the fail-safe was armed.
    pub async fn commit(&self) -> Result<(), PeekError> {
        let step = "CommissioningComplete";

        let handle = self
            .exchange()
            .await
            .map_err(|e| failed(step, e))?
            .general_commissioning()
            .commissioning_complete(ROOT_ENDPOINT_ID)
            .await
            .map_err(|e| failed(step, e))?;

        let code = handle.response().and_then(|resp| resp.error_code());
        handle.complete().await.map_err(|e| failed(step, e))?;

        commissioning_status(step, code.map_err(|e| failed(step, e))?)
    }

    /// Add a network to the Network Commissioning instance on `endpoint`, or
    /// update a known one: where it is in the list, if the device tells.
    pub async fn add_or_update(
        &self,
        endpoint: EndptId,
        creds: &WirelessCreds<'_>,
    ) -> Result<Option<u8>, PeekError> {
        let exchange = self
            .exchange()
            .await
            .map_err(|e| failed("AddOrUpdate", e))?;

        let (step, handle) = match creds {
            WirelessCreds::Wifi { ssid, pass } => {
                let step = "AddOrUpdateWiFiNetwork";
                let handle = exchange
                    .network_commissioning()
                    .add_or_update_wi_fi_network(endpoint, |req| {
                        req.ssid(OctetStr::new(ssid))?
                            .credentials(OctetStr::new(pass))?
                            .breadcrumb(None)?
                            .end()
                    })
                    .await;
                (step, handle)
            }
            WirelessCreds::Thread { dataset_tlv } => {
                let step = "AddOrUpdateThreadNetwork";
                let handle = exchange
                    .network_commissioning()
                    .add_or_update_thread_network(endpoint, |req| {
                        req.operational_dataset(OctetStr::new(dataset_tlv))?
                            .breadcrumb(None)?
                            .end()
                    })
                    .await;
                (step, handle)
            }
        };
        let handle = handle.map_err(|e| failed(step, e))?;

        let status = handle.response().and_then(|resp| {
            Ok((
                (resp.networking_status()?, text(resp.debug_text()?)),
                resp.network_index()?,
            ))
        });
        handle.complete().await.map_err(|e| failed(step, e))?;

        let (status, index) = status.map_err(|e| failed(step, e))?;
        network_status(step, status)?;

        Ok(index)
    }

    /// Forget network `id`.
    pub async fn remove(&self, endpoint: EndptId, id: &[u8]) -> Result<(), PeekError> {
        let step = "RemoveNetwork";

        let handle = self
            .exchange()
            .await
            .map_err(|e| failed(step, e))?
            .network_commissioning()
            .remove_network(endpoint, |req| {
                req.network_id(OctetStr::new(id))?.breadcrumb(None)?.end()
            })
            .await
            .map_err(|e| failed(step, e))?;

        let status = handle
            .response()
            .and_then(|resp| Ok((resp.networking_status()?, text(resp.debug_text()?))));
        handle.complete().await.map_err(|e| failed(step, e))?;

        network_status(step, status.map_err(|e| failed(step, e))?)
    }

    /// Move network `id` to `index` in the order the device tries them.
    pub async fn reorder(&self, endpoint: EndptId, id: &[u8], index: u8) -> Result<(), PeekError> {
        let step = "ReorderNetwork";

        let handle = self
            .exchange()
            .await
            .map_err(|e| failed(step, e))?
            .network_commissioning()
            .reorder_network(endpoint, |req| {
                req.network_id(OctetStr::new(id))?
                    .network_index(index)?
                    .breadcrumb(None)?
                    .end()
            })
            .await
            .map_err(|e| failed(step, e))?;

        let status = handle
            .response()
            .and_then(|resp| Ok((resp.networking_status()?, text(resp.debug_text()?))));
        handle.complete().await.map_err(|e| failed(step, e))?;

        network_status(step, status.map_err(|e| failed(step, e))?)
    }

    /// The access points of Wi-Fi network `ssid` the device sees, as
    /// `AA:BB:CC:DD:EE:FF`.
    pub async fn scan_wifi(
        &self,
        endpoint: EndptId,
        ssid: &[u8],
    ) -> Result<Vec<String>, PeekError> {
        let step = "ScanNetworks";

        let handle = self
            .exchange()
            .await
            .map_err(|e| failed(step, e))?
            .network_commissioning()
            .scan_networks(endpoint, |req| {
                req.ssid(Some(Nullable::some(OctetStr::new(ssid))))?
                    .breadcrumb(None)?
                    .end()
            })
            .await
            .map_err(|e| failed(step, e))?;

        let scanned = handle.response().and_then(|resp| {
            let mut bssids = Vec::new();
            if let Some(results) = resp.wi_fi_scan_results()? {
                for result in results {
                    bssids.push(hardware_address(result?.bssid()?.0));
                }
            }
            Ok((resp.networking_status()?, text(resp.debug_text()?), bssids))
        });
        handle.complete().await.map_err(|e| failed(step, e))?;

        let (status, text, bssids) = scanned.map_err(|e| failed(step, e))?;
        if status == NetworkCommissioningStatusEnum::NetworkNotFound {
            return Ok(Vec::new());
        }
        network_status(step, (status, text))?;

        Ok(bssids)
    }

    /// Connect to known network `id`: the device's answer, or `Err` when there
    /// was none. It may not get its answer out while it changes networks, so
    /// the caller bounds the wait too.
    pub async fn connect(
        &self,
        endpoint: EndptId,
        id: &[u8],
    ) -> Result<Result<(), PeekError>, Error> {
        let step = "ConnectNetwork";

        let handle = self
            .exchange()
            .await?
            .network_commissioning()
            .connect_network(endpoint, |req| {
                req.network_id(OctetStr::new(id))?.breadcrumb(None)?.end()
            })
            .await?;

        let status = handle.response().and_then(|resp| {
            let error = resp.error_value()?.into_option();
            let text = text(resp.debug_text()?);
            let text = match (text, error) {
                (Some(text), Some(error)) => Some(format!("{text}, error {error}")),
                (None, Some(error)) => Some(format!("error {error}")),
                (text, None) => text,
            };
            Ok((resp.networking_status()?, text))
        });
        handle.complete().await?;

        Ok(network_status(step, status?))
    }
}

fn failed(step: &str, e: Error) -> PeekError {
    let code = e.code();

    // The device judges the credentials by the formats Wi-Fi allows.
    if step == "AddOrUpdateWiFiNetwork" && code == ErrorCode::ConstraintError {
        return PeekError::failed(format!(
            "{step} failed: the device rejected the SSID or the password ({code:?}); an SSID is \
             1 to 32 bytes, and a Wi-Fi password 8 to 63 characters (64 hex digits for a raw \
             key, 5 or 13 characters for WEP, none for an open network)"
        ));
    }

    PeekError::failed(format!("{step} failed: {}", crate::explain::describe(code)))
}

fn text(text: Option<&str>) -> Option<String> {
    text.filter(|text| !text.is_empty())
        .map(ToString::to_string)
}

fn commissioning_status(step: &str, code: CommissioningErrorEnum) -> Result<(), PeekError> {
    let why = match code {
        CommissioningErrorEnum::OK => return Ok(()),
        CommissioningErrorEnum::BusyWithOtherAdmin => {
            "another administrator is changing the device"
        }
        CommissioningErrorEnum::NoFailSafe => "the fail-safe expired",
        CommissioningErrorEnum::ValueOutsideRange => "the device refused the fail-safe duration",
        CommissioningErrorEnum::InvalidAuthentication => "not allowed",
        _ => "refused",
    };

    Err(PeekError::failed(format!("{step}: {why}")))
}

fn network_status(
    step: &str,
    (status, text): (NetworkCommissioningStatusEnum, Option<String>),
) -> Result<(), PeekError> {
    if status == NetworkCommissioningStatusEnum::Success {
        return Ok(());
    }

    let label = crate::network::status(status).label();
    Err(PeekError::failed(match text {
        Some(text) => format!("{step}: {label} ({text})"),
        None => format!("{step}: {label}"),
    }))
}
