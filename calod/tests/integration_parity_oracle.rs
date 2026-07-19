mod common;

use calod::cld_srv::command::Command;
use calod::parser::value::Value;
use common::{bulk_to_string, Harness};

struct OracleCase {
    name: &'static str,
    command: Command,
    expected: Value,
}

fn cases() -> Vec<OracleCase> {
    vec![
        OracleCase {
            name: "ping",
            command: Command::Ping {},
            expected: Value::SimpleString("PONG".to_string()),
        },
        OracleCase {
            name: "set",
            command: Command::Set {
                key: "oracle:set:key".to_string(),
                value: b"v".to_vec(),
                expire_option: None,
                set_option: None,
            },
            expected: Value::SimpleString("OK".to_string()),
        },
        OracleCase {
            name: "get",
            command: Command::Get {
                key: "oracle:set:key".to_string(),
            },
            expected: Value::BulkString(b"v".to_vec()),
        },
        OracleCase {
            name: "exists",
            command: Command::Exists {
                keys: vec!["oracle:set:key".to_string(), "missing".to_string()],
            },
            expected: Value::Integer(1),
        },
        OracleCase {
            name: "touch",
            command: Command::Touch {
                keys: vec!["oracle:set:key".to_string(), "missing".to_string()],
            },
            expected: Value::Integer(1),
        },
        OracleCase {
            name: "rename",
            command: Command::Rename {
                key: "oracle:set:key".to_string(),
                new_key: "oracle:set:key2".to_string(),
            },
            expected: Value::SimpleString("OK".to_string()),
        },
        OracleCase {
            name: "renamenx",
            command: Command::RenameNx {
                key: "oracle:set:key2".to_string(),
                new_key: "oracle:set:key3".to_string(),
            },
            expected: Value::Integer(1),
        },
        OracleCase {
            name: "expire",
            command: Command::Expire {
                key: "oracle:set:key3".to_string(),
                seconds: 20,
            },
            expected: Value::Integer(1),
        },
    ]
}

#[tokio::test]
async fn parity_oracle_for_implemented_subset_is_stable() {
    let h = Harness::new().await;

    let cases = cases();
    let mut passed = 0usize;
    let total = cases.len();

    for case in cases {
        let got = h.exec(case.command).await;
        assert_eq!(got, case.expected, "oracle mismatch in {}", case.name);
        passed += 1;
    }

    assert_eq!(passed, total, "oracle cases did not fully pass");

    let random = h.exec(Command::RandomKey {}).await;
    match random {
        Value::BulkString(bs) => {
            let key = String::from_utf8_lossy(&bs);
            assert!(key.starts_with("oracle:set:key"));
        }
        other => panic!("expected bulk string from RANDOMKEY, got {other:?}"),
    }

    let cmd_list = h.exec(Command::CommandList {}).await;
    let Value::Array(items) = cmd_list else {
        panic!("expected command list array");
    };
    let list: Vec<String> = items.iter().filter_map(bulk_to_string).collect();
    for cmd in ["expireat", "pexpireat", "expiretime", "pexpiretime"] {
        assert!(
            list.contains(&cmd.to_string()),
            "missing {cmd} in command list"
        );
    }

    for cmd in ["copy", "move", "object", "wait"] {
        assert!(
            list.contains(&cmd.to_string()),
            "missing {cmd} in command list"
        );
    }

    for cmd in ["setnx", "getdel", "getex", "msetnx"] {
        assert!(
            list.contains(&cmd.to_string()),
            "missing {cmd} in command list"
        );
    }

    for cmd in ["mget", "mset", "append"] {
        assert!(
            list.contains(&cmd.to_string()),
            "missing {cmd} in command list"
        );
    }

    assert!(
        list.contains(&"getset".to_string()),
        "missing getset in command list"
    );

    for cmd in [
        "sadd",
        "srem",
        "smembers",
        "sismember",
        "scard",
        "spop",
        "srandmember",
    ] {
        assert!(
            list.contains(&cmd.to_string()),
            "missing {cmd} in command list"
        );
    }
}
