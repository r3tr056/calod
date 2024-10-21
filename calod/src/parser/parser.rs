use std::io::{ErrorKind};

#[derive(Debug, PartialEq)]
pub enum RESPOutput {
	SimpleString(String),
	Error(String),
	BulkString(String),
	Integer(i64),
	Array(Vec<RESPOutput>),
	Nullm
}

#[derive(Debug, PartialEq)]
pub enum ParseError {
	UnrecognizedSymbol,
	CRLFNotFound,
	IncompleteInput,
	InvalidInput,
}

pub type ParseResult<'a> = std::result::Result<(RESPOutput, &'a [u8]), ParseError>;
pub type ParseCRLFResult<'a> = std::result::Result<(&'a [u8], &'a [u8]), ParseError>;

const CR: u8 = b'\r';
const LF: u8 = b'\n';

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

