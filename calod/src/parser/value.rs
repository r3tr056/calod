use std::fmt;
use std::str;

/// Represents a RESP (Redis Serialization Protocol) value
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
    
    /// Null value (null bulk string or null array)
    Nil,
    
    // RESP3 types (Redis 6.0+)
    /// Boolean value (RESP3)
    Boolean(bool),
    
    /// Double value (RESP3)
    Double(f64),
    
    /// Map value (RESP3)
    Map(Vec<(Value, Value)>),
    
    /// Set value (RESP3)
    Set(Vec<Value>),
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

    /// Serializes the value into a RESP-formatted byte vector
    pub fn serialize(&self) -> Vec<u8> {
        match self {
            Value::SimpleString(s) => {
                let mut result = Vec::with_capacity(3 + s.len());
                result.push(b'+');
                result.extend_from_slice(s.as_bytes());
                result.extend_from_slice(b"\r\n");
                result
            }
            Value::Error(s) => {
                let mut result = Vec::with_capacity(3 + s.len());
                result.push(b'-');
                result.extend_from_slice(s.as_bytes());
                result.extend_from_slice(b"\r\n");
                result
            }
            Value::Integer(i) => {
                let s = i.to_string();
                let mut result = Vec::with_capacity(3 + s.len());
                result.push(b':');
                result.extend_from_slice(s.as_bytes());
                result.extend_from_slice(b"\r\n");
                result
            }
            Value::BulkString(b) => {
                let len = b.len().to_string();
                let mut result = Vec::with_capacity(5 + len.len() + b.len());
                result.push(b'$');
                result.extend_from_slice(len.as_bytes());
                result.extend_from_slice(b"\r\n");
                result.extend_from_slice(b);
                result.extend_from_slice(b"\r\n");
                result
            }
            Value::Array(a) => {
                let len = a.len().to_string();
                let mut result = Vec::with_capacity(3 + len.len());
                result.push(b'*');
                result.extend_from_slice(len.as_bytes());
                result.extend_from_slice(b"\r\n");
                for item in a {
                    result.extend_from_slice(&item.serialize());
                }
                result
            }
            Value::Nil => b"$-1\r\n".to_vec(),
            Value::Boolean(b) => {
                if *b {
                    b"#t\r\n".to_vec()
                } else {
                    b"#f\r\n".to_vec()
                }
            }
            Value::Double(d) => {
                let s = d.to_string();
                let mut result = Vec::with_capacity(3 + s.len());
                result.push(b',');
                result.extend_from_slice(s.as_bytes());
                result.extend_from_slice(b"\r\n");
                result
            }
            Value::Map(m) => {
                let len = m.len().to_string();
                let mut result = Vec::with_capacity(3 + len.len());
                result.push(b'%');
                result.extend_from_slice(len.as_bytes());
                result.extend_from_slice(b"\r\n");
                for (k, v) in m {
                    result.extend_from_slice(&k.serialize());
                    result.extend_from_slice(&v.serialize());
                }
                result
            }
            Value::Set(s) => {
                let len = s.len().to_string();
                let mut result = Vec::with_capacity(3 + len.len());
                result.push(b'~');
                result.extend_from_slice(len.as_bytes());
                result.extend_from_slice(b"\r\n");
                for item in s {
                    result.extend_from_slice(&item.serialize());
                }
                result
            }
        }
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
            Value::Boolean(b) => write!(f, "{}", b),
            Value::Double(d) => write!(f, "{}", d),
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
        }
    }
}