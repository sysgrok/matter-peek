//! A device's data model: read, written and invoked on, cluster by cluster.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;

use serde_json::Value;

use rs_matter::crypto::Crypto;
use rs_matter::persist::KvBlobStore;

use crate::api::{
    AttributeModel, ClusterModel, CommandModel, CommandOutcome, DeviceModel, DeviceTypeInfo,
    EndpointModel, FieldType, NamedValue, PeekError, TypeInfo, ValueKind, ValueType,
};
use crate::datamodel::{self, AttrReport, ModelError};
use crate::ota::OtaImageStore;
use crate::schema::{self, ClusterSchema, FieldSchema, TypeRef, ValueSchema};

use super::{failed, Ctx};

/// The Descriptor cluster, and its device types and server clusters.
const DESCRIPTOR: u32 = 0x001D;
const DEVICE_TYPE_LIST: u32 = 0x0000;
const SERVER_LIST: u32 = 0x0001;

/// The global attribute listing the commands a cluster accepts.
const ACCEPTED_COMMAND_LIST: u32 = 0xFFF9;

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// Read the data model of a listed device under `endpoint` and `cluster`
    /// (an ID or a name), each all if `None`.
    pub(super) async fn read_model(
        &self,
        node_id: u64,
        endpoint: Option<u16>,
        cluster: Option<&str>,
    ) -> Result<DeviceModel, PeekError> {
        let dev = self.lookup_device(node_id)?;
        let cluster = cluster.map(cluster_id).transpose()?;

        let reports = datamodel::read(
            self.matter,
            self.crypto,
            dev.handle,
            endpoint,
            cluster,
            None,
        )
        .await
        .map_err(|e| failed(&format!("device 0x{node_id:016X} did not answer"), e))?;

        Ok(model(node_id, &reports))
    }

    /// Write `value` (JSON) to an attribute of a listed device, and answer
    /// with its cluster read afresh.
    pub(super) async fn write_attribute(
        &self,
        node_id: u64,
        endpoint: u16,
        cluster: &str,
        attribute: &str,
        value: &str,
    ) -> Result<DeviceModel, PeekError> {
        let dev = self.lookup_device(node_id)?;
        let cluster = cluster_schema(cluster)?;
        let attr = match parse_id(attribute) {
            Some(id) => cluster.attribute(id),
            None => cluster.attribute_named(attribute),
        }
        .ok_or_else(|| {
            PeekError::failed(format!("{} has no attribute {attribute}", cluster.name))
        })?;
        if !attr.writable {
            return Err(PeekError::failed(format!("{} is read-only", attr.name)));
        }
        let value = json(value)?;

        datamodel::write(
            self.matter,
            self.crypto,
            dev.handle,
            endpoint,
            cluster.id,
            attr,
            &value,
        )
        .await
        .map_err(|e| model_error(&format!("writing {} failed", attr.name), e))?;

        self.read_model(node_id, Some(endpoint), Some(&cluster.id.to_string()))
            .await
    }

    /// Invoke a command of a listed device with `fields` (a JSON object).
    pub(super) async fn invoke_command(
        &self,
        node_id: u64,
        endpoint: u16,
        cluster: &str,
        command: &str,
        fields: &str,
    ) -> Result<CommandOutcome, PeekError> {
        let dev = self.lookup_device(node_id)?;
        let cluster = cluster_schema(cluster)?;
        let cmd = match parse_id(command) {
            Some(id) => cluster.command(id),
            None => cluster.command_named(command),
        }
        .ok_or_else(|| PeekError::failed(format!("{} has no command {command}", cluster.name)))?;
        let fields = if fields.trim().is_empty() {
            Value::Null
        } else {
            json(fields)?
        };

        let answer = datamodel::invoke(
            self.matter,
            self.crypto,
            dev.handle,
            endpoint,
            cluster.id,
            cmd,
            &fields,
        )
        .await
        .map_err(|e| model_error(&format!("{} failed", cmd.name), e))?;

        Ok(match answer {
            Some((name, fields)) => CommandOutcome {
                response: Some(name),
                fields: Some(fields.to_string()),
            },
            None => CommandOutcome {
                response: None,
                fields: None,
            },
        })
    }
}

/// The model of what `reports` say, by endpoint and cluster.
pub(super) fn model(node_id: u64, reports: &[AttrReport]) -> DeviceModel {
    let mut types = Types::default();
    let mut endpoints: BTreeMap<u16, BTreeMap<u32, Vec<&AttrReport>>> = BTreeMap::new();

    for report in reports {
        endpoints
            .entry(report.endpoint)
            .or_default()
            .entry(report.cluster)
            .or_default()
            .push(report);
    }

    let endpoints = endpoints
        .into_iter()
        .map(|(id, mut clusters)| {
            let descriptor = clusters.get(&DESCRIPTOR);
            let device_types = descriptor
                .and_then(|attrs| value_of(attrs, DEVICE_TYPE_LIST))
                .map(device_types)
                .unwrap_or_default();

            // The clusters it serves, also those that reported nothing.
            if let Some(Value::Array(ids)) =
                descriptor.and_then(|attrs| value_of(attrs, SERVER_LIST))
            {
                let ids: Vec<u32> = ids
                    .iter()
                    .filter_map(|id| id.as_u64().map(|id| id as u32))
                    .collect();
                for id in ids {
                    clusters.entry(id).or_default();
                }
            }

            EndpointModel {
                id,
                device_types,
                clusters: clusters
                    .into_iter()
                    .map(|(id, attrs)| cluster_model(id, &attrs, &mut types))
                    .collect(),
            }
        })
        .collect();

    DeviceModel {
        node_id,
        endpoints,
        types: types.infos,
    }
}

fn cluster_model(id: u32, attrs: &[&AttrReport], types: &mut Types) -> ClusterModel {
    let schema = schema::cluster(id);

    let attributes = attrs
        .iter()
        .map(|report| {
            let attr = schema.and_then(|c| c.attribute(report.attribute));
            let (value, error) = match &report.value {
                Ok(value) => (Some(value.to_string()), None),
                Err(e) => (None, Some(e.clone())),
            };
            AttributeModel {
                id: report.attribute,
                name: attr.map(|a| a.name.to_string()),
                writable: attr.is_some_and(|a| a.writable),
                timed: attr.is_some_and(|a| a.timed),
                value_type: attr.map(|a| types.value_type(&a.value)),
                value,
                error,
            }
        })
        .collect();

    let accepted = value_of(attrs, ACCEPTED_COMMAND_LIST)
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_u64).collect::<Vec<_>>())
        .unwrap_or_default();
    let commands = accepted
        .into_iter()
        .map(|id| {
            let cmd = schema.and_then(|c| c.command(id as u32));
            CommandModel {
                id: id as u32,
                name: cmd.map(|c| c.name.to_string()),
                fields: cmd.map_or_else(Vec::new, |c| types.fields(c.fields)),
                response: cmd
                    .and_then(|c| c.response.as_ref())
                    .map(|r| r.name.to_string()),
                timed: cmd.is_some_and(|c| c.timed),
            }
        })
        .collect();

    ClusterModel {
        id,
        name: schema.map(|c| c.name.to_string()),
        attributes,
        commands,
    }
}

/// The value of `attr` among `attrs`, if it has one.
fn value_of<'r>(attrs: &[&'r AttrReport], attr: u32) -> Option<&'r Value> {
    attrs
        .iter()
        .find(|report| report.attribute == attr)
        .and_then(|report| report.value.as_ref().ok())
}

fn device_types(list: &Value) -> Vec<DeviceTypeInfo> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(DeviceTypeInfo {
                id: entry.get("DeviceType")?.as_u64()? as u32,
                revision: entry.get("Revision").and_then(Value::as_u64).unwrap_or(0) as u16,
            })
        })
        .collect()
}

/// The enums, bitmaps and structs of a model's values, each once, as
/// [`TypeInfo`]s.
#[derive(Default)]
struct Types {
    infos: Vec<TypeInfo>,
    /// Their indices, by the address of their schema.
    index: BTreeMap<usize, u32>,
}

impl Types {
    fn value_type(&mut self, value: &ValueSchema) -> ValueType {
        let (kind, bits, schema) = match value.ty {
            TypeRef::Bool => (ValueKind::Bool, 0, None),
            TypeRef::UInt(bits) => (ValueKind::UInt, bits, None),
            TypeRef::Int(bits) => (ValueKind::Int, bits, None),
            TypeRef::Float => (ValueKind::Float, 32, None),
            TypeRef::Double => (ValueKind::Float, 64, None),
            TypeRef::Text => (ValueKind::Text, 0, None),
            TypeRef::Octets => (ValueKind::Octets, 0, None),
            TypeRef::Enum(e) => {
                let index = self.add(e as *const _ as usize, || TypeInfo {
                    name: e.name.to_string(),
                    values: named(e.values),
                    fields: Vec::new(),
                });
                (ValueKind::Enum, 0, Some(index))
            }
            TypeRef::Bitmap(b) => {
                let index = self.add(b as *const _ as usize, || TypeInfo {
                    name: b.name.to_string(),
                    values: named(b.bits),
                    fields: Vec::new(),
                });
                (ValueKind::Bitmap, 0, Some(index))
            }
            TypeRef::Struct(s) => {
                let key = s as *const _ as usize;
                let index = match self.index.get(&key) {
                    Some(index) => *index,
                    None => {
                        // Reserved first: a field's type may be added meanwhile.
                        let index = self.add(key, || TypeInfo {
                            name: s.name.to_string(),
                            values: Vec::new(),
                            fields: Vec::new(),
                        });
                        let fields = self.fields(s.fields);
                        self.infos[index as usize].fields = fields;
                        index
                    }
                };
                (ValueKind::Struct, 0, Some(index))
            }
        };

        ValueType {
            kind,
            idl: value.idl.to_string(),
            bits,
            list: value.list,
            nullable: value.nullable,
            optional: value.optional,
            schema,
        }
    }

    fn fields(&mut self, fields: &[FieldSchema]) -> Vec<FieldType> {
        fields
            .iter()
            .map(|f| FieldType {
                tag: f.tag,
                name: f.name.to_string(),
                value_type: self.value_type(&f.value),
            })
            .collect()
    }

    fn add(&mut self, key: usize, info: impl FnOnce() -> TypeInfo) -> u32 {
        if let Some(index) = self.index.get(&key) {
            return *index;
        }

        let index = self.infos.len() as u32;
        self.infos.push(info());
        self.index.insert(key, index);
        index
    }
}

fn named(values: &[(u64, &str)]) -> Vec<NamedValue> {
    values
        .iter()
        .map(|(value, name)| NamedValue {
            value: *value,
            name: name.to_string(),
        })
        .collect()
}

/// A cluster ID, given as an ID or a name.
fn cluster_id(cluster: &str) -> Result<u32, PeekError> {
    match parse_id(cluster) {
        Some(id) => Ok(id),
        None => Ok(cluster_schema(cluster)?.id),
    }
}

fn cluster_schema(cluster: &str) -> Result<&'static ClusterSchema, PeekError> {
    match parse_id(cluster) {
        Some(id) => schema::cluster(id),
        None => schema::cluster_named(cluster),
    }
    .ok_or_else(|| PeekError::failed(format!("no cluster {cluster} in the Matter IDL")))
}

/// An ID, in decimal or `0x` hex.
fn parse_id(s: &str) -> Option<u32> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

fn json(text: &str) -> Result<Value, PeekError> {
    serde_json::from_str(text).map_err(|e| PeekError::failed(format!("not JSON: {e}")))
}

fn model_error(context: &str, e: ModelError) -> PeekError {
    match e {
        ModelError::Value(why) => PeekError::failed(format!("{context}: {why}")),
        ModelError::Device(e) => failed(context, e),
    }
}
