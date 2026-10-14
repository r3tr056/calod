//! Phase P1c: automated backup-restore verification and crash durability.
//!
//! These tests drive the real `calod` binary as a child process against a
//! temp data directory:
//!
//! - `binary_value_every_byte_survives_kill_and_restart` — binary-safety
//!   regression: SET a value containing every byte 0x00-0xFF through a raw
//!   RESP frame, hard-kill the server, restart, and assert the exact bytes
//!   come back (the old AOF path corrupted them via `from_utf8_lossy`).
//! - `backup_restore_recovers_all_data_exactly` — write data across multiple
//!   logical DBs (including binary values), SAVE, copy the data directory
//!   (backup), start a fresh server against the copy (restore), and assert
//!   everything is recovered exactly.
//! - `kill_preserves_acknowledged_writes` — writes acknowledged under the
//!   `everysec` durability class (written to the OS buffer) must survive a
//!   kill -9 style termination and restart.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use redis::AsyncCommands;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, Command as ProcCommand};

fn temp_dir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("calod-backup-test-{}", uuid::Uuid::new_v4()));
    p
}

struct ServerProc {
    child: Child,
    port: u16,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Start the calod binary with persistence rooted at `data_dir` and wait for
/// it to answer PING.
async fn start_server(data_dir: &Path, fsync_strategy: &str) -> ServerProc {
    let port = free_port();
    let child = ProcCommand::new(env!("CARGO_BIN_EXE_calod"))
        .args(["--address", "127.0.0.1", "--port"])
        .arg(port.to_string())
        .current_dir(data_dir)
        .env("CALOD_PERSISTENCE__ENABLE_SNAPSHOT", "true")
        .env("CALOD_PERSISTENCE__SNAPSHOT_DIR", data_dir)
        .env("CALOD_PERSISTENCE__ENABLE_AOF", "true")
        .env("CALOD_PERSISTENCE__AOF_DIR", data_dir)
        .env("CALOD_PERSISTENCE__AOF_FSYNC_STRATEGY", fsync_strategy)
        // Keep the scheduled-snapshot loop out of these tests.
        .env("CALOD_PERSISTENCE__SNAPSHOT_INTERVAL_SECS", "3600")
        .env("CALOD_PERSISTENCE__SNAPSHOT_MIN_CHANGES", "100000")
        // These tests exercise the RESP listener only; keep the REST API off
        // so parallel tests never contend for its default port.
        .env("CALOD_API__ENABLED", "false")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn calod server");

    let server = ServerProc { child, port };
    wait_ready(&server).await;
    server
}

async fn wait_ready(server: &ServerProc) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match TcpStream::connect(("127.0.0.1", server.port)).await {
            Ok(mut stream) => {
                let _ = stream.write_all(b"*1\r\n$4\r\nPING\r\n").await;
                let mut buf = [0u8; 16];
                if tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
                    .await
                    .is_ok_and(|r| r.is_ok_and(|n| n > 0))
                {
                    return;
                }
            }
            Err(_) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "server did not become ready in time"
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Hard kill (no graceful shutdown, no flush): the durability contract under
/// test.
async fn hard_kill(server: &mut ServerProc) {
    server.child.kill().await.expect("kill server");
    server.child.wait().await.expect("wait for killed server");
}

fn redis_url(server: &ServerProc) -> String {
    format!("redis://127.0.0.1:{}/", server.port)
}

async fn redis_conn(server: &ServerProc) -> redis::aio::MultiplexedConnection {
    redis::Client::open(redis_url(server))
        .expect("redis client")
        .get_multiplexed_async_connection()
        .await
        .expect("redis connection")
}

/// Copy a directory tree recursively (the "backup").
fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create backup dir");
    for entry in std::fs::read_dir(src).expect("read source dir") {
        let entry = entry.expect("dir entry");
        let target = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

fn build_frame(args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Send one raw RESP frame and read one reply (simple string or bulk string).
async fn raw_cmd(stream: &mut TcpStream, args: &[&[u8]]) -> Vec<u8> {
    stream
        .write_all(&build_frame(args))
        .await
        .expect("write frame");
    let mut buf = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(reply) = try_parse_reply(&buf) {
            return reply;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for reply"
        );
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.expect("read reply");
        assert!(n > 0, "connection closed while reading reply");
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Parse a complete reply out of `buf`; returns the reply bytes (without the
/// type byte and framing for simple/bulk strings), or None if incomplete.
fn try_parse_reply(buf: &[u8]) -> Option<Vec<u8>> {
    let (&first, rest) = buf.split_first()?;
    match first {
        b'+' | b'-' => {
            let pos = rest.windows(2).position(|w| w == b"\r\n")?;
            Some(rest[..pos].to_vec())
        }
        b'$' => {
            let pos = rest.windows(2).position(|w| w == b"\r\n")?;
            let len: usize = std::str::from_utf8(&rest[..pos]).ok()?.parse().ok()?;
            let start = pos + 2;
            if rest.len() < start + len + 2 {
                return None;
            }
            Some(rest[start..start + len].to_vec())
        }
        other => panic!("unexpected reply type byte: {other:#x}"),
    }
}

#[tokio::test]
async fn binary_value_every_byte_survives_kill_and_restart() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).expect("temp dir");

    let every_byte: Vec<u8> = (0u8..=255).collect();
    let mut server = start_server(&dir, "always").await;

    // SET through a hand-built RESP frame: the value holds every possible
    // byte, including NUL, CR, LF and 0xFF.
    let mut stream = TcpStream::connect(("127.0.0.1", server.port))
        .await
        .expect("connect");
    let reply = raw_cmd(&mut stream, &[b"SET", b"bin:key", &every_byte]).await;
    assert_eq!(reply, b"OK", "SET must be acknowledged");
    drop(stream);

    // No graceful shutdown: kill -9 semantics.
    hard_kill(&mut server).await;

    let server = start_server(&dir, "always").await;
    let mut stream = TcpStream::connect(("127.0.0.1", server.port))
        .await
        .expect("connect");
    let value = raw_cmd(&mut stream, &[b"GET", b"bin:key"]).await;
    assert_eq!(
        value, every_byte,
        "binary value must come back byte-for-byte after kill + restart"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn backup_restore_recovers_all_data_exactly() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).expect("temp dir");

    let binary: Vec<u8> = (0u8..=255).rev().collect();
    let mut server = start_server(&dir, "everysec").await;

    {
        let mut con = redis_conn(&server).await;
        // DB 0: string, hash, set.
        let _: () = con.set("plain", "hello").await.expect("SET plain");
        let _: () = con.hset("hash", "field", "value").await.expect("HSET hash");
        let _: i64 = con.sadd("tags", &["a", "b", "c"]).await.expect("SADD");
        // DB 3: counter.
        let _: () = redis::cmd("SELECT")
            .arg(3)
            .query_async(&mut con)
            .await
            .expect("SELECT 3");
        let _: () = con.set("counter", 41).await.expect("SET counter");
        let _: i64 = con.incr("counter", 1).await.expect("INCR counter");
        // DB 5: binary value.
        let _: () = redis::cmd("SELECT")
            .arg(5)
            .query_async(&mut con)
            .await
            .expect("SELECT 5");
        let _: () = redis::cmd("SET")
            .arg("blob")
            .arg(&binary)
            .query_async(&mut con)
            .await
            .expect("SET blob");
        // Take the snapshot, then verify the manifest landed.
        let _: () = redis::cmd("SAVE")
            .query_async(&mut con)
            .await
            .expect("SAVE");
    }
    assert!(
        dir.join("manifest.json").exists(),
        "SAVE must write a manifest"
    );

    hard_kill(&mut server).await;

    // Backup: copy the whole data directory; restore into a fresh server.
    let backup = temp_dir();
    copy_dir(&dir, &backup);
    let server = start_server(&backup, "everysec").await;

    {
        let mut con = redis_conn(&server).await;
        let plain: String = con.get("plain").await.expect("GET plain");
        assert_eq!(plain, "hello");
        let field: String = con.hget("hash", "field").await.expect("HGET hash");
        assert_eq!(field, "value");
        let mut tags: Vec<String> = con.smembers("tags").await.expect("SMEMBERS");
        tags.sort();
        assert_eq!(tags, ["a", "b", "c"]);

        let _: () = redis::cmd("SELECT")
            .arg(3)
            .query_async(&mut con)
            .await
            .expect("SELECT 3");
        // The counter is `integer`-typed after INCR (GET on it is WRONGTYPE
        // in this server); prove the value survived by incrementing it.
        let counter: i64 = con.incr("counter", 1).await.expect("INCR counter");
        assert_eq!(counter, 43, "INCR across restart in DB 3");

        let _: () = redis::cmd("SELECT")
            .arg(5)
            .query_async(&mut con)
            .await
            .expect("SELECT 5");
        let blob: Vec<u8> = con.get("blob").await.expect("GET blob");
        assert_eq!(blob, binary, "binary blob in DB 5 must round-trip exactly");
    }

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&backup);
}

#[tokio::test]
async fn kill_preserves_acknowledged_writes() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).expect("temp dir");

    // everysec: an acknowledged write is in the OS buffer, which survives a
    // process kill (this is the `appendfsync everysec` contract).
    let mut server = start_server(&dir, "everysec").await;

    {
        let mut con = redis_conn(&server).await;
        for i in 0..50 {
            let _: () = con
                .set(format!("durable:{i}"), format!("value-{i}"))
                .await
                .expect("SET must be acknowledged");
        }
        let _: i64 = con.incr("durable:counter", 7).await.expect("INCR");
        let _: () = redis::cmd("SELECT")
            .arg(2)
            .query_async(&mut con)
            .await
            .expect("SELECT 2");
        let _: () = con.set("durable:db2", "x").await.expect("SET in DB 2");
    }

    // Kill without any shutdown flush or SAVE.
    hard_kill(&mut server).await;

    let server = start_server(&dir, "everysec").await;

    {
        let mut con = redis_conn(&server).await;
        for i in 0..50 {
            let value: String = con
                .get(format!("durable:{i}"))
                .await
                .unwrap_or_else(|_| panic!("durable:{i} must survive the kill"));
            assert_eq!(value, format!("value-{i}"));
        }
        // The INCR-created counter holds an `integer`-typed entry (GET on it
        // is WRONGTYPE in this server, live or recovered); prove the value
        // survived by incrementing it again.
        let counter: i64 = con
            .incr("durable:counter", 1)
            .await
            .expect("INCR after restart");
        assert_eq!(counter, 8, "counter value must survive the kill");

        let _: () = redis::cmd("SELECT")
            .arg(2)
            .query_async(&mut con)
            .await
            .expect("SELECT 2");
        let value: String = con.get("durable:db2").await.expect("GET durable:db2");
        assert_eq!(value, "x", "acknowledged write in DB 2 must survive");
    }

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
