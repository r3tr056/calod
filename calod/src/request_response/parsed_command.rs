

use crate::request_response::{command::Command};

#[derive(Debug, PartialEq)]
pub struct ParsedCommand {
	pub command: Option<Command>,
	pub args: Vec<String>,
}

impl ParsedCommand {
	pub fn new() -> ParsedCommand {
		ParsedCommand { command: None, args: Vec::new(), }
	}

	pub fn command(&self) -> &Option<Command> {
		&self.command
	}

	pub fn args(&self) -> &Vec<String> {
		&self.args
	}

	pub fn set_command(&mut self, command: Option<Command>) {
		self.command = command;
	}

	pub fn set_args(&mut self, args: Vec<String>) {
		self.args = args;
	}

	pub fn append_args(&mut self, arg: String) {
		self.args.push(arg);
	}
}