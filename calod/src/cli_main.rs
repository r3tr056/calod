

use crate::store::calod_data::{CacheEntry, DataType}
use crate::store::calod_store::{CalodStore, SetOptionalArgs};
use crate::request_response::command::{Command, parse_command};

fn climain() {
	let store_file = "calod_store.json";
	let store = Arc::new(RwLock::new(CalodStore::load_from_file(store_file).unwrap_or_else(|_| {
		println!("Creating a new store");
		CalodStore::new(100)
	})));

	loop {
		print!("> ");
		io::stdout().flush().unwrap();

		let mut input = String::new();
		io::stdin().read_line(&mut input).unwrap();

		match parse_command(&input) {
			Ok(Command::PING) => {
				println!("PONG");
			}

			Ok(Command::ECHO(msg)) => {
				println!("{}", msg);
			}

			Ok(Command::SET(key, value)) => {
				let mut store = store.write().unwrap();

				let data_type_value = if value.starts_with("[") && value.ends_with("]") {
					let items: Vec<&str> = value[1..value.len() - 1].split(",").collect();
					DataType::List(items.iter().map(|&s| s.trim().to_string()).collect())
				} else if value.starts_with("{") && value.ends_with("}") {
					let mut hash = HashMap::new();
					let pairs: Vec<&str> = value[1..value.len() - 1].split(",").collect();
					for pair in pairs {
						let kv: Vec<&str> = pair.split(":").collect();
						if kv.len() == 2 {
							hash.insert(kv[0].trim().to_string(), kv[1].trim().to_string());
						}
					}

					DataType::Hash(hash)
				} else {
					DataType::String(value.clone())
				};

				store.data.insert(key.clone(), CacheEntry {
					value: data_type_value,
					frequency: 1,
					last_accessed: Utc::now(),
					ttl: None,
				});

				println!("Set key: {} to value: {:?}", key, data_type_value);
				store.save_to_file(store_file).expect("Failed to save store");
			}

			OK(Command::GEt(key)) => {
				let store = store.read().unwrap();
				if let Some(entry) = store.data.get(&key) {
					println!("Value: {:?}", entry.value);
				} else {
					println!("Key not found");
				}
			}
		}
	}
}