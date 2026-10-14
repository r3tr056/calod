//! Bridge from the storage-layer [`DataType`] to the protocol-layer [`Value`].
//!
//! Keeping this mapping separate lets the store stay completely ignorant of RESP
//! framing: a command handler returns a `DataType`, this module turns it into the
//! canonical [`Value`] reply tree, and [`Value::encode`] then renders RESP2 or
//! RESP3 bytes according to the connection's negotiated protocol version.
//!
//! The RESP2 rendering is byte-for-byte identical to the previous hand-rolled
//! encoder (List/Set → flat `*` array, Hash → flat `*` array of field/value
//! pairs); RESP3 upgrades Set → `~` and Hash → `%` automatically because that
//! choice lives entirely in `Value::encode`.

use crate::parser::value::Value;
use calod_core::store::calod_data::DataType;
use calod_core::store::stream::StreamId;

/// Convert a successful command result into the canonical reply [`Value`].
/// Protocol-agnostic: RESP2/RESP3 differences are applied later by
/// [`Value::encode`].
pub fn datatype_to_value(dt: DataType) -> Value {
    match dt {
        DataType::Nil => Value::Nil,
        DataType::NilArray => Value::NilArray,
        DataType::Integer(i) => Value::Integer(i),
        DataType::Status(s) => Value::SimpleString(s),
        DataType::String(b) => Value::BulkString(b),
        DataType::Array(items) => Value::Array(items.into_iter().map(datatype_to_value).collect()),
        DataType::List(items) => Value::Array(items.into_iter().map(Value::BulkString).collect()),
        DataType::Set(set) => Value::Set(set.into_iter().map(Value::BulkString).collect()),
        DataType::Hash(map) => Value::Map(
            map.into_iter()
                .map(|(k, v)| (Value::BulkString(k), Value::BulkString(v)))
                .collect(),
        ),
        DataType::Document(json) => match serde_json::to_vec(&json) {
            Ok(bytes) => Value::BulkString(bytes),
            Err(_) => Value::Error("ERR serialization error".to_string()),
        },
        DataType::Object { .. } => Value::Error("ERR Object type not implemented".to_string()),
        // A floating-point reply (ZSCORE/ZINCRBY): RESP2 bulk / RESP3 `,` double.
        DataType::Double(d) => Value::Double(d),
        // A sorted set is normally returned via explicit array shapes by its
        // command handlers; this is only a defensive fallback (flat member/score
        // array in ascending order).
        DataType::SortedSet(ss) => Value::Array(
            ss.entries()
                .into_iter()
                .flat_map(|(m, s)| [Value::BulkString(m), Value::Double(s)])
                .collect(),
        ),
        // A stream is normally returned via explicit array shapes by its command
        // handlers (XRANGE/XREAD); this defensive fallback renders all entries as
        // `[id, [field, value, ...]]` pairs in ascending id order.
        DataType::Stream(st) => Value::Array(
            st.range(StreamId::MIN, StreamId::MAX, false, None)
                .into_iter()
                .map(|(id, fields)| {
                    let flat = fields
                        .into_iter()
                        .flat_map(|(f, v)| [Value::BulkString(f), Value::BulkString(v)])
                        .collect();
                    Value::Array(vec![
                        Value::BulkString(id.to_redis_string().into_bytes()),
                        Value::Array(flat),
                    ])
                })
                .collect(),
        ),
    }
}
