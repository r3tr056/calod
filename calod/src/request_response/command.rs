
#[derive(Debug)]
pub enum Command {
	// Generic
	Keys { pattern: String },
	Exists { key: String },
	Expire { key: String, seconds: u64 },
	Ttl { key: String },
	Persist { key: String },
	Scan { cursor: u64, pattern: String, count: u64 },
	Del { key: String },
	Info { section: String },

	// strings/numbers
	Set { key: String, value: String },
	Get { key: String },
	MGet { key: String },
	Incr { key: String },
	Decr { key: String },
	
	// hashes
	HSet { key: String, field: String, value: String },
	HGet { key: String, field: String },
	HGetAll { key: String },
	HDel { key: String, field: String },
	HMGet { key: String, field: String },

	// Sets
	XAdd { key: String, member: String },
	XRead { key: String },
	XDel { key: String },
	XTrim { key: String, maxlen: u64 },
	XRem { key: String, member: String },

	// sorted sets
	ZAdd { key: String, score: f64, memeber: String },
	ZRange { key: String, start: isize, stop: isize },


	// Lists
	ListLPush { key: String, value: String },
	ListRPush { key: String, value: String },
	ListLRange { key: String, start: isize, end: isize },

	// Shared between Lists and Streams
	LLen { key: String },
	LPop { key: String },
	RPop { key: String },

	// Streams
	StreamXAdd { key: String, value: String },
	StreamXRead { key: String, value: String },
	StreamXRange { key: String, start: isize, end: isize },

	JsonSet { key: String, path: String, value: serde_json::Value },
	JsonGet { key: String, path: String },
}


impl Command {
	pub fn from(str: &str, args: Vec<&str>) -> Result<Command, String> {
		match input.to_uppercase().as_str() {
			// Generic Commands
			"KEYS" if args.len() == 1 => Ok(Command::Keys { pattern: args[0].to_string() }),
			"EXISTS" if args.len() == 1 => Ok(Command::Keys { key: args[0].to_string() }),
			"EXPIRE" if args.len() == 2 => {
				let seconds = args[1].parse().map_err(|_| "Invalid seconds".to_string())?;
				Ok(Command::Expire { key: args[0].to_string(), seconds })
			},
			"TTL" if args.len() == 1 => Ok(Command::Ttl { key: args[0].to_string() }),
			"PERSIST" if args.len() == 1 => Ok(Command::Persist { key: args[0].to_string() }),
			"SCAN" if args.len() == 3 => {
				let cursor = args[0].parse().map_err(|_| "Invalid cursor".to_string())?;
				let count = args[2].parse().map_err(|_| "Invalid count".to_string())?;
				Ok(Command::Scan { cursor, pattern: args[1].to_string(), count })
			}

			"DEL" if args.len() == 1 => Ok(Command::Del { key: args[0].to_string() }),
			"INFO" if args.len() == 1 => Ok(Command::Info { section: args[0].to_string() }),

			// Strings/Numbers
			"SET" if args.len() == 2 => Ok(Command::Set { key: args[0].to_string(), value: args[1].to_string() }),
			"GET" if args.len() == 1 => Ok(Command::Get { key: args[0].to_string() }),
			// TODO : Fix MGET
			"MGET" => Ok(Command::MGet { key: args[0].to_string() }),
			"INCR" if args.len() == 1 => Ok(Command::Incr { key: args[0].to_string() }),
			"DECR" if args.len() == 1 => Ok(Command::Decr { key: args[0].to_string() }),

			// Hashes
			"HSET" if args.len() == 3 => Ok(Command::HSet { key: args[0].to_string(), field: args[1].to_string(), value: args[2].to_string() }),
			"HGET" if args.len() == 2 => Ok(Command::HGet { key: args[0].to_string(), field: args[1].to_string() }),
			"HGETALL" if args.len() == 1 => Ok(Command::HGetAll { key: args[0].to_string() }),
			"HDEL" if args.len() == 2 => Ok(Command::HDel { key: args[0].to_string(), field: args[1].to_string() }),
			"HMGET" if args.len() >= 2 => Ok(Command::HMGet { key: args[0].to_string(), field: args[1].to_string() }),

			// Sets
			"XADD" if args.len() == 2 => Ok(Command::XAdd { key: args[0].to_string(), member: args[1].to_string()})
			"XREAD" if args.len() == 1 => Ok(Command::XRead { key: args[0].to_string() }),
            "XDEL" if args.len() == 1 => Ok(Command::XDel { key: args[0].to_string() }),
            "XTRIM" if args.len() == 2 => {
                let maxlen = args[1].parse().map_err(|_| "Invalid maxlen".to_string())?;
                Ok(Command::XTrim { key: args[0].to_string(), maxlen })
            },
            "XREM" if args.len() == 2 => Ok(Command::XRem { key: args[0].to_string(), member: args[1].to_string() }),

            // Sorted Sets
            "ZADD" if args.len() == 3 => {
                let score = args[1].parse().map_err(|_| "Invalid score".to_string())?;
                Ok(Command::ZAdd { key: args[0].to_string(), score, memeber: args[2].to_string() })
            },
            "ZRANGE" if args.len() == 3 => {
                let start = args[1].parse().map_err(|_| "Invalid start".to_string())?;
                let stop = args[2].parse().map_err(|_| "Invalid stop".to_string())?;
                Ok(Command::ZRange { key: args[0].to_string(), start, stop })
            },

            // Lists
            "LPUSH" if args.len() == 2 => Ok(Command::ListLPush { key: args[0].to_string(), value: args[1].to_string() }),
            "RPUSH" if args.len() == 2 => Ok(Command::ListRPush { key: args[0].to_string(), value: args[1].to_string() }),
            "LRANGE" if args.len() == 3 => {
                let start = args[1].parse().map_err(|_| "Invalid start".to_string())?;
                let end = args[2].parse().map_err(|_| "Invalid end".to_string())?;
                Ok(Command::ListLRange { key: args[0].to_string(), start, end })
            },

            // Streams
            "XADD" if args.len() == 2 => Ok(Command::StreamXAdd { key: args[0].to_string(), value: args[1].to_string() }),
            "XRANGE" if args.len() == 3 => {
                let start = args[1].parse().map_err(|_| "Invalid start".to_string())?;
                let end = args[2].parse().map_err(|_| "Invalid end".to_string())?;
                Ok(Command::StreamXRange { key: args[0].to_string(), start, end })
            },

            // JSON
            "JSON.SET" if args.len() == 3 => {
                let value: serde_json::Value = serde_json::from_str(args[2]).map_err(|e| e.to_string())?;
                Ok(Command::JsonSet { key: args[0].to_string(), path: args[1].to_string(), value })
            },
            "JSON.GET" if args.len() == 2 => Ok(Command::JsonGet { key: args[0].to_string(), path: args[1].to_string() }),

            _ => Err(format!("Unknown command or incorrect arguments: {}", input)),
		}
	}
}
