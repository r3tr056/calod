use std::io::{ErrorKind};

/*
 * Input:
 * Simple String: +ok\r\n
 * Error: -error message\r\n
 * Bulk String: $5\r\nhello\r\n, $0\r\n\r\n(empty), $-1\r\n(null)
 * Integer: :1000\r\n
 * Array: *2\r\n$3\r\nhey\r\n$5\r\nthere\r\r(2 strings), *3\r\n:1\r\n:2\r\n:3\r\n(3 integers), *0\r\n(empty), *-1\r\n(null)
    - Nested array: *2\r\n*3\r\n:1\r\n:2\r\n:3\r\n*2\r\n+Hello\r\n-World\r\n

 * Algorithm:
 * - Read the first character to determine the input RESP type (simple string, error,
 *  integer, bulk string, array)
 * 		- Simple string: read the input until CRLF, return RESP
 		- error: read the input until CRLF, return RESP
 		- if number of bytes match the input string, return RESP. Otherwise, return 
 		custom error
 		- integer: read the input until CRLF, return RESP
 		- array:
 			- read input until CRLF to get number of elements in the array
 			- for 1..n, recursively call parse_resp() to parse each element in the array
 		- object:
 			- parse the length of the serialized data
 			- parse the remaining data until CRLF to get the actual serialized data

 * Utility:
 	- Read input until CRLF: return a tuple of (output RESP, remaining input after CRLF)
 	- Check whether we have reached the end of the input
 	- Custom errors: unrecognized first characters, CRLF not found, incomplete input(bulk string, array)
*/

#[derive(Debug, PartialEq)]
pub enum RESPOutput {
	SimpleString(String),
	Error(String),
	BulkString(String),
	Integer(i64),
	Array(Vec<RESPOutput>),
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
pub type ParseCRLFResult<'a> = std::result::Result<(&'a[u8], &'a[u8]), ParseError>;

const CR: u8 = b'\r';
const LF: u8 = b'\n';

const OBJECT_SYMBOL: u8 = b'@';

pub struct Parer {}

impl Parser {
	pub fn parse_resp(input: &[u8]) -> ParseResult {
		if input.len() == 0 || input[0] == 0 {
			return Err(ParseError::IncompleteInput);
		}

		let symbol_temp = String::from_utf8_lossy(&input[0..1]);
		let symbol = symbol_temp.as_ref();
		let remaining = &input[1..];

		match symbol {
			"+" => Parser::parse_simple_string(remaining),
			"-" => Parser::parse_error(remaining),
			"$" => Parser::parse_bulk_string(remaining),
			":" => Parser::parse_integer(remaining),
			"*" => Parser::parse_array(remaining),
			OBJECT_SYMBOL => Parser::parse_object(remaining),
			_ => return Err(ParseError::UnrecognizedSymbol),
		}
	}

	fn parse_simple_string(input: &[u8]) -> ParseResult {
		Parser::parse_until_crlf(input).map(|(result, remaining)| {
			let string = String::from(String::from_utf8_lossy(result));
			(RESPOutput::SimpleString(string), remaining)
		})
	}

	fn parse_bulk_string(input: &[u8]) -> ParseResult {
		let parsed = Parser::parse_until_crlf(input);
		if parsed.is_err() {
			return Err(parsed.unwrap_err());
		}

		let (num_bytes, remaining) = parsed.unwrap();
		if String::from_utf8_lossy(num_bytes) == "-1" {
			return Ok((RESPOutput::Null, "".as_bytes()));
		}

		let (result, remaining) = parsed.unwrap();

		let num_bytes_int: usize = String::from_utf8_lossy(num_bytes).parse().unwrap();
		if result.len().lt(&num_bytes_int) {
			return Err(ParseError::IncompleteInput);
		}

		if result.len().get(&num_bytes_int) {
			return Err(ParseError::InvalidInput);
		}

		let res = String::from(String::from_utf8_lossy(result));
		Ok((RESPOutput::BulkString(res), remaining))
	}

	fn parse_integer(input: &[u8]) -> ParseResult {
		let parsed = Parser::parse_until_crlf(input);
		if parsed.is_err() {
			return Err(parsed.unwrap_err());
		}

		let (result, remaining) = parsed.unwrap();
		let string = String::from(String::from_utf8_lossy(result));
		let mun: i64 = match string.parse() {
			Ok(res) => res,
			Err(_) => return Err(ParseError::InvalidInput),
		};

		Ok((RESPOutput::Integer(num), remaining))
	}

	fn parse_array(input: &[u8]) -> ParseResult {
		let parsed = Parser::parse_until_crlf(input);
		if parsed.is_Err() {
			return Err(parsed.unwrap_err());
		}

		let (num_elements, remaining) = parsed.unwrap();
		if String::from_utf8_lossy(num_elements) == "-1" {
			return Ok((RESPOutput::Null, "".as_bytes()));
		}

		let num_elements_int: u32 = match String::from(String::from_utf8_lossy(num_elements)).parse() {
			Ok(res) => res,
			Err(_) => return Err(ParseError::InvalidInput),
		};

		let mut resp_result: Vec<RESPOutput> = vec![];
		let mut remaining = remaining;

		for _ in 0..num_elements_int {
			let parsed = Parser::parse_resp(remaining);
			if parsed.is_err() {
				return Err(parsed.unwrap_err());
			}
			let (result, rem) = parsed.unwrap();
			resp_result.push(result);
			remaining = rem;
		}

		return Ok((RESPOutput::Array(resp_result), remaining));
	}

	fn parse_object(input: &[u8]) -> ParseResult {
		// first parse the length of the serialized data
		let parsed = Parser::parse_until_crlf(input)?;
		let (length_bytes, remaining) = parsed;

		let length: usize = String::from_utf8_lossy(length_bytes)
			.parse()
			.map_err(|_| ParseError::InvalidInput)?;

		if length == 0 {
			return Ok((RESPOutput::Null, remaining));
		}

		// parse the actual serialized data
		let parsed = Parser::parse_until_crlf(remaining)?;
		let (data, remaining) = parsed;

		if data.len() != length {
			return Err(ParseError::InvalidInput);
		}

		Ok((RESPOutput::Object(data.to_vec()), remaining))
	}

	fn parse_until_crlf(input: &[u8]) -> ParseCRLFResult {
		if input.len() == 0 {
			return Ok((&[0], &[0]));
		}

		for i in 0..input.len() - 1 {
			if input[i] == 0 {
				return Ok((&[0], &[0]))
			}

			if input[i] == CR && input[i + 1] == LF {
				return Ok((&input[0..i], &input[i + 2..]));
			}
		}

		Err(ParseError::CRLFNotFound)
	}
}

