//! End-to-end GEO coverage via redis-rs (the classic Sicily dataset).

mod common;

use common::TestServer;

#[tokio::test]
async fn geo_add_dist_pos_search() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let added: i64 = redis::cmd("GEOADD")
        .arg("Sicily")
        .arg(13.361389)
        .arg(38.115556)
        .arg("Palermo")
        .arg(15.087269)
        .arg(37.502669)
        .arg("Catania")
        .query_async(&mut conn)
        .await
        .expect("geoadd");
    assert_eq!(added, 2);

    // Palermo<->Catania ~166.274 km.
    let dist_m: f64 = redis::cmd("GEODIST")
        .arg("Sicily")
        .arg("Palermo")
        .arg("Catania")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!((dist_m - 166_274.0).abs() < 1500.0, "dist_m = {dist_m}");

    let dist_km: f64 = redis::cmd("GEODIST")
        .arg("Sicily")
        .arg("Palermo")
        .arg("Catania")
        .arg("km")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!((dist_km - 166.27).abs() < 1.5, "dist_km = {dist_km}");

    // GEOPOS recovers the coordinates (within geohash precision).
    let pos: Vec<Option<(f64, f64)>> = redis::cmd("GEOPOS")
        .arg("Sicily")
        .arg("Palermo")
        .query_async(&mut conn)
        .await
        .unwrap();
    let (lon, lat) = pos[0].expect("Palermo present");
    assert!((lon - 13.361389).abs() < 0.001, "lon = {lon}");
    assert!((lat - 38.115556).abs() < 0.001, "lat = {lat}");

    // GEOSEARCH from a point near Catania, ascending by distance.
    let found: Vec<String> = redis::cmd("GEOSEARCH")
        .arg("Sicily")
        .arg("FROMLONLAT")
        .arg(15.0)
        .arg(37.0)
        .arg("BYRADIUS")
        .arg(250)
        .arg("km")
        .arg("ASC")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(found, vec!["Catania", "Palermo"]);

    server.shutdown().await;
}
