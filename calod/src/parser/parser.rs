
use memchr::memchr2;
use thiserror::Error;
use tracing::{debug, error, trace};

// use std::simd::{LaneCount, Simd, StdFloat};

// #[cfg(target_arch = "x86_64")]
// type TargetSimd<const N: usize> = Simd<u8, N>;

// #[cfg(not(target_arch = "x86_64"))] // Fallback for non-x86_64 architectures
// type TargetSimd<const N: usize> = Simd<u8, N>;

#[derive(Debug, PartialEq, Clone)]
pub enum Value {
    Integer(i64),
    SimpleString(String),
    Error(String),
    BulkString(Vec<u8>),
    Array(Vec<Value>),
    Nil,
}

#[derive(Debug, Error)]
pub enum RespError {
    #[error("Incomplete message")]
    Incomplete,
    #[error("Invalid format")]
    InvalidFormat,
    #[error("Unsupported type")]
    UnsupportedType,
}

pub struct RESPParser;

impl RESPParser {
    pub fn parse_value(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if buffer.is_empty() {
            trace!("parse_value: Incomplete - empty buffer");
            return Err(RespError::Incomplete);
        }

        trace!("parse_value: Starting parsing for buffer: {:?}", buffer);
        match buffer[0] {
            b'+' => Self::parse_simd_string(buffer),
            b'$' => Self::parse_simd_bulk(buffer),
            b'*' => Self::parse_simd_array(buffer),
            b'-' => Self::parse_simd_error(buffer),
            b':' => Self::parse_simd_integer(buffer),
            _ => {
                error!("parse_value: Invalid format - unexpected start byte: {}", buffer[0]);
                Err(RespError::InvalidFormat)
            },
        }
    }

    pub fn parse_multiple(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        let mut position = 0;
        let mut responses = Vec::new();

        while position < buffer.len() {
            let (value, consumed) = Self::parse_value(&buffer[position..])?;
            responses.push(value);
            position += consumed;
            if consumed == 0 { break; }
        }

        Ok((Value::Array(responses), position))
    }

    // #[inline(always)]
    // fn find_crlf_simd(buf: &[u8]) -> Option<usize> where LaneCount<N>: StdLaneCount {
    //     const SIMD_WIDTH: usize = 32;
        
    //     let mut offset = 0;
    //     let cr = Simd::<u8, SIMD_WIDTH>::splat(b'\r');
    //     let lf = Simd::<u8, SIMD_WIDTH>::splat(b'\n');

    //     while offset + SIMD_WIDTH <= buf.len() {
    //         let chunk = Simd::from_slice(&buf[offset..offset + SIMD_WIDTH]);
    //         let cr_mask: u32 = chunk.simd_eq(cr).to_bitmask();
    //         let lf_mask: u32 = chunk.simd_eq(lf).to_bitmask();

    //         if cr_mask != 0 && lf_mask != 0 {
    //             for i in 0..SIMD_WIDTH - 1 {
    //                 if buf[offset + i] == b'\r' && buf[offset + i + 1] == b'\n' {
    //                     return Some(offset + i);
    //                 }
    //             }
    //         }
    //         offset += SIMD_WIDTH;
    //     }

    //     // Fallback for remaining bytes
    //     buf.windows(2).skip(offset).position(|w| w == b"\r\n").map(|pos| offset + pos)
    // }

    fn find_crlf(buf: &[u8]) -> Option<usize> {
        let mut offset = 0;
        loop {
            match memchr2(b'\r', b'\n', &buf[offset..]) {
                Some(pos) => {
                    let absolute_pos = offset + pos;
                    if absolute_pos + 1 < buf.len()
                        && buf[absolute_pos] == b'\r'
                        && buf[absolute_pos + 1] == b'\n' {
                            return Some(absolute_pos);
                        }
                        offset = absolute_pos + 1;
                }
                None => return None,
            }
        }
    }

    fn parse_simd_string(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if let Some(end) = Self::find_crlf(buffer) {
            let s = std::str::from_utf8(&buffer[1..end]).map_err(|_| RespError::InvalidFormat)?;
            let value = Value::SimpleString(s.to_owned());
            debug!("Parsed SimpleString: {:?}", value);
            Ok((value, end + 2))
        } else {
            trace!("parse_simd_string: Incomplete - CRLF not found");
            Err(RespError::Incomplete)
        }
    }

    fn parse_simd_error(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if let Some(end) = Self::find_crlf(buffer) {
            let s = std::str::from_utf8(&buffer[1..end]).map_err(|_| RespError::InvalidFormat)?;
            let value = Value::Error(s.to_owned());
            debug!("Parsed Error: {:?}", value);
            Ok((value, end + 2))
        } else {
            trace!("parse_simd_error: Incomplete - CRLF not found");
            Err(RespError::Incomplete)
        }
    }

    fn parse_simd_integer(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if let Some(end) = Self::find_crlf(buffer) {
            let s = std::str::from_utf8(&buffer[1..end]).map_err(|_| RespError::InvalidFormat)?;
            let num = s.parse::<i64>().map_err(|_| RespError::InvalidFormat)?;
            let value = Value::Integer(num);
            debug!("Parsed Integer: {:?}", value);
            Ok((value, end + 2))
        } else {
            trace!("parse_simd_integer: Incomplete - CRLF not found");
            Err(RespError::Incomplete)
        }
    }

    fn parse_simd_bulk(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        let Some(len_end) = Self::find_crlf(buffer) else {
            trace!("parse_simd_bulk: Incomplete - length CRLF not found");
            return Err(RespError::Incomplete);
        };

        let len: i64 = std::str::from_utf8(&buffer[1..len_end])
        .map_err(|_| RespError::InvalidFormat)?
        .parse()
        .map_err(|_| RespError::InvalidFormat)?;

        if len == -1 {
            debug!("Parsed Nil BulkString");
            return Ok((Value::Nil, len_end + 2));
        }
        if len < 0 {
            error!("parse_simd_bulk: Invalid format - negative length but not -1: {}", len);
            return Err(RespError::InvalidFormat);
        }

        let len = len as usize;

        let bulk_start = len_end.checked_add(2).ok_or(RespError::InvalidFormat)?;
        let bulk_end = bulk_start.checked_add(len + 2).ok_or(RespError::InvalidFormat)?;

        if buffer.len() < bulk_end {
            trace!("parse_simd_bulk: Incomplete - bulk data not fully received, expected length: {}, received buffer length: {}", bulk_end, buffer.len());
            return Err(RespError::Incomplete);
        }

        let data = buffer.get(bulk_start..bulk_start + len)
            .ok_or(RespError::InvalidFormat)?
            .to_vec();
        // check for trailing CRLF after bulk data
        if buffer.get(bulk_start + len..bulk_end) != Some(b"\r\n") {
            error!("parse_simd_bulk: Invalid format - missing trailing CRLF after bulk data");
            return Err(RespError::InvalidFormat);
        }
        let value = Value::BulkString(data.to_vec());
        debug!("Parsed BulkString: {:?}, length: {}", value, len);
        Ok((value, bulk_end))
    }

    fn parse_simd_array(buf: &[u8]) -> Result<(Value, usize), RespError> {
        let Some(len_end) = Self::find_crlf(buf) else {
            trace!("parse_simd_array: Incomplete - array length CRLF not found");
            return Err(RespError::Incomplete);
        };
    
        let len_str = std::str::from_utf8(&buf[1..len_end])
            .map_err(|_| RespError::InvalidFormat)?;

        let len: i64 = len_str
            .parse()
            .map_err(|_| RespError::InvalidFormat)?;
        
        if len < 0 {
            error!("parse_simd_array: Invalid format - negative array length: {}", len);
            return Err(RespError::InvalidFormat);
        }
        let len = len as usize;
    
        let mut position = len_end.checked_add(2).ok_or(RespError::InvalidFormat)?;
        let mut elements = Vec::with_capacity(len);
    
        for _ in 0..len {
            if position >= buf.len() {
                trace!("parse_simd_array: Incomplete - not enough elements, expected: {}, parsed: {}", len, elements.len());
                return Err(RespError::Incomplete);
            }
    
            let (value, consumed) = match buf[position] {
                b'+' => Self::parse_simd_string(&buf[position..])?,
                b'$' => Self::parse_simd_bulk(&buf[position..])?,
                b'*' => Self::parse_simd_array(&buf[position..])?,
                b'-' => Self::parse_simd_error(&buf[position..])?,
                b':' => Self::parse_simd_integer(&buf[position..])?,
                _ => {
                    error!("parse_simd_array: Invalid format - unexpected array element type start byte: {}", buf[position]);
                    return Err(RespError::InvalidFormat)
                },
            };
    
            elements.push(value);
            position += consumed;
        }
        
        let value = Value::Array(elements);
        debug!("Parsed Array: {:?}, length: {}", value, len);
        Ok((value, position))
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_string() {
        let buffer = b"+OK\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::SimpleString("OK".to_string()));
        assert_eq!(consumed, 5);
    }

    #[test]
    fn test_parse_error() {
        let buffer = b"-Error message\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::Error("Error message".to_string()));
        assert_eq!(consumed, 14);
    }

    #[test]
    fn test_parse_integer() {
        let buffer = b":123\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::Integer(123));
        assert_eq!(consumed, 6);
    }

    #[test]
    fn test_parse_bulk_string() {
        let buffer = b"$6\r\nfoobar\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::BulkString(b"foobar".to_vec()));
        assert_eq!(consumed, 12);
    }

    #[test]
    fn test_parse_nil_bulk_string() {
        let buffer = b"$-1\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::Nil);
        assert_eq!(consumed, 5);
    }

    #[test]
    fn test_parse_array() {
        let buffer = b"*2\r\n+OK\r\n$5\r\nhello\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::Array(vec![
            Value::SimpleString("OK".to_string()),
            Value::BulkString(b"hello".to_vec()),
        ]));
        assert_eq!(consumed, 19);
    }

    #[test]
    fn test_parse_nested_array() {
        let buffer = b"*2\r\n*2\r\n:1\r\n:2\r\n*1\r\n+hello\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::Array(vec![
            Value::Array(vec![
                Value::Integer(1),
                Value::Integer(2),
            ]),
            Value::Array(vec![
                Value::SimpleString("hello".to_string()),
            ]),
        ]));
        assert_eq!(consumed, 27);
    }

    #[test]
    fn test_parse_multiple_values() {
        let buffer = b"+PONG\r\n+OK\r\n*2\r\n:1\r\n:2\r\n";
        let (value, consumed) = RESPParser::parse_multiple(buffer).unwrap();
        assert_eq!(value, Value::Array(vec![
            Value::SimpleString("PONG".to_string()),
            Value::SimpleString("OK".to_string()),
            Value::Array(vec![
                Value::Integer(1),
                Value::Integer(2),
            ]),
        ]));
        assert_eq!(consumed, 25);
    }

}