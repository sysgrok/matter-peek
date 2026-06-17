//! Fetching the Diagnostic Logs cluster (crash dumps / logs) over BDX.
//!
//! Flow (mirrors rs-matter's `tests/diag_logs.rs` receiver + the
//! `DiagnosticLogsClient` request side):
//!
//! 1. Stand up a [`Bdx`] responder wrapping a [`DiagLogsBdxHandler`] over our
//!    [`CollectingReceiver`] (a [`DiagLogsReceiver`]).
//! 2. Send a `RetrieveLogsRequest` with `RequestedProtocol = BDX` and the chosen
//!    [`IntentEnum`] via the generated `DiagnosticLogsClient` proxy on a CASE
//!    exchange.
//! 3. Small logs come back **inline** in the response's `log_content`; large
//!    logs trigger the device to open a BDX *upload* back to us, which the
//!    concurrently-running responder routes to our receiver.
//!
//! Both paths are awaited concurrently with the responder; we return whichever
//! bytes arrive. The transport pump (`matter.run`) must already be running in a
//! sibling task (the CLI / RPC host owns it).
//!
//! Templates in rs-matter:
//! - `tests/diag_logs.rs` — the client receiver end.
//! - `examples/src/bin/system_tests.rs` — the device/server end + BDX wiring.

use core::cell::RefCell;
use core::num::NonZeroU8;

use embassy_futures::select::{select, Either};

use log::{debug, info};

use rs_matter::bdx::{Bdx, BdxReader};
use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::diagnostic_logs::DiagnosticLogsClient as _;
use rs_matter::dm::clusters::diag_logs::client::{DiagLogsBdxHandler, DiagLogsReceiver};
use rs_matter::dm::clusters::diag_logs::{IntentEnum, TransferProtocolEnum};
use rs_matter::dm::EndptId;
use rs_matter::error::Error;
use rs_matter::respond::Responder;
use rs_matter::transport::exchange::Exchange;
use rs_matter::utils::sync::Notification;
use rs_matter::Matter;

use crate::model::LogIntent;

extern crate alloc;
use alloc::vec::Vec;

/// The root node endpoint, where the Diagnostic Logs cluster lives.
const ROOT_ENDPOINT: EndptId = 0;

/// The file designator we name in the request; the device echoes it back on the
/// BDX transfer so the receiver can correlate.
const FILE_DESIGNATOR: &str = "matter-peek";

impl From<LogIntent> for IntentEnum {
    fn from(i: LogIntent) -> Self {
        match i {
            LogIntent::EndUserSupport => IntentEnum::EndUserSupport,
            LogIntent::NetworkDiag => IntentEnum::NetworkDiag,
            LogIntent::CrashLogs => IntentEnum::CrashLogs,
        }
    }
}

/// A [`DiagLogsReceiver`] that collects a pushed log into memory and signals
/// completion. (This is the seam for the future consent-gated cloud upload:
/// swap the in-memory `Vec` for a streaming HTTPS sink.)
struct CollectingReceiver {
    data: RefCell<Vec<u8>>,
    done: Notification,
}

impl CollectingReceiver {
    fn new() -> Self {
        Self {
            data: RefCell::new(Vec::new()),
            done: Notification::new(),
        }
    }
}

impl DiagLogsReceiver for CollectingReceiver {
    async fn receive(
        &self,
        _file_designator: &[u8],
        reader: &mut BdxReader<'_>,
    ) -> Result<(), Error> {
        let mut buf = [0u8; 512];
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            self.data.borrow_mut().extend_from_slice(&buf[..n]);
        }
        self.done.notify();
        Ok(())
    }
}

/// Retrieve a diagnostic log of the given `intent` from the device identified by
/// `(fab_idx, node_id)`, returning the raw bytes.
///
/// Handles both inline (small log) and BDX-pushed (large log) delivery. Runs a
/// BDX-receive responder concurrently with sending the request, so the device's
/// push is caught while the request is still in flight.
pub async fn retrieve_logs<C: Crypto + Copy>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    intent: LogIntent,
) -> Result<Vec<u8>, Error> {
    let receiver = CollectingReceiver::new();
    let responder = Responder::new(
        "matter-peek-diaglogs",
        Bdx::new(DiagLogsBdxHandler::new(&receiver)),
        matter,
        0,
    );

    // Run the BDX-receive responder concurrently with the request/await flow.
    // The request flow terminates the `select`; the responder only needs to live
    // long enough to service the device's push.
    let outcome = select(
        responder.run::<4>(),
        request_and_collect(matter, crypto, fab_idx, node_id, intent, &receiver),
    )
    .await;

    match outcome {
        Either::First(r) => {
            // Responder exited first — unexpected; surface its error (or an
            // empty success if it returned Ok).
            r?;
            Ok(Vec::new())
        }
        Either::Second(bytes) => bytes,
    }
}

/// Send the `RetrieveLogsRequest` and gather the resulting bytes (inline or via
/// the BDX `receiver`).
async fn request_and_collect<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    intent: LogIntent,
    receiver: &CollectingReceiver,
) -> Result<Vec<u8>, Error> {
    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;

    let handle = exchange
        .diagnostic_logs()
        .retrieve_logs_request(ROOT_ENDPOINT, |req| {
            req.intent(intent.into())?
                .requested_protocol(TransferProtocolEnum::BDX)?
                .transfer_file_designator(Some(FILE_DESIGNATOR))?
                .end()
        })
        .await?;

    // Inline content (small logs ride back in the response itself).
    let inline = {
        let resp = handle.response()?;
        let content = resp.log_content()?;
        if content.is_empty() {
            None
        } else {
            Some(content.to_vec())
        }
    };
    handle.complete().await?;

    if let Some(bytes) = inline {
        info!("retrieved {} bytes inline", bytes.len());
        return Ok(bytes);
    }

    // Otherwise the device pushes over BDX; wait for the receiver to drain it.
    debug!("no inline content; awaiting BDX push");
    receiver.done.wait().await;
    let bytes = receiver.data.borrow().clone();
    info!("retrieved {} bytes over BDX", bytes.len());
    Ok(bytes)
}
