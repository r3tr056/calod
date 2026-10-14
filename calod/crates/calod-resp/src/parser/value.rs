use std::fmt;
use std::str;

use crate::cld_srv::protocol::ProtocolVersion;

/// Represents a RESP (Redis Serialization Protocol) value.
///
/// This is both the parsed inbound representation and the canonical reply type.
/// Each variant carries enough information to be rendered to either RESP2 or
/// RESP3 wire bytes by [`Value::encode`]; the RESP2/RESP3 choice (e.g. nil
/// framing, map vs flat array, double vs bulk) lives entirely in the encoder.
#[derive(Debug, PartialEq, Clone)]
pub enum Value {
    /// Simple string (+)
    SimpleString(String),

    /// Error (-)
    Error(String),

    /// Integer (:)
    Integer(i64),

    /// Bulk string ($)
    BulkString(Vec<u8>),

    /// Array (*)
    Array(Vec<Value>),

    /// Null. RESP2 renders this as a null bulk string (`$-1`); RESP3 renders the
    /// dedicated null (`_`).
    Nil,

    /// Null aggregate. RESP2 renders this as a null array (`*-1`); RESP3 renders
    /// the dedicated null (`_`). Distinct from [`Value::Nil`] so commands that
    /// must return a null *array* (e.g. EXEC abort, blocking-pop timeout) stay
    /// wire-correct for RESP2 clients.
    NilArray,

    // RESP3 types (Redis 6.0+). Each has a RESP2 fallback in the encoder so
    // RESP2 clients still receive a parseable value.
    /// Boolean (`#t`/`#f` in RESP3; `:1`/`:0` in RESP2).
    Boolean(bool),

    /// Double (`,` in RESP3; bulk string in RESP2).
    Double(f64),

    /// Big number (`(` in RESP3; bulk string in RESP2). Held as its decimal text.
    BigNumber(String),

    /// Verbatim string (`=` in RESP3; bulk string in RESP2). `format` is the
    /// 3-byte type hint such as `txt` or `mkd`.
    VerbatimString { format: [u8; 3], data: Vec<u8> },

    /// Map (`%` in RESP3; flat `*` array of alternating key/value in RESP2).
    Map(Vec<(Value, Value)>),

    /// Set (`~` in RESP3; `*` array in RESP2).
    Set(Vec<Value>),

    /// Out-of-band push message (`>` in RESP3; `*` array in RESP2). Used for
    /// pub/sub and keyspace notifications.
    Push(Vec<Value>),
}

impl Value {
    /// Returns true if the value is nil
    pub fn is_nil(&self) -> bool {
        matches!(self, Value::Nil)
    }

    /// Converts the value to a string if it's a SimpleString or BulkString
    pub fn as_string(&self) -> Option<String> {
        match self {
            Value::SimpleString(s) => Some(s.clone()),
            Value::BulkString(b) => String::from_utf8(b.clone()).ok(),
            _ => None,
        }
    }

    /// Converts the value to bytes if it's a BulkString
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::BulkString(b) => Some(b),
            _ => None,
        }
    }

    /// Converts the value to an integer if it's an Integer
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// Converts the value to an array if it's an Array
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Serialize to RESP2 wire bytes. Retained for callers (and tests) that
    /// predate protocol negotiation; prefer [`Value::encode`] on the reply path.
    pub fn serialize(&self) -> Vec<u8> {
        self.encode(ProtocolVersion::Resp2)
    }

    /// Encode this value to RESP wire bytes for the given protocol version.
    pub fn encode(&self, proto: ProtocolVersion) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_size_hint());
        self.encode_into(&mut out, proto);
        out
    }

    /// Rough lower bound on the encoded size, to pre-size the output buffer.
    fn encoded_size_hint(&self) -> usize {
        match self {
            Value::SimpleString(s) | Value::Error(s) | Value::BigNumber(s) => s.len() + 3,
            Value::Integer(_) | Value::Double(_) | Value::Boolean(_) => 24,
            Value::BulkString(b) => b.len() + 16,
            Value::VerbatimString { data, .. } => data.len() + 16,
            Value::Nil | Value::NilArray => 5,
            Value::Array(items) | Value::Set(items) | Value::Push(items) => 16 + items.len() * 16,
            Value::Map(pairs) => 16 + pairs.len() * 32,
        }
    }

    /// Append the RESP encoding of this value to `out`, recursing for aggregates.
    pub fn encode_into(&self, out: &mut Vec<u8>, proto: ProtocolVersion) {
        match self {
            Value::SimpleString(s) => {
                out.push(b'+');
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Value::Error(s) => {
                out.push(b'-');
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Value::Integer(i) => {
                out.push(b':');
                out.extend_from_slice(i.to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Value::BulkString(b) => encode_bulk(out, b),
            Value::Array(items) => {
                encode_aggregate_header(out, b'*', items.len());
                for item in items {
                    item.encode_into(out, proto);
                }
            }
            Value::Nil => {
                if proto.is_resp3() {
                    out.extend_from_slice(b"_\r\n");
                } else {
                    out.extend_from_slice(b"$-1\r\n");
                }
            }
            Value::NilArray => {
                if proto.is_resp3() {
                    out.extend_from_slice(b"_\r\n");
                } else {
                    out.extend_from_slice(b"*-1\r\n");
                }
            }
            Value::Boolean(b) => {
                if proto.is_resp3() {
                    out.extend_from_slice(if *b { b"#t\r\n" } else { b"#f\r\n" });
                } else {
                    out.extend_from_slice(if *b { b":1\r\n" } else { b":0\r\n" });
                }
            }
            Value::Double(d) => {
                let s = format_double(*d);
                if proto.is_resp3() {
                    out.push(b',');
                    out.extend_from_slice(s.as_bytes());
                    out.extend_from_slice(b"\r\n");
                } else {
                    encode_bulk(out, s.as_bytes());
                }
            }
            Value::BigNumber(s) => {
                if proto.is_resp3() {
                    out.push(b'(');
                    out.extend_from_slice(s.as_bytes());
                    out.extend_from_slice(b"\r\n");
                } else {
                    encode_bulk(out, s.as_bytes());
                }
            }
            Value::VerbatimString { format, data } => {
                if proto.is_resp3() {
                    // `=<len>\r\n<fmt>:<payload>\r\n`, where len counts the 3-byte
                    // format, the ':' separator, and the payload.
                    let total = 4 + data.len();
                    out.push(b'=');
                    out.extend_from_slice(total.to_string().as_bytes());
                    out.extend_from_slice(b"\r\n");
                    out.extend_from_slice(format);
                    out.push(b':');
                    out.extend_from_slice(data);
                    out.extend_from_slice(b"\r\n");
                } else {
                    encode_bulk(out, data);
                }
            }
            Value::Map(pairs) => {
                if proto.is_resp3() {
                    encode_aggregate_header(out, b'%', pairs.len());
                } else {
                    encode_aggregate_header(out, b'*', pairs.len() * 2);
                }
                for (k, v) in pairs {
                    k.encode_into(out, proto);
                    v.encode_into(out, proto);
                }
            }
            Value::Set(items) => {
                encode_aggregate_header(
                    out,
                    if proto.is_resp3() { b'~' } else { b'*' },
                    items.len(),
                );
                for item in items {
                    item.encode_into(out, proto);
                }
            }
            Value::Push(items) => {
                encode_aggregate_header(
                    out,
                    if proto.is_resp3() { b'>' } else { b'*' },
                    items.len(),
                );
                for item in items {
                    item.encode_into(out, proto);
                }
            }
        }
    }
}

/// Write a `$<len>\r\n<bytes>\r\n` bulk string.
#[inline]
fn encode_bulk(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'$');
    out.extend_from_slice(bytes.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
}

/// Write an aggregate header byte followed by its element count and CRLF.
#[inline]
fn encode_aggregate_header(out: &mut Vec<u8>, marker: u8, count: usize) {
    out.push(marker);
    out.extend_from_slice(count.to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// Format an `f64` the way Redis renders scores/doubles: `inf`/`-inf`/`nan` for
/// the non-finite cases, an integer with no decimal point when the value is
/// whole, and otherwise the shortest round-trippable decimal.
pub fn format_double(d: f64) -> String {
    if d.is_nan() {
        "nan".to_string()
    } else if d.is_infinite() {
        if d > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        }
    } else if d == d.trunc() && d.abs() < 1e17 {
        format!("{}", d as i64)
    } else {
        format!("{}", d)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::SimpleString(s) => write!(f, "{}", s),
            Value::Error(s) => write!(f, "Error: {}", s),
            Value::Integer(i) => write!(f, "{}", i),
            Value::BulkString(b) => match str::from_utf8(b) {
                Ok(s) => write!(f, "{}", s),
                Err(_) => write!(f, "{:?}", b),
            },
            Value::Array(a) => {
                write!(f, "[")?;
                for (i, item) in a.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
            Value::Nil => write!(f, "(nil)"),
            Value::NilArray => write!(f, "(nil)"),
            Value::Boolean(b) => write!(f, "{}", b),
            Value::Double(d) => write!(f, "{}", d),
            Value::BigNumber(s) => write!(f, "{}", s),
            Value::VerbatimString { data, .. } => match str::from_utf8(data) {
                Ok(s) => write!(f, "{}", s),
                Err(_) => write!(f, "{:?}", data),
            },
            Value::Map(m) => {
                write!(f, "{{")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", k, v)?;
                }
                write!(f, "}}")
            }
            Value::Set(s) => {
                write!(f, "{{")?;
                for (i, item) in s.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "}}")
            }
            Value::Push(items) => {
                write!(f, "push[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
        }
    }
}

#[cfg(test)]
mod encode_tests {
    use super::*;

    const R2: ProtocolVersion = ProtocolVersion::Resp2;
    const R3: ProtocolVersion = ProtocolVersion::Resp3;

    fn enc(v: &Value, p: ProtocolVersion) -> Vec<u8> {
        v.encode(p)
    }

    #[test]
    fn scalars_are_protocol_invariant() {
        assert_eq!(enc(&Value::SimpleString("OK".into()), R2), b"+OK\r\n");
        assert_eq!(enc(&Value::SimpleString("OK".into()), R3), b"+OK\r\n");
        assert_eq!(enc(&Value::Integer(42), R2), b":42\r\n");
        assert_eq!(enc(&Value::Integer(42), R3), b":42\r\n");
        assert_eq!(enc(&Value::Error("ERR x".into()), R2), b"-ERR x\r\n");
        assert_eq!(
            enc(&Value::BulkString(b"abc".to_vec()), R2),
            b"$3\r\nabc\r\n"
        );
        assert_eq!(
            enc(&Value::BulkString(b"abc".to_vec()), R3),
            b"$3\r\nabc\r\n"
        );
    }

    #[test]
    fn nil_framing_differs_by_protocol() {
        assert_eq!(enc(&Value::Nil, R2), b"$-1\r\n");
        assert_eq!(enc(&Value::Nil, R3), b"_\r\n");
        assert_eq!(enc(&Value::NilArray, R2), b"*-1\r\n");
        assert_eq!(enc(&Value::NilArray, R3), b"_\r\n");
    }

    #[test]
    fn boolean_downgrades_to_integer_in_resp2() {
        assert_eq!(enc(&Value::Boolean(true), R2), b":1\r\n");
        assert_eq!(enc(&Value::Boolean(false), R2), b":0\r\n");
        assert_eq!(enc(&Value::Boolean(true), R3), b"#t\r\n");
        assert_eq!(enc(&Value::Boolean(false), R3), b"#f\r\n");
    }

    #[test]
    fn double_is_bulk_in_resp2_and_comma_in_resp3() {
        assert_eq!(enc(&Value::Double(3.0), R2), b"$1\r\n3\r\n");
        assert_eq!(enc(&Value::Double(3.0), R3), b",3\r\n");
        assert_eq!(enc(&Value::Double(f64::INFINITY), R3), b",inf\r\n");
        assert_eq!(enc(&Value::Double(f64::NEG_INFINITY), R3), b",-inf\r\n");
    }

    #[test]
    fn map_is_flat_array_in_resp2_and_map_in_resp3() {
        let m = Value::Map(vec![
            (Value::BulkString(b"f1".to_vec()), Value::Integer(1)),
            (Value::BulkString(b"f2".to_vec()), Value::Integer(2)),
        ]);
        assert_eq!(enc(&m, R2), b"*4\r\n$2\r\nf1\r\n:1\r\n$2\r\nf2\r\n:2\r\n");
        assert_eq!(enc(&m, R3), b"%2\r\n$2\r\nf1\r\n:1\r\n$2\r\nf2\r\n:2\r\n");
    }

    #[test]
    fn set_is_array_in_resp2_and_set_in_resp3() {
        let s = Value::Set(vec![Value::BulkString(b"a".to_vec())]);
        assert_eq!(enc(&s, R2), b"*1\r\n$1\r\na\r\n");
        assert_eq!(enc(&s, R3), b"~1\r\n$1\r\na\r\n");
    }

    #[test]
    fn push_is_array_in_resp2_and_push_in_resp3() {
        let p = Value::Push(vec![
            Value::BulkString(b"message".to_vec()),
            Value::BulkString(b"ch".to_vec()),
            Value::BulkString(b"hi".to_vec()),
        ]);
        assert_eq!(&enc(&p, R2)[..4], b"*3\r\n");
        assert_eq!(&enc(&p, R3)[..4], b">3\r\n");
    }

    #[test]
    fn big_number_and_verbatim_have_resp2_fallbacks() {
        let bn = Value::BigNumber("12345".into());
        assert_eq!(enc(&bn, R2), b"$5\r\n12345\r\n");
        assert_eq!(enc(&bn, R3), b"(12345\r\n");

        let vb = Value::VerbatimString {
            format: *b"txt",
            data: b"hello".to_vec(),
        };
        assert_eq!(enc(&vb, R2), b"$5\r\nhello\r\n");
        assert_eq!(enc(&vb, R3), b"=9\r\ntxt:hello\r\n");
    }

    #[test]
    fn nested_array_recurses_with_protocol() {
        let a = Value::Array(vec![
            Value::Integer(1),
            Value::Set(vec![Value::BulkString(b"x".to_vec())]),
        ]);
        // Inner set must respect the protocol of the outer encode call.
        assert_eq!(enc(&a, R2), b"*2\r\n:1\r\n*1\r\n$1\r\nx\r\n");
        assert_eq!(enc(&a, R3), b"*2\r\n:1\r\n~1\r\n$1\r\nx\r\n");
    }

    #[test]
    fn serialize_shim_matches_resp2_encode() {
        let v = Value::Map(vec![(Value::Nil, Value::Boolean(true))]);
        assert_eq!(v.serialize(), v.encode(R2));
    }
}
