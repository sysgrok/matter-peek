//! Data model values as JSON, read from TLV and written as TLV, by their
//! [schema](crate::schema) where there is one.
//!
//! A struct is an object keyed by field name (by context tag if unknown), a
//! list an array, an enum its value's name, a bitmap the names of its bits
//! set, an octet string hex. Writing takes those, numbers for enums and
//! bitmaps, and integers as decimal or `0x` strings too.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use serde_json::{Map, Number, Value};

use rs_matter::error::{Error, ErrorCode};
use rs_matter::tlv::{TLVElement, TLVTag, TLVValue, TLVWrite};

use crate::schema::{BitmapSchema, FieldSchema, TypeRef, ValueSchema};

/// Why a value cannot be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueError(pub String);

impl ValueError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl From<Error> for ValueError {
    fn from(e: Error) -> Self {
        Self(format!("{:?}", e.code()))
    }
}

/// `element` as JSON, typed by `schema` if given.
pub fn to_json(element: &TLVElement, schema: Option<&ValueSchema>) -> Result<Value, Error> {
    decode(
        element,
        schema.map(|s| s.ty),
        schema.is_some_and(|s| s.list),
    )
}

/// One item of a list of `schema` as JSON: a list sent item by item.
pub fn item_to_json(element: &TLVElement, schema: Option<&ValueSchema>) -> Result<Value, Error> {
    decode(element, schema.map(|s| s.ty), false)
}

/// Fields of a command or a response, as JSON.
pub fn fields_to_json(element: &TLVElement, fields: &[FieldSchema]) -> Result<Value, Error> {
    decode_struct(element, Some(fields))
}

/// Write `value` as `schema` says, with `tag`.
pub fn write<W: TLVWrite>(
    w: &mut W,
    tag: &TLVTag,
    value: &Value,
    schema: &ValueSchema,
) -> Result<(), ValueError> {
    if value.is_null() {
        if schema.nullable {
            return Ok(w.null(tag)?);
        }
        return Err(ValueError::new("cannot be null"));
    }

    if schema.list {
        let items = value
            .as_array()
            .ok_or_else(|| ValueError::new("expected a list"))?;
        w.start_array(tag)?;
        for item in items {
            write_item(w, &TLVTag::Anonymous, item, schema.ty)?;
        }
        return Ok(w.end_container()?);
    }

    write_item(w, tag, value, schema.ty)
}

/// Write `value` as a struct of `fields`, with `tag`: a command's fields.
pub fn write_fields<W: TLVWrite>(
    w: &mut W,
    tag: &TLVTag,
    value: &Value,
    fields: &[FieldSchema],
) -> Result<(), ValueError> {
    let empty = Map::new();
    let object = match value {
        Value::Null => &empty,
        Value::Object(object) => object,
        _ => return Err(ValueError::new("expected an object of fields")),
    };

    if let Some(unknown) = object.keys().find(|key| field_named(fields, key).is_none()) {
        return Err(ValueError::new(format!("no field {unknown}")));
    }

    w.start_struct(tag)?;
    for field in fields {
        let given = object
            .iter()
            .find(|(key, _)| field_named(core::slice::from_ref(field), key).is_some())
            .map(|(_, value)| value);
        match given {
            Some(value) => write(w, &TLVTag::Context(field.tag as u8), value, &field.value)
                .map_err(|e| ValueError::new(format!("{}: {}", field.name, e.0)))?,
            None if field.value.optional => {}
            None => return Err(ValueError::new(format!("{} is missing", field.name))),
        }
    }
    Ok(w.end_container()?)
}

fn decode(element: &TLVElement, ty: Option<TypeRef>, list: bool) -> Result<Value, Error> {
    let value = element.value()?;

    Ok(match value {
        TLVValue::Null => Value::Null,
        TLVValue::Array | TLVValue::List => {
            let item_ty = if list { ty } else { None };
            let items = element
                .container()?
                .iter()
                .map(|item| decode(&item?, item_ty, false))
                .collect::<Result<Vec<_>, Error>>()?;
            Value::Array(items)
        }
        TLVValue::Struct => {
            let fields = match ty {
                Some(TypeRef::Struct(s)) => Some(s.fields),
                _ => None,
            };
            decode_struct(element, fields)?
        }
        TLVValue::U8(v) => unsigned(v as u64, ty),
        TLVValue::U16(v) => unsigned(v as u64, ty),
        TLVValue::U32(v) => unsigned(v as u64, ty),
        TLVValue::U64(v) => unsigned(v, ty),
        TLVValue::S8(v) => Value::from(v),
        TLVValue::S16(v) => Value::from(v),
        TLVValue::S32(v) => Value::from(v),
        TLVValue::S64(v) => Value::from(v),
        TLVValue::False => Value::Bool(false),
        TLVValue::True => Value::Bool(true),
        TLVValue::F32(v) => Number::from_f64(v as f64).map_or(Value::Null, Value::Number),
        TLVValue::F64(v) => Number::from_f64(v).map_or(Value::Null, Value::Number),
        TLVValue::Utf8l(s) | TLVValue::Utf16l(s) | TLVValue::Utf32l(s) | TLVValue::Utf64l(s) => {
            Value::String(s.to_string())
        }
        TLVValue::Str8l(b) | TLVValue::Str16l(b) | TLVValue::Str32l(b) | TLVValue::Str64l(b) => {
            Value::String(hex(b))
        }
        TLVValue::EndCnt => return Err(ErrorCode::InvalidData.into()),
    })
}

fn decode_struct(element: &TLVElement, fields: Option<&[FieldSchema]>) -> Result<Value, Error> {
    let mut object = Map::new();

    for item in element.structure()?.iter() {
        let item = item?;
        let tag = match item.tag()? {
            TLVTag::Context(tag) => tag as u32,
            _ => continue,
        };
        let field = fields.and_then(|fields| fields.iter().find(|f| f.tag == tag));
        let value = decode(
            &item,
            field.map(|f| f.value.ty),
            field.is_some_and(|f| f.value.list),
        )?;
        let key = field.map_or_else(|| tag.to_string(), |f| f.name.to_string());
        object.insert(key, value);
    }

    Ok(Value::Object(object))
}

/// An unsigned integer: an enum's value by name, a bitmap's bits by name.
fn unsigned(v: u64, ty: Option<TypeRef>) -> Value {
    match ty {
        Some(TypeRef::Enum(e)) => e.name_of(v).map_or(Value::from(v), Value::from),
        Some(TypeRef::Bitmap(b)) => bit_names(b, v).map_or(Value::from(v), |names| {
            Value::Array(names.into_iter().map(Value::from).collect())
        }),
        _ => Value::from(v),
    }
}

/// The names of the bits set in `v`, if they are all single, named bits.
fn bit_names(b: &BitmapSchema, v: u64) -> Option<Vec<&'static str>> {
    let mut left = v;
    let mut names = Vec::new();

    for (mask, name) in b.bits {
        if mask.count_ones() == 1 && v & mask != 0 {
            names.push(*name);
            left &= !mask;
        }
    }

    (left == 0).then_some(names)
}

fn write_item<W: TLVWrite>(
    w: &mut W,
    tag: &TLVTag,
    value: &Value,
    ty: TypeRef,
) -> Result<(), ValueError> {
    match ty {
        TypeRef::Bool => {
            let v = value
                .as_bool()
                .ok_or_else(|| ValueError::new("expected true or false"))?;
            Ok(w.bool(tag, v)?)
        }
        TypeRef::UInt(bits) => {
            let v = uint(value)?;
            if bits < 64 && v >> bits != 0 {
                return Err(ValueError::new(format!("{v} does not fit in {bits} bits")));
            }
            Ok(w.u64(tag, v)?)
        }
        TypeRef::Int(bits) => {
            let v = int(value)?;
            let max = (1i64 << (bits - 1)) - 1;
            if bits < 64 && (v > max || v < -max - 1) {
                return Err(ValueError::new(format!("{v} does not fit in {bits} bits")));
            }
            Ok(w.i64(tag, v)?)
        }
        TypeRef::Float => Ok(w.f32(tag, float(value)? as f32)?),
        TypeRef::Double => Ok(w.f64(tag, float(value)?)?),
        TypeRef::Text => {
            let v = value
                .as_str()
                .ok_or_else(|| ValueError::new("expected a string"))?;
            Ok(w.utf8(tag, v)?)
        }
        TypeRef::Octets => {
            let v = value
                .as_str()
                .and_then(unhex)
                .ok_or_else(|| ValueError::new("expected hex"))?;
            Ok(w.str(tag, &v)?)
        }
        TypeRef::Enum(e) => {
            let v = match value.as_str() {
                Some(name) if !is_number(name) => e
                    .value_of(name)
                    .ok_or_else(|| ValueError::new(format!("{} has no {name}", e.name)))?,
                _ => uint(value)?,
            };
            Ok(w.u64(tag, v)?)
        }
        TypeRef::Bitmap(b) => {
            let v = match value {
                Value::Array(names) => {
                    let mut v = 0;
                    for name in names {
                        let name = name
                            .as_str()
                            .ok_or_else(|| ValueError::new("expected bit names"))?;
                        let (mask, _) = b
                            .bits
                            .iter()
                            .find(|(_, n)| n.eq_ignore_ascii_case(name))
                            .ok_or_else(|| {
                            ValueError::new(format!("{} has no {name}", b.name))
                        })?;
                        v |= mask;
                    }
                    v
                }
                _ => uint(value)?,
            };
            Ok(w.u64(tag, v)?)
        }
        TypeRef::Struct(s) => write_fields(w, tag, value, s.fields),
    }
}

/// The field of `fields` named `key`, in any case, or by its tag.
fn field_named<'f>(fields: &'f [FieldSchema], key: &str) -> Option<&'f FieldSchema> {
    fields.iter().find(|f| {
        f.name.eq_ignore_ascii_case(key) || key.parse::<u32>().is_ok_and(|tag| tag == f.tag)
    })
}

fn uint(value: &Value) -> Result<u64, ValueError> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => parse_u64(s),
        _ => None,
    }
    .ok_or_else(|| ValueError::new("expected an unsigned integer"))
}

fn int(value: &Value) -> Result<i64, ValueError> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .ok_or_else(|| ValueError::new("expected an integer"))
}

fn float(value: &Value) -> Result<f64, ValueError> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .ok_or_else(|| ValueError::new("expected a number"))
}

fn parse_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

fn is_number(s: &str) -> bool {
    parse_u64(s).is_some()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }

    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use rs_matter::tlv::TLVElement;
    use rs_matter::utils::storage::WriteBuf;
    use serde_json::json;

    use super::*;
    use crate::schema::cluster;

    /// `value` written as `schema` says, and read back.
    fn round_trip(value: Value, schema: &ValueSchema) -> Value {
        let mut buf = [0; 512];
        let mut wb = WriteBuf::new(&mut buf);
        write(&mut wb, &TLVTag::Anonymous, &value, schema).unwrap();
        let len = wb.as_slice().len();

        to_json(&TLVElement::new(&buf[..len]), Some(schema)).unwrap()
    }

    #[test]
    fn enums_are_read_and_written_by_name() {
        let mode = cluster(0x0201)
            .unwrap()
            .attribute_named("SystemMode")
            .unwrap();

        assert_eq!(round_trip(json!("heat"), &mode.value), json!("Heat"));
        assert_eq!(round_trip(json!(3), &mode.value), json!("Cool"));
    }

    #[test]
    fn integers_are_checked_against_their_width() {
        let level = cluster(0x0008)
            .unwrap()
            .command_named("MoveToLevel")
            .unwrap();
        let mut buf = [0; 64];
        let mut wb = WriteBuf::new(&mut buf);

        let too_big = json!({ "Level": 300, "TransitionTime": null, "OptionsMask": [], "OptionsOverride": [] });
        assert!(write_fields(&mut wb, &TLVTag::Anonymous, &too_big, level.fields).is_err());
    }

    #[test]
    fn a_command_is_written_as_a_struct_of_its_fields() {
        let level = cluster(0x0008)
            .unwrap()
            .command_named("MoveToLevel")
            .unwrap();
        let fields = json!({ "level": 128, "transitionTime": "0x0A", "OptionsMask": [], "OptionsOverride": [] });

        let mut buf = [0; 64];
        let mut wb = WriteBuf::new(&mut buf);
        write_fields(&mut wb, &TLVTag::Anonymous, &fields, level.fields).unwrap();
        let len = wb.as_slice().len();

        let read = fields_to_json(&TLVElement::new(&buf[..len]), level.fields).unwrap();
        assert_eq!(read["Level"], json!(128));
        assert_eq!(read["TransitionTime"], json!(10));
    }

    #[test]
    fn a_missing_field_is_refused_unless_optional() {
        let level = cluster(0x0008)
            .unwrap()
            .command_named("MoveToLevel")
            .unwrap();
        let mut buf = [0; 64];
        let mut wb = WriteBuf::new(&mut buf);

        let missing = json!({ "Level": 1 });
        let e = write_fields(&mut wb, &TLVTag::Anonymous, &missing, level.fields).unwrap_err();
        assert!(e.0.contains("TransitionTime"), "{e:?}");
    }
}
