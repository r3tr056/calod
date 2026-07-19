use std::sync::{Arc, Weak};

use ahash::RandomState;
use calod::cld_srv::auth::AuthManager;
use calod::cld_srv::command::Command;
use calod::cld_srv::connection::{Connection, ConnectionConfig};
use calod::cld_srv::metrics::MetricsAggregator;
use calod::cld_srv::served_store::ServedCalodStore;
use calod::parser::parser::RESPParser;
use calod::parser::value::Value;
use dashmap::DashMap;
use tokio::sync::Mutex;
use uuid::Uuid;

type ClientId = Uuid;

async fn make_store_and_conn_with_password(
    password: &str,
) -> (
    Arc<ServedCalodStore>,
    Arc<Mutex<Connection>>,
    Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
) {
    let store = Arc::new(ServedCalodStore::new().await);
    let metrics = Arc::new(MetricsAggregator::new());
    let auth = Arc::new(AuthManager::new(true));
    auth.add_password(password);
    let conn = Arc::new(Mutex::new(Connection::new(
        store.clone(),
        ConnectionConfig::default(),
        metrics,
        auth,
    )));
    let active = Arc::new(DashMap::with_hasher(ahash::RandomState::new()));
    (store, conn, active)
}

#[tokio::test]
async fn auth_command_returns_ok_for_valid_password() {
    let (store, conn, active) = make_store_and_conn_with_password("s3cr3t").await;
    let raw = store
        .execute(
            Command::Auth {
                password: "s3cr3t".to_string(),
            },
            active,
            conn,
            calod::cld_srv::protocol::ConnCtx::default(),
        )
        .await;
    let (val, _) = RESPParser::parse_value(&raw).expect("valid resp");
    assert_eq!(val, Value::SimpleString("OK".to_string()));
}
