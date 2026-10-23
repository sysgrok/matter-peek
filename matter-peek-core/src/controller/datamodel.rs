//! Reads, writes and invokes on any cluster of a device, with the values as
//! JSON (see [`crate::values`]), typed by the [schema](crate::schema).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use serde_json::Value;

use rs_matter::crypto::Crypto;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::im::client::ImClient as _;
use rs_matter::im::encoding::{AttrResp, CmdResp};
use rs_matter::im::{AttrDataTag, AttrPath, CmdDataTag, GenericPath, IMStatusCode};
use rs_matter::tlv::TLVTag;
use rs_matter::transport::exchange::Exchange;
use rs_matter::utils::storage::WriteBuf;
use rs_matter::Matter;

use crate::model::DeviceHandle;
use crate::schema::{self, AttrSchema, CmdSchema};
use crate::values::{self, ValueError};

/// How long a timed write or invoke may take to follow its request, ms.
const TIMED_TIMEOUT_MS: u16 = 5_000;

/// The largest value written or command invoked.
const MAX_PAYLOAD_LEN: usize = 1024;

/// An attribute as a device reports it: its value, or why it has none.
#[derive(Debug, Clone)]
pub struct AttrReport {
    pub endpoint: u16,
    pub cluster: u32,
    pub attribute: u32,
    pub value: Result<Value, String>,
}

/// Why a write or an invoke failed.
#[derive(Debug)]
pub enum ModelError {
    /// The value or the fields given do not fit the schema.
    Value(String),
    /// The device refused, or did not answer.
    Device(Error),
}

impl From<Error> for ModelError {
    fn from(e: Error) -> Self {
        Self::Device(e)
    }
}

impl From<ErrorCode> for ModelError {
    fn from(code: ErrorCode) -> Self {
        Self::Device(code.into())
    }
}

/// Read the attributes of `dev` under `endpoint`, `cluster` and
/// `attribute`, each a wildcard if `None`. A list a device sends item by item
/// comes back whole.
pub async fn read<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    dev: DeviceHandle,
    endpoint: Option<u16>,
    cluster: Option<u32>,
    attribute: Option<u32>,
) -> Result<Vec<AttrReport>, Error> {
    let paths = [AttrPath::from_gp(&GenericPath::new(
        endpoint, cluster, attribute,
    ))];

    let exchange = Exchange::initiate(matter, crypto, dev.fabric_index, dev.node_id).await?;
    let mut chunk = exchange
        .read_with(|msg| {
            msg.attr_requests_from(&paths)?
                .fabric_filtered(false)?
                .end()
        })
        .await?;

    let mut reports: Vec<AttrReport> = Vec::new();
    loop {
        {
            let resp = chunk.response()?;
            for report in resp.attr_reports.iter().flat_map(|r| r.iter()) {
                note(&mut reports, report?);
            }
        }

        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return Ok(reports),
        }
    }
}

/// Write `value` to attribute `attr` of `cluster` on `endpoint`.
pub async fn write<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    dev: DeviceHandle,
    endpoint: u16,
    cluster: u32,
    attr: &AttrSchema,
    value: &Value,
) -> Result<(), ModelError> {
    let tag = TLVTag::Context(AttrDataTag::Data as u8);
    check(|w| values::write(w, &tag, value, &attr.value))?;

    let exchange = Exchange::initiate(matter, crypto, dev.fabric_index, dev.node_id).await?;
    let handle = exchange
        .write_with(attr.timed.then_some(TIMED_TIMEOUT_MS), |msg| {
            msg.write_requests()?
                .push()?
                .path(endpoint, cluster, attr.id)?
                .data(|w| values::write(w, &tag, value, &attr.value).map_err(invalid))?
                .end()?
                .end()?
                .end()
        })
        .await?;

    let resp = handle.response()?;
    for status in resp.write_responses.iter() {
        let status = status?;
        if status.status.status != IMStatusCode::Success {
            return Err(status_error(status.status.status).into());
        }
    }

    Ok(())
}

/// Invoke command `cmd` of `cluster` on `endpoint` with `fields`; answers
/// with the response's name and fields, unless the device answered with a
/// status.
pub async fn invoke<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    dev: DeviceHandle,
    endpoint: u16,
    cluster: u32,
    cmd: &CmdSchema,
    fields: &Value,
) -> Result<Option<(String, Value)>, ModelError> {
    let tag = TLVTag::Context(CmdDataTag::Data as u8);
    check(|w| values::write_fields(w, &tag, fields, cmd.fields))?;

    let exchange = Exchange::initiate(matter, crypto, dev.fabric_index, dev.node_id).await?;
    let chunk = exchange
        .invoke_with(cmd.timed.then_some(TIMED_TIMEOUT_MS), |msg| {
            msg.invoke_requests()?
                .push()?
                .path(endpoint, cluster, cmd.id)?
                .data(|w| values::write_fields(w, &tag, fields, cmd.fields).map_err(invalid))?
                .end()?
                .end()?
                .end()
        })
        .await?;

    let answer = {
        let resp = chunk.response()?.ok_or(ErrorCode::InvalidData)?;
        let first = resp
            .invoke_responses
            .as_ref()
            .and_then(|responses| responses.iter().next())
            .ok_or(ErrorCode::InvalidData)??;

        match first {
            CmdResp::Cmd(data) => {
                let id = data.path.cmd.unwrap_or_default();
                let response = schema::cluster(cluster).and_then(|c| c.response(id));
                let fields = match response {
                    Some(response) => values::fields_to_json(&data.data, response.fields)?,
                    None => values::to_json(&data.data, None)?,
                };
                let name = response.map_or_else(|| format!("0x{id:02X}"), |r| r.name.to_string());
                Some((name, fields))
            }
            CmdResp::Status(status) if status.status.status == IMStatusCode::Success => None,
            CmdResp::Status(status) => return Err(status_error(status.status.status).into()),
        }
    };

    chunk.complete().await?;

    Ok(answer)
}

/// Note `report` in `reports`: a list item adds to the list reported before.
fn note(reports: &mut Vec<AttrReport>, report: AttrResp<'_>) {
    match report {
        AttrResp::Data(data) => {
            let p = &data.path;
            let (Some(endpoint), Some(cluster), Some(attribute)) = (p.endpoint, p.cluster, p.attr)
            else {
                return;
            };
            let schema = schema::cluster(cluster).and_then(|c| c.attribute(attribute));

            if p.list_index.is_some() {
                let item =
                    values::item_to_json(&data.data, schema.map(|s| &s.value)).map_err(describe);
                let list = reports.iter_mut().rev().find(|r| {
                    (r.endpoint, r.cluster, r.attribute) == (endpoint, cluster, attribute)
                });
                if let (
                    Some(AttrReport {
                        value: Ok(Value::Array(items)),
                        ..
                    }),
                    Ok(item),
                ) = (list, item)
                {
                    items.push(item);
                }
                return;
            }

            let value = values::to_json(&data.data, schema.map(|s| &s.value)).map_err(describe);
            reports.push(AttrReport {
                endpoint,
                cluster,
                attribute,
                value,
            });
        }
        AttrResp::Status(status) => {
            let p = &status.path;
            if let (Some(endpoint), Some(cluster), Some(attribute)) =
                (p.endpoint, p.cluster, p.attr)
            {
                reports.push(AttrReport {
                    endpoint,
                    cluster,
                    attribute,
                    value: Err(format!("{:?}", status.status.status)),
                });
            }
        }
    }
}

/// Encode with `f` into a scratch buffer: the value's errors before anything
/// is sent.
fn check(f: impl FnOnce(&mut WriteBuf<'_>) -> Result<(), ValueError>) -> Result<(), ModelError> {
    let mut buf = [0; MAX_PAYLOAD_LEN];
    let mut wb = WriteBuf::new(&mut buf);

    f(&mut wb).map_err(|e| ModelError::Value(e.0))
}

fn invalid(e: ValueError) -> Error {
    log::warn!("datamodel: {}", e.0);
    ErrorCode::InvalidData.into()
}

fn status_error(status: IMStatusCode) -> Error {
    status.to_error_code().unwrap_or(ErrorCode::Failure).into()
}

fn describe(e: Error) -> String {
    format!("{:?}", e.code())
}
