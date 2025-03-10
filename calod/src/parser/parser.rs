use tracing::{debug, error, trace};
use crate::parser::errors::RespError;
use super::{simd, value::Value};

pub struct RESPParser;

impl RESPParser {
    pub fn parse_value(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if buffer.is_empty() {
            trace!("parse_value: Incomplete - empty buffer");
            return Err(RespError::Incomplete);
        }

        trace!("parse_value: Starting parsing for buffer: {:?}", buffer);
        match buffer[0] {
            b'+' => Self::parse_simple_string(buffer),
            b'-' => Self::parse_error(buffer),
            b':' => Self::parse_integer(buffer),
            b'$' => Self::parse_bulk_string(buffer),
            b'*' => Self::parse_array(buffer),
            // RESP3 types
            b'#' => Self::parse_boolean(buffer),
            b',' => Self::parse_double(buffer),
            b'%' => Self::parse_map(buffer),
            b'~' => Self::parse_set(buffer),
            _ => {
                error!("parse_value: Invalid format - unexpected start byte: {}", buffer[0]);
                Err(RespError::invalid_format(format!("Unexpected type identifier: {}", buffer[0])))
            },
        }
    }

    /// Parse multiple RESP values from a buffer, returning an array of responses
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

    /// Parse a simple string (+)
    fn parse_simple_string(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if let Some(end) = simd::find_crlf(buffer) {
            let s = std::str::from_utf8(&buffer[1..end]).map_err(|e| RespError::Utf8(e))?;
            let value = Value::SimpleString(s.to_owned());
            debug!("Parsed SimpleString: {:?}", value);
            Ok((value, end + 2))
        } else {
            trace!("parse_simple_string: Incomplete - CRLF not found");
            Err(RespError::Incomplete)
        }
    }

    /// Parse an error (-)
    fn parse_error(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if let Some(end) = simd::find_crlf(buffer) {
            let s = std::str::from_utf8(&buffer[1..end])
                .map_err(|e| RespError::Utf8(e))?;
            let value = Value::Error(s.to_owned());
            debug!("Parsed Error: {:?}", value);
            Ok((value, end + 2))
        } else {
            trace!("parse_error: Incomplete - CRLF not found");
            Err(RespError::Incomplete)
        }
    }

    /// Parse an integer (:)
    fn parse_integer(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if let Some(end) = simd::find_crlf(buffer) {
            let s = std::str::from_utf8(&buffer[1..end]).map_err(|e| RespError::Utf8(e))?;
            let num = s.parse::<i64>().map_err(|e| RespError::ParseInt(e))?;
            let value = Value::Integer(num);
            debug!("Parsed Integer: {:?}", value);
            Ok((value, end + 2))
        } else {
            trace!("parse_integer: Incomplete - CRLF not found");
            Err(RespError::Incomplete)
        }
    }

    /// Parse a bulk string ($)
    fn parse_bulk_string(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        let Some(len_end) = simd::find_crlf(buffer) else {
            trace!("parse_bulk_string: Incomplete - length CRLF not found");
            return Err(RespError::Incomplete);
        };

        let len: i64 = std::str::from_utf8(&buffer[1..len_end])
            .map_err(|e| RespError::Utf8(e))?
            .parse()
            .map_err(|e| RespError::ParseInt(e))?;

        if len == -1 {
            debug!("Parsed Nil BulkString");
            return Ok((Value::Nil, len_end + 2));
        }

        if len < 0 {
            error!("parse_bulk_string: Invalid format - negative length but not -1: {}", len);
            return Err(RespError::invalid_format(format!("Invalid bulk string length: {}", len)));
        }

        let len = len as usize;

        let bulk_start = len_end.checked_add(2)
            .ok_or_else(|| RespError::Overflow)?;
        let bulk_end = bulk_start.checked_add(len)
            .and_then(|end| end.checked_add(2))
            .ok_or_else(|| RespError::Overflow)?;

        if buffer.len() < bulk_end {
            trace!("parse_bulk_string: Incomplete - bulk data not fully received, expected length: {}, received buffer length: {}", 
                  bulk_end, buffer.len());
            return Err(RespError::Incomplete);
        }

        let data = buffer.get(bulk_start..bulk_start + len)
            .ok_or_else(|| RespError::invalid_format("Invalid bulk string bounds"))?
            .to_vec();

        // check for trailing CRLF after bulk data
        if buffer.get(bulk_start + len..bulk_end) != Some(b"\r\n") {
            error!("parse_bulk_string: Invalid format - missing trailing CRLF after bulk data");
            return Err(RespError::invalid_format("Missing trailing CRLF after bulk data"));
        }

        let value = Value::BulkString(data);
        debug!("Parsed BulkString, length: {}", len);
        Ok((value, bulk_end))
    }

    /// Parse an array (*)
    fn parse_array(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        let Some(len_end) = simd::find_crlf(buffer) else {
            trace!("parse_array: Incomplete - array length CRLF not found");
            return Err(RespError::Incomplete);
        };

        let len: i64 = std::str::from_utf8(&buffer[1..len_end])
            .map_err(|e| RespError::Utf8(e))?
            .parse()
            .map_err(|e| RespError::ParseInt(e))?;

        if len == -1 {
            debug!("Parsed Nil Array");
            return Ok((Value::Nil, len_end + 2));
        }

        if len < 0 {
            error!("parse_array: Invalid format - negative array length: {}", len);
            return Err(RespError::invalid_format(format!("Invalid array length: {}", len)));
        }

        let len = len as usize;

        let mut position = len_end.checked_add(2).ok_or_else(|| RespError::Overflow)?;
        let mut elements = Vec::with_capacity(len);

        for i in 0..len {
            if position >= buffer.len() {
                trace!("parse_array: Incomplete - not enough elements, excepted: {}, parsed: {}", len, i);
                return Err(RespError::Incomplete);
            }

            let (value, consumed) = Self::parse_value(&buffer[position..])?;
            elements.push(value);
            position += consumed;
        }

        let value = Value::Array(elements);
        debug!("Parsed Array with length: {}", len);
        Ok((value, position))
    }

    /// Parse a boolean (#) - RESP3
    fn parse_boolean(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if buffer.len() < 3 {
            return Err(RespError::Incomplete);
        }

        match buffer[1] {
            b't' => {
                if buffer.len() < 4 || &buffer[2..4] != b"\r\n" {
                    return Err(RespError::Incomplete);
                }
                Ok((Value::Boolean(true), 4))
            },
            b'f' => {
                if buffer.len() < 4 || &buffer[2..4] != b"\r\n" {
                    return Err(RespError::Incomplete);
                }
                Ok((Value::Boolean(false), 4))
            },
            _ => Err(RespError::invalid_format(format!("Invalid boolean value: {}", buffer[1])))
        }
    }

    /// Parse a double (,) - RESP3
    fn parse_double(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if let Some(end) = simd::find_crlf(buffer) {
            let s = std::str::from_utf8(&buffer[1..end])
                .map_err(|e| RespError::Utf8(e))?;

            let num = s.parse::<f64>().map_err(|_| RespError::invalid_format(format!("Invalid double value: {}", s)))?;

            let value = Value::Double(num);
            debug!("Parsed Double: {}", num);
            Ok((value, end + 2))
        } else {
            trace!("parse_double: Incomplete - CRLF not found");
            Err(RespError::Incomplete)
        }
    }

    /// Parse a map (%) - RESP3
    fn parse_map(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        let Some(len_end) = simd::find_crlf(buffer) else {
            trace!("parse_map: Incomplete - map length CRLF not found");
            return Err(RespError::Incomplete);
        };
    
        let len: i64 = std::str::from_utf8(&buffer[1..len_end])
            .map_err(|e| RespError::Utf8(e))?
            .parse()
            .map_err(|e| RespError::ParseInt(e))?;
        
        if len < 0 {
            error!("parse_map: Invalid format - negative map length: {}", len);
            return Err(RespError::invalid_format(format!("Invalid map length: {}", len)));
        }
        
        let len = len as usize;
    
        let mut position = len_end.checked_add(2)
            .ok_or_else(|| RespError::Overflow)?;
        let mut entries = Vec::with_capacity(len);
    
        for _ in 0..len {
            if position >= buffer.len() {
                trace!("parse_map: Incomplete - not enough entries");
                return Err(RespError::Incomplete);
            }
    
            // Parse key
            let (key, key_consumed) = Self::parse_value(&buffer[position..])?;
            position += key_consumed;
            
            // Parse value
            if position >= buffer.len() {
                trace!("parse_map: Incomplete - missing value for key");
                return Err(RespError::Incomplete);
            }
            
            let (value, value_consumed) = Self::parse_value(&buffer[position..])?;
            position += value_consumed;
            
            entries.push((key, value));
        }
        
        let value = Value::Map(entries);
        debug!("Parsed Map with length: {}", len);
        Ok((value, position))
    }

    /// Parse a set (~) - RESP3
    fn parse_set(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        let Some(len_end) = simd::find_crlf(buffer) else {
            trace!("parse_set: Incomplete - set length CRLF not found");
            return Err(RespError::Incomplete);
        };
    
        let len: i64 = std::str::from_utf8(&buffer[1..len_end])
            .map_err(|e| RespError::Utf8(e))?
            .parse()
            .map_err(|e| RespError::ParseInt(e))?;
        
        if len < 0 {
            error!("parse_set: Invalid format - negative set length: {}", len);
            return Err(RespError::invalid_format(format!("Invalid set length: {}", len)));
        }
        
        let len = len as usize;
    
        let mut position = len_end.checked_add(2)
            .ok_or_else(|| RespError::Overflow)?;
        let mut elements = Vec::with_capacity(len);
    
        for _ in 0..len {
            if position >= buffer.len() {
                trace!("parse_set: Incomplete - not enough elements");
                return Err(RespError::Incomplete);
            }
    
            let (value, consumed) = Self::parse_value(&buffer[position..])?;
            elements.push(value);
            position += consumed;
        }
        
        let value = Value::Set(elements);
        debug!("Parsed Set with length: {}", len);
        Ok((value, position))
    }
}

