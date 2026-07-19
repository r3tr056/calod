use std::convert::TryFrom;

use calod::cld_srv::command::Command;
use calod::parser::parser::RESPParser;
use calod::parser::value::Value;

fn bulk(s: &str) -> Value {
    Value::BulkString(s.as_bytes().to_vec())
}

#[test]
fn parses_randomkey_touch_unlink_commands() {
    let randomkey = Command::try_from(&[bulk("RANDOMKEY")][..]).expect("RANDOMKEY should parse");
    assert!(matches!(randomkey, Command::RandomKey {}));

    let touch = Command::try_from(&[bulk("TOUCH"), bulk("k1"), bulk("k2")][..])
        .expect("TOUCH should parse");
    match touch {
        Command::Touch { keys } => assert_eq!(keys, vec!["k1".to_string(), "k2".to_string()]),
        other => panic!("expected TOUCH, got {other:?}"),
    }

    let unlink = Command::try_from(&[bulk("UNLINK"), bulk("a"), bulk("b")][..])
        .expect("UNLINK should parse");
    match unlink {
        Command::Unlink { keys } => assert_eq!(keys, vec!["a".to_string(), "b".to_string()]),
        other => panic!("expected UNLINK, got {other:?}"),
    }
}

#[test]
fn parses_json_arrpop_with_path_and_index() {
    let values = vec![
        Value::BulkString(b"JSON.ARRPOP".to_vec()),
        Value::BulkString(b"json:key".to_vec()),
        Value::BulkString(b"$.arr".to_vec()),
        Value::BulkString(b"1".to_vec()),
    ];

    let command = Command::try_from(values.as_slice()).expect("JSON.ARRPOP should parse");
    match command {
        Command::JsonArrPop { key, path, index } => {
            assert_eq!(key, "json:key");
            assert_eq!(path, "$.arr");
            assert_eq!(index, Some(1));
        }
        other => panic!("expected JSON.ARRPOP, got {other:?}"),
    }
}

#[test]
fn parses_rename_and_renamenx_commands() {
    let rename = Command::try_from(&[bulk("RENAME"), bulk("src"), bulk("dst")][..])
        .expect("RENAME should parse");
    match rename {
        Command::Rename { key, new_key } => {
            assert_eq!(key, "src");
            assert_eq!(new_key, "dst");
        }
        other => panic!("expected RENAME, got {other:?}"),
    }

    let renamenx = Command::try_from(&[bulk("RENAMENX"), bulk("src2"), bulk("dst2")][..])
        .expect("RENAMENX should parse");
    match renamenx {
        Command::RenameNx { key, new_key } => {
            assert_eq!(key, "src2");
            assert_eq!(new_key, "dst2");
        }
        other => panic!("expected RENAMENX, got {other:?}"),
    }
}

#[test]
fn validates_argument_errors_for_new_commands() {
    let err = Command::try_from(&[bulk("RANDOMKEY"), bulk("extra")][..]).unwrap_err();
    assert!(err.contains("RANDOMKEY command expects no arguments"));

    let err = Command::try_from(&[bulk("TOUCH")][..]).unwrap_err();
    assert!(err.contains("TOUCH command expects at least one argument"));

    let err = Command::try_from(&[bulk("UNLINK")][..]).unwrap_err();
    assert!(err.contains("UNLINK command expects at least one argument"));

    let err = Command::try_from(&[bulk("RENAME"), bulk("a")][..]).unwrap_err();
    assert!(err.contains("RENAME command expects exactly two arguments"));

    let err = Command::try_from(&[bulk("RENAMENX"), bulk("a")][..]).unwrap_err();
    assert!(err.contains("RENAMENX command expects exactly two arguments"));
}

#[test]
fn parses_new_expiry_commands() {
    let expireat = Command::try_from(&[bulk("EXPIREAT"), bulk("k"), bulk("1700000000")][..])
        .expect("EXPIREAT should parse");
    assert!(matches!(expireat, Command::ExpireAt { .. }));

    let pexpireat = Command::try_from(&[bulk("PEXPIREAT"), bulk("k"), bulk("1700000000000")][..])
        .expect("PEXPIREAT should parse");
    assert!(matches!(pexpireat, Command::PExpireAt { .. }));

    let expiretime =
        Command::try_from(&[bulk("EXPIRETIME"), bulk("k")][..]).expect("EXPIRETIME should parse");
    assert!(matches!(expiretime, Command::ExpireTime { .. }));

    let pexpiretime =
        Command::try_from(&[bulk("PEXPIRETIME"), bulk("k")][..]).expect("PEXPIRETIME should parse");
    assert!(matches!(pexpiretime, Command::PExpireTime { .. }));

    let err = Command::try_from(&[bulk("EXPIREAT"), bulk("k"), bulk("bad")][..]).unwrap_err();
    assert!(err.contains("EXPIREAT timestamp argument must be a valid unsigned integer"));
}

#[test]
fn parses_hash_field_value_pairs() {
    let hset = Command::try_from(&[bulk("HSET"), bulk("h"), bulk("f"), bulk("v")][..])
        .expect("HSET should parse one field-value pair");
    match hset {
        Command::HSet { key, field_values } => {
            assert_eq!(key, "h");
            assert_eq!(field_values, vec![(b"f".to_vec(), b"v".to_vec())]);
        }
        other => panic!("expected HSET, got {other:?}"),
    }

    let hmset = Command::try_from(
        &[
            bulk("HMSET"),
            bulk("h"),
            bulk("f1"),
            bulk("v1"),
            bulk("f2"),
            bulk("v2"),
        ][..],
    )
    .expect("HMSET should parse field-value pairs");
    match hmset {
        Command::HMSet { key, field_values } => {
            assert_eq!(key, "h");
            assert_eq!(
                field_values,
                vec![
                    (b"f1".to_vec(), b"v1".to_vec()),
                    (b"f2".to_vec(), b"v2".to_vec()),
                ]
            );
        }
        other => panic!("expected HMSET, got {other:?}"),
    }
}

#[test]
fn parses_info_command() {
    let info = Command::try_from(&[bulk("INFO")][..]).expect("INFO should parse");
    assert!(matches!(info, Command::Info { section: None }));

    let memory =
        Command::try_from(&[bulk("INFO"), bulk("memory")][..]).expect("INFO section should parse");
    assert!(matches!(
        memory,
        Command::Info {
            section: Some(section)
        } if section == "memory"
    ));
}

#[test]
fn parses_phase1_generic_keyspace_commands() {
    let copy = Command::try_from(&[bulk("COPY"), bulk("src"), bulk("dst")][..])
        .expect("COPY should parse");
    assert!(matches!(copy, Command::Copy { .. }));

    let copy_with_opts = Command::try_from(
        &[
            bulk("COPY"),
            bulk("src"),
            bulk("dst"),
            bulk("DB"),
            bulk("0"),
            bulk("REPLACE"),
        ][..],
    )
    .expect("COPY with options should parse");
    assert!(matches!(copy_with_opts, Command::Copy { .. }));

    let mov =
        Command::try_from(&[bulk("MOVE"), bulk("k"), bulk("0")][..]).expect("MOVE should parse");
    assert!(matches!(mov, Command::Move { .. }));

    let obj = Command::try_from(&[bulk("OBJECT"), bulk("ENCODING"), bulk("k")][..])
        .expect("OBJECT ENCODING should parse");
    assert!(matches!(obj, Command::ObjectEncoding { .. }));

    let wait =
        Command::try_from(&[bulk("WAIT"), bulk("1"), bulk("100")][..]).expect("WAIT should parse");
    assert!(matches!(wait, Command::Wait { .. }));

    let obj_refcount = Command::try_from(&[bulk("OBJECT"), bulk("REFCOUNT"), bulk("k")][..])
        .expect("OBJECT REFCOUNT should parse");
    assert!(matches!(obj_refcount, Command::ObjectRefCount { .. }));

    let obj_idletime = Command::try_from(&[bulk("OBJECT"), bulk("IDLETIME"), bulk("k")][..])
        .expect("OBJECT IDLETIME should parse");
    assert!(matches!(obj_idletime, Command::ObjectIdleTime { .. }));

    let err = Command::try_from(&[bulk("OBJECT"), bulk("UNKNOWN"), bulk("k")][..]).unwrap_err();
    assert!(err.contains("OBJECT supports ENCODING|REFCOUNT|IDLETIME"));

    let setnx =
        Command::try_from(&[bulk("SETNX"), bulk("a"), bulk("b")][..]).expect("SETNX should parse");
    assert!(matches!(setnx, Command::SetNx { .. }));

    let getdel = Command::try_from(&[bulk("GETDEL"), bulk("a")][..]).expect("GETDEL should parse");
    assert!(matches!(getdel, Command::GetDel { .. }));

    let getex = Command::try_from(&[bulk("GETEX"), bulk("a"), bulk("PERSIST")][..])
        .expect("GETEX PERSIST should parse");
    assert!(matches!(getex, Command::GetEx { .. }));

    let msetnx = Command::try_from(
        &[
            bulk("MSETNX"),
            bulk("k1"),
            bulk("v1"),
            bulk("k2"),
            bulk("v2"),
        ][..],
    )
    .expect("MSETNX should parse");
    assert!(matches!(msetnx, Command::MSetNx { .. }));

    let sadd = Command::try_from(&[bulk("SADD"), bulk("sk"), bulk("a"), bulk("b")][..])
        .expect("SADD should parse");
    assert!(matches!(sadd, Command::SAdd { .. }));

    let smembers =
        Command::try_from(&[bulk("SMEMBERS"), bulk("sk")][..]).expect("SMEMBERS should parse");
    assert!(matches!(smembers, Command::SMembers { .. }));

    let sismember = Command::try_from(&[bulk("SISMEMBER"), bulk("sk"), bulk("a")][..])
        .expect("SISMEMBER should parse");
    assert!(matches!(sismember, Command::SIsMember { .. }));

    let spop =
        Command::try_from(&[bulk("SPOP"), bulk("sk"), bulk("2")][..]).expect("SPOP should parse");
    assert!(matches!(spop, Command::SPop { .. }));

    let srandmember = Command::try_from(&[bulk("SRANDMEMBER"), bulk("sk"), bulk("-2")][..])
        .expect("SRANDMEMBER should parse");
    assert!(matches!(srandmember, Command::SRandMember { .. }));
}

#[test]
fn parses_resp_frames_for_new_commands_into_command_enum() {
    let frames = [
        (b"*1\r\n$9\r\nRANDOMKEY\r\n".as_slice(), "RANDOMKEY"),
        (
            b"*3\r\n$5\r\nTOUCH\r\n$2\r\nk1\r\n$2\r\nk2\r\n".as_slice(),
            "TOUCH",
        ),
        (
            b"*3\r\n$6\r\nUNLINK\r\n$1\r\na\r\n$1\r\nb\r\n".as_slice(),
            "UNLINK",
        ),
        (
            b"*3\r\n$6\r\nRENAME\r\n$3\r\nsrc\r\n$3\r\ndst\r\n".as_slice(),
            "RENAME",
        ),
        (
            b"*3\r\n$8\r\nRENAMENX\r\n$3\r\nsrc\r\n$3\r\ndst\r\n".as_slice(),
            "RENAMENX",
        ),
    ];

    for (frame, name) in frames {
        let (value, consumed) = RESPParser::parse_value(frame).expect("valid RESP frame");
        assert_eq!(consumed, frame.len());
        let Value::Array(items) = value else {
            panic!("expected RESP array for {name}");
        };
        let cmd = Command::try_from(items.as_slice()).expect("command decode");
        match name {
            "RANDOMKEY" => assert!(matches!(cmd, Command::RandomKey {})),
            "TOUCH" => assert!(matches!(cmd, Command::Touch { .. })),
            "UNLINK" => assert!(matches!(cmd, Command::Unlink { .. })),
            "RENAME" => assert!(matches!(cmd, Command::Rename { .. })),
            "RENAMENX" => assert!(matches!(cmd, Command::RenameNx { .. })),
            _ => unreachable!(),
        }
    }
}
