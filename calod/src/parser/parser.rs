use super::{simd, value::Value};
use crate::parser::errors::RespError;
use tracing::{debug, error, trace};

pub struct RESPParser;

/// Maximum accepted bulk-string payload length (mirrors Redis `proto-max-bulk-len`, 512MB).
pub const MAX_BULK_LEN: usize = 512 * 1024 * 1024;
/// Maximum number of elements in a single aggregate (mirrors Redis multibulk cap, 1M).
pub const MAX_MULTIBULK_LEN: i64 = 1024 * 1024;
/// Maximum aggregate nesting depth, to bound recursion on hostile input.
pub const MAX_DEPTH: usize = 128;
/// Maximum length of an inline command line (mirrors Redis `PROTO_INLINE_MAX_SIZE`, 64KB).
pub const MAX_INLINE_LEN: usize = 64 * 1024;

impl RESPParser {
    /// Parse a single value from a live client connection. A leading byte that
    /// is not a RESP type marker is treated as an inline command (e.g. a bare
    /// `PING\r\n` from a healthcheck or telnet), matching Redis.
    pub fn parse_value(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        Self::parse_value_depth(buffer, 0, true)
    }

    /// Parse a single value with the inline-command fallback disabled. Used for
    /// trusted-but-must-be-well-formed sources such as AOF replay, where an
    /// unexpected leading byte indicates corruption and must surface as an error
    /// rather than being reinterpreted as an inline command.
    pub fn parse_value_strict(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        Self::parse_value_depth(buffer, 0, false)
    }

    /// Depth-tracked dispatch. `depth` counts how many aggregates we are nested
    /// inside, so a maliciously deep payload cannot blow the stack. `allow_inline`
    /// is only ever true for the top-level frame of a live client connection;
    /// nested elements never permit inline syntax.
    fn parse_value_depth(
        buffer: &[u8],
        depth: usize,
        allow_inline: bool,
    ) -> Result<(Value, usize), RespError> {
        if buffer.is_empty() {
            trace!("parse_value: Incomplete - empty buffer");
            return Err(RespError::Incomplete);
        }

        if depth > MAX_DEPTH {
            error!("parse_value: nesting depth {} exceeds maximum", depth);
            return Err(RespError::DepthExceeded);
        }

        trace!("parse_value: Starting parsing for buffer: {:?}", buffer);
        match buffer[0] {
            b'+' => Self::parse_simple_string(buffer),
            b'-' => Self::parse_error(buffer),
            b':' => Self::parse_integer(buffer),
            b'$' => Self::parse_bulk_string(buffer),
            b'*' => Self::parse_array(buffer, depth),
            // RESP3 types
            b'#' => Self::parse_boolean(buffer),
            b',' => Self::parse_double(buffer),
            b'%' => Self::parse_map(buffer, depth),
            b'~' => Self::parse_set(buffer, depth),
            b'_' => Self::parse_null(buffer),
            b'(' => Self::parse_big_number(buffer),
            b'!' => Self::parse_blob_error(buffer),
            b'=' => Self::parse_verbatim(buffer),
            b'>' => Self::parse_push(buffer, depth),
            b'|' => Self::parse_attribute(buffer, depth),
            // Any other leading byte at the top level of a client connection is
            // treated as an inline command. Inside an aggregate, or for strict
            // (AOF) parsing, it is a protocol error.
            _ => {
                if allow_inline && depth == 0 {
                    Self::parse_inline(buffer)
                } else {
                    error!(
                        "parse_value: Invalid format - unexpected start byte: {}",
                        buffer[0]
                    );
                    Err(RespError::invalid_format(format!(
                        "Unexpected type identifier: {}",
                        buffer[0]
                    )))
                }
            }
        }
    }

    /// Parse a Redis inline command: a single newline-terminated line whose
    /// whitespace-separated tokens become bulk-string arguments. Returns an
    /// `Array` of `BulkString`s so the command layer can treat it uniformly.
    fn parse_inline(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        match memchr::memchr(b'\n', buffer) {
            Some(nl) => {
                if nl > MAX_INLINE_LEN {
                    return Err(RespError::InlineTooLong);
                }
                // Strip the optional trailing CR so both `\n` and `\r\n` work.
                let mut line_end = nl;
                if line_end > 0 && buffer[line_end - 1] == b'\r' {
                    line_end -= 1;
                }
                let line = &buffer[..line_end];
                let args: Vec<Value> = line
                    .split(|&b| b == b' ' || b == b'\t')
                    .filter(|tok| !tok.is_empty())
                    .map(|tok| Value::BulkString(tok.to_vec()))
                    .collect();
                debug!("Parsed inline command with {} args", args.len());
                Ok((Value::Array(args), nl + 1))
            }
            None => {
                // No newline yet: keep buffering unless the line is already too long.
                if buffer.len() > MAX_INLINE_LEN {
                    Err(RespError::InlineTooLong)
                } else {
                    Err(RespError::Incomplete)
                }
            }
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
            if consumed == 0 {
                break;
            }
        }

        Ok((Value::Array(responses), position))
    }

    /// Parse a simple string (+)
    fn parse_simple_string(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if let Some(end) = simd::find_crlf(buffer) {
            let s = std::str::from_utf8(&buffer[1..end]).map_err(RespError::Utf8)?;
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
            let s = std::str::from_utf8(&buffer[1..end]).map_err(RespError::Utf8)?;
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
            let s = std::str::from_utf8(&buffer[1..end]).map_err(RespError::Utf8)?;
            let num = s.parse::<i64>().map_err(RespError::ParseInt)?;
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
            .map_err(RespError::Utf8)?
            .parse()
            .map_err(RespError::ParseInt)?;

        if len == -1 {
            debug!("Parsed Nil BulkString");
            return Ok((Value::Nil, len_end + 2));
        }

        if len < 0 {
            error!(
                "parse_bulk_string: Invalid format - negative length but not -1: {}",
                len
            );
            return Err(RespError::invalid_format(format!(
                "Invalid bulk string length: {}",
                len
            )));
        }

        let len = len as usize;

        if len > MAX_BULK_LEN {
            error!(
                "parse_bulk_string: bulk length {} exceeds maximum {}",
                len, MAX_BULK_LEN
            );
            return Err(RespError::BulkLengthExceeded);
        }

        let bulk_start = len_end.checked_add(2).ok_or(RespError::Overflow)?;
        let bulk_end = bulk_start
            .checked_add(len)
            .and_then(|end| end.checked_add(2))
            .ok_or(RespError::Overflow)?;

        if buffer.len() < bulk_end {
            trace!("parse_bulk_string: Incomplete - bulk data not fully received, expected length: {}, received buffer length: {}", 
                  bulk_end, buffer.len());
            return Err(RespError::Incomplete);
        }

        let data = buffer
            .get(bulk_start..bulk_start + len)
            .ok_or_else(|| RespError::invalid_format("Invalid bulk string bounds"))?
            .to_vec();

        // check for trailing CRLF after bulk data
        if buffer.get(bulk_start + len..bulk_end) != Some(b"\r\n") {
            error!("parse_bulk_string: Invalid format - missing trailing CRLF after bulk data");
            return Err(RespError::invalid_format(
                "Missing trailing CRLF after bulk data",
            ));
        }

        let value = Value::BulkString(data);
        debug!("Parsed BulkString, length: {}", len);
        Ok((value, bulk_end))
    }

    /// Parse an array (*)
    fn parse_array(buffer: &[u8], depth: usize) -> Result<(Value, usize), RespError> {
        let Some(len_end) = simd::find_crlf(buffer) else {
            trace!("parse_array: Incomplete - array length CRLF not found");
            return Err(RespError::Incomplete);
        };

        let len: i64 = std::str::from_utf8(&buffer[1..len_end])
            .map_err(RespError::Utf8)?
            .parse()
            .map_err(RespError::ParseInt)?;

        if len == -1 {
            debug!("Parsed Nil Array");
            return Ok((Value::NilArray, len_end + 2));
        }

        if len < 0 {
            error!(
                "parse_array: Invalid format - negative array length: {}",
                len
            );
            return Err(RespError::invalid_format(format!(
                "Invalid array length: {}",
                len
            )));
        }

        if len > MAX_MULTIBULK_LEN {
            error!("parse_array: array length {} exceeds maximum", len);
            return Err(RespError::MultibulkLengthExceeded);
        }

        let len = len as usize;

        let mut position = len_end.checked_add(2).ok_or(RespError::Overflow)?;
        // Cap the eager allocation: the declared count is attacker-controlled, so
        // grow on demand rather than trusting it up front.
        let mut elements = Vec::with_capacity(len.min(1024));

        for i in 0..len {
            if position >= buffer.len() {
                trace!(
                    "parse_array: Incomplete - not enough elements, excepted: {}, parsed: {}",
                    len,
                    i
                );
                return Err(RespError::Incomplete);
            }

            let (value, consumed) = Self::parse_value_depth(&buffer[position..], depth + 1, false)?;
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
            }
            b'f' => {
                if buffer.len() < 4 || &buffer[2..4] != b"\r\n" {
                    return Err(RespError::Incomplete);
                }
                Ok((Value::Boolean(false), 4))
            }
            _ => Err(RespError::invalid_format(format!(
                "Invalid boolean value: {}",
                buffer[1]
            ))),
        }
    }

    /// Parse a double (,) - RESP3
    fn parse_double(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if let Some(end) = simd::find_crlf(buffer) {
            let s = std::str::from_utf8(&buffer[1..end]).map_err(RespError::Utf8)?;

            let num = s
                .parse::<f64>()
                .map_err(|_| RespError::invalid_format(format!("Invalid double value: {}", s)))?;

            let value = Value::Double(num);
            debug!("Parsed Double: {}", num);
            Ok((value, end + 2))
        } else {
            trace!("parse_double: Incomplete - CRLF not found");
            Err(RespError::Incomplete)
        }
    }

    /// Parse a map (%) - RESP3
    fn parse_map(buffer: &[u8], depth: usize) -> Result<(Value, usize), RespError> {
        let Some(len_end) = simd::find_crlf(buffer) else {
            trace!("parse_map: Incomplete - map length CRLF not found");
            return Err(RespError::Incomplete);
        };

        let len: i64 = std::str::from_utf8(&buffer[1..len_end])
            .map_err(RespError::Utf8)?
            .parse()
            .map_err(RespError::ParseInt)?;

        if len < 0 {
            error!("parse_map: Invalid format - negative map length: {}", len);
            return Err(RespError::invalid_format(format!(
                "Invalid map length: {}",
                len
            )));
        }

        if len > MAX_MULTIBULK_LEN {
            error!("parse_map: map length {} exceeds maximum", len);
            return Err(RespError::MultibulkLengthExceeded);
        }

        let len = len as usize;

        let mut position = len_end.checked_add(2).ok_or(RespError::Overflow)?;
        let mut entries = Vec::with_capacity(len.min(1024));

        for _ in 0..len {
            if position >= buffer.len() {
                trace!("parse_map: Incomplete - not enough entries");
                return Err(RespError::Incomplete);
            }

            // Parse key
            let (key, key_consumed) =
                Self::parse_value_depth(&buffer[position..], depth + 1, false)?;
            position += key_consumed;

            // Parse value
            if position >= buffer.len() {
                trace!("parse_map: Incomplete - missing value for key");
                return Err(RespError::Incomplete);
            }

            let (value, value_consumed) =
                Self::parse_value_depth(&buffer[position..], depth + 1, false)?;
            position += value_consumed;

            entries.push((key, value));
        }

        let value = Value::Map(entries);
        debug!("Parsed Map with length: {}", len);
        Ok((value, position))
    }

    /// Parse a set (~) - RESP3
    fn parse_set(buffer: &[u8], depth: usize) -> Result<(Value, usize), RespError> {
        let Some(len_end) = simd::find_crlf(buffer) else {
            trace!("parse_set: Incomplete - set length CRLF not found");
            return Err(RespError::Incomplete);
        };

        let len: i64 = std::str::from_utf8(&buffer[1..len_end])
            .map_err(RespError::Utf8)?
            .parse()
            .map_err(RespError::ParseInt)?;

        if len < 0 {
            error!("parse_set: Invalid format - negative set length: {}", len);
            return Err(RespError::invalid_format(format!(
                "Invalid set length: {}",
                len
            )));
        }

        if len > MAX_MULTIBULK_LEN {
            error!("parse_set: set length {} exceeds maximum", len);
            return Err(RespError::MultibulkLengthExceeded);
        }

        let len = len as usize;

        let mut position = len_end.checked_add(2).ok_or(RespError::Overflow)?;
        let mut elements = Vec::with_capacity(len.min(1024));

        for _ in 0..len {
            if position >= buffer.len() {
                trace!("parse_set: Incomplete - not enough elements");
                return Err(RespError::Incomplete);
            }

            let (value, consumed) = Self::parse_value_depth(&buffer[position..], depth + 1, false)?;
            elements.push(value);
            position += consumed;
        }

        let value = Value::Set(elements);
        debug!("Parsed Set with length: {}", len);
        Ok((value, position))
    }

    /// Parse a null (_) - RESP3
    fn parse_null(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if buffer.len() < 3 {
            return Err(RespError::Incomplete);
        }
        if &buffer[1..3] != b"\r\n" {
            return Err(RespError::invalid_format("Invalid null encoding"));
        }
        Ok((Value::Nil, 3))
    }

    /// Parse a big number (`(`) - RESP3. Held as decimal text since it may not
    /// fit an `i64`.
    fn parse_big_number(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        if let Some(end) = simd::find_crlf(buffer) {
            let s = std::str::from_utf8(&buffer[1..end]).map_err(RespError::Utf8)?;
            let bytes = s.as_bytes();
            let body = match bytes.first() {
                Some(b'+') | Some(b'-') => &bytes[1..],
                _ => bytes,
            };
            if body.is_empty() || !body.iter().all(u8::is_ascii_digit) {
                return Err(RespError::invalid_format(format!(
                    "Invalid big number value: {}",
                    s
                )));
            }
            Ok((Value::BigNumber(s.to_owned()), end + 2))
        } else {
            Err(RespError::Incomplete)
        }
    }

    /// Parse the `<len>\r\n<payload>\r\n` body shared by `$`, `!`, and `=`
    /// frames, where `buffer[0]` is the already-dispatched type byte. Returns the
    /// payload slice and the total bytes consumed (including the type byte).
    fn parse_length_prefixed(buffer: &[u8]) -> Result<(&[u8], usize), RespError> {
        let Some(len_end) = simd::find_crlf(buffer) else {
            return Err(RespError::Incomplete);
        };

        let len: i64 = std::str::from_utf8(&buffer[1..len_end])
            .map_err(RespError::Utf8)?
            .parse()
            .map_err(RespError::ParseInt)?;

        if len < 0 {
            return Err(RespError::invalid_format(format!(
                "Invalid length-prefixed value length: {}",
                len
            )));
        }

        let len = len as usize;
        if len > MAX_BULK_LEN {
            return Err(RespError::BulkLengthExceeded);
        }

        let start = len_end.checked_add(2).ok_or(RespError::Overflow)?;
        let end = start
            .checked_add(len)
            .and_then(|e| e.checked_add(2))
            .ok_or(RespError::Overflow)?;

        if buffer.len() < end {
            return Err(RespError::Incomplete);
        }

        if buffer.get(start + len..end) != Some(b"\r\n") {
            return Err(RespError::invalid_format(
                "Missing trailing CRLF after length-prefixed payload",
            ));
        }

        Ok((&buffer[start..start + len], end))
    }

    /// Parse a blob error (!) - RESP3
    fn parse_blob_error(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        let (data, consumed) = Self::parse_length_prefixed(buffer)?;
        let s = std::str::from_utf8(data)
            .map_err(RespError::Utf8)?
            .to_owned();
        Ok((Value::Error(s), consumed))
    }

    /// Parse a verbatim string (=) - RESP3. Layout is `<fmt(3)>:<payload>`.
    fn parse_verbatim(buffer: &[u8]) -> Result<(Value, usize), RespError> {
        let (data, consumed) = Self::parse_length_prefixed(buffer)?;
        if data.len() < 4 || data[3] != b':' {
            return Err(RespError::invalid_format(
                "Invalid verbatim string format header",
            ));
        }
        let mut format = [0u8; 3];
        format.copy_from_slice(&data[0..3]);
        let payload = data[4..].to_vec();
        Ok((
            Value::VerbatimString {
                format,
                data: payload,
            },
            consumed,
        ))
    }

    /// Parse a push message (>) - RESP3. Same framing as an array.
    fn parse_push(buffer: &[u8], depth: usize) -> Result<(Value, usize), RespError> {
        let Some(len_end) = simd::find_crlf(buffer) else {
            return Err(RespError::Incomplete);
        };

        let len: i64 = std::str::from_utf8(&buffer[1..len_end])
            .map_err(RespError::Utf8)?
            .parse()
            .map_err(RespError::ParseInt)?;

        if len < 0 {
            return Err(RespError::invalid_format(format!(
                "Invalid push length: {}",
                len
            )));
        }
        if len > MAX_MULTIBULK_LEN {
            return Err(RespError::MultibulkLengthExceeded);
        }

        let len = len as usize;
        let mut position = len_end.checked_add(2).ok_or(RespError::Overflow)?;
        let mut elements = Vec::with_capacity(len.min(1024));

        for _ in 0..len {
            if position >= buffer.len() {
                return Err(RespError::Incomplete);
            }
            let (value, consumed) = Self::parse_value_depth(&buffer[position..], depth + 1, false)?;
            elements.push(value);
            position += consumed;
        }

        Ok((Value::Push(elements), position))
    }

    /// Parse an attribute (|) - RESP3. The attribute map is out-of-band metadata,
    /// so it is parsed and discarded, and the value that follows is returned.
    fn parse_attribute(buffer: &[u8], depth: usize) -> Result<(Value, usize), RespError> {
        let Some(len_end) = simd::find_crlf(buffer) else {
            return Err(RespError::Incomplete);
        };

        let len: i64 = std::str::from_utf8(&buffer[1..len_end])
            .map_err(RespError::Utf8)?
            .parse()
            .map_err(RespError::ParseInt)?;

        if len < 0 {
            return Err(RespError::invalid_format(format!(
                "Invalid attribute length: {}",
                len
            )));
        }
        if len > MAX_MULTIBULK_LEN {
            return Err(RespError::MultibulkLengthExceeded);
        }

        let len = len as usize;
        let mut position = len_end.checked_add(2).ok_or(RespError::Overflow)?;

        for _ in 0..len {
            if position >= buffer.len() {
                return Err(RespError::Incomplete);
            }
            let (_k, kc) = Self::parse_value_depth(&buffer[position..], depth + 1, false)?;
            position += kc;
            if position >= buffer.len() {
                return Err(RespError::Incomplete);
            }
            let (_v, vc) = Self::parse_value_depth(&buffer[position..], depth + 1, false)?;
            position += vc;
        }

        // The attribute frame prefixes the actual reply value.
        if position >= buffer.len() {
            return Err(RespError::Incomplete);
        }
        let (value, consumed) = Self::parse_value_depth(&buffer[position..], depth, false)?;
        Ok((value, position + consumed))
    }
}

#[cfg(test)]
mod hardening_tests {
    use super::*;

    #[test]
    fn parses_inline_command() {
        let (val, consumed) = RESPParser::parse_value(b"PING\r\n").unwrap();
        assert_eq!(consumed, 6);
        assert_eq!(val, Value::Array(vec![Value::BulkString(b"PING".to_vec())]));
    }

    #[test]
    fn parses_inline_command_lf_only_and_multiarg() {
        let (val, consumed) = RESPParser::parse_value(b"SET key value\n").unwrap();
        assert_eq!(consumed, 14);
        assert_eq!(
            val,
            Value::Array(vec![
                Value::BulkString(b"SET".to_vec()),
                Value::BulkString(b"key".to_vec()),
                Value::BulkString(b"value".to_vec()),
            ])
        );
    }

    #[test]
    fn inline_without_newline_is_incomplete() {
        assert!(matches!(
            RESPParser::parse_value(b"PING"),
            Err(RespError::Incomplete)
        ));
    }

    #[test]
    fn resp_array_still_parses() {
        let (val, _) = RESPParser::parse_value(b"*1\r\n$4\r\nPING\r\n").unwrap();
        assert_eq!(val, Value::Array(vec![Value::BulkString(b"PING".to_vec())]));
    }

    #[test]
    fn rejects_oversized_bulk_length() {
        let huge = MAX_BULK_LEN + 1;
        let frame = format!("${}\r\n", huge);
        assert!(matches!(
            RESPParser::parse_value(frame.as_bytes()),
            Err(RespError::BulkLengthExceeded)
        ));
    }

    #[test]
    fn rejects_oversized_multibulk_length() {
        let huge = MAX_MULTIBULK_LEN + 1;
        let frame = format!("*{}\r\n", huge);
        assert!(matches!(
            RESPParser::parse_value(frame.as_bytes()),
            Err(RespError::MultibulkLengthExceeded)
        ));
    }

    #[test]
    fn rejects_excessive_nesting_depth() {
        // Build a payload nested deeper than MAX_DEPTH: each level is a 1-element array.
        let mut frame = Vec::new();
        for _ in 0..(MAX_DEPTH + 2) {
            frame.extend_from_slice(b"*1\r\n");
        }
        frame.extend_from_slice(b"$1\r\nx\r\n");
        assert!(matches!(
            RESPParser::parse_value(&frame),
            Err(RespError::DepthExceeded)
        ));
    }

    #[test]
    fn nested_type_byte_is_not_treated_as_inline() {
        // A bare 'x' inside an array element is a protocol error, not an inline command.
        let res = RESPParser::parse_value(b"*1\r\nxYZ\r\n");
        assert!(matches!(res, Err(RespError::InvalidFormat(_))));
    }

    #[test]
    fn strict_parse_rejects_inline_input() {
        // The strict entry point (used by AOF replay) must NOT treat a non-RESP
        // leading byte as an inline command — it must surface a protocol error.
        let res = RESPParser::parse_value_strict(b"GARBAGE\r\n");
        assert!(matches!(res, Err(RespError::InvalidFormat(_))));
        // But it must still parse well-formed RESP frames identically.
        let (val, _) = RESPParser::parse_value_strict(b"*1\r\n$4\r\nPING\r\n").unwrap();
        assert_eq!(val, Value::Array(vec![Value::BulkString(b"PING".to_vec())]));
    }

    #[test]
    fn does_not_eagerly_allocate_for_declared_but_absent_elements() {
        // Declares 1M elements but provides none: must report Incomplete, not OOM.
        let frame = format!("*{}\r\n", MAX_MULTIBULK_LEN);
        assert!(matches!(
            RESPParser::parse_value(frame.as_bytes()),
            Err(RespError::Incomplete)
        ));
    }
}
