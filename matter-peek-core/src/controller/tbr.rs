//! The Thread networks of a Matter border router: its own, through Thread
//! Border Router Management, and those of its Thread Network Directory.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::{thread_border_router_management, thread_network_directory};
use rs_matter::dm::EndptId;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::im::encoding::{AttrResp, CmdResp};
use rs_matter::im::{AttrPath, CmdDataTag, GenericPath, IMStatusCode, ImClient as _};
use rs_matter::sc::ExchangeInitiator as _;
use rs_matter::tlv::{TLVTag, TLVWrite};
use rs_matter::transport::exchange::Exchange;

use crate::api::PeekError;
use crate::netconfig::Target;

/// How long a timed invoke may take to follow its request, ms.
const TIMED_TIMEOUT_MS: u16 = 5_000;

const TBRM: u32 = thread_border_router_management::FULL_CLUSTER.id;
const DIRECTORY: u32 = thread_network_directory::FULL_CLUSTER.id;

/// Thread Border Router Management's attributes and commands.
const TBRM_ACTIVE_DATASET_TIMESTAMP: u32 = 4;
const TBRM_FEATURE_MAP: u32 = 0xFFFC;
const TBRM_GET_ACTIVE_DATASET: u32 = 0;
const TBRM_SET_ACTIVE_DATASET: u32 = 3;

/// Thread Network Directory's attributes and commands.
const DIRECTORY_PREFERRED: u32 = 0;
const DIRECTORY_ADD_NETWORK: u32 = 0;
const DIRECTORY_GET_DATASET: u32 = 2;

/// Where a device has the two clusters, and what they say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadClusters {
    /// Thread Border Router Management: its endpoint, and whether the border
    /// router has a network (an active dataset).
    pub border_router: Option<(EndptId, bool)>,
    /// Whether it changes networks with a pending dataset (PAN Change).
    pub pan_change: bool,
    /// Thread Network Directory: its endpoint and preferred network.
    pub directory: Option<(EndptId, Option<Vec<u8>>)>,
}

/// Which of the two clusters the device has, on which endpoints.
pub async fn read<C: Crypto>(target: &Target<'_, '_, C>) -> Result<ThreadClusters, Error> {
    let path =
        |cluster, attr| AttrPath::from_gp(&GenericPath::new(None, Some(cluster), Some(attr)));
    let paths = [
        path(TBRM, TBRM_ACTIVE_DATASET_TIMESTAMP),
        path(TBRM, TBRM_FEATURE_MAP),
        path(DIRECTORY, DIRECTORY_PREFERRED),
    ];

    let mut clusters = ThreadClusters::default();

    let exchange = exchange(target).await?;
    let mut chunk = exchange
        .read_with(|msg| msg.attr_requests_from(&paths)?.fabric_filtered(true)?.end())
        .await?;

    loop {
        for report in chunk.response()?.attr_reports.iter().flatten() {
            let AttrResp::Data(data) = report? else {
                continue;
            };
            let p = &data.path;
            let (Some(endpoint), Some(cluster), Some(attr)) = (p.endpoint, p.cluster, p.attr)
            else {
                continue;
            };

            match (cluster, attr) {
                (TBRM, TBRM_ACTIVE_DATASET_TIMESTAMP) => {
                    clusters.border_router = Some((endpoint, data.data.null().is_err()));
                }
                (TBRM, TBRM_FEATURE_MAP) => {
                    clusters.pan_change = data.data.u32().is_ok_and(|features| features & 1 != 0);
                }
                (DIRECTORY, DIRECTORY_PREFERRED) => {
                    let preferred = data.data.octets().ok().map(<[u8]>::to_vec);
                    clusters.directory = Some((endpoint, preferred));
                }
                _ => (),
            }
        }

        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return Ok(clusters),
        }
    }
}

/// The active dataset of the border router's own network; empty if it has
/// none.
pub async fn active_dataset<C: Crypto>(
    target: &Target<'_, '_, C>,
    endpoint: EndptId,
) -> Result<Vec<u8>, PeekError> {
    invoke(target, endpoint, TBRM, TBRM_GET_ACTIVE_DATASET, false, None)
        .await?
        .ok_or_else(|| PeekError::failed("the border router did not answer with its dataset"))
}

/// The dataset of network `extended_pan_id` in the device's Thread Network
/// Directory.
pub async fn directory_dataset<C: Crypto>(
    target: &Target<'_, '_, C>,
    endpoint: EndptId,
    extended_pan_id: &[u8],
) -> Result<Vec<u8>, PeekError> {
    invoke(
        target,
        endpoint,
        DIRECTORY,
        DIRECTORY_GET_DATASET,
        false,
        Some(extended_pan_id),
    )
    .await?
    .ok_or_else(|| PeekError::failed("the device did not answer with the dataset"))
}

/// Give the border router, which has no network yet, `dataset` as its
/// network. Needs the armed fail-safe.
pub async fn set_active_dataset<C: Crypto>(
    target: &Target<'_, '_, C>,
    endpoint: EndptId,
    dataset: &[u8],
) -> Result<(), PeekError> {
    invoke(
        target,
        endpoint,
        TBRM,
        TBRM_SET_ACTIVE_DATASET,
        true,
        Some(dataset),
    )
    .await
    .map(|_| ())
}

/// Add `dataset` to the device's Thread Network Directory, or update it there.
pub async fn add_to_directory<C: Crypto>(
    target: &Target<'_, '_, C>,
    endpoint: EndptId,
    dataset: &[u8],
) -> Result<(), PeekError> {
    invoke(
        target,
        endpoint,
        DIRECTORY,
        DIRECTORY_ADD_NETWORK,
        true,
        Some(dataset),
    )
    .await
    .map(|_| ())
}

/// Make network `extended_pan_id` the preferred one of the device's Thread
/// Network Directory.
pub async fn set_preferred<C: Crypto>(
    target: &Target<'_, '_, C>,
    endpoint: EndptId,
    extended_pan_id: &[u8],
) -> Result<(), PeekError> {
    let step = "PreferredExtendedPanID";
    let tag = TLVTag::Context(rs_matter::im::AttrDataTag::Data as u8);

    let handle = exchange(target)
        .await
        .map_err(|e| failed(step, e))?
        .write_with(None, |msg| {
            msg.timed_request(false)?
                .write_requests()?
                .push()?
                .path(endpoint, DIRECTORY, DIRECTORY_PREFERRED)?
                .data(|w| w.str(&tag, extended_pan_id))?
                .end()?
                .end()?
                .end()
        })
        .await
        .map_err(|e| failed(step, e))?;

    let resp = handle.response().map_err(|e| failed(step, e))?;
    for status in resp.write_responses.iter() {
        let status = status.map_err(|e| failed(step, e))?;
        if status.status.status != IMStatusCode::Success {
            return Err(status_failed(step, status.status.status));
        }
    }

    Ok(())
}

async fn exchange<'a, C: Crypto>(target: &'a Target<'_, '_, C>) -> Result<Exchange<'a>, Error> {
    Exchange::initiate(target.matter, target.crypto, target.fab_idx, target.node_id).await
}

/// Invoke `cmd` of `cluster`, with `field` as its field 0 if any; the octet
/// string of the response's field 0, if it has one. (Every command here has
/// at most that one field.)
async fn invoke<C: Crypto>(
    target: &Target<'_, '_, C>,
    endpoint: EndptId,
    cluster: u32,
    cmd: u32,
    timed: bool,
    field: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, PeekError> {
    let step = command_name(cluster, cmd);
    let tag = TLVTag::Context(CmdDataTag::Data as u8);

    let chunk = exchange(target)
        .await
        .map_err(|e| failed(step, e))?
        .invoke_with(timed.then_some(TIMED_TIMEOUT_MS), |msg| {
            msg.timed_request(timed)?
                .invoke_requests()?
                .push()?
                .path(endpoint, cluster, cmd)?
                .data(|w| {
                    w.start_struct(&tag)?;
                    if let Some(field) = field {
                        w.str(&TLVTag::Context(0), field)?;
                    }
                    w.end_container()
                })?
                .end()?
                .end()?
                .end()
        })
        .await
        .map_err(|e| failed(step, e))?;

    let answer = {
        let resp = chunk
            .response()
            .map_err(|e| failed(step, e))?
            .ok_or_else(|| failed(step, ErrorCode::InvalidData.into()))?;
        let first = resp
            .invoke_responses
            .as_ref()
            .and_then(|responses| responses.iter().next())
            .ok_or_else(|| failed(step, ErrorCode::InvalidData.into()))?
            .map_err(|e| failed(step, e))?;

        match first {
            CmdResp::Cmd(data) => Some(
                data.data
                    .structure()
                    .and_then(|fields| fields.find_ctx(0))
                    .and_then(|field| field.octets())
                    .map(<[u8]>::to_vec)
                    .map_err(|e| failed(step, e))?,
            ),
            CmdResp::Status(status) if status.status.status == IMStatusCode::Success => None,
            CmdResp::Status(status) => return Err(status_failed(step, status.status.status)),
        }
    };

    chunk.complete().await.map_err(|e| failed(step, e))?;

    Ok(answer)
}

fn command_name(cluster: u32, cmd: u32) -> &'static str {
    match (cluster, cmd) {
        (TBRM, TBRM_GET_ACTIVE_DATASET) => "GetActiveDatasetRequest",
        (TBRM, TBRM_SET_ACTIVE_DATASET) => "SetActiveDatasetRequest",
        (DIRECTORY, DIRECTORY_ADD_NETWORK) => "AddNetwork",
        (DIRECTORY, DIRECTORY_GET_DATASET) => "GetOperationalDataset",
        _ => "the command",
    }
}

fn failed(step: &str, e: Error) -> PeekError {
    PeekError::failed(format!(
        "{step} failed: {}",
        crate::explain::describe(e.code())
    ))
}

/// What a status the device answered `step` with means here.
fn status_failed(step: &str, status: IMStatusCode) -> PeekError {
    let why: String = match status {
        IMStatusCode::InvalidInState if step == "SetActiveDatasetRequest" => {
            "the border router has a Thread network already".into()
        }
        IMStatusCode::InvalidInState if step == "AddNetwork" => {
            "the directory has this network already, as recent or newer".into()
        }
        IMStatusCode::FailSafeRequired => "the device wants an armed fail-safe".into(),
        IMStatusCode::UnsupportedAccess => "the device refused: it wants an administrator".into(),
        IMStatusCode::NotFound => "the directory has no such network".into(),
        IMStatusCode::ConstraintError => "the dataset lacks something the device needs".into(),
        IMStatusCode::ResourceExhausted => "the directory is full".into(),
        IMStatusCode::Busy => "the device is busy with another change".into(),
        IMStatusCode::Timeout => "the border router did not get onto the network in time".into(),
        status => format!("the device answered {status:?}"),
    };

    PeekError::failed(format!("{step}: {why}"))
}
