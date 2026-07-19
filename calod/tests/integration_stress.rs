mod common;

use calod::cld_srv::command::Command;
use common::Harness;

#[tokio::test]
async fn concurrent_set_get_stress_smoke() {
    let h = Harness::new().await;
    let h = std::sync::Arc::new(h);

    let mut joins = Vec::new();
    for worker in 0..16usize {
        let hh = h.clone();
        joins.push(tokio::spawn(async move {
            for i in 0..200usize {
                let key = format!("stress:{worker}:{i}");
                let value = format!("val:{i}");

                let _ = hh
                    .exec(Command::Set {
                        key: key.clone(),
                        value: value.clone().into_bytes(),
                        expire_option: None,
                        set_option: None,
                    })
                    .await;

                let _ = hh.exec(Command::Get { key }).await;
            }
        }));
    }

    for j in joins {
        j.await.expect("worker task join");
    }
}
