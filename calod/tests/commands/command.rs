#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn return_none_if_no_command_match() {
		let result = Command::from("random");
		assert!(result.is_none());
	}

	#[test]
	fn return_echo_command() {
		let result = Command::from("echo");
		assert!(result.is_some());
		assert_eq!(result.unwrap(), Command::ECHO);
	}

	#[test]
	fn return_ping_command() {
		let result = Command::from("ping");
		assert!(result.is_some());
		assert_eq!(result.unwrap(), Command::PING);
	}
}