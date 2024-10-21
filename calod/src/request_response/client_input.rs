
use std::borrow::Borrow;
use std::io::{Error, ErrorKind, Read, Write};

use crate::request_response::{command::Command, parsed_command::ParsedCommand, response_helper};
use crate::store::calod_store::{CalodStore, Store};
use crate::store::calod_ops::SetOptionalArgs;
use crate::parser::parser::{Parser, ParserResult, ParserError, RESPOutput};

pub struct ClientInput {
	input: Vec<u8>,
}

pub trait HandleClientInput {
	fn parse_input(&mut self, buffer: &[u8]) -> Result<RESPOutput, ParserError>;

	fn respond<T: Write>(&self, stream: &mut T, parsed: ParsedCommand);

	fn respond_error<T: Write>(&self, stream: &mut T, error: &str);

	fn reset(&mut self);
}

impl HandleClientInput for ClientInput {
	fn parse_input(&mut self, buffer: &[u8]) -> Result<RESPOutput, ParserError> {
		self.append_input(&buffer);
		let parsed = Parser::parse_resp(self.get_input());
		match parsed {
			Ok(res) => Ok(res.0),
			Err(e) => Err(e),
		}
	}

	fn respond<T: Write>(&self, stream: &mut T, parsed: ParsedCommand) {
		let args = parsed.args();
		let command = parsed.command();

		if command.is_none() {
			return response_helper::send_error_response(stream, "Unrecognized command");
		}

		let command_unwrapped = command.as_ref().unwrap();

		if command_unwrapped == &Command::PING {
			response_helper::send_pong_response(stream);
		} else if command_unwrapped == &Command::ECHO {
			let mut result = String::from("");

			for arg in args.iter() {
				let str: &str = arg.borrow();
				result.push_str(str);
			}
			response_helper::send_bulk_string_response(stream, Some(&result));
		} else if command_unwrapped == &Command::GET {
			let key_expired_for_get = self.get_key_and_expiry(args);
			if key_expired_for_get.is_none() {
				response_helper::send_bulk_string_response(stream, None);
				return;
			}

			let KeyValueExpiry {
				key,
				value,
				is_expired,
			} = key_expired_for_get.unwrap();

			if is_expired {
				self.delete_expired_keys(vec![&key]);
				response_helper::send_bulk_string_response(stream, None);
			} else {
				let value = value.as_ref().map(|v| &**v);
				response_helper::send_bulk_string_response(stream, value);
			}
		} else if command_unwrapped == &Command::SET {
			let store = &mut CalodStore::get_store();

			let key = args.get(0).unwrap();
			let value = args.get(1).unwrap();
			let optional_args = self.determine_set_optional_args(args);

			store.set(key, value, &optional_args);
			response_helper::send_bulk_string_response(stream, Some("OK"));
		}
	}

	fn response_error<T: Write>(&self, stream: &mut T, error: &str) {
		response_helper::send_error_response(stream, error);
	}

	fn reset(&mut self) {
		self.input = Vec::new();
	}
}

impl ClientInput {
	pub fn new() -> ClientInput {
		ClientInput { input: Vec::new(), }
	}

	pub fn get_input(&self) -> &[u8] {
		self.input.as_slice()
	}

	fn append_input(&mut self, input: &[u8]) {
		self.input.extend_from_slice(input);
	}

	fn determine_set_optional_args(&self, args: &Vec<String>) -> Option<SetOptionalArgs> {
		let mut optional_args: Option<SetOptionalArgs> = None;

		if args.len() != 4 {
			return optional_args;
		}

		let variant = args.get(2).unwrap();
		let duration = args.get(3).unwrap();
		let mut duration_ms: u64 = 0;

		if variant.to_lowercase() == "ex" || variant.to_lowercase() == "px" {
			duration_ms = match duration.parse() {
				Ok(d) => d,
				Err(e) => {
					println!("Error parsing duration: {}", e);
					0
				}
			};

			if variant.to_lowercase() == "ex" {
				duration_ms *= 1000;
			}
		}

		if duration_ms != 0 {
			optional_args = Some(SetOptionalArgs {
				expire_in_ms: Some(duration_ms),
			});
		}
		optional_args
	}

	fn delete_expired_keys(&self, keys: Vec<&str>) {
		let store = &mut CalodStore::get_store();
		store.delete(keys);
	}
}