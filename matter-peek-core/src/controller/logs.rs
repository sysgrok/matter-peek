//! Fetching the Diagnostic Logs cluster (crash dumps / logs) over BDX.
//!
//! Flow (mirrors rs-matter's `tests/diag_logs.rs` receiver + the
//! `DiagnosticLogsClient` request side):
//!
//! 1. Register an upload with [`LogUploads`], which names the file designator of
//!    this request.
//! 2. Send a `RetrieveLogsRequest` with `RequestedProtocol = BDX` and the chosen
//!    [`IntentEnum`] via the generated `DiagnosticLogsClient` proxy on a CASE
//!    exchange.
//! 3. Small logs come back **inline** in the response's `log_content`; large
//!    logs trigger the device to open a BDX *upload* back to us, which the
//!    controller node's responder ([`crate::node::run`]) hands to [`LogUploads`].
//!
//! The transport pump (`matter.run`) and the node's responder must already be
//! running in sibling tasks (the service runs them).
//!
//! Templates in rs-matter:
//! - `tests/diag_logs.rs` — the client receiver end.
//! - `examples/src/bin/system_tests.rs` — the device/server end + BDX wiring.

use core::cell::RefCell;
use core::num::NonZeroU8;

use embassy_time::{with_timeout, Duration};

use log::{debug, info};

use rs_matter::bdx::{BdxHandler, BdxReader, BdxResponder};
use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::diagnostic_logs::DiagnosticLogsClient as _;
use rs_matter::dm::clusters::diag_logs::client::{DiagLogsBdxHandler, DiagLogsReceiver};
use rs_matter::dm::clusters::diag_logs::{IntentEnum, StatusEnum, TransferProtocolEnum};
use rs_matter::dm::EndptId;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::sc::ExchangeInitiator as _;
use rs_matter::transport::exchange::Exchange;
use rs_matter::utils::sync::blocking::Mutex;
use rs_matter::utils::sync::Notification;
use rs_matter::Matter;

use crate::model::LogIntent;

extern crate alloc;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// The root node endpoint, where the Diagnostic Logs cluster lives.
const ROOT_ENDPOINT: EndptId = 0;

/// How long to wait for a log the device is to push over BDX.
const BDX_PUSH_TIMEOUT_SECS: u64 = 60;

impl From<LogIntent> for IntentEnum {
    fn from(i: LogIntent) -> Self {
        match i {
            LogIntent::EndUserSupport => IntentEnum::EndUserSupport,
            LogIntent::NetworkDiag => IntentEnum::NetworkDiag,
            LogIntent::CrashLogs => IntentEnum::CrashLogs,
        }
    }
}

/// Routes the logs devices push over BDX to the requests awaiting them, by file
/// designator: the device echoes the one its request named. Shared by
/// [`retrieve_logs`] and the controller node's responder ([`crate::node::run`]).
/// (This is the seam for the future consent-gated cloud upload: swap the
/// in-memory `Vec` for a streaming HTTPS sink.)
pub struct LogUploads {
    state: Mutex<RefCell<State>>,
}

struct State {
    next_id: u32,
    slots: Vec<Arc<Slot>>,
}

/// One request's upload.
struct Slot {
    designator: String,
    data: Mutex<RefCell<Vec<u8>>>,
    done: Notification,
}

impl LogUploads {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(RefCell::new(State {
                next_id: 0,
                slots: Vec::new(),
            })),
        }
    }

    /// Take the upload named by the returned registration's designator, until
    /// the registration is dropped.
    // A slot is `Send` and `Sync` with `send-sync` only.
    #[cfg_attr(not(feature = "send-sync"), allow(clippy::arc_with_non_send_sync))]
    fn register(&self) -> Registration<'_> {
        let slot = self.state.lock(|state| {
            let mut state = state.borrow_mut();
            let id = state.next_id;
            state.next_id = id.wrapping_add(1);

            let slot = Arc::new(Slot {
                designator: format!("matter-peek-{id}"),
                data: Mutex::new(RefCell::new(Vec::new())),
                done: Notification::new(),
            });
            state.slots.push(slot.clone());

            slot
        });

        Registration {
            uploads: self,
            slot,
        }
    }

    fn slot(&self, designator: &[u8]) -> Option<Arc<Slot>> {
        self.state.lock(|state| {
            state
                .borrow()
                .slots
                .iter()
                .find(|slot| slot.designator.as_bytes() == designator)
                .cloned()
        })
    }
}

impl Default for LogUploads {
    fn default() -> Self {
        Self::new()
    }
}

impl BdxHandler for LogUploads {
    async fn handles(&self, responder: &BdxResponder<'_>) -> bool {
        matches!(responder, BdxResponder::Upload(upload) if self.slot(upload.fd()).is_some())
    }

    async fn handle(&self, responder: BdxResponder<'_>) -> Result<(), Error> {
        DiagLogsBdxHandler::new(self).handle(responder).await
    }
}

impl DiagLogsReceiver for LogUploads {
    async fn receive(
        &self,
        file_designator: &[u8],
        reader: &mut BdxReader<'_>,
    ) -> Result<(), Error> {
        let slot = self.slot(file_designator).ok_or(ErrorCode::NotFound)?;

        let mut buf = [0u8; 512];
        loop {
            let n = reader.read(&mut buf).await?;
            // The request gave up: abandon the transfer, or the device takes
            // the log as delivered, and may delete it.
            if self.slot(file_designator).is_none() {
                return Err(ErrorCode::RxTimeout.into());
            }
            if n == 0 {
                break;
            }
            slot.data
                .lock(|data| data.borrow_mut().extend_from_slice(&buf[..n]));
        }
        slot.done.notify();
        Ok(())
    }
}

struct Registration<'a> {
    uploads: &'a LogUploads,
    slot: Arc<Slot>,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.uploads.state.lock(|state| {
            state
                .borrow_mut()
                .slots
                .retain(|slot| !Arc::ptr_eq(slot, &self.slot))
        });
    }
}

/// Retrieve a diagnostic log of the given `intent` from the device identified by
/// `(fab_idx, node_id)`, returning the raw bytes.
///
/// Handles both inline (small log) and BDX-pushed (large log) delivery; a push
/// arrives through `uploads`.
pub async fn retrieve_logs<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    intent: LogIntent,
    uploads: &LogUploads,
) -> Result<Vec<u8>, Error> {
    // Before the request: the device may push while it is in flight.
    let upload = uploads.register();

    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;

    let handle = exchange
        .diagnostic_logs()
        .retrieve_logs_request(ROOT_ENDPOINT, |req| {
            req.intent(intent.into())?
                .requested_protocol(TransferProtocolEnum::BDX)?
                .transfer_file_designator(Some(upload.slot.designator.as_str()))?
                .end()
        })
        .await?;

    // A small log rides back in the response itself. `Success` with no content
    // means the device is pushing it over BDX instead.
    let inline = {
        let resp = handle.response()?;
        match resp.status()? {
            StatusEnum::NoLogs => Ok(Some(Vec::new())),
            StatusEnum::Busy => Err(ErrorCode::Busy),
            StatusEnum::Denied => Err(ErrorCode::InvalidAction),
            _ => {
                let content = resp.log_content()?;
                Ok((!content.is_empty()).then(|| content.to_vec()))
            }
        }
    };
    handle.complete().await?;

    if let Some(bytes) = inline? {
        info!("retrieved {} bytes inline", bytes.len());
        return Ok(bytes);
    }

    debug!("no inline content; awaiting BDX push");
    with_timeout(
        Duration::from_secs(BDX_PUSH_TIMEOUT_SECS),
        upload.slot.done.wait(),
    )
    .await
    .map_err(|_| Error::from(ErrorCode::RxTimeout))?;
    let bytes = upload
        .slot
        .data
        .lock(|data| core::mem::take(&mut *data.borrow_mut()));
    info!("retrieved {} bytes over BDX", bytes.len());
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_request_has_its_own_designator() {
        let uploads = LogUploads::new();

        let first = uploads.register();
        let second = uploads.register();

        assert_ne!(first.slot.designator, second.slot.designator);
        // The Diagnostic Logs cluster allows a designator of at most 32 bytes.
        assert!(first.slot.designator.len() <= 32);

        for upload in [&first, &second] {
            let found = uploads.slot(upload.slot.designator.as_bytes()).unwrap();
            assert!(Arc::ptr_eq(&found, &upload.slot));
        }
    }

    #[test]
    fn an_upload_nobody_awaits_has_no_slot() {
        let uploads = LogUploads::new();
        assert!(uploads.slot(b"ota/some-image").is_none());

        let upload = uploads.register();
        let designator = upload.slot.designator.clone();
        assert!(uploads.slot(designator.as_bytes()).is_some());

        drop(upload);
        assert!(uploads.slot(designator.as_bytes()).is_none());
    }
}
