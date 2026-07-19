mod common;

use calod::cld_srv::command::Command;
use common::Harness;

#[tokio::test]
async fn mutating_commands_execute_without_regression() {
    let h = Harness::new().await;

    let _ = h
        .exec(Command::Set {
            key: "aof-key".to_string(),
            value: b"1".to_vec(),
            expire_option: None,
            set_option: None,
        })
        .await;

    let _ = h
        .exec(Command::HSet {
            key: "aof-hash".to_string(),
            field_values: vec![(b"f".to_vec(), b"v".to_vec())],
        })
        .await;

    let _ = h
        .exec(Command::ListLPush {
            key: "aof-list".to_string(),
            values: vec![b"x".to_vec(), b"y".to_vec()],
        })
        .await;

    let _ = h
        .exec(Command::JsonSet {
            key: "aof-json".to_string(),
            path: "$".to_string(),
            value: "{}".to_string(),
        })
        .await;

    let _ = h
        .exec(Command::Rename {
            key: "aof-key".to_string(),
            new_key: "aof-key-2".to_string(),
        })
        .await;

    let _ = h
        .exec(Command::Unlink {
            keys: vec!["aof-key-2".to_string()],
        })
        .await;
}
