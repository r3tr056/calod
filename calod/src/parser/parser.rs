#[derive(Debug, PartialEq)]
pub enum RESPOutput {
    SimpleString(String),
    Error(String),
    BulkString(Option<String>),
    Integer(i64),
    Array(Option<Vec<RESPOutput>>),
    Null,
    // Serialized objects
    Object(Vec<u8>),
}

#[derive(Debug, PartialEq)]
pub enum ParseError {
    UnrecognizedSymbol,
    CRLFNotFound,
    IncompleteInput,
    InvalidInput,
    SerializationError(String),
}

pub type ParseResult<'a> = std::result::Result<(RESPOutput, &'a [u8]), ParseError>;

const CR: u8 = b'\r';
const LF: u8 = b'\n';

pub struct Parser {}

impl Parser {
    pub fn parse_resp(mut input: &[u8]) -> ParseResult {
        if input.is_empty() {
            return Err(ParseError::IncompleteInput);
        }

        let (resp, remaining) = match input[0] {
            b'+' => Parser::parse_simple_string(input)?,
            b'-' => Parser::parse_error(input)?,
            b'$' => Parser::parse_bulk_string(input)?,
            b':' => Parser::parse_integer(input)?,
            b'*' => Parser::parse_array(input)?,
            b'@' => Parser::parse_object(input)?,
            _ => return Err(ParseError::UnrecognizedSymbol),
        };

        input = remaining;

        Ok((resp, input))
    }

    fn parse_simple_string(input: &[u8]) -> ParseResult {
        let (remaining, _) = Parser::parse_crlf(&input[1..])?;
        let string = std::str::from_utf8(&input[1..input.len() - remaining.len()])
            .map_err(|_| ParseError::InvalidInput)?
            .to_owned();
        Ok((RESPOutput::SimpleString(string), remaining))
    }

    fn parse_error(input: &[u8]) -> ParseResult {
        let (remaining, _) = Parser::parse_crlf(&input[1..])?;
        let err_msg = std::str::from_utf8(&input[1..input.len() - remaining.len()])
            .map_err(|_| ParseError::InvalidInput)?
            .to_owned();

        Ok((RESPOutput::Error(err_msg), remaining))
    }

    fn parse_bulk_string(input: &[u8]) -> ParseResult {
        let (remaining_after_length, _) = Parser::parse_crlf(&input[1..])?;
        let length_str = std::str::from_utf8(&input[1..input.len() - remaining_after_length.len()])
            .map_err(|_| ParseError::InvalidInput)?;

        let length: i64 = length_str.parse().map_err(|_| ParseError::InvalidInput)?;

        if length < 0 {
            return Ok((RESPOutput::BulkString(None), remaining_after_length));
        }

        let length = length as usize;
        if remaining_after_length.len() < length + 2 {
            return Err(ParseError::IncompleteInput);
        }

        let (remaining, _) = Parser::parse_crlf(&remaining_after_length[length..])?;
        let bulk_string = std::str::from_utf8(&remaining_after_length[..length])
            .map_err(|_| ParseError::InvalidInput)?
            .to_owned();

        Ok((RESPOutput::BulkString(Some(bulk_string)), remaining))
    }

    fn parse_integer(input: &[u8]) -> ParseResult {
        let (remaining, _) = Parser::parse_crlf(&input[1..])?;
        let int_str = std::str::from_utf8(&input[1..input.len() - remaining.len()])
            .map_err(|_| ParseError::InvalidInput)?;

        let num: i64 = int_str.parse().map_err(|_| ParseError::InvalidInput)?;
        Ok((RESPOutput::Integer(num), remaining))
    }

    fn parse_array(input: &[u8]) -> ParseResult {
        let (remaining_after_count, _) = Parser::parse_crlf(&input[1..])?;
        let count_str = std::str::from_utf8(&input[1..input.len() - remaining_after_count.len()])
            .map_err(|_| ParseError::InvalidInput)?;

        let count: i64 = count_str.parse().map_err(|_| ParseError::InvalidInput)?;

        if count < 0 {
            return Ok((RESPOutput::Array(None), remaining_after_count));
        }

        let mut elements = Vec::with_capacity(count as usize);
        let mut remaining = remaining_after_count;
        for _ in 0..count {
            let (element, new_remaining) = Parser::parse_resp(remaining)?;
            elements.push(element);
            remaining = new_remaining;
        }

        return Ok((RESPOutput::Array(Some(elements)), remaining));
    }

    fn parse_object(input: &[u8]) -> ParseResult {
        let (remaining_after_length, _) = Parser::parse_crlf(&input[1..])?;
        let length_str = std::str::from_utf8(&input[1..input.len() - remaining_after_length.len()])
            .map_err(|_| ParseError::InvalidInput)?;

        let length: usize = length_str.parse().map_err(|_| ParseError::InvalidInput)?;
        if remaining_after_length.len() < length + 2 {
            return Err(ParseError::IncompleteInput);
        }

        let (remaining, _) = Parser::parse_crlf(&remaining_after_length[length..])?;

        Ok((
            RESPOutput::Object(remaining_after_length[..length].to_vec()),
            remaining,
        ))
    }

    fn parse_crlf(input: &[u8]) -> Result<(&[u8], &[u8]), ParseError> {
        if input.len() < 2 {
            return Err(ParseError::IncompleteInput);
        }
        if input[0] == CR && input[1] == LF {
            Ok((&input[2..], &input[..2]))
        } else {
            Err(ParseError::CRLFNotFound)
        }
    }
}
