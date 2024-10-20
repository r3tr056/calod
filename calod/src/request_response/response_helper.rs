

use std::io::Write;
use crate store::calod_store::{CalodStore};


pub fn send_bulk_string_response<T: Write>(stream: &mut T, data: Option<&str>) {
	let response: String;
	if data.is_some() {
		let str = data.unwrap();
		response = format!("${}\r\n{}\r\n", str.len(), str);
	} else {
		response = String::from("$-1\r\n");
	}

	match stream.write(response.as_bytes()) {
		Ok(t) => {
			println!("Wrote {} bytes to output", t);
		},
		Err(e) => {
			println!("unable to write to response: {}", e);
		}
	}
}

pub fn send_pong_response<T: Write>(stream: &mut T) {
	let store = &mut CalodStore::get_store();
	let response = store.get_stats();

	let full_response = format!("PONG\nStats: {}", response);
	send_simple_string_response(stream, full_response);
}

pub fn send_simple_string_response<T: Write>(stream: &mut T, str: &str) {
	match stream.write(format_simple_string_response(str).as_bytes()) {
		Ok(t) => { println!("Wrote {} bytes to output", t); },
		Err(e) => { println!("unable to write to response: {}", e); },
	}
}

pub fn send_error_response<T: Write>(stream: &mut T, str: &str) {
	match stream.write(format_error_response(str).as_bytes()) {
		Ok(t) => { println!("Wrote {} bytes to output", t); },
		Err(e) => { println!("unable to write to response: {}", e); }
	}
}

pub fn format_simple_string_response(res: &str) -> String {
	format!("+{}\r\n", res)
}

pub fn format_error_response(res: &str) -> String {
	format!("+{}\r\n", res)
}

pub fn format_error_response(res: &str) -> String { format!("-{}\r\n", res) }