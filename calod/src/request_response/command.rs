
#[derive(Debug, PartialEq)]
pub enum Command {
	PING,
	ECHO(String),
	GET(String),
	SET(String, String),
	DELETE(String),
	EXIT
}


impl Command {
	pub fn from(str: &str) -> Option<Command> {
		let mut command: Option<Command> = None;

		if str.to_lowercase() == "echo" {
			command = Some(Command::ECHO);
		} else if str.to_lowercase() == "ping" {
			command = Some(Command::PING);
		} else if str.to_lowercase() == "get" {
			command = Some(Command::GET);
		} else if str.to_lowercase() == "set" {
			command = Some(Command::SET);
		}

		command
	}
}


fn parse_command(input: &str) -> Result<Command, &str> {
	let parts: Vec<&str> = input.trim().split_whitespace().collect();
	match parts.as_slice() {
		["PING"] => Ok(Command::PING),
		["ECHO", msg @ ..] => {
			let message = msg.join(" ");
			Ok(Command::ECHO(message))
		}
		["SET", key, value] => Ok(Command::SET(key.to_string(), value.to_string())),
		["GET", key] => Ok(Command::GET(key.to_string())),
		["DELETE", key] => Ok(Command::DELETE(key.to_string())),
		["EXIT"] => Ok(Command::EXIT),
		_ => Err("Invalid command"),
	}
}