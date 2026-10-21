//! Discovery and commissioning.

use core::net::SocketAddr;
use core::sync::atomic::Ordering;

use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;

use log::{error, warn};

use rs_matter::crypto::Crypto;
use rs_matter::error::Error;
use rs_matter::persist::KvBlobStore;
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::Address;

use crate::api::{
    CommissionListener, CommissionResult, CommissionStep, CommissionVia, Discovered,
    DiscoveredDevice, NetworkCredentials, PeekError, StepStatus,
};
use crate::matter::CommissionFlow;
use crate::model::CommissionOutcome;
use crate::ota::OtaImageStore;
use crate::setup_code::SetupCode;

use super::devices::new_record;
use super::{convert, failed, Ctx, FIRST_DEVICE_NODE_ID};

/// How long a discovery scans for devices advertising over Bluetooth LE.
#[cfg(feature = "ble")]
const BLE_SCAN_SECS: u16 = 5;

/// How a commission reaches the device.
enum Reach {
    Ip(SocketAddr),
    Ble {
        /// Matches the device's advertisement (from its setup code).
        filter: CommissionableFilter,
        addr: Option<rs_matter::transport::network::BtAddr>,
        network: Option<NetworkCredentials>,
    },
}

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// Browse for commissionable devices, on the network and over BLE.
    pub(super) async fn discover(
        &self,
        discriminator: Option<u16>,
        setup_code: Option<&str>,
        max: u8,
    ) -> Result<Discovered, PeekError> {
        let filter = match (discriminator, setup_code) {
            (Some(discriminator), _) => CommissionableFilter {
                discriminator: Some(discriminator),
                ..Default::default()
            },
            (None, Some(code)) => SetupCode::parse(code).map_err(invalid_setup_code)?.filter,
            (None, None) => CommissionableFilter::default(),
        };

        // A device can advertise as commissionable while not taking commissioning
        // (`CM=0`, e.g. once decommissioned); listing it would only invite a failure.
        let filter = CommissionableFilter {
            commissioning_mode_only: true,
            ..filter
        };

        // The mDNS backend runs already; discovery must not start a second one.
        let network = self.controller.discover_all(&filter, max.max(1), None);

        #[cfg(feature = "ble")]
        let (devices, ble_devices) = {
            let (devices, ble_devices) =
                embassy_futures::join::join(network, crate::ble::scan_ble(&filter, BLE_SCAN_SECS))
                    .await;

            let ble_devices = ble_devices
                .unwrap_or_else(|e| {
                    warn!("discover: no Bluetooth scan: {e:?}");
                    Vec::new()
                })
                .into_iter()
                .take(max.into())
                .map(|d| crate::api::BleDevice {
                    addr: d.addr.to_string(),
                    discriminator: d.discriminator,
                    vendor_id: d.vendor_id,
                    product_id: d.product_id,
                })
                .collect();

            (devices, ble_devices)
        };
        #[cfg(not(feature = "ble"))]
        let (devices, ble_devices) = (network.await, Vec::new());

        let devices = devices
            .into_iter()
            .map(|d| DiscoveredDevice {
                addr: match d.address {
                    Address::Udp(a) => a.to_string(),
                    other => format!("{other:?}"),
                },
                commissionable_id: d.commissionable_id,
            })
            .collect();

        Ok(Discovered {
            devices,
            ble_devices,
        })
    }

    /// Commission a device onto our fabric; with `remember`, also keep a record
    /// of it.
    pub(super) async fn commission(
        &self,
        via: &CommissionVia,
        setup_code: &str,
        remember: bool,
        progress: Option<&dyn CommissionListener>,
    ) -> Result<CommissionResult, PeekError> {
        let code = SetupCode::parse(setup_code).map_err(invalid_setup_code)?;

        let reach = match via {
            CommissionVia::Ip { addr } => Reach::Ip(
                addr.parse()
                    .map_err(|e| PeekError::failed(format!("invalid peer address: {e}")))?,
            ),
            CommissionVia::Ble { addr, network } => Reach::Ble {
                filter: code.filter,
                addr: addr.as_deref().map(parse_bt_addr).transpose()?,
                network: network.clone(),
            },
        };

        self.commission_flow(reach, code.passcode, remember, progress)
            .await
    }

    /// The commission proper, reporting its steps to `progress`: commission,
    /// then list the device and, with `remember`, keep a record of it.
    async fn commission_flow(
        &self,
        reach: Reach,
        passcode: u32,
        remember: bool,
        progress: Option<&dyn CommissionListener>,
    ) -> Result<CommissionResult, PeekError> {
        let step = |status| {
            if let Some(progress) = progress {
                progress.on_step(CommissionStep::Commission, status);
            }
        };

        // A cancel requested before this commission is not for it.
        self.cancel.store(false, Ordering::SeqCst);

        // A NodeID never handed out before, and never to be handed out again (the
        // counter is saved right away): the device keeps it, remembered or not, and
        // even if this run goes wrong.
        let device_node_id = self
            .store
            .allocate_node_id(FIRST_DEVICE_NODE_ID)
            .map_err(|e| failed("cannot allocate a node ID", e))?;

        // Before the commission: the device has no fabric of ours yet.
        if self.cancel.load(Ordering::SeqCst) {
            return Err(PeekError::Cancelled {
                decommissioned: false,
            });
        }

        step(StepStatus::Started);

        let peer_addr = match reach {
            Reach::Ip(peer_addr) => peer_addr,
            Reach::Ble {
                filter,
                addr,
                network,
            } => {
                return match self
                    .commission_ble(&filter, addr, network, passcode, device_node_id)
                    .await
                {
                    Ok(outcome) => {
                        step(StepStatus::Done);
                        self.commissioned(outcome, remember).await
                    }
                    Err(e) => {
                        step(step_failed(&e));
                        Err(failed("commission failed", e))
                    }
                };
            }
        };

        // Cancellable up to AddNOC, which abandons the (often-hung-on-a-closed-
        // window) orphan sweep / PASE: no device state is created before it.
        let cancel = self.cancel.clone();
        // Fabrics we still have a record for are live commissionings, not leaks:
        // the sweep must not reclaim them (that would decommission the device we
        // are about to re-commission). Everything else of ours on the device is
        // fair game.
        let live_node_ids = self.live_node_ids();
        let outcome = match self
            .controller
            .commission(
                &self.ca,
                self.fab_idx,
                peer_addr,
                passcode,
                device_node_id,
                &live_node_ids,
                move || cancel.load(Ordering::SeqCst),
            )
            .await
        {
            Ok(CommissionFlow::Done(outcome)) => {
                step(StepStatus::Done);
                outcome
            }
            Ok(CommissionFlow::Cancelled) => {
                step(StepStatus::Failed {
                    message: "cancelled".into(),
                });
                return Err(PeekError::Cancelled {
                    decommissioned: false,
                });
            }
            Err(e) => {
                step(step_failed(&e));
                return Err(failed("commission failed", e));
            }
        };

        // After the commission: the device holds our fabric. A cancel that arrived
        // during it takes effect here, decommissioning the device so that our
        // fabric is not left orphaned on it.
        if self.cancel.load(Ordering::SeqCst) {
            let removed = self.controller.decommission(outcome.handle).await.is_ok();
            if !removed {
                error!(
                    "service: cancel cleanup decommission failed for the just-commissioned device"
                );
            }
            return Err(PeekError::Cancelled {
                decommissioned: removed,
            });
        }

        self.commissioned(outcome, remember).await
    }

    /// Commission a device over BLE, provisioning it with `network`.
    #[cfg(feature = "ble")]
    async fn commission_ble(
        &self,
        filter: &CommissionableFilter,
        addr: Option<rs_matter::transport::network::BtAddr>,
        network: Option<NetworkCredentials>,
        passcode: u32,
        device_node_id: u64,
    ) -> Result<CommissionOutcome, Error> {
        use crate::ble::{self, WirelessCreds};

        let dataset;
        let creds = match &network {
            None => None,
            Some(NetworkCredentials::Wifi { ssid, password }) => Some(WirelessCreds::Wifi {
                ssid: ssid.as_bytes(),
                pass: password.as_bytes(),
            }),
            Some(NetworkCredentials::Thread { dataset: hex }) => {
                dataset = ble::decode_hex(hex)?;
                Some(WirelessCreds::Thread {
                    dataset_tlv: &dataset,
                })
            }
        };

        ble::commission_ble(
            self.matter,
            self.crypto,
            &self.ca,
            self.fab_idx,
            self.btp,
            filter,
            addr,
            passcode,
            device_node_id,
            creds.as_ref(),
        )
        .await
    }

    #[cfg(not(feature = "ble"))]
    async fn commission_ble(
        &self,
        _filter: &CommissionableFilter,
        _addr: Option<rs_matter::transport::network::BtAddr>,
        _network: Option<NetworkCredentials>,
        _passcode: u32,
        _device_node_id: u64,
    ) -> Result<CommissionOutcome, Error> {
        warn!("commission: this build has no Bluetooth");
        Err(rs_matter::error::ErrorCode::NoNetworkInterface.into())
    }

    /// The device holds our fabric now: keep our fabric across restarts, list
    /// the device and, with `remember`, keep a record of it. Re-matching reuses
    /// the record of a re-commissioned unit; a fingerprint-only hit comes back as
    /// a candidate to offer for linking.
    async fn commissioned(
        &self,
        outcome: CommissionOutcome,
        remember: bool,
    ) -> Result<CommissionResult, PeekError> {
        if let Err(e) = self
            .matter
            .with_state(|state| self.store.save_fabric(state.fabrics.fabric(self.fab_idx)?))
        {
            error!("service: saving our fabric failed: {e:?}");
        }

        let rematch_candidate = if remember {
            self.persist_commissioned(outcome.handle)
                .await
                .unwrap_or_else(|e| {
                    error!("service: remembering the commissioned device failed: {e:?}");
                    None
                })
        } else {
            let ident = self.controller.read_basic_info(outcome.handle).await;
            self.transient
                .borrow_mut()
                .push(new_record(outcome.handle, &ident));
            None
        };

        self.set_online(outcome.handle.node_id, true);

        Ok(CommissionResult {
            handle: convert::handle(outcome.handle),
            rematch_candidate,
        })
    }
}

/// A Bluetooth address, `AA:BB:CC:DD:EE:FF`.
fn parse_bt_addr(addr: &str) -> Result<rs_matter::transport::network::BtAddr, PeekError> {
    let invalid = || PeekError::failed(format!("invalid Bluetooth address: {addr}"));

    let mut bytes = [0; 6];
    let mut parts = addr.split(':');
    for byte in &mut bytes {
        let part = parts.next().ok_or_else(invalid)?;
        *byte = u8::from_str_radix(part, 16).map_err(|_| invalid())?;
    }

    if parts.next().is_some() {
        return Err(invalid());
    }

    Ok(rs_matter::transport::network::BtAddr(bytes))
}

fn invalid_setup_code(e: Error) -> PeekError {
    PeekError::failed(format!(
        "invalid setup code ({:?}): expected a passcode, a manual pairing code or an MT: QR code payload",
        e.code()
    ))
}

fn step_failed(e: &Error) -> StepStatus {
    StepStatus::Failed {
        message: format!("{:?}", e.code()),
    }
}
